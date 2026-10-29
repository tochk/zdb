//! Connection management: connect/switch/save connections, the settings
//! modal + theme switch, and starting the `sqls` completion server.

use super::*;

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
    pub(super) params: Entity<InputState>,
    /// SQL run on every connection of the pool.
    pub(super) init_sql: Entity<InputState>,
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
            cx.new(|cx| InputState::new(window, cx).auto_grow(2, 6).placeholder(ph))
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
        };
        let e = entry.unwrap_or(&blank);
        for (input, value) in [
            (&self.name, e.name.clone()),
            (&self.host, e.host.clone()),
            (&self.port, e.port.to_string()),
            (&self.user, e.user.clone()),
            (&self.db, e.dbname.clone()),
            (&self.password, String::new()),
            (&self.ssl, e.ssl_mode.clone()),
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
        .unwrap_or_else(zdb_config::new_conn_id)
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
        theme: zdb_config::Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings.theme == theme {
            return;
        }
        self.settings.theme = theme;
        let mode = match theme {
            zdb_config::Theme::Light => gpui_component::ThemeMode::Light,
            zdb_config::Theme::Dark => gpui_component::ThemeMode::Dark,
        };
        gpui_component::Theme::change(mode, Some(window), cx);
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
        match entry_to_config(&entry, pw) {
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
        let init_sql = read(&self.form.init_sql);
        let params_text = self.form.params.read(cx).value().to_string();
        let password = self.form.password.read(cx).value().to_string();

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
            self.store_entry(None, &entry, pw.clone(), cx);
            let mut cfg = entry_config(&entry, pw);
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
            )
        };
        let moved = target(&old) != target(&entry) || pw.is_some();
        let new_setup = old.params != entry.params || old.init_sql != entry.init_sql;
        match live {
            Some(_) if moved => {
                let renamed = self.store_entry(Some(&old.name), &entry, pw.clone(), cx);
                let mut cfg = entry_config(
                    &entry,
                    pw.or_else(|| self.passwords.get(&entry.name).cloned()),
                );
                cfg.setup = setup;
                let key = Some(entry.id.clone());
                match renamed.filter(|_| cfg.password.is_none()) {
                    // The password is on its way to the new name in the
                    // keychain: connect with it once it has been read.
                    Some(moving) => {
                        cx.spawn(async move |this, cx| {
                            cfg.password = moving.await;
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
                                this.finish_edit(&old.name, &entry, pw, Some(applied), cx);
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
                self.finish_edit(&old.name, &entry, pw, setup, cx);
                self.status = format!("Saved {}", entry.name);
            }
        }
        cx.notify();
    }

    /// Store an edited entry and go back to the list. A live connection's
    /// config follows a rename and the new `setup`, if one is given.
    fn finish_edit(
        &mut self,
        old_name: &str,
        entry: &ConnectionEntry,
        pw: Option<String>,
        setup: Option<ConnSetup>,
        cx: &mut Context<Self>,
    ) {
        self.store_entry(Some(old_name), entry, pw, cx);
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
    /// the list keeps its order — and a typed password to the keychain (off
    /// the UI thread). A rename without a new password moves the saved one to
    /// the new name; the returned task yields it once read.
    fn store_entry(
        &mut self,
        old_name: Option<&str>,
        entry: &ConnectionEntry,
        pw: Option<String>,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Task<Option<String>>> {
        match self
            .settings
            .connections
            .iter_mut()
            .find(|c| c.id == entry.id)
        {
            Some(slot) => *slot = entry.clone(),
            None => {
                self.settings.connections.retain(|c| c.name != entry.name);
                self.settings.connections.push(entry.clone());
            }
        }
        self.save_settings();
        let name = entry.name.clone();
        if let Some(pw) = pw {
            self.passwords.insert(name.clone(), pw.clone());
            cx.background_spawn(async move {
                if zdb_config::secret::set_password(&name, &pw) {
                    log(format!("password saved to keychain for '{name}'"));
                }
            })
            .detach();
            return None;
        }
        let old = old_name.filter(|old| *old != name)?.to_string();
        if let Some(cached) = self.passwords.remove(&old) {
            self.passwords.insert(name.clone(), cached);
        }
        Some(cx.background_spawn(async move {
            let pw = zdb_config::secret::get_password(&old)?;
            if zdb_config::secret::set_password(&name, &pw) {
                zdb_config::secret::delete_password(&old);
            }
            Some(pw)
        }))
    }

    pub(super) fn start_connect(&mut self, cfg: ConnectionConfig, cx: &mut Context<Self>) {
        self.status = format!("Connecting to {}…", cfg.name);
        let db = self.db.clone();
        let key = self.conn_key.clone();
        // Resolve a stored password off the UI thread: a macOS Keychain read can
        // block (and pop a system prompt), which would freeze the render loop and
        // make the app appear to hang. `background_spawn` runs it on a worker
        // thread; the connect future awaits the result.
        let pw_task = cfg.password.is_none().then(|| {
            let name = cfg.name.clone();
            cx.background_spawn(async move { zdb_config::secret::get_password(&name) })
        });
        let fut = async move {
            let mut cfg = cfg;
            if let Some(task) = pw_task {
                cfg.password = task.await;
            }
            // A copy (with the resolved password) to configure the completion
            // server after a successful connect.
            let lsp_cfg = cfg.clone();
            (db.connect(cfg).await, lsp_cfg)
        };
        self.spawn_db(cx, fut, move |this, (result, lsp_cfg), cx| match result {
            Ok(conn) => {
                log(format!("connected (conn={conn})"));
                this.conn = Some(conn);
                this.db.set_pool_size(conn, this.settings.pool_size);
                // Not if the user has moved on to another connection meanwhile.
                this.pending_restore = !this.session_ready && this.conn_key == key;
                this.status = "Connected. Loading schemas…".into();
                this.start_lsp(&lsp_cfg);
                this.load_schemas(cx);
            }
            Err(e) => {
                log(format!("connect failed: {e}"));
                this.status = format!("Connection failed: {e}");
            }
        });
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
        }
    }

    fn type_into(input: &Entity<InputState>, text: &str, window: &mut Window, cx: &mut App) {
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

                type_into(&ws.form.params, "keepalives=maybe", window, cx);
                ws.save_connection(cx);
                assert!(ws.form.error.as_deref().unwrap().contains("line 1"));
                assert!(ws.settings.connections[0].params.is_empty());

                type_into(&ws.form.name, "b", window, cx);
                type_into(&ws.form.params, "application_name=x", window, cx);
                ws.save_connection(cx);
                assert!(ws.form.error.as_deref().unwrap().contains("already exists"));

                type_into(&ws.form.name, "a2", window, cx);
                type_into(
                    &ws.form.params,
                    "application_name=x\n\nwork_mem=64MB",
                    window,
                    cx,
                );
                type_into(&ws.form.init_sql, " SET lock_timeout = '5s' ", window, cx);
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
        let window = new_workspace(cx);
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
        // Let the in-flight connect results land (they arrive from the real
        // DB thread, so poll rather than a single run_until_parked).
        for _ in 0..300 {
            cx.run_until_parked();
            let failed = window
                .update(cx, |ws, _w, _cx| ws.status.starts_with("Connection failed"))
                .unwrap();
            if failed {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
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
        const NAME: &str = "zdb-regression-ui-thread-probe";
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                ws.settings.connections.push(saved("", NAME));
                // No session-cached password → the keychain is the only source.
                assert_eq!(zdb_config::secret::probe::get_password_calls(NAME), 0);
                ws.connect_saved(0, cx);
                // The synchronous UI-thread click handler must NOT have touched
                // the keychain (the deferred background task, not yet driven, will).
                assert_eq!(
                    zdb_config::secret::probe::get_password_calls(NAME),
                    0,
                    "connect_saved read the keychain on the UI thread (hangs macOS)"
                );
            })
            .unwrap();

        // Drive the executor so start_connect's background password lookup runs.
        let mut resolved = false;
        for _ in 0..300 {
            cx.run_until_parked();
            if zdb_config::secret::probe::get_password_calls(NAME) > 0 {
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
