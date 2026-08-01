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
            cx.new(|cx| InputState::new(window, cx).placeholder(ph).default_value(val))
        };
        Self {
            name: input(window, cx, "name", ""),
            host: input(window, cx, "host", "127.0.0.1"),
            port: input(window, cx, "port", "5432"),
            user: input(window, cx, "user", ""),
            db: input(window, cx, "database", ""),
            password: input(window, cx, "password", ""),
            ssl: input(window, cx, "sslmode (disable/prefer/require/verify-full)", "prefer"),
        }
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
            self.conn_manager_open = true;
            cx.notify();
        }
    }

    pub(super) fn toggle_connections(&mut self, cx: &mut Context<Self>) {
        self.conn_manager_open = !self.conn_manager_open;
        if self.conn_manager_open {
            // No saved connections → go straight to the add form.
            self.conn_adding = self.settings.connections.is_empty();
        }
        cx.notify();
    }

    pub(super) fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = !self.settings_open;
        cx.notify();
    }

    /// Switch the theme live and persist it to settings.json.
    pub(super) fn set_theme(&mut self, theme: zdb_config::Theme, window: &mut Window, cx: &mut Context<Self>) {
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

    pub(super) fn switch_connection(&mut self, cfg: ConnectionConfig, cx: &mut Context<Self>) {
        if let Some(old) = self.conn.take() {
            self.db.disconnect(old);
        }
        self.tree.reset(cx);
        self.close_all_tabs(cx);
        self.cfg = Some(cfg.clone());
        self.conn_manager_open = false;
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
        let Some(entry) = self.settings.connections.get(idx).cloned() else { return };
        // Only the session cache here — no keychain read. On macOS a Keychain
        // lookup can block (and pop a system prompt); doing it on this UI-thread
        // click handler freezes the render loop (the app "hangs" on select).
        // `start_connect` resolves the stored password off the UI thread.
        let pw = self.passwords.get(&entry.name).cloned();
        let cfg = entry_to_config(&entry, pw);
        self.switch_connection(cfg, cx);
    }

    pub(super) fn add_connection(&mut self, cx: &mut Context<Self>) {
        let name = self.form.name.read(cx).value().trim().to_string();
        let host = self.form.host.read(cx).value().trim().to_string();
        let user = self.form.user.read(cx).value().trim().to_string();
        let dbname = self.form.db.read(cx).value().trim().to_string();
        let port = self.form.port.read(cx).value().trim().parse().unwrap_or(5432);
        let ssl = self.form.ssl.read(cx).value().trim().to_string();
        let password = self.form.password.read(cx).value().to_string();

        if name.is_empty() || host.is_empty() || user.is_empty() || dbname.is_empty() {
            self.status = "Fill name, host, user, and database".into();
            cx.notify();
            return;
        }

        let entry = ConnectionEntry {
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
        self.switch_connection(entry_to_config(&entry, pw), cx);
    }

    pub(super) fn start_connect(&mut self, cfg: ConnectionConfig, cx: &mut Context<Self>) {
        self.status = format!("Connecting to {}…", cfg.name);
        let db = self.db.clone();
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
        self.spawn_db(cx, fut, move |this, (result, lsp_cfg), cx| {
            match result {
                Ok(conn) => {
                    log(format!("connected (conn={conn})"));
                    this.conn = Some(conn);
                    this.status = "Connected. Loading schemas…".into();
                    this.start_lsp(&lsp_cfg);
                    this.load_schemas(cx);
                }
                Err(e) => {
                    log(format!("connect failed: {e}"));
                    this.status = format!("Connection failed: {e}");
                }
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
        let config_path = match lsp::write_config(cfg) {
            Ok(p) => p,
            Err(e) => {
                log(format!("sqls config write failed: {e}"));
                return;
            }
        };
        match lsp::LspHandle::spawn(&exe, config_path, cfg.password.clone()) {
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

    #[gpui::test]
    fn no_connection_opens_add_form(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, _cx| {
                assert!(ws.conn.is_none());
                assert!(ws.conn_manager_open);
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
                assert!(ws.status.starts_with("Connection failed"), "status: {}", ws.status);
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
