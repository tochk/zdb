//! Minimal stdio LSP client driving the bundled `sqls` server for schema-aware
//! SQL completion.
//!
//! zdb speaks just enough LSP to get completions: `initialize` (pipelined — we
//! don't block on its reply, sqls processes messages in order), full-text
//! `didOpen`/`didChange` document sync, and `textDocument/completion`. The
//! server runs as a child process; a blocking reader thread parses framed
//! JSON-RPC and completes a [`futures_channel::oneshot`] per request, so the
//! gpui-side [`CompletionProvider`] can `.await` a reply from its own executor.
//!
//! `sqls` opens its **own** Postgres connection to introspect (separate from
//! zdb's). Its password is passed via the `PGPASSWORD` environment variable, not
//! written to the on-disk config — matching zdb's keychain policy.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;

use futures_channel::oneshot;
use gpui::{AppContext as _, Context, Task, Window};
use gpui_component::input::{CompletionProvider, InputState};
use gpui_component::{Rope, RopeExt};
use lsp_types::{CompletionContext, CompletionResponse};
use serde_json::{json, Value};
use zdb_db::{ConnectionConfig, SslMode};

/// The active LSP handle, shared between the workspace (which fills it on
/// connect) and every SQL editor's completion provider. `None` when no server
/// is running (not connected, or `sqls` is not installed) — completion degrades
/// to nothing.
pub type LspSlot = Rc<RefCell<Option<LspHandle>>>;

pub fn new_slot() -> LspSlot {
    Rc::new(RefCell::new(None))
}

/// Cloneable handle to a running `sqls` subprocess. Dropping the last clone
/// kills the process and removes its temp config.
#[derive(Clone)]
pub struct LspHandle {
    inner: Arc<Inner>,
}

struct Inner {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    next_id: AtomicI64,
    version: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    config_path: PathBuf,
}

impl Inner {
    fn send(&self, msg: Value) {
        let body = msg.to_string();
        if let Ok(mut w) = self.stdin.lock() {
            let _ = write!(w, "Content-Length: {}\r\n\r\n{}", body.len(), body);
            let _ = w.flush();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Ok(mut c) = self.child.lock() {
            let _ = c.kill();
        }
        let _ = std::fs::remove_file(&self.config_path);
    }
}

impl LspHandle {
    /// Spawn `sqls -config <config_path>` and run the initialize handshake.
    /// `password`, if present, is passed via `PGPASSWORD` (never on disk).
    pub fn spawn(
        exe: &Path,
        config_path: PathBuf,
        password: Option<String>,
    ) -> std::io::Result<LspHandle> {
        let mut cmd = Command::new(exe);
        cmd.arg("-config")
            .arg(&config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(pw) = password {
            cmd.env("PGPASSWORD", pw);
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let inner = Arc::new(Inner {
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            next_id: AtomicI64::new(1),
            version: AtomicI64::new(1),
            pending: Mutex::new(HashMap::new()),
            config_path,
        });

        // The reader holds only a Weak ref so it never keeps the process alive:
        // when the last handle drops, Inner::drop kills the child, stdout hits
        // EOF, and the reader returns.
        let weak = Arc::downgrade(&inner);
        thread::Builder::new()
            .name("zdb-lsp-reader".into())
            .spawn(move || reader_loop(BufReader::new(stdout), weak))
            .ok();

        let handle = LspHandle { inner };
        let init_id = handle.inner.next_id.fetch_add(1, Ordering::Relaxed);
        handle.inner.send(json!({
            "jsonrpc": "2.0", "id": init_id, "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": null,
                "capabilities": { "textDocument": { "completion": {} } }
            }
        }));
        handle
            .inner
            .send(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
        Ok(handle)
    }

    pub fn did_open(&self, uri: &str, text: &str) {
        self.inner.send(json!({
            "jsonrpc": "2.0", "method": "textDocument/didOpen",
            "params": {"textDocument": {"uri": uri, "languageId": "sql", "version": 1, "text": text}}
        }));
    }

    pub fn did_change(&self, uri: &str, text: &str) {
        let v = self.inner.version.fetch_add(1, Ordering::Relaxed) + 1;
        self.inner.send(json!({
            "jsonrpc": "2.0", "method": "textDocument/didChange",
            "params": {"textDocument": {"uri": uri, "version": v}, "contentChanges": [{"text": text}]}
        }));
    }

    /// Request completion at `(line, character)` (UTF-16). The reply arrives as
    /// the raw `result` value on the returned channel.
    pub fn completion(&self, uri: &str, line: u32, character: u32) -> oneshot::Receiver<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().unwrap().insert(id, tx);
        self.inner.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "textDocument/completion",
            "params": {"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}
        }));
        rx
    }
}

fn reader_loop(mut r: BufReader<ChildStdout>, weak: Weak<Inner>) {
    loop {
        // Parse the Content-Length header block.
        let mut length = 0usize;
        loop {
            let mut line = String::new();
            match r.read_line(&mut line) {
                Ok(0) | Err(_) => return, // EOF or broken pipe
                Ok(_) => {}
            }
            let t = line.trim_end();
            if t.is_empty() {
                break;
            }
            if let Some(rest) = t.strip_prefix("Content-Length:") {
                length = rest.trim().parse().unwrap_or(0);
            }
        }
        if length == 0 {
            continue;
        }
        let mut buf = vec![0u8; length];
        if r.read_exact(&mut buf).is_err() {
            return;
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
            continue;
        };
        let Some(inner) = weak.upgrade() else {
            return; // handle dropped
        };
        let id = msg.get("id").and_then(Value::as_i64);
        let is_request = msg.get("method").is_some();
        match (id, is_request) {
            // Response to one of our requests.
            (Some(id), false) => {
                if let Some(tx) = inner.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(msg.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            // Server-to-client request: reply null so it doesn't block on us.
            (Some(id), true) => {
                inner.send(json!({"jsonrpc": "2.0", "id": id, "result": null}));
            }
            // Notifications (window/logMessage, publishDiagnostics, …): ignore.
            _ => {}
        }
    }
}

// ---- locating + configuring the server ----------------------------------

/// Find the `sqls` binary bundled next to the zdb executable, else on `PATH`.
pub fn sqls_path() -> Option<PathBuf> {
    let name = if cfg!(windows) { "sqls.exe" } else { "sqls" };
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let bundled = dir.join(name);
            if bundled.exists() {
                return Some(bundled);
            }
        }
    }
    // Fall back to PATH.
    which(name)
}

fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|p| p.join(name))
        .find(|p| p.exists())
}

fn ssl_str(mode: SslMode) -> &'static str {
    match mode {
        SslMode::Disable => "disable",
        SslMode::Prefer => "prefer",
        SslMode::Require => "require",
        SslMode::VerifyCa => "verify-ca",
        SslMode::VerifyFull => "verify-full",
    }
}

/// Write a temp `sqls` config for `cfg` (no password — that goes via env).
/// Values are JSON-quoted, which is valid YAML, to avoid injection.
pub fn write_config(cfg: &ConnectionConfig) -> std::io::Result<PathBuf> {
    let q = |s: &str| serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into());
    let yaml = format!(
        "connections:\n  - alias: zdb\n    driver: postgresql\n    proto: tcp\n    \
         user: {user}\n    host: {host}\n    port: {port}\n    dbName: {db}\n    \
         params:\n      sslmode: {ssl}\n",
        user = q(&cfg.user),
        host = q(&cfg.host),
        port = cfg.port,
        db = q(&cfg.dbname),
        ssl = ssl_str(cfg.ssl_mode),
    );
    let path = std::env::temp_dir().join(format!("zdb-sqls-{}.yml", std::process::id()));
    std::fs::write(&path, yaml)?;
    Ok(path)
}

// ---- completion provider -------------------------------------------------

/// A per-editor `CompletionProvider` that forwards to the shared `sqls` handle.
/// Each editor gets a unique document URI so the server tracks them separately.
pub struct SqlCompletion {
    slot: LspSlot,
    uri: String,
    opened: AtomicBool,
}

impl SqlCompletion {
    pub fn new(slot: LspSlot, tab_id: u64) -> Self {
        Self {
            slot,
            uri: format!("file:///zdb/tab-{tab_id}.sql"),
            opened: AtomicBool::new(false),
        }
    }
}

impl CompletionProvider for SqlCompletion {
    fn completions(
        &self,
        rope: &Rope,
        offset: usize,
        _trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut Context<InputState>,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        let empty = || -> Task<anyhow::Result<CompletionResponse>> {
            Task::ready(Ok(CompletionResponse::Array(vec![])))
        };
        let handle = match self.slot.borrow().as_ref() {
            Some(h) => h.clone(),
            None => return empty(),
        };
        let pos = rope.offset_to_position(offset);
        let text = rope.to_string();
        // Full-text sync: open once, then change on every request.
        if self.opened.swap(true, Ordering::Relaxed) {
            handle.did_change(&self.uri, &text);
        } else {
            handle.did_open(&self.uri, &text);
        }
        let rx = handle.completion(&self.uri, pos.line, pos.character);
        cx.background_spawn(async move {
            match rx.await {
                Ok(v) => Ok(serde_json::from_value::<CompletionResponse>(v)
                    .unwrap_or(CompletionResponse::Array(vec![]))),
                Err(_) => Ok(CompletionResponse::Array(vec![])),
            }
        })
    }

    fn is_completion_trigger(
        &self,
        _offset: usize,
        new_text: &str,
        _cx: &mut Context<InputState>,
    ) -> bool {
        new_text
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end: spawn the real `sqls` against the local Postgres, sync a doc,
    /// and confirm `textDocument/completion` returns items. Gated on the sqls
    /// binary (`ZDB_TEST_SQLS`) + the same Postgres env the db tests use, so it's
    /// a no-op in a bare `cargo test`.
    #[test]
    fn completion_end_to_end() {
        let Some(exe) = std::env::var_os("ZDB_TEST_SQLS").map(PathBuf::from) else {
            return;
        };
        let (Ok(host), Ok(user), Ok(db)) = (
            std::env::var("ZDB_TEST_HOST"),
            std::env::var("ZDB_TEST_USER"),
            std::env::var("ZDB_TEST_DB"),
        ) else {
            return;
        };
        let pw = std::env::var("ZDB_TEST_PASSWORD").ok();

        let mut cfg = ConnectionConfig::new("test".to_string(), host, db, user);
        cfg.ssl_mode = SslMode::Disable;
        let config_path = write_config(&cfg).expect("write config");
        let handle = LspHandle::spawn(&exe, config_path, pw).expect("spawn sqls");

        let uri = "file:///zdb_test.sql";
        let doc = "SELECT * FROM ";
        handle.did_open(uri, doc);
        // Give sqls a moment to build its schema cache, then request completion
        // at end-of-line (col 14, UTF-16) where table names are expected.
        let mut rx = handle.completion(uri, 0, doc.len() as u32);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            match rx.try_recv() {
                Ok(Some(v)) => {
                    let resp: CompletionResponse =
                        serde_json::from_value(v).expect("parse completion");
                    let n = match resp {
                        CompletionResponse::Array(a) => a.len(),
                        CompletionResponse::List(l) => l.items.len(),
                    };
                    assert!(n > 0, "expected completions, got none");
                    return;
                }
                Ok(None) => {
                    assert!(std::time::Instant::now() < deadline, "completion timed out");
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                Err(_) => panic!("completion channel cancelled"),
            }
        }
    }
}
