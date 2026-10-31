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
//!
//! sqls can die under us (v0.2.47 overflowed its stack on any `CASE` without an
//! `END` — i.e. while one is being typed). When its stdout closes, everything
//! still waiting resolves to "no completions" instead of hanging (a hung
//! request froze gpui-component's menu on screen), and the next request
//! starts a fresh server.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

use futures_channel::oneshot;
use gpui::App;
use gpui::{AppContext as _, Task, Window};
use gpui_component::input::CompletionProvider;
use gpui_component::{Rope, RopeExt};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    Documentation, Position, Range, TextEdit,
};
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

/// A server that died sooner than this after starting isn't restarted until
/// this long has passed — in case it dies on every request.
const RESTART_BACKOFF: Duration = Duration::from_secs(5);

/// Numbers each spawned server (its config file name, and which server a
/// document was opened on — see `SqlCompletion::opened`).
static SPAWNS: AtomicU64 = AtomicU64::new(0);

/// Cloneable handle to a running `sqls` subprocess. Dropping the last clone
/// kills the process and removes its temp config.
#[derive(Clone)]
pub struct LspHandle {
    inner: Arc<Inner>,
    /// What it was started from, to start it again (`restart`).
    exe: PathBuf,
    cfg: ConnectionConfig,
}

struct Inner {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    next_id: AtomicI64,
    version: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    config_path: PathBuf,
    /// Its stdout closed: the process is gone. Set under the `pending` lock.
    dead: AtomicBool,
    started: Instant,
    generation: u64,
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
    /// Write a config for `cfg`, spawn `sqls -config <it>` and run the
    /// initialize handshake. The password, if any, is passed via `PGPASSWORD`
    /// (never on disk).
    pub fn spawn(exe: &Path, cfg: &ConnectionConfig) -> std::io::Result<LspHandle> {
        let generation = SPAWNS.fetch_add(1, Ordering::Relaxed) + 1;
        let config_path = write_config(cfg, generation)?;
        let mut cmd = Command::new(exe);
        cmd.arg("-config")
            .arg(&config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(pw) = &cfg.password {
            cmd.env("PGPASSWORD", pw);
        }
        // sqls is a console subprocess; without this a GUI-subsystem zdb.exe pops
        // a visible console window for it on Windows. CREATE_NO_WINDOW hides it.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
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
            dead: AtomicBool::new(false),
            started: Instant::now(),
            generation,
        });

        // The reader holds only a Weak ref so it never keeps the process alive:
        // when the last handle drops, Inner::drop kills the child, stdout hits
        // EOF, and the reader returns.
        let weak = Arc::downgrade(&inner);
        thread::Builder::new()
            .name("zdb-lsp-reader".into())
            .spawn(move || reader_loop(BufReader::new(stdout), weak))
            .ok();

        let handle = LspHandle {
            inner,
            exe: exe.to_path_buf(),
            cfg: cfg.clone(),
        };
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

    /// A fresh server for the same connection.
    pub fn restart(&self) -> std::io::Result<LspHandle> {
        Self::spawn(&self.exe, &self.cfg)
    }

    /// Whether the process has gone and a `restart` is due (not straight away
    /// if it died young — see `RESTART_BACKOFF`).
    fn needs_restart(&self) -> bool {
        self.inner.dead.load(Ordering::Relaxed) && self.inner.started.elapsed() >= RESTART_BACKOFF
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
    /// On a dead server the channel is closed at once (the caller treats that
    /// as no completions).
    pub fn completion(&self, uri: &str, line: u32, character: u32) -> oneshot::Receiver<Value> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending.lock().unwrap();
            if self.inner.dead.load(Ordering::Relaxed) {
                return rx;
            }
            pending.insert(id, tx);
        }
        self.inner.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "textDocument/completion",
            "params": {"textDocument": {"uri": uri}, "position": {"line": line, "character": character}}
        }));
        rx
    }
}

fn reader_loop(r: BufReader<ChildStdout>, weak: Weak<Inner>) {
    read_messages(r, &weak);
    // stdout closed: the server exited or crashed. Fail what's waiting (the
    // dropped senders read as "no completions") and mark the handle dead.
    if let Some(inner) = weak.upgrade() {
        let mut pending = inner.pending.lock().unwrap();
        inner.dead.store(true, Ordering::Relaxed);
        pending.clear();
    }
}

fn read_messages(mut r: BufReader<ChildStdout>, weak: &Weak<Inner>) {
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
/// Values are JSON-quoted, which is valid YAML, to avoid injection. Each
/// server gets its own file (`n`): a dying one deletes its config on drop.
fn write_config(cfg: &ConnectionConfig, n: u64) -> std::io::Result<PathBuf> {
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
    let path = std::env::temp_dir().join(format!("zdb-sqls-{}-{n}.yml", std::process::id()));
    std::fs::write(&path, yaml)?;
    Ok(path)
}

// ---- completion provider -------------------------------------------------

/// Byte offset of the start of the identifier (`[A-Za-z0-9_]`) ending at
/// `offset`. Returns `offset` when the char before the cursor isn't an
/// identifier char (empty word).
fn word_start(text: &str, offset: usize) -> usize {
    let b = text.as_bytes();
    let mut i = offset.min(b.len());
    while i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_') {
        i -= 1;
    }
    i
}

/// Give every completion item an explicit `textEdit` replacing `start..end`
/// (the typed word) with the completion text, so the editor replaces the prefix
/// instead of inserting after it. Uses the item's `insert_text` if present, else
/// its `label`.
fn with_word_range(resp: CompletionResponse, start: Position, end: Position) -> CompletionResponse {
    let fix = |item: &mut CompletionItem| {
        let new_text = item
            .insert_text
            .take()
            .unwrap_or_else(|| item.label.clone());
        item.text_edit = Some(CompletionTextEdit::Edit(TextEdit {
            range: Range { start, end },
            new_text,
        }));
    };
    match resp {
        CompletionResponse::Array(mut items) => {
            items.iter_mut().for_each(fix);
            CompletionResponse::Array(items)
        }
        CompletionResponse::List(mut list) => {
            list.items.iter_mut().for_each(fix);
            CompletionResponse::List(list)
        }
    }
}

/// Shape sqls' items for a compact menu. sqls attaches markdown docs, which
/// gpui-component shows in a side panel — for a table, a full column listing
/// taller than the window — so the docs go, and a column's type (the one
/// useful line of them) becomes its inline detail instead of `column from "t"`.
/// `clause` (a WHERE / ORDER BY box over one table) also drops table names,
/// which have no place there.
///
/// `typed` is the word under the cursor. Each item's `filter_text` is set to
/// the part of its label that word matches: the menu highlights the first
/// `filter_text.len()` bytes of the label, else the first `query.len()` — and
/// its query runs on across spaces while the menu stays open (`WHERE c` against
/// a label `c1`), which made gpui panic on a highlight past the label's end.
fn tidy_items(resp: CompletionResponse, clause: bool, typed: &str) -> CompletionResponse {
    let tidy = |items: Vec<CompletionItem>| -> Vec<CompletionItem> {
        items
            .into_iter()
            .filter(|i| !(clause && i.kind == Some(CompletionItemKind::CLASS)))
            .map(|mut i| {
                i.filter_text = Some(i.label[..matched_prefix(&i.label, typed)].to_string());
                let doc = match i.documentation.take() {
                    Some(Documentation::String(s)) => s,
                    Some(Documentation::MarkupContent(m)) => m.value,
                    None => String::new(),
                };
                if i.kind == Some(CompletionItemKind::FIELD) {
                    if let Some(ty) = column_type(&doc) {
                        i.detail = Some(ty);
                    }
                }
                i
            })
            .collect()
    };
    match resp {
        CompletionResponse::Array(items) => CompletionResponse::Array(tidy(items)),
        CompletionResponse::List(mut list) => {
            list.items = tidy(list.items);
            CompletionResponse::List(list)
        }
    }
}

/// Byte length of the longest prefix of `label` that matches `typed`, ignoring
/// case (always a char boundary of `label`).
fn matched_prefix(label: &str, typed: &str) -> usize {
    label
        .char_indices()
        .zip(typed.chars())
        .take_while(|((_, a), b)| a.to_lowercase().eq(b.to_lowercase()))
        .last()
        .map_or(0, |((i, a), _)| i + a.len_utf8())
}

/// `integer PK` out of sqls' column doc (`` `t`.`c` column\n\n`integer` PRIMARY KEY ``).
fn column_type(doc: &str) -> Option<String> {
    let ty = doc.split("\n\n").nth(1)?.replace('`', "");
    let ty = ty.trim().replace("PRIMARY KEY", "PK");
    (!ty.is_empty()).then_some(ty)
}

/// Byte offset where the comma-separated item under `offset` starts: just past
/// the last top-level comma before it (none inside quotes or parentheses), else
/// 0. sqls loses track of the columns after `col DESC,`, so a clause box only
/// ever shows it the item being typed.
fn list_item_start(text: &str, offset: usize) -> usize {
    let mut start = 0;
    let mut depth = 0i32;
    let mut quote = None;
    for (i, &b) in text.as_bytes()[..offset.min(text.len())].iter().enumerate() {
        match (quote, b) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, b'\'' | b'"') => quote = Some(b),
            (None, b'(') => depth += 1,
            (None, b')') => depth -= 1,
            (None, b',') if depth <= 0 => start = i + 1,
            _ => {}
        }
    }
    start
}

/// `SELECT * FROM s.t <keyword> ` — the statement a Table tab's WHERE / ORDER
/// BY box continues, as sqls should see it. Plain names stay unquoted: sqls
/// resolves no columns at all for a quoted `"s"."t"`.
pub fn clause_head(schema: &str, table: &str, keyword: &str) -> String {
    let ident = |s: &str| {
        let plain = s.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
            && s.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if plain {
            s.to_string()
        } else {
            format!("\"{}\"", s.replace('"', "\"\""))
        }
    };
    format!(
        "SELECT * FROM {}.{} {keyword} ",
        ident(schema),
        ident(table)
    )
}

/// A per-editor `CompletionProvider` that forwards to the shared `sqls` handle.
/// Each editor gets a unique document URI so the server tracks them separately.
pub struct SqlCompletion {
    slot: LspSlot,
    uri: String,
    /// Generation of the server this document was opened on (0 = none): a
    /// restarted server needs a fresh `didOpen`.
    opened: AtomicU64,
    /// For a clause box: the statement its text continues (see `clause`).
    head: Option<String>,
}

impl SqlCompletion {
    pub fn new(slot: LspSlot, tab_id: u64) -> Self {
        Self {
            slot,
            uri: format!("file:///zdb/tab-{tab_id}.sql"),
            opened: AtomicU64::new(0),
            head: None,
        }
    }

    /// For a one-line box that holds a single clause of a generated statement
    /// (a Table tab's WHERE / ORDER BY). On its own, `id > 3 AND na` names no
    /// table, so sqls has no columns to offer; it is shown `head` (from
    /// `clause_head`) followed by the item being typed instead.
    pub fn clause(slot: LspSlot, key: &str, head: String) -> Self {
        Self {
            slot,
            uri: format!("file:///zdb/{key}.sql"),
            opened: AtomicU64::new(0),
            head: Some(head),
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
        cx: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        let empty = || -> Task<anyhow::Result<CompletionResponse>> {
            Task::ready(Ok(CompletionResponse::Array(vec![])))
        };
        let handle = {
            let mut slot = self.slot.borrow_mut();
            if slot.as_ref().is_some_and(LspHandle::needs_restart) {
                *slot = slot.as_ref().and_then(|h| h.restart().ok());
            }
            match slot.as_ref() {
                Some(h) => h.clone(),
                None => return empty(),
            }
        };
        let pos = rope.offset_to_position(offset);
        let text = rope.to_string();
        // The span the completion should replace = the identifier being typed
        // (word start .. cursor). sqls returns bare `label`s with no textEdit, so
        // gpui-component would fall back to its own trigger-offset range — which
        // mis-replaces and duplicates/eats characters. We attach an explicit
        // textEdit over this range so insertion is deterministic.
        let word_at = word_start(&text, offset);
        let typed = text[word_at..offset].to_string();
        let start_pos = rope.offset_to_position(word_at);
        let end_pos = pos;
        // What sqls sees: the editor's text, or for a (one-line) clause box its
        // statement head + the item being typed, asked about at its end.
        let (doc, at) = match &self.head {
            Some(head) => {
                let doc = format!("{head}{}", &text[list_item_start(&text, offset)..offset]);
                let at = Position {
                    line: 0,
                    character: doc.encode_utf16().count() as u32,
                };
                (doc, at)
            }
            None => (text, pos),
        };
        // Full-text sync: open once (per server), then change on every request.
        let generation = handle.inner.generation;
        if self.opened.swap(generation, Ordering::Relaxed) == generation {
            handle.did_change(&self.uri, &doc);
        } else {
            handle.did_open(&self.uri, &doc);
        }
        let rx = handle.completion(&self.uri, at.line, at.character);
        let clause = self.head.is_some();
        cx.background_spawn(async move {
            let resp = match rx.await {
                Ok(v) => serde_json::from_value::<CompletionResponse>(v)
                    .unwrap_or(CompletionResponse::Array(vec![])),
                Err(_) => CompletionResponse::Array(vec![]),
            };
            Ok(tidy_items(
                with_word_range(resp, start_pos, end_pos),
                clause,
                &typed,
            ))
        })
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
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
        cfg.password = pw;
        let handle = LspHandle::spawn(&exe, &cfg).expect("spawn sqls");

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

    /// A server that dies leaves nothing hanging: a waiting request and every
    /// later one resolve (as no completions) instead of freezing the menu.
    #[cfg(unix)]
    #[test]
    fn dead_server_never_leaves_a_request_waiting() {
        let cfg = ConnectionConfig::new("t", "h", "d", "u");
        // `true` exits at once: a server that dies right after starting.
        let handle = LspHandle::spawn(Path::new("true"), &cfg).expect("spawn");
        let mut rx = handle.completion("file:///t.sql", 0, 0);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match rx.try_recv() {
                Err(_) => break,
                Ok(Some(v)) => panic!("unexpected reply {v}"),
                Ok(None) => {
                    assert!(Instant::now() < deadline, "request left waiting");
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
        assert!(handle.inner.dead.load(Ordering::Relaxed));
        assert!(handle.completion("file:///t.sql", 0, 0).try_recv().is_err());
        // It died young: no restart until the backoff has passed.
        assert!(!handle.needs_restart());
    }

    #[test]
    fn word_start_finds_identifier_start() {
        assert_eq!(word_start("SELECT * FROM zdb_sel", 21), 14); // "zdb_sel"
        assert_eq!(word_start("abc", 3), 0);
        assert_eq!(word_start("a.b", 3), 2); // '.' breaks the word
        assert_eq!(word_start("SELECT ", 7), 7); // trailing space -> empty word
        assert_eq!(word_start("", 0), 0);
        assert_eq!(word_start("id123", 5), 0);
    }

    fn pos(line: u32, character: u32) -> Position {
        Position { line, character }
    }

    #[test]
    fn with_word_range_rewrites_label_items() {
        // A bare label item (what sqls sends) must come back with a textEdit that
        // replaces the typed word — otherwise the editor duplicates the prefix.
        let start = pos(0, 14);
        let end = pos(0, 21);
        let items = vec![CompletionItem {
            label: "zdb_selftest".into(),
            ..Default::default()
        }];
        let resp = with_word_range(CompletionResponse::Array(items), start, end);
        let CompletionResponse::Array(items) = resp else {
            panic!("expected array");
        };
        let it = &items[0];
        assert!(it.insert_text.is_none());
        match it.text_edit.as_ref().expect("text_edit set") {
            CompletionTextEdit::Edit(e) => {
                assert_eq!(e.new_text, "zdb_selftest");
                assert_eq!(e.range.start, start);
                assert_eq!(e.range.end, end);
            }
            other => panic!("expected Edit, got {other:?}"),
        }
    }

    #[test]
    fn clause_boxes_send_only_the_current_item() {
        assert_eq!(list_item_start("\"name\" DESC, i", 14), 12);
        assert_eq!(list_item_start("id > 3 AND na", 13), 0);
        // Commas inside calls, strings and quoted names don't split.
        assert_eq!(list_item_start("coalesce(a, n", 13), 0);
        assert_eq!(list_item_start("name = 'a,b' AND i", 18), 0);
        assert_eq!(list_item_start("\"a,b\" ASC, c", 12), 10);
        assert_eq!(
            clause_head("public", "bbb", "WHERE"),
            "SELECT * FROM public.bbb WHERE "
        );
        assert_eq!(
            clause_head("Sales", "my table", "ORDER BY"),
            "SELECT * FROM \"Sales\".\"my table\" ORDER BY "
        );
    }

    #[test]
    fn items_lose_docs_and_columns_show_their_type() {
        let doc = |v: &str| {
            Some(Documentation::MarkupContent(lsp_types::MarkupContent {
                kind: lsp_types::MarkupKind::Markdown,
                value: v.into(),
            }))
        };
        let items = vec![
            CompletionItem {
                label: "id".into(),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some("column from \"bbb\"".into()),
                documentation: doc("`bbb`.`id` column\n\n`integer` PRIMARY KEY\n"),
                ..Default::default()
            },
            CompletionItem {
                label: "bbb".into(),
                kind: Some(CompletionItemKind::CLASS),
                detail: Some("table".into()),
                documentation: doc("# `bbb` table\n\n| Name | Type |"),
                ..Default::default()
            },
        ];
        let CompletionResponse::Array(all) =
            tidy_items(CompletionResponse::Array(items.clone()), false, "")
        else {
            panic!("expected array");
        };
        assert_eq!(all[0].detail.as_deref(), Some("integer PK"));
        assert!(all.iter().all(|i| i.documentation.is_none()));
        assert_eq!(all[1].detail.as_deref(), Some("table"));
        // A clause box has no use for table names.
        let CompletionResponse::Array(clause) =
            tidy_items(CompletionResponse::Array(items), true, "")
        else {
            panic!("expected array");
        };
        assert_eq!(clause.len(), 1);
    }

    /// The menu highlights `filter_text.len()` bytes of the label, so that must
    /// never run past it (it used to be the whole query: a gpui panic).
    #[test]
    fn highlight_covers_only_the_matched_prefix() {
        assert_eq!(matched_prefix("c1", "WHERE c"), 0);
        assert_eq!(matched_prefix("name", "NA"), 2);
        assert_eq!(matched_prefix("id", "id_long"), 2);
        assert_eq!(matched_prefix("ñandú", "ÑA"), "ña".len());
        let items = vec![CompletionItem {
            label: "SELECT".into(),
            ..Default::default()
        }];
        let CompletionResponse::Array(items) =
            tidy_items(CompletionResponse::Array(items), false, "sel")
        else {
            panic!("expected array");
        };
        assert_eq!(items[0].filter_text.as_deref(), Some("SEL"));
    }

    #[test]
    fn with_word_range_prefers_insert_text() {
        let items = vec![CompletionItem {
            label: "display".into(),
            insert_text: Some("insert_me".into()),
            ..Default::default()
        }];
        let resp = with_word_range(CompletionResponse::Array(items), pos(0, 0), pos(0, 3));
        let CompletionResponse::Array(items) = resp else {
            panic!("expected array");
        };
        match items[0].text_edit.as_ref().unwrap() {
            CompletionTextEdit::Edit(e) => assert_eq!(e.new_text, "insert_me"),
            _ => panic!("expected Edit"),
        }
    }
}
