//! Connection management: connect/switch/save connections, the settings
//! modal + theme switch, and starting the `sqls` completion server.

use super::*;
use skimrow_config::secret::ssh_account;
use skimrow_config::{SshAuthKind, SshEntry};
use skimrow_db::{DbError, HostKey, SslMode};
use std::net::SocketAddr;

/// The connection form, adding a connection or editing a saved one: its
/// inputs, which entry it edits, and how the last save went.
pub(super) struct ConnForm {
    pub(super) name: Entity<InputState>,
    pub(super) host: Entity<InputState>,
    pub(super) port: Entity<InputState>,
    pub(super) user: Entity<InputState>,
    pub(super) db: Entity<InputState>,
    pub(super) password: Entity<InputState>,
    pub(super) ssl: Entity<InputState>,
    /// Extra parameters for every connection of the pool, `key=value` a line.
    pub(super) params: Entity<TextareaState>,
    /// SQL run on every connection of the pool.
    pub(super) init_sql: Entity<TextareaState>,
    /// Reach the database through an SSH server, the `ssh_*` fields.
    pub(super) ssh: bool,
    pub(super) ssh_host: Entity<InputState>,
    pub(super) ssh_port: Entity<InputState>,
    pub(super) ssh_user: Entity<InputState>,
    pub(super) ssh_auth: SshAuthKind,
    /// The private key file; empty = the default ones.
    pub(super) ssh_key: Entity<InputState>,
    /// The SSH password, or the key's passphrase.
    pub(super) ssh_secret: Entity<InputState>,
    /// A key file picked with Browse, for `render` to put in `ssh_key` (that
    /// needs the window).
    pub(super) pending_key: Option<String>,
    /// The id of the saved connection being edited; `None` = adding one.
    pub(super) editing: Option<String>,
    /// Why the last save was refused: a missing field, a bad parameter line,
    /// a name already taken, or the live connection rejecting a new setup.
    pub(super) error: Option<String>,
    /// Waiting for the live connection to try out a new setup.
    pub(super) applying: bool,
}

impl ConnForm {
    pub(super) fn new(window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let input = |window: &mut Window, cx: &mut Context<Workspace>, ph: &str, val: &str| {
            let ph = ph.to_string();
            let val = val.to_string();
            cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder(ph)
                    .default_value(val)
            })
        };
        // Multi-line, and auto-growing rather than a plain text area (that one
        // takes Tab for indenting, trapping the dialog's Tab cycle). Their
        // placeholders must stay ONE line: gpui-component shapes each line of
        // an empty multi-line input's placeholder with a run as long as the
        // whole placeholder, and on Windows DirectWrite slices the text by that
        // run — a newline in the placeholder crashes the app there.
        let lines = |window: &mut Window, cx: &mut Context<Workspace>, ph: &str| {
            let ph = ph.to_string();
            cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 6)
                    .placeholder(ph)
            })
        };
        Self {
            name: input(window, cx, "name", ""),
            host: input(window, cx, "host", "127.0.0.1"),
            port: input(window, cx, "port", "5432"),
            user: input(window, cx, "user", ""),
            db: input(window, cx, "database", ""),
            password: cx.new(|cx| {
                InputState::new(window, cx)
                    .placeholder("password")
                    .masked(true)
            }),
            ssl: input(
                window,
                cx,
                "sslmode (disable/prefer/require/verify-full)",
                "prefer",
            ),
            params: lines(window, cx, "statement_timeout=30s"),
            init_sql: lines(window, cx, "SET search_path TO app, public;"),
            ssh: false,
            ssh_host: input(window, cx, "ssh.example.com", ""),
            ssh_port: input(window, cx, "22", "22"),
            ssh_user: input(window, cx, "user", ""),
            ssh_auth: SshAuthKind::default(),
            ssh_key: input(window, cx, "empty: ~/.ssh/id_ed25519, id_ecdsa, id_rsa", ""),
            ssh_secret: cx.new(|cx| InputState::new(window, cx).masked(true)),
            pending_key: None,
            editing: None,
            error: None,
            applying: false,
        }
    }

    /// Load `entry` for editing, or blank defaults for a new connection. The
    /// password is never loaded (it stays in the keychain): left empty, an
    /// edit keeps the saved one.
    pub(super) fn fill(
        &mut self,
        entry: Option<&ConnectionEntry>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let blank = ConnectionEntry {
            id: String::new(),
            name: String::new(),
            host: "127.0.0.1".into(),
            port: 5432,
            dbname: String::new(),
            user: String::new(),
            ssl_mode: "prefer".into(),
            params: Vec::new(),
            init_sql: String::new(),
            ssh: None,
        };
        let e = entry.unwrap_or(&blank);
        let blank_ssh = SshEntry {
            host: String::new(),
            port: 22,
            user: String::new(),
            auth: SshAuthKind::default(),
            key_file: String::new(),
        };
        let ssh = e.ssh.as_ref().unwrap_or(&blank_ssh);
        for (input, value) in [
            (&self.name, e.name.clone()),
            (&self.host, e.host.clone()),
            (&self.port, e.port.to_string()),
            (&self.user, e.user.clone()),
            (&self.db, e.dbname.clone()),
            (&self.password, String::new()),
            (&self.ssl, e.ssl_mode.clone()),
            (&self.ssh_host, ssh.host.clone()),
            (&self.ssh_port, ssh.port.to_string()),
            (&self.ssh_user, ssh.user.clone()),
            (&self.ssh_key, ssh.key_file.clone()),
            (&self.ssh_secret, String::new()),
        ] {
            input.update(cx, |i, cx| i.set_value(value, window, cx));
        }
        for (input, value) in [
            (&self.params, e.params.join("\n")),
            (&self.init_sql, e.init_sql.clone()),
        ] {
            input.update(cx, |i, cx| i.set_value(value, window, cx));
        }
        let ph = if entry.is_some() {
            "unchanged — type to replace"
        } else {
            "password"
        };
        self.password
            .update(cx, |i, cx| i.set_placeholder(ph, window, cx));
        // Likewise the SSH secret, which a key may not have at all.
        let ph = if e.ssh.is_some() {
            "unchanged — type to replace"
        } else {
            ""
        };
        self.ssh_secret
            .update(cx, |i, cx| i.set_placeholder(ph, window, cx));
        self.ssh = e.ssh.is_some();
        self.ssh_auth = ssh.auth;
        self.pending_key = None;
        self.editing = entry.map(|e| e.id.clone());
        self.error = None;
        self.applying = false;
    }
}

/// The parameter lines worth keeping from the editor text.
fn param_lines(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// The id for a connection being saved as `name`: the existing entry's when
/// one has that name, else a fresh one.
fn conn_id_for(connections: &[ConnectionEntry], name: &str) -> String {
    connections
        .iter()
        .find(|c| c.name == name && !c.id.is_empty())
        .map(|c| c.id.clone())
        .unwrap_or_else(skimrow_config::new_conn_id)
}

/// The config `sqls` connects with: at the SSH tunnel's local end, when there
/// is one. Its TLS can't be told the database's real name there, so
/// verify-full checks the certificate chain only (verify-ca).
fn lsp_config(cfg: &ConnectionConfig, local: Option<SocketAddr>) -> ConnectionConfig {
    let mut cfg = cfg.clone();
    if let Some(addr) = local {
        cfg.host = addr.ip().to_string();
        cfg.port = addr.port();
        cfg.ssh = None;
        if cfg.ssl_mode == SslMode::VerifyFull {
            cfg.ssl_mode = SslMode::VerifyCa;
        }
    }
    cfg
}

/// Record a host key the user trusted. Not under test: tests must never
/// write the dev machine's config dir.
fn record_host_key(file: &std::path::Path, key: &HostKey) -> Result<(), String> {
    #[cfg(not(test))]
    return skimrow_db::trust_host_key(file, key);
    #[cfg(test)]
    {
        let _ = (file, key);
        Ok(())
    }
}

impl Workspace {
    // ---- connections -----------------------------------------------------

    pub(super) fn connect_or_refresh(&mut self, cx: &mut Context<Self>) {
        if self.conn.is_some() {
            self.load_schemas(cx);
        } else if let Some(cfg) = self.cfg.clone() {
            self.start_connect(cfg, cx);
        } else {
            self.modal = Some(Modal::Connections);
            cx.notify();
        }
    }

    pub(super) fn toggle_connections(&mut self, cx: &mut Context<Self>) {
        self.toggle_modal(Modal::Connections, cx);
        if self.is_modal(Modal::Connections) {
            // No saved connections → go straight to the add form.
            self.conn_adding = self.settings.connections.is_empty();
        }
    }

    pub(super) fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        self.toggle_modal(Modal::Settings, cx);
    }

    /// Switch the theme live and persist it to settings.json.
    pub(super) fn set_theme(
        &mut self,
        theme: skimrow_config::Theme,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings.theme == theme {
            return;
        }
        self.settings.theme = theme;
        let mode = match theme {
            skimrow_config::Theme::Light => gpui_component::ThemeMode::Light,
            skimrow_config::Theme::Dark => gpui_component::ThemeMode::Dark,
        };
        super::colors::change_theme(mode, cx);
        if let Some(term) = &self.terminal {
            term.restyle(crate::terminal::TerminalStyle::from_theme(cx), cx);
        }
        if let Err(e) = self.settings.save() {
            self.status = format!("Theme set (disk write failed: {e})");
        }
        cx.notify();
    }

    /// `key` is what the new connection's session files are keyed by (see
    /// `Workspace::conn_key`).
    pub(super) fn switch_connection(
        &mut self,
        cfg: ConnectionConfig,
        key: Option<String>,
        cx: &mut Context<Self>,
    ) {
        // Save the tabs of the connection being left, then stop saving until
        // the new one's own set has been restored.
        self.flush_session(cx);
        self.session_ready = false;
        self.pending_restore = false;
        self.last_saved = None;
        self.conn_key = key;
        if let Some(old) = self.conn.take() {
            self.db.disconnect(old);
        }
        self.tree.reset(cx);
        self.close_all_tabs(cx);
        self.cfg = Some(cfg.clone());
        if self.is_modal(Modal::Connections) {
            self.modal = None;
        }
        self.start_connect(cfg, cx);
    }

    pub(super) fn show_add_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.form.fill(None, window, cx);
        self.conn_adding = true;
        cx.notify();
    }

    /// The list's edit button: the form, loaded with saved connection `idx`.
    pub(super) fn show_edit_form(
        &mut self,
        idx: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.settings.connections.get(idx).cloned() else {
            return;
        };
        self.form.fill(Some(&entry), window, cx);
        self.conn_adding = true;
        cx.notify();
    }

    /// The form's Cancel: back to the list, or closed when there is none.
    pub(super) fn cancel_conn_form(&mut self, cx: &mut Context<Self>) {
        if self.settings.connections.is_empty() {
            self.toggle_connections(cx);
        } else {
            self.show_conn_list(cx);
        }
    }

    pub(super) fn show_conn_list(&mut self, cx: &mut Context<Self>) {
        self.conn_adding = false;
        cx.notify();
    }

    pub(super) fn connect_saved(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.settings.connections.get(idx).cloned() else {
            return;
        };
        // Only the session cache here — no keychain read. On macOS a Keychain
        // lookup can block (and pop a system prompt); doing it on this UI-thread
        // click handler freezes the render loop (the app "hangs" on select).
        // `start_connect` resolves the stored password off the UI thread.
        let pw = self.passwords.get(&entry.name).cloned();
        let ssh_secret = self.passwords.get(&ssh_account(&entry.name)).cloned();
        match entry_to_config(&entry, pw, ssh_secret) {
            Ok(cfg) => {
                let key = (!entry.id.is_empty()).then(|| entry.id.clone());
                self.switch_connection(cfg, key, cx);
            }
            // A bad parameter line in a hand-edited settings.json.
            Err(e) => {
                self.status = format!("Connection failed: {e}");
                cx.notify();
            }
        }
    }

    /// The form's Save. Adding: save the entry (a name that is already saved
    /// is overwritten, keeping its id) and connect to it. Editing: update the
    /// entry in place — same id, so its saved tabs and scratch stay — and if
    /// it is the live connection, reconnect when what it connects to changed,
    /// or hand its pool the new setup when only that did (saved only once the
    /// pool took it).
    pub(super) fn save_connection(&mut self, cx: &mut Context<Self>) {
        if self.form.applying {
            return;
        }
        let read = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let name = read(&self.form.name);
        let host = read(&self.form.host);
        let user = read(&self.form.user);
        let dbname = read(&self.form.db);
        let port = read(&self.form.port).parse().unwrap_or(5432);
        let ssl = read(&self.form.ssl);
        let init_sql = self.form.init_sql.read(cx).value().trim().to_string();
        let params_text = self.form.params.read(cx).value().to_string();
        let password = self.form.password.read(cx).value().to_string();
        let ssh_secret = self.form.ssh_secret.read(cx).value().to_string();

        let fail = |this: &mut Self, msg: String, cx: &mut Context<Self>| {
            this.form.error = Some(msg);
            cx.notify();
        };
        if name.is_empty() || host.is_empty() || user.is_empty() || dbname.is_empty() {
            return fail(self, "Fill name, host, user, and database".into(), cx);
        }
        let setup = match ConnSetup::parse(&params_text, &init_sql) {
            Ok(setup) => setup,
            Err(e) => return fail(self, format!("Parameters, {e}"), cx),
        };
        let ssh = if self.form.ssh {
            let (host, user) = (read(&self.form.ssh_host), read(&self.form.ssh_user));
            if host.is_empty() || user.is_empty() {
                return fail(self, "Fill the SSH host and user".into(), cx);
            }
            let auth = self.form.ssh_auth;
            Some(SshEntry {
                host,
                port: read(&self.form.ssh_port).parse().unwrap_or(22),
                user,
                auth,
                key_file: match auth {
                    SshAuthKind::Key => read(&self.form.ssh_key),
                    _ => String::new(),
                },
            })
        } else {
            None
        };
        let ssh_secret = ssh
            .as_ref()
            .filter(|s| s.auth != SshAuthKind::Agent && !ssh_secret.is_empty())
            .map(|_| ssh_secret);
        let editing = self.form.editing.clone();
        if let Some(id) = &editing {
            if self
                .settings
                .connections
                .iter()
                .any(|c| c.name == name && &c.id != id)
            {
                return fail(
                    self,
                    format!("A connection named \"{name}\" already exists"),
                    cx,
                );
            }
        }
        self.form.error = None;
        let entry = ConnectionEntry {
            id: editing
                .clone()
                .unwrap_or_else(|| conn_id_for(&self.settings.connections, &name)),
            name,
            host,
            port,
            dbname,
            user,
            ssl_mode: if ssl.is_empty() { "prefer".into() } else { ssl },
            params: param_lines(&params_text),
            init_sql,
            ssh,
        };
        let pw = (!password.is_empty()).then_some(password);

        let Some(old) = editing.and_then(|id| {
            self.settings
                .connections
                .iter()
                .find(|c| c.id == id)
                .cloned()
        }) else {
            // Adding.
            self.store_entry(None, &entry, pw.clone(), ssh_secret.clone(), cx);
            let mut cfg = entry_config(&entry, pw, ssh_secret);
            cfg.setup = setup;
            self.switch_connection(cfg, Some(entry.id.clone()), cx);
            return;
        };

        let live = self
            .conn
            .filter(|_| self.conn_key.as_deref() == Some(old.id.as_str()));
        let target = |e: &ConnectionEntry| {
            (
                e.host.clone(),
                e.port,
                e.dbname.clone(),
                e.user.clone(),
                e.ssl_mode.clone(),
                e.ssh.clone(),
            )
        };
        let moved = target(&old) != target(&entry) || pw.is_some() || ssh_secret.is_some();
        let new_setup = old.params != entry.params || old.init_sql != entry.init_sql;
        match live {
            Some(_) if moved => {
                let renamed =
                    self.store_entry(Some(&old.name), &entry, pw.clone(), ssh_secret.clone(), cx);
                let mut cfg = entry_config(
                    &entry,
                    pw.or_else(|| self.passwords.get(&entry.name).cloned()),
                    ssh_secret.or_else(|| self.passwords.get(&ssh_account(&entry.name)).cloned()),
                );
                cfg.setup = setup;
                let key = Some(entry.id.clone());
                match renamed {
                    // The passwords are on their way to the new name in the
                    // keychain: connect with them once they have been read.
                    Some(moving) => {
                        cx.spawn(async move |this, cx| {
                            let [pw, ssh_secret] = moving.await;
                            cfg.password = cfg.password.or(pw);
                            if let Some(ssh) = &mut cfg.ssh {
                                ssh.secret = ssh.secret.take().or(ssh_secret);
                            }
                            let _ =
                                this.update(cx, |this, cx| this.switch_connection(cfg, key, cx));
                        })
                        .detach();
                    }
                    None => self.switch_connection(cfg, key, cx),
                }
            }
            Some(conn) if new_setup => {
                self.form.applying = true;
                let db = self.db.clone();
                let applied = setup.clone();
                self.spawn_db(
                    cx,
                    async move { db.set_setup(conn, setup).await },
                    move |this, res, cx| {
                        this.form.applying = false;
                        match res {
                            Ok(()) => {
                                this.finish_edit(&old.name, &entry, None, Some(applied), cx);
                                this.status = format!(
                                    "Saved {}; its connections now use the new setup",
                                    entry.name
                                );
                            }
                            Err(e) => this.form.error = Some(e.to_string()),
                        }
                    },
                );
            }
            _ => {
                let setup = live.map(|_| setup);
                self.finish_edit(&old.name, &entry, Some([pw, ssh_secret]), setup, cx);
                self.status = format!("Saved {}", entry.name);
            }
        }
        cx.notify();
    }

    /// Store an edited entry (with the database password and SSH secret
    /// typed, if any) and go back to the list. A live connection's config
    /// follows a rename and the new `setup`, if one is given.
    fn finish_edit(
        &mut self,
        old_name: &str,
        entry: &ConnectionEntry,
        typed: Option<[Option<String>; 2]>,
        setup: Option<ConnSetup>,
        cx: &mut Context<Self>,
    ) {
        let [pw, ssh_secret] = typed.unwrap_or_default();
        if let Some(moving) = self.store_entry(Some(old_name), entry, pw, ssh_secret, cx) {
            moving.detach();
        }
        if self.conn_key.as_deref() == Some(entry.id.as_str()) {
            if let Some(cfg) = &mut self.cfg {
                cfg.name = entry.name.clone();
                if let Some(setup) = setup {
                    cfg.setup = setup;
                }
            }
        }
        self.conn_adding = false;
    }

    /// Write `entry` to the settings — in place of the one with its id, so
    /// the list keeps its order — and typed secrets to the keychain (off the
    /// UI thread): the database password under the connection's name, the
    /// SSH password / key passphrase under `ssh_account(name)`. A rename moves
    /// the saved ones that weren't retyped to the new name; the returned task
    /// yields both (typed or moved) once done. An SSH secret no longer used
    /// (SSH turned off, or the agent logs in) is deleted.
    fn store_entry(
        &mut self,
        old_name: Option<&str>,
        entry: &ConnectionEntry,
        pw: Option<String>,
        ssh_secret: Option<String>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Task<[Option<String>; 2]>> {
        let uses_secret =
            |e: &ConnectionEntry| e.ssh.as_ref().is_some_and(|s| s.auth != SshAuthKind::Agent);
        let slot = self
            .settings
            .connections
            .iter_mut()
            .find(|c| c.id == entry.id);
        let dropped_ssh = slot.as_ref().is_some_and(|old| uses_secret(old)) && !uses_secret(entry);
        match slot {
            Some(slot) => *slot = entry.clone(),
            None => {
                self.settings.connections.retain(|c| c.name != entry.name);
                self.settings.connections.push(entry.clone());
            }
        }
        self.save_settings();

        let name = entry.name.clone();
        let old = old_name.filter(|old| *old != name).map(str::to_string);
        let accounts = |n: &str| [n.to_string(), ssh_account(n)];
        let new = accounts(&name);
        let old = old.as_deref().map(accounts);
        let typed = [pw, ssh_secret];
        // The session cache follows the same moves.
        for i in 0..2 {
            let cached = match &typed[i] {
                Some(secret) => Some(secret.clone()),
                None => old.as_ref().and_then(|old| self.passwords.remove(&old[i])),
            };
            if let Some(secret) = cached {
                self.passwords.insert(new[i].clone(), secret);
            }
        }
        if dropped_ssh {
            self.passwords.remove(&new[1]);
        }
        let renamed = old.is_some();
        let task = cx.background_spawn(async move {
            use skimrow_config::secret::{delete_password, get_password, set_password};
            let mut out: [Option<String>; 2] = [None, None];
            for (i, typed) in typed.into_iter().enumerate() {
                if i == 1 && dropped_ssh {
                    delete_password(&new[1]);
                    if let Some(old) = &old {
                        delete_password(&old[1]);
                    }
                    continue;
                }
                out[i] = match typed {
                    Some(secret) => {
                        if set_password(&new[i], &secret) {
                            log(format!("password saved to keychain for '{}'", new[i]));
                        }
                        Some(secret)
                    }
                    None => old.as_ref().and_then(|old| {
                        let secret = get_password(&old[i])?;
                        if set_password(&new[i], &secret) {
                            delete_password(&old[i]);
                        }
                        Some(secret)
                    }),
                };
            }
            out
        });
        if renamed {
            Some(task)
        } else {
            task.detach();
            None
        }
    }

    pub(super) fn start_connect(&mut self, cfg: ConnectionConfig, cx: &mut Context<Self>) {
        self.status = format!("Connecting to {}…", cfg.name);
        let db = self.db.clone();
        let key = self.conn_key.clone();
        // Resolve stored passwords off the UI thread: a macOS Keychain read can
        // block (and pop a system prompt), which would freeze the render loop and
        // make the app appear to hang. `background_spawn` runs it on a worker
        // thread; the connect future awaits the result.
        let pw_task = cfg.password.is_none().then(|| {
            let name = cfg.name.clone();
            cx.background_spawn(async move { skimrow_config::secret::get_password(&name) })
        });
        let ssh_task = cfg
            .ssh
            .as_ref()
            .filter(|ssh| ssh.wants_secret() && ssh.secret.is_none())
            .map(|_| {
                let account = ssh_account(&cfg.name);
                cx.background_spawn(async move { skimrow_config::secret::get_password(&account) })
            });
        let fut = async move {
            let mut cfg = cfg;
            if let Some(task) = pw_task {
                cfg.password = task.await;
            }
            if let (Some(task), Some(ssh)) = (ssh_task, &mut cfg.ssh) {
                ssh.secret = task.await;
            }
            // Kept, with the resolved passwords: it configures the completion
            // server, or is connected again once a new SSH host key is trusted.
            (db.open(cfg.clone()).await, cfg)
        };
        self.spawn_db(cx, fut, move |this, (result, cfg), cx| match result {
            Ok(connected) => {
                let conn = connected.id;
                log(format!("connected (conn={conn})"));
                this.conn = Some(conn);
                this.db.set_pool_size(conn, this.settings.pool_size);
                // Not if the user has moved on to another connection meanwhile.
                this.pending_restore = !this.session_ready && this.conn_key == key;
                this.status = "Connected. Loading schemas…".into();
                this.start_lsp(&lsp_config(&cfg, connected.local));
                this.load_schemas(cx);
            }
            Err(e) => {
                log(format!("connect failed: {e}"));
                this.status = format!("Connection failed: {e}");
                // Ask whether to trust it — unless the user has moved on.
                if let DbError::UnknownHostKey(host_key) = e {
                    if this.conn.is_none() && this.conn_key == key {
                        this.host_key = Some((host_key, cfg));
                        this.modal = Some(Modal::TrustHostKey);
                    }
                }
            }
        });
    }

    /// The host-key dialog's "Trust": record the SSH server's key in skimrow's
    /// known_hosts file, then connect again.
    pub(super) fn trust_host_key(&mut self, cx: &mut Context<Self>) {
        let Some((host_key, cfg)) = self.host_key.take() else {
            return;
        };
        self.close_modal(cx);
        let Some(file) = skimrow_config::known_hosts_path() else {
            self.status = "No config directory to record the host key in".into();
            return;
        };
        let key = self.conn_key.clone();
        let fut = cx.background_spawn(async move { record_host_key(&file, &host_key) });
        self.spawn_db(cx, fut, move |this, written, cx| match written {
            Ok(()) if this.conn.is_none() && this.conn_key == key => this.start_connect(cfg, cx),
            Ok(()) => {}
            Err(e) => this.status = format!("Couldn't record the host key: {e}"),
        });
    }

    /// The connection form's Browse: pick the SSH private key file.
    pub(super) fn browse_ssh_key(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Use key".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            this.update(cx, |this, cx| {
                this.form.pending_key = Some(path.to_string_lossy().into_owned());
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// A key file picked with Browse goes into the form's field.
    pub(super) fn flush_pending_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.form.pending_key.take() {
            self.form
                .ssh_key
                .update(cx, |i, cx| i.set_value(path, window, cx));
        }
    }

    /// (Re)start the bundled `sqls` server for the just-connected database, so
    /// SQL editors get schema-aware completion. Best-effort: if `sqls` isn't
    /// bundled/installed we log and carry on with no completion.
    pub(super) fn start_lsp(&mut self, cfg: &ConnectionConfig) {
        // Drop any previous server (kills the process) before starting a new one.
        *self.lsp_slot.borrow_mut() = None;
        let Some(exe) = lsp::sqls_path() else {
            log("sqls not found; SQL completion disabled");
            return;
        };
        match lsp::LspHandle::spawn(&exe, cfg) {
            Ok(h) => {
                *self.lsp_slot.borrow_mut() = Some(h);
                log("sqls started");
            }
            Err(e) => log(format!("sqls spawn failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::test_support::*;
    use gpui::TestAppContext;

    #[test]
    fn param_lines_drop_blanks() {
        assert_eq!(
            param_lines("  a=1 \n\n b = 2\n"),
            vec!["a=1".to_string(), "b = 2".to_string()]
        );
    }

    fn saved(id: &str, name: &str) -> ConnectionEntry {
        ConnectionEntry {
            id: id.into(),
            name: name.into(),
            host: "127.0.0.1".into(),
            port: 1, // nothing listens here: a connect fails fast
            dbname: "d".into(),
            user: "u".into(),
            ssl_mode: "disable".into(),
            params: Vec::new(),
            init_sql: String::new(),
            ssh: None,
        }
    }

    fn type_into(input: &Entity<InputState>, text: &str, window: &mut Window, cx: &mut App) {
        let text = text.to_string();
        input.update(cx, |i, cx| i.set_value(text, window, cx));
    }

    fn type_lines(input: &Entity<TextareaState>, text: &str, window: &mut Window, cx: &mut App) {
        let text = text.to_string();
        input.update(cx, |i, cx| i.set_value(text, window, cx));
    }

    /// A broken parameter in settings.json stops the connect with a message
    /// instead of being dropped silently.
    #[gpui::test]
    fn bad_saved_setup_fails_the_connect(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                let mut entry = saved("id-a", "a");
                entry.params = vec!["connect_timeout=soon".into()];
                ws.settings.connections.push(entry);
                ws.connect_saved(0, cx);
                assert!(
                    ws.status.contains("a: parameters, line 1"),
                    "status: {}",
                    ws.status
                );
                assert!(ws.cfg.is_none());
            })
            .unwrap();
    }

    /// Edit keeps the entry's id and place in the list, takes its parameters,
    /// refuses a bad parameter line or a name another entry has, and the
    /// add form starts blank again afterwards.
    #[gpui::test]
    fn editing_a_connection(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.settings.connections = vec![saved("id-a", "a"), saved("id-b", "b")];
                ws.show_edit_form(0, window, cx);
                assert!(ws.conn_adding);
                assert_eq!(ws.form.editing.as_deref(), Some("id-a"));
                assert_eq!(ws.form.name.read(cx).value(), "a");
                assert_eq!(ws.form.port.read(cx).value(), "1");

                type_lines(&ws.form.params, "keepalives=maybe", window, cx);
                ws.save_connection(cx);
                assert!(ws.form.error.as_deref().unwrap().contains("line 1"));
                assert!(ws.settings.connections[0].params.is_empty());

                type_into(&ws.form.name, "b", window, cx);
                type_lines(&ws.form.params, "application_name=x", window, cx);
                ws.save_connection(cx);
                assert!(ws.form.error.as_deref().unwrap().contains("already exists"));

                type_into(&ws.form.name, "a2", window, cx);
                type_lines(
                    &ws.form.params,
                    "application_name=x\n\nwork_mem=64MB",
                    window,
                    cx,
                );
                type_lines(&ws.form.init_sql, " SET lock_timeout = '5s' ", window, cx);
                ws.save_connection(cx);
                assert_eq!(ws.form.error, None);
                assert!(!ws.conn_adding, "back to the list");
                let a = &ws.settings.connections[0];
                assert_eq!((a.id.as_str(), a.name.as_str()), ("id-a", "a2"));
                assert_eq!(a.params, ["application_name=x", "work_mem=64MB"]);
                assert_eq!(a.init_sql, "SET lock_timeout = '5s'");
                assert_eq!(ws.settings.connections[1].name, "b");

                ws.show_add_form(window, cx);
                assert_eq!(ws.form.editing, None);
                assert_eq!(ws.form.name.read(cx).value(), "");
                assert_eq!(ws.form.params.read(cx).value(), "");
            })
            .unwrap();
        cx.run_until_parked();
    }

    /// SSH fields: required host and user, the key file kept for key logins
    /// only, and a rename taking the cached SSH secret along.
    #[gpui::test]
    fn editing_ssh_settings(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.settings.connections = vec![saved("id-a", "a")];
                ws.passwords.insert(ssh_account("a"), "s3cret".into());
                ws.show_edit_form(0, window, cx);
                assert!(!ws.form.ssh, "no SSH on a connection without it");

                ws.form.ssh = true;
                ws.save_connection(cx);
                assert!(ws.form.error.as_deref().unwrap().contains("SSH host"));
                assert!(ws.settings.connections[0].ssh.is_none());

                type_into(&ws.form.ssh_host, " bastion ", window, cx);
                type_into(&ws.form.ssh_port, "2222", window, cx);
                type_into(&ws.form.ssh_user, "ops", window, cx);
                type_into(&ws.form.ssh_key, "~/.ssh/work", window, cx);
                ws.form.ssh_auth = SshAuthKind::Password;
                type_into(&ws.form.name, "a2", window, cx);
                ws.save_connection(cx);
                assert_eq!(ws.form.error, None);
                let ssh = ws.settings.connections[0].ssh.clone().unwrap();
                assert_eq!(
                    ssh,
                    SshEntry {
                        host: "bastion".into(),
                        port: 2222,
                        user: "ops".into(),
                        auth: SshAuthKind::Password,
                        key_file: String::new(),
                    }
                );
                // The renamed connection's SSH secret is cached under its new name.
                assert_eq!(ws.passwords.get(&ssh_account("a2")).unwrap(), "s3cret");
                assert!(!ws.passwords.contains_key(&ssh_account("a")));

                // Reopened: the fields come back, the secret doesn't.
                ws.show_edit_form(0, window, cx);
                assert!(ws.form.ssh);
                assert_eq!(ws.form.ssh_auth, SshAuthKind::Password);
                assert_eq!(ws.form.ssh_host.read(cx).value(), "bastion");
                assert_eq!(ws.form.ssh_port.read(cx).value(), "2222");
                assert_eq!(ws.form.ssh_secret.read(cx).value(), "");

                ws.form.ssh_auth = SshAuthKind::Key;
                type_into(&ws.form.ssh_key, "~/.ssh/work", window, cx);
                ws.save_connection(cx);
                let ssh = ws.settings.connections[0].ssh.clone().unwrap();
                assert_eq!(ssh.key_file, "~/.ssh/work");

                // What it connects with.
                let cfg = entry_config(&ws.settings.connections[0], None, Some("pp".into()));
                let ssh = cfg.ssh.unwrap();
                assert_eq!(ssh.auth, skimrow_db::SshAuth::Key("~/.ssh/work".into()));
                assert_eq!(ssh.secret.as_deref(), Some("pp"));
                assert_eq!((ssh.host.as_str(), ssh.port), ("bastion", 2222));
            })
            .unwrap();
        cx.run_until_parked();
    }

    #[test]
    fn sqls_connects_at_the_tunnel() {
        let mut cfg = ConnectionConfig::new("n", "db.internal", "d", "u");
        cfg.ssl_mode = SslMode::VerifyFull;
        assert_eq!(lsp_config(&cfg, None).host, "db.internal");
        let local = lsp_config(&cfg, Some("127.0.0.1:40001".parse().unwrap()));
        assert_eq!((local.host.as_str(), local.port), ("127.0.0.1", 40001));
        // No host name to check the certificate against there.
        assert_eq!(local.ssl_mode, SslMode::VerifyCa);
        cfg.ssl_mode = SslMode::Require;
        let local = lsp_config(&cfg, Some("127.0.0.1:40001".parse().unwrap()));
        assert_eq!(local.ssl_mode, SslMode::Require);
    }

    /// A first connect to an SSH server asks whether to trust its key;
    /// trusting connects again, cancelling leaves it.
    #[gpui::test]
    fn unknown_host_key_asks_to_trust_it(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (db, mut unserved) = DbHandle::unserved();
        let window =
            cx.add_window(|window, cx| Workspace::new(db, Settings::default(), None, window, cx));
        let key = HostKey {
            host: "bastion".into(),
            port: 22,
            algorithm: "ssh-ed25519".into(),
            fingerprint: "SHA256:abc".into(),
            openssh: "ssh-ed25519 AAAA".into(),
        };
        window
            .update(cx, |ws, _w, cx| {
                let mut entry = saved("id-a", "a");
                entry.ssh = Some(SshEntry {
                    host: "bastion".into(),
                    port: 22,
                    user: "ops".into(),
                    auth: SshAuthKind::Agent,
                    key_file: String::new(),
                });
                ws.settings.connections.push(entry);
                ws.connect_saved(0, cx);
            })
            .unwrap();
        cx.run_until_parked();
        unserved.fail_all(DbError::UnknownHostKey(key.clone()));
        cx.run_until_parked();
        window
            .update(cx, |ws, _w, cx| {
                assert!(ws.is_modal(Modal::TrustHostKey));
                assert_eq!(ws.host_key.as_ref().map(|(k, _)| k), Some(&key));
                assert!(ws.status.contains("not a known host"), "{}", ws.status);
                ws.trust_host_key(cx);
                assert!(ws.modal.is_none() && ws.host_key.is_none());
            })
            .unwrap();
        // Recorded (a no-op under test), then connecting again.
        cx.run_until_parked();
        window
            .update(cx, |ws, _w, _cx| {
                assert!(ws.status.starts_with("Connecting to a"), "{}", ws.status);
            })
            .unwrap();
        cx.run_until_parked();
        unserved.fail_all(DbError::UnknownHostKey(key));
        cx.run_until_parked();
        window
            .update(cx, |ws, _w, _cx| assert!(ws.is_modal(Modal::TrustHostKey)))
            .unwrap();
    }

    #[test]
    fn resaving_a_connection_keeps_its_id() {
        let conns = vec![saved("id-a", "a"), saved("", "old")];
        assert_eq!(conn_id_for(&conns, "a"), "id-a");
        // A new name, or an entry that never had an id, gets a fresh one.
        let fresh = conn_id_for(&conns, "b");
        assert!(!fresh.is_empty() && fresh != "id-a");
        assert!(!conn_id_for(&conns, "old").is_empty());
    }

    #[gpui::test]
    fn no_connection_opens_add_form(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, _cx| {
                assert!(ws.conn.is_none());
                assert!(ws.is_modal(Modal::Connections));
                // No saved connections → straight to the add form.
                assert!(ws.conn_adding);
            })
            .unwrap();
    }

    #[gpui::test]
    fn switching_saved_connections_survives_failures(cx: &mut TestAppContext) {
        cx.update(gpui_component::init);
        let (db, mut unserved) = DbHandle::unserved();
        let window =
            cx.add_window(|window, cx| Workspace::new(db, Settings::default(), None, window, cx));
        window
            .update(cx, |ws, _w, cx| {
                for name in ["a", "b"] {
                    ws.settings.connections.push(saved("", name));
                }
                // Rapid switches, including while a connect is still in flight.
                ws.connect_saved(0, cx);
                ws.connect_saved(1, cx);
                ws.connect_saved(0, cx);
            })
            .unwrap();
        // Let the in-flight connects go out (each reads the keychain first) and
        // their failures land.
        for _ in 0..2 {
            cx.run_until_parked();
            unserved.refuse_all();
        }
        cx.run_until_parked();
        window
            .update(cx, |ws, _w, cx| {
                assert!(ws.conn.is_none());
                assert!(
                    ws.status.starts_with("Connection failed"),
                    "status: {}",
                    ws.status
                );
                // And one more switch after a failure must not blow up.
                ws.connect_saved(1, cx);
            })
            .unwrap();
        cx.run_until_parked();
    }

    // Regression (macOS hang on selecting a saved connection): the keychain read
    // must be deferred to a background task. `connect_saved` runs on the UI
    // thread; a synchronous keychain read there froze gpui's render loop (and can
    // pop a system prompt) → the app hung indefinitely. Assert the UI-thread
    // portion never reads the keychain and the background task does. Cross-platform
    // by construction; exercised on the macOS CI runner where the bug manifested.
    #[gpui::test]
    fn connect_saved_defers_keychain_off_ui_thread(cx: &mut TestAppContext) {
        const NAME: &str = "skimrow-regression-ui-thread-probe";
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                ws.settings.connections.push(saved("", NAME));
                // No session-cached password → the keychain is the only source.
                assert_eq!(skimrow_config::secret::probe::get_password_calls(NAME), 0);
                ws.connect_saved(0, cx);
                // The synchronous UI-thread click handler must NOT have touched
                // the keychain (the deferred background task, not yet driven, will).
                assert_eq!(
                    skimrow_config::secret::probe::get_password_calls(NAME),
                    0,
                    "connect_saved read the keychain on the UI thread (hangs macOS)"
                );
            })
            .unwrap();

        // Drive the executor so start_connect's background password lookup runs.
        let mut resolved = false;
        for _ in 0..300 {
            cx.run_until_parked();
            if skimrow_config::secret::probe::get_password_calls(NAME) > 0 {
                resolved = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            resolved,
            "start_connect never resolved the password off the UI thread"
        );
    }

    #[gpui::test]
    fn conn_manager_toggles_list_and_form(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.settings.connections.push(saved("", "x"));
                ws.show_conn_list(cx);
                assert!(!ws.conn_adding);
                ws.show_add_form(window, cx);
                assert!(ws.conn_adding);
                // Cancel goes back to the list while there is one.
                ws.cancel_conn_form(cx);
                assert!(!ws.conn_adding && ws.is_modal(Modal::Connections));
            })
            .unwrap();
    }
}
