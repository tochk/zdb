//! Connection management: connect/switch/save connections, the settings
//! modal + theme switch, and starting the `sqls` completion server.

use super::*;

/// The add-connection form's input fields.
pub(super) struct ConnForm {
    pub(super) name: Entity<InputState>,
    pub(super) host: Entity<InputState>,
    pub(super) port: Entity<InputState>,
    pub(super) user: Entity<InputState>,
    pub(super) db: Entity<InputState>,
    pub(super) password: Entity<InputState>,
    pub(super) ssl: Entity<InputState>,
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
        Self {
            name: input(window, cx, "name", ""),
            host: input(window, cx, "host", "127.0.0.1"),
            port: input(window, cx, "port", "5432"),
            user: input(window, cx, "user", ""),
            db: input(window, cx, "database", ""),
            password: input(window, cx, "password", ""),
            ssl: input(
                window,
                cx,
                "sslmode (disable/prefer/require/verify-full)",
                "prefer",
            ),
        }
    }
}

/// Settings → connection setup: the parameters and init-SQL editors, and how
/// the last Apply went.
pub(super) struct SetupForm {
    pub(super) params: Entity<InputState>,
    pub(super) init_sql: Entity<InputState>,
    /// Why the last Apply was refused (bad line, or the trial connection
    /// failed).
    pub(super) error: Option<String>,
    /// Waiting for the trial connection of an Apply.
    pub(super) applying: bool,
}

impl SetupForm {
    pub(super) fn new(settings: &Settings, window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let editor = |window: &mut Window, cx: &mut Context<Workspace>, ph: &str, val: String| {
            let ph = ph.to_string();
            cx.new(|cx| {
                // Auto-grow rather than a plain text area: that one takes Tab
                // for indenting, which would trap the dialog's Tab cycle.
                InputState::new(window, cx)
                    .auto_grow(3, 8)
                    .placeholder(ph)
                    .default_value(val)
            })
        };
        Self {
            params: editor(
                window,
                cx,
                "application_name=zdb\nstatement_timeout=30s",
                settings.connection_params.join("\n"),
            ),
            init_sql: editor(
                window,
                cx,
                "SET search_path TO app, public;",
                settings.connection_init_sql.clone(),
            ),
            error: None,
            applying: false,
        }
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

/// The app-wide connection setup in `settings`.
fn conn_setup(settings: &Settings) -> Result<ConnSetup, String> {
    ConnSetup::parse(
        &settings.connection_params.join("\n"),
        &settings.connection_init_sql,
    )
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

    pub(super) fn show_add_form(&mut self, cx: &mut Context<Self>) {
        self.conn_adding = true;
        cx.notify();
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
        let cfg = entry_to_config(&entry, pw);
        let key = (!entry.id.is_empty()).then(|| entry.id.clone());
        self.switch_connection(cfg, key, cx);
    }

    pub(super) fn add_connection(&mut self, cx: &mut Context<Self>) {
        let name = self.form.name.read(cx).value().trim().to_string();
        let host = self.form.host.read(cx).value().trim().to_string();
        let user = self.form.user.read(cx).value().trim().to_string();
        let dbname = self.form.db.read(cx).value().trim().to_string();
        let port = self
            .form
            .port
            .read(cx)
            .value()
            .trim()
            .parse()
            .unwrap_or(5432);
        let ssl = self.form.ssl.read(cx).value().trim().to_string();
        let password = self.form.password.read(cx).value().to_string();

        if name.is_empty() || host.is_empty() || user.is_empty() || dbname.is_empty() {
            self.status = "Fill name, host, user, and database".into();
            cx.notify();
            return;
        }

        // Saving over an existing name edits that connection: it keeps its id,
        // and with it its saved tabs and scratch.
        let id = conn_id_for(&self.settings.connections, &name);
        let entry = ConnectionEntry {
            id,
            name: name.clone(),
            host,
            port,
            dbname,
            user,
            ssl_mode: if ssl.is_empty() { "prefer".into() } else { ssl },
        };
        self.settings.connections.retain(|c| c.name != name);
        self.settings.connections.push(entry.clone());
        if let Err(e) = self.settings.save() {
            self.status = format!("Connection saved for session (disk write failed: {e})");
        }
        let pw = (!password.is_empty()).then(|| password.clone());
        if let Some(pw) = &pw {
            self.passwords.insert(name.clone(), pw.clone());
            if zdb_config::secret::set_password(&name, pw) {
                log(format!("password saved to keychain for '{name}'"));
            }
        }
        self.switch_connection(entry_to_config(&entry, pw), Some(entry.id.clone()), cx);
    }

    pub(super) fn start_connect(&mut self, mut cfg: ConnectionConfig, cx: &mut Context<Self>) {
        // Hand-editable settings: a bad parameter is reported, not ignored.
        match conn_setup(&self.settings) {
            Ok(setup) => cfg.setup = setup,
            Err(e) => {
                self.status = format!("Connection failed: Settings → connection parameters, {e}");
                cx.notify();
                return;
            }
        }
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

    /// Whether the setup editors hold something other than what is saved.
    pub(super) fn setup_edited(&self, cx: &App) -> bool {
        let params = param_lines(&self.setup_form.params.read(cx).value());
        let init_sql = self.setup_form.init_sql.read(cx).value().trim().to_string();
        params != self.settings.connection_params || init_sql != self.settings.connection_init_sql
    }

    /// Settings → connection setup → Apply. Connected, the live pool tries a
    /// connection with the new setup first and takes it over only if that
    /// works; the setup is saved once it is in use (or, not connected, once
    /// it parses).
    pub(super) fn apply_conn_setup(&mut self, cx: &mut Context<Self>) {
        let params_text = self.setup_form.params.read(cx).value().to_string();
        let init_sql = self.setup_form.init_sql.read(cx).value().trim().to_string();
        let setup = match ConnSetup::parse(&params_text, &init_sql) {
            Ok(setup) => setup,
            Err(e) => {
                self.setup_form.error = Some(format!("Connection parameters, {e}"));
                cx.notify();
                return;
            }
        };
        self.setup_form.error = None;
        let params = param_lines(&params_text);
        let Some(conn) = self.conn else {
            self.store_conn_setup(params, init_sql);
            self.status = "Connection setup saved; used from the next connect".into();
            cx.notify();
            return;
        };
        self.setup_form.applying = true;
        let db = self.db.clone();
        self.spawn_db(
            cx,
            async move { db.set_setup(conn, setup).await },
            move |this, res, _cx| {
                this.setup_form.applying = false;
                match res {
                    Ok(()) => {
                        this.store_conn_setup(params, init_sql);
                        this.status = "Connection setup applied to every connection".into();
                    }
                    Err(e) => this.setup_form.error = Some(e.to_string()),
                }
            },
        );
        cx.notify();
    }

    fn store_conn_setup(&mut self, params: Vec<String>, init_sql: String) {
        self.settings.connection_params = params;
        self.settings.connection_init_sql = init_sql;
        self.save_settings();
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

    /// A broken parameter in settings.json stops the connect with a message
    /// instead of being dropped silently.
    #[gpui::test]
    fn bad_saved_setup_fails_the_connect(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                ws.settings.connection_params = vec!["connect_timeout=soon".into()];
                ws.start_connect(ConnectionConfig::new("n", "127.0.0.1", "d", "u"), cx);
                assert!(
                    ws.status.contains("connection parameters, line 1"),
                    "status: {}",
                    ws.status
                );
            })
            .unwrap();
    }

    /// Not connected, Apply only checks the text and saves it; a bad line is
    /// reported in the dialog and nothing is saved.
    #[gpui::test]
    fn apply_setup_checks_then_saves(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let set = |ws: &mut Workspace, params: &str, window: &mut Window, cx: &mut Context<Workspace>| {
                    ws.setup_form
                        .params
                        .update(cx, |i, cx| i.set_value(params.to_string(), window, cx));
                };
                assert!(!ws.setup_edited(cx));
                set(ws, "keepalives=maybe", window, cx);
                ws.setup_form
                    .init_sql
                    .update(cx, |i, cx| i.set_value(" SET lock_timeout = '5s' ", window, cx));
                assert!(ws.setup_edited(cx));
                ws.apply_conn_setup(cx);
                assert!(ws.setup_form.error.as_deref().unwrap().contains("line 1"));
                assert!(ws.settings.connection_params.is_empty());

                set(ws, "application_name=x\n\nwork_mem=64MB", window, cx);
                ws.apply_conn_setup(cx);
                assert_eq!(ws.setup_form.error, None);
                assert_eq!(ws.settings.connection_params, ["application_name=x", "work_mem=64MB"]);
                assert_eq!(ws.settings.connection_init_sql, "SET lock_timeout = '5s'");
                assert!(!ws.setup_edited(cx));
            })
            .unwrap();
    }

    #[test]
    fn resaving_a_connection_keeps_its_id() {
        let entry = |id: &str, name: &str| ConnectionEntry {
            id: id.into(),
            name: name.into(),
            host: "h".into(),
            port: 5432,
            dbname: "d".into(),
            user: "u".into(),
            ssl_mode: "prefer".into(),
        };
        let conns = vec![entry("id-a", "a"), entry("", "old")];
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
                    ws.settings.connections.push(ConnectionEntry {
                        id: String::new(),
                        name: name.into(),
                        host: "127.0.0.1".into(),
                        port: 1, // nothing listens here: connect fails fast
                        dbname: "d".into(),
                        user: "u".into(),
                        ssl_mode: "disable".into(),
                    });
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
                ws.settings.connections.push(ConnectionEntry {
                        id: String::new(),
                    name: NAME.into(),
                    host: "127.0.0.1".into(),
                    port: 1, // nothing listens: the connect itself fails fast
                    dbname: "d".into(),
                    user: "u".into(),
                    ssl_mode: "disable".into(),
                });
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
            .update(cx, |ws, _w, cx| {
                ws.settings.connections.push(ConnectionEntry {
                        id: String::new(),
                    name: "x".into(),
                    host: "h".into(),
                    port: 5432,
                    dbname: "d".into(),
                    user: "u".into(),
                    ssl_mode: "prefer".into(),
                });
                ws.show_conn_list(cx);
                assert!(!ws.conn_adding);
                ws.show_add_form(cx);
                assert!(ws.conn_adding);
            })
            .unwrap();
    }
}
