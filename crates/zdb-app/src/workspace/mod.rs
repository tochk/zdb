//! The main application view: schema sidebar, center (SQL editor + results
//! table), and a bottom area (query log + pending-edit review).
//!
//! Editing works for any query whose result maps to a single table with a known
//! primary key (resolved via `DbHandle::describe`). Cells are edited inline in
//! the grid; an edit is staged and its SQL shown before you Apply it. Clicking a
//! column header re-runs the query ordered by that column. Every executed query
//! is recorded in the query log.

use gpui::{
    actions, div, prelude::FluentBuilder as _, px, rgba, App, AppContext, ClickEvent, Context,
    Entity, Focusable, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, WindowControlArea,
};
// Used only for the Linux title-bar drag (Windows drags natively via hit-test).
#[cfg(not(target_os = "windows"))]
use gpui::MouseButton;
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputEvent, InputState},
    resizable::{h_resizable, resizable_panel, v_resizable},
    table::{ColumnSort, Table},
    v_flex, Disableable, Icon, IconName, Sizable,
};
use std::collections::HashMap;
use zdb_config::{ConnectionEntry, Settings};
use zdb_db::{
    CellValue, ConnId, ConnectionConfig, DbHandle, EditTarget, QueryEvent, RelationKind, RowEdit,
};

mod colors;
mod grid;
mod util;
mod view;

use colors::{palette, Colors};
use grid::{Tab, TabKind};
use util::{entry_to_config, oneline, order_by_sql, rel_icon};

actions!(
    zdb,
    [
        RunQuery,
        CancelQuery,
        TogglePalette,
        ClosePalette,
        ToggleTerminal,
        ToggleConnections,
        ToggleScratch,
        ToggleSettings
    ]
);

const ROW_LIMIT: usize = 500;
const LOG_CAP: usize = 500;

/// One command-palette entry.
#[derive(Clone, Copy)]
enum PaletteCmd {
    Run,
    Cancel,
    Refresh,
    Terminal,
    Connections,
    Settings,
}

const PALETTE_COMMANDS: &[(&str, PaletteCmd)] = &[
    ("Run query", PaletteCmd::Run),
    ("Cancel query", PaletteCmd::Cancel),
    ("Refresh schemas", PaletteCmd::Refresh),
    ("Manage connections", PaletteCmd::Connections),
    ("Open settings", PaletteCmd::Settings),
    ("Toggle terminal", PaletteCmd::Terminal),
];

/// Lightweight milestone logging to stderr, enabled by setting `ZDB_LOG`.
fn log(msg: impl AsRef<str>) {
    if std::env::var_os("ZDB_LOG").is_some() {
        eprintln!("[zdb] {}", msg.as_ref());
    }
}

struct SchemaNode {
    name: String,
    expanded: bool,
    relations: Option<Vec<RelNode>>,
}

struct RelNode {
    name: String,
    kind: RelationKind,
}

struct LogEntry {
    sql: String,
    ok: bool,
}

// ---- workspace ----------------------------------------------------------

pub struct Workspace {
    db: DbHandle,
    cfg: Option<ConnectionConfig>,
    conn: Option<ConnId>,
    status: String,
    tree: Vec<SchemaNode>,

    /// Open center tabs and the index of the active one (None = welcome pane).
    tabs: Vec<Tab>,
    active: Option<usize>,
    /// Monotonic id source for tabs + the "Query N" counter.
    next_tab_id: u64,
    /// A table to open on the next render (set from windowless async contexts,
    /// e.g. the selftest, where no `&mut Window` is available).
    pending_open: Option<(String, String)>,

    /// Shared inline-cell editor (only the active tab edits a cell at a time).
    cell_input: Entity<InputState>,

    // Query log (most recent last).
    log_entries: Vec<LogEntry>,

    // Command palette.
    palette_open: bool,
    palette_input: Entity<InputState>,

    // Embedded terminal.
    terminal: Option<crate::terminal::Terminal>,
    terminal_open: bool,

    // Connections.
    settings: Settings,
    passwords: HashMap<String, String>,
    conn_manager_open: bool,
    /// Whether the connection manager is showing the add form.
    conn_adding: bool,
    settings_open: bool,
    f_name: Entity<InputState>,
    f_host: Entity<InputState>,
    f_port: Entity<InputState>,
    f_user: Entity<InputState>,
    f_db: Entity<InputState>,
    f_password: Entity<InputState>,
    f_ssl: Entity<InputState>,
}

impl Workspace {
    pub fn new(
        db: DbHandle,
        settings: Settings,
        auto: Option<ConnectionConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let palette_input = cx.new(|cx| InputState::new(window, cx).placeholder("Type a command…"));
        let cell_input = cx.new(|cx| InputState::new(window, cx));

        // Commit/cancel inline edits on Enter / focus loss (acts on the active tab).
        cx.subscribe(&cell_input, |this, _input, event: &InputEvent, cx| match event {
            InputEvent::PressEnter { .. } => this.commit_cell_edit(cx),
            InputEvent::Blur => this.cancel_cell_edit(cx),
            _ => {}
        })
        .detach();

        let input = |window: &mut Window, cx: &mut Context<Self>, ph: &str, val: &str| {
            let ph = ph.to_string();
            let val = val.to_string();
            cx.new(|cx| InputState::new(window, cx).placeholder(ph).default_value(val))
        };
        let f_name = input(window, cx, "name", "");
        let f_host = input(window, cx, "host", "127.0.0.1");
        let f_port = input(window, cx, "port", "5432");
        let f_user = input(window, cx, "user", "");
        let f_db = input(window, cx, "database", "");
        let f_password = input(window, cx, "password", "");
        let f_ssl = input(window, cx, "sslmode (disable/prefer/require/verify-full)", "prefer");

        let mut this = Self {
            db,
            cfg: auto.clone(),
            conn: None,
            status: "Not connected".into(),
            tree: Vec::new(),
            tabs: Vec::new(),
            active: None,
            next_tab_id: 1,
            pending_open: None,
            cell_input,
            log_entries: Vec::new(),
            palette_open: false,
            palette_input,
            terminal: None,
            terminal_open: false,
            settings,
            passwords: HashMap::new(),
            conn_manager_open: false,
            conn_adding: false,
            settings_open: false,
            f_name,
            f_host,
            f_port,
            f_user,
            f_db,
            f_password,
            f_ssl,
        };
        if let Some(cfg) = auto {
            if let Some(pw) = &cfg.password {
                this.passwords.insert(cfg.name.clone(), pw.clone());
            }
            this.start_connect(cfg, cx);
        } else {
            this.conn_manager_open = true;
            this.conn_adding = this.settings.connections.is_empty();
        }
        this
    }

    // ---- tab management --------------------------------------------------

    fn tab(&self, id: u64) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    fn tab_mut(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    fn active_tab(&self) -> Option<&Tab> {
        self.active.and_then(|i| self.tabs.get(i))
    }

    fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        match self.active {
            Some(i) => self.tabs.get_mut(i),
            None => None,
        }
    }

    fn active_id(&self) -> Option<u64> {
        self.active_tab().map(|t| t.id)
    }

    /// Make tab `idx` active: cancel any in-flight cell edit and focus its input.
    fn activate_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        self.cancel_cell_edit(cx);
        self.active = Some(idx);
        let focus = match self.tabs[idx].kind {
            TabKind::Table { .. } => self.tabs[idx].where_input.clone(),
            _ => self.tabs[idx].editor.clone(),
        };
        // Focusing a gpui-component input touches the window's `Root` layer, which
        // only exists under a real app (not in headless `#[gpui::test]` windows).
        if window.root::<gpui_component::Root>().flatten().is_some() {
            focus.read(cx).focus_handle(cx).focus(window);
        }
        cx.notify();
    }

    /// Open a fresh blank "Query N" tab and focus it.
    fn open_query_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let n = self
            .tabs
            .iter()
            .filter(|t| matches!(t.kind, TabKind::Query))
            .count() as u64
            + 1;
        let tab = Tab::query(id, n, window, cx);
        self.tabs.push(tab);
        self.activate_tab(self.tabs.len() - 1, window, cx);
    }

    /// Open (or focus, if already open) a table-browse tab and run its query.
    fn open_table_tab(
        &mut self,
        schema: String,
        table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(idx) = self.tabs.iter().position(
            |t| matches!(&t.kind, TabKind::Table { schema: s, table: tt } if *s == schema && *tt == table),
        ) {
            self.activate_tab(idx, window, cx);
            return;
        }
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let tab = Tab::table(id, schema, table, window, cx);
        self.tabs.push(tab);
        self.activate_tab(self.tabs.len() - 1, window, cx);
        let sql = self.table_query(id, cx);
        self.run_new_query(id, sql, cx);
    }

    /// Focus the singleton scratch tab, opening it if absent.
    fn focus_scratch_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(idx) = self.tabs.iter().position(|t| matches!(t.kind, TabKind::Scratch)) {
            self.activate_tab(idx, window, cx);
            return;
        }
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let tab = Tab::scratch(id, window, cx);
        self.tabs.push(tab);
        self.activate_tab(self.tabs.len() - 1, window, cx);
    }

    fn close_tab(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        let active_id = self.active_id();
        self.tabs.remove(idx);
        if self.tabs.is_empty() {
            self.active = None;
        } else if active_id == Some(id) {
            // Closed the active tab: focus the neighbor at the same slot (clamped).
            self.active = Some(idx.min(self.tabs.len() - 1));
        } else {
            // Some other tab closed: keep the same active tab (index may have shifted).
            self.active = active_id.and_then(|aid| self.tabs.iter().position(|t| t.id == aid));
        }
        cx.notify();
    }

    /// Drop every tab (e.g. on connection switch — old results are invalid).
    fn close_all_tabs(&mut self, cx: &mut Context<Self>) {
        self.tabs.clear();
        self.active = None;
        cx.notify();
    }

    // ---- connections -----------------------------------------------------

    fn connect_or_refresh(&mut self, cx: &mut Context<Self>) {
        if self.conn.is_some() {
            self.load_schemas(cx);
        } else if let Some(cfg) = self.cfg.clone() {
            self.start_connect(cfg, cx);
        } else {
            self.conn_manager_open = true;
            cx.notify();
        }
    }

    fn toggle_connections(&mut self, cx: &mut Context<Self>) {
        self.conn_manager_open = !self.conn_manager_open;
        if self.conn_manager_open {
            // No saved connections → go straight to the add form.
            self.conn_adding = self.settings.connections.is_empty();
        }
        cx.notify();
    }

    fn toggle_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_open = !self.settings_open;
        cx.notify();
    }

    /// Switch the theme live and persist it to settings.json.
    fn set_theme(&mut self, theme: zdb_config::Theme, window: &mut Window, cx: &mut Context<Self>) {
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

    fn switch_connection(&mut self, cfg: ConnectionConfig, cx: &mut Context<Self>) {
        if let Some(old) = self.conn.take() {
            self.db.disconnect(old);
        }
        self.tree.clear();
        self.close_all_tabs(cx);
        self.cfg = Some(cfg.clone());
        self.conn_manager_open = false;
        self.start_connect(cfg, cx);
    }

    fn show_add_form(&mut self, cx: &mut Context<Self>) {
        self.conn_adding = true;
        cx.notify();
    }

    fn show_conn_list(&mut self, cx: &mut Context<Self>) {
        self.conn_adding = false;
        cx.notify();
    }

    fn connect_saved(&mut self, idx: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.settings.connections.get(idx).cloned() else { return };
        let pw = self
            .passwords
            .get(&entry.name)
            .cloned()
            .or_else(|| zdb_config::secret::get_password(&entry.name));
        let cfg = entry_to_config(&entry, pw);
        self.switch_connection(cfg, cx);
    }

    fn add_connection(&mut self, cx: &mut Context<Self>) {
        let name = self.f_name.read(cx).value().trim().to_string();
        let host = self.f_host.read(cx).value().trim().to_string();
        let user = self.f_user.read(cx).value().trim().to_string();
        let dbname = self.f_db.read(cx).value().trim().to_string();
        let port = self.f_port.read(cx).value().trim().parse().unwrap_or(5432);
        let ssl = self.f_ssl.read(cx).value().trim().to_string();
        let password = self.f_password.read(cx).value().to_string();

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

    fn start_connect(&mut self, cfg: ConnectionConfig, cx: &mut Context<Self>) {
        self.status = format!("Connecting to {}…", cfg.name);
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let result = db.connect(cfg).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(conn) => {
                        log(format!("connected (conn={conn})"));
                        this.conn = Some(conn);
                        this.status = "Connected. Loading schemas…".into();
                        this.load_schemas(cx);
                    }
                    Err(e) => {
                        log(format!("connect failed: {e}"));
                        this.status = format!("Connection failed: {e}");
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn load_schemas(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = self.conn else { return };
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let result = db.schemas(conn).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(schemas) => {
                        this.tree = schemas
                            .into_iter()
                            .map(|s| SchemaNode {
                                name: s.name,
                                expanded: false,
                                relations: None,
                            })
                            .collect();
                        log(format!("schemas loaded: {}", this.tree.len()));
                        this.status = format!("{} schema(s)", this.tree.len());
                        if std::env::var_os("ZDB_SELFTEST").is_some() {
                            this.selftest(cx);
                        }
                    }
                    Err(e) => this.status = format!("Failed to load schemas: {e}"),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_schema(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(node) = self.tree.get_mut(ix) else { return };
        node.expanded = !node.expanded;
        let need_load = node.expanded && node.relations.is_none();
        let schema = node.name.clone();
        if need_load {
            self.load_relations(ix, schema, cx);
        }
        cx.notify();
    }

    fn load_relations(&mut self, ix: usize, schema: String, cx: &mut Context<Self>) {
        let Some(conn) = self.conn else { return };
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let result = db.relations(conn, schema).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(rels) => {
                        if let Some(node) = this.tree.get_mut(ix) {
                            node.relations = Some(
                                rels.into_iter()
                                    .map(|r| RelNode {
                                        name: r.name,
                                        kind: r.kind,
                                    })
                                    .collect(),
                            );
                        }
                    }
                    Err(e) => this.status = format!("Failed to load relations: {e}"),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// `SELECT * FROM <table> [WHERE …] LIMIT n` for a table tab.
    fn table_query(&self, tab_id: u64, cx: &App) -> String {
        let Some(tab) = self.tab(tab_id) else {
            return String::new();
        };
        let TabKind::Table { schema: s, table: t } = &tab.kind else {
            return String::new();
        };
        let w = tab.where_input.read(cx).value().trim().to_string();
        let filter = if w.is_empty() {
            String::new()
        } else {
            format!(" WHERE {w}")
        };
        format!(
            "SELECT * FROM \"{}\".\"{}\"{} LIMIT {}",
            s.replace('"', "\"\""),
            t.replace('"', "\"\""),
            filter,
            ROW_LIMIT
        )
    }

    fn apply_where(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if !matches!(self.tab(tab_id).map(|t| &t.kind), Some(TabKind::Table { .. })) {
            return;
        }
        let sql = self.table_query(tab_id, cx);
        self.run_new_query(tab_id, sql, cx);
    }

    // ---- query execution -------------------------------------------------

    /// Run the active tab: Query/Scratch run their editor SQL; a Table tab reloads.
    fn run_active_tab(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else { return };
        let id = tab.id;
        let is_table = matches!(tab.kind, TabKind::Table { .. });
        let sql = tab.editor.read(cx).value().to_string();
        if is_table {
            self.reload_data(id, cx);
        } else {
            self.run_new_query(id, sql, cx);
        }
    }

    /// Re-run a tab's current query/table from the DB (discards staged edits).
    fn reload_data(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(sql) = self.tab(tab_id).and_then(|t| t.base_sql.clone()) {
            self.run_new_query(tab_id, sql, cx);
        }
    }

    /// Start a new query for `tab_id`: remember it as the sort base, drop stale
    /// editability, then describe (for editability) and execute it.
    fn run_new_query(&mut self, tab_id: u64, base: String, cx: &mut Context<Self>) {
        let table = {
            let Some(tab) = self.tab_mut(tab_id) else { return };
            tab.base_sql = Some(base.clone());
            tab.sort_state = None;
            tab.edit_target = None;
            tab.edit_cols.clear();
            tab.editing = None;
            tab.current_row = None;
            tab.pending.clear();
            tab.edited_cells.clear();
            tab.table.clone()
        };
        // Drop the Table widget's own row highlight; otherwise the previously
        // selected index stays lit when switching to a different table / reloading.
        table.update(cx, |ts, cx| ts.clear_selection(cx));
        self.describe_async(tab_id, base.clone(), cx);
        self.run_sql(tab_id, base, cx);
    }

    fn describe_async(&mut self, tab_id: u64, sql: String, cx: &mut Context<Self>) {
        let Some(conn) = self.conn else { return };
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let res = db.describe(conn, sql).await;
            this.update(cx, |this, cx| {
                let Some(tab) = this.tab_mut(tab_id) else { return };
                match res {
                    Ok(Some(d)) => {
                        tab.edit_target = Some(d.target);
                        tab.edit_cols = d.columns;
                    }
                    _ => {
                        tab.edit_target = None;
                        tab.edit_cols.clear();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Execute `sql` for display in `tab_id` (does not change editability or sort base).
    fn run_sql(&mut self, tab_id: u64, sql: String, cx: &mut Context<Self>) {
        let Some(conn) = self.conn else {
            self.status = "Not connected".into();
            cx.notify();
            return;
        };
        if sql.trim().is_empty() {
            if let Some(tab) = self.tab_mut(tab_id) {
                tab.running = false;
            }
            self.status = "Nothing to run".into();
            cx.notify();
            return;
        }
        {
            let Some(tab) = self.tab_mut(tab_id) else { return };
            tab.running = true;
            tab.editing = None;
            tab.new_row_idx = None;
            tab.last_sql = Some(sql.clone());
        }
        self.status = "Running…".into();
        log(format!("run: {sql}"));
        let db = self.db.clone();
        let logged = sql.clone();
        cx.spawn(async move |this, cx| {
            let mut rx = db.query(conn, sql);
            let mut headers: Vec<String> = Vec::new();
            let mut rows: Vec<Vec<CellValue>> = Vec::new();
            let mut affected = 0u64;
            let mut elapsed = std::time::Duration::ZERO;
            let mut error: Option<String> = None;
            while let Some(ev) = rx.recv().await {
                match ev {
                    QueryEvent::Columns(c) => headers = c.iter().map(|m| m.name.clone()).collect(),
                    QueryEvent::Rows(mut r) => rows.append(&mut r),
                    QueryEvent::Done { affected: a, elapsed: e } => {
                        affected = a;
                        elapsed = e;
                    }
                    QueryEvent::Failed(e) => error = Some(e.to_string()),
                }
            }
            this.update(cx, |this, cx| {
                // The tab may have been closed while the query ran.
                let Some(tab) = this.tab_mut(tab_id) else { return };
                tab.running = false;
                let status = match error {
                    Some(e) => {
                        let s = format!("Error: {e}");
                        this.push_log(&logged, false);
                        s
                    }
                    None => {
                        let is_select = !headers.is_empty();
                        let n = rows.len();
                        let table = {
                            let Some(tab) = this.tab_mut(tab_id) else { return };
                            tab.headers = headers.clone();
                            tab.rows = rows.clone();
                            tab.orig_rows = rows.clone();
                            // Fresh rows just landed: drop any prior selection so a
                            // previously-selected index doesn't stay lit on the new
                            // data (switching tables, re-sorting, reload). Single
                            // point every result passes through.
                            tab.current_row = None;
                            tab.table.clone()
                        };
                        table.update(cx, |ts, cx| {
                            ts.clear_selection(cx);
                            ts.delegate_mut().set(&headers, rows);
                            ts.refresh(cx);
                        });
                        this.push_log(&logged, true);
                        if is_select {
                            format!("{n} row(s) in {elapsed:?}")
                        } else {
                            format!("{affected} row(s) affected in {elapsed:?}")
                        }
                    }
                };
                this.status = status;
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(conn) = self.conn {
            self.db.cancel(conn);
            self.status = "Cancel requested".into();
            cx.notify();
        }
    }

    fn push_log(&mut self, sql: &str, ok: bool) {
        self.log_entries.push(LogEntry {
            sql: sql.to_string(),
            ok,
        });
        if self.log_entries.len() > LOG_CAP {
            self.log_entries.remove(0);
        }
    }

    // ---- sorting ---------------------------------------------------------

    /// Clicking a header cycles its sort: none → ascending → descending → none,
    /// re-running the query ordered by that column.
    fn toggle_sort(&mut self, tab_id: u64, col_ix: usize, cx: &mut Context<Self>) {
        let (next, base, headers) = {
            let Some(tab) = self.tab_mut(tab_id) else { return };
            let next = match tab.sort_state {
                Some((c, false)) if c == col_ix => Some((col_ix, true)),
                Some((c, true)) if c == col_ix => None,
                _ => Some((col_ix, false)),
            };
            tab.sort_state = next;
            (next, tab.base_sql.clone(), tab.headers.clone())
        };
        let Some(base) = base else {
            log("sort: no base query");
            return;
        };
        let sql = match next {
            Some((c, desc)) => {
                let col = headers.get(c).cloned().unwrap_or_default();
                let dir = if desc {
                    ColumnSort::Descending
                } else {
                    ColumnSort::Ascending
                };
                order_by_sql(&base, &col, dir)
            }
            None => base,
        };
        log(format!("sort: {sql}"));
        self.run_sql(tab_id, sql, cx);
    }

    fn set_current_row(&mut self, tab_id: u64, row: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.current_row = Some(row);
        }
        cx.notify();
    }

    // ---- inline editing --------------------------------------------------

    fn begin_edit(
        &mut self,
        tab_id: u64,
        row: usize,
        col: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = {
            let Some(tab) = self.tab_mut(tab_id) else { return };
            if tab.edit_cols.get(col).map_or(true, |o| o.is_none()) {
                return;
            }
            let text = match tab.rows.get(row).and_then(|r| r.get(col)) {
                Some(CellValue::Text(s)) => s.clone(),
                _ => String::new(),
            };
            tab.editing = Some((row, col));
            tab.current_row = Some(row);
            text
        };
        self.cell_input.update(cx, |inp, cx| inp.set_value(text, window, cx));
        let handle = self.cell_input.read(cx).focus_handle(cx);
        handle.focus(window);
        cx.notify();
    }

    fn cancel_cell_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_tab_mut() {
            if tab.editing.take().is_some() {
                cx.notify();
            }
        }
    }

    /// Stage the active tab's cell edit (build the UPDATE and show it for review).
    fn commit_cell_edit(&mut self, cx: &mut Context<Self>) {
        let Some(tab_id) = self.active_id() else { return };
        let Some((row, col)) = self.tab_mut(tab_id).and_then(|t| t.editing.take()) else {
            return;
        };
        let text = self.cell_input.read(cx).value().to_string();
        let value = if text.is_empty() {
            CellValue::Null
        } else {
            CellValue::Text(text)
        };

        // The unsaved new row accumulates in memory until "Save row".
        if Some(row) == self.tab(tab_id).and_then(|t| t.new_row_idx) {
            if let Some(tab) = self.tab_mut(tab_id) {
                if let Some(c) = tab.rows.get_mut(row).and_then(|r| r.get_mut(col)) {
                    *c = value;
                }
            }
            self.refresh_table(tab_id, cx);
            cx.notify();
            return;
        }

        let Some(target) = self.tab(tab_id).and_then(|t| t.edit_target.clone()) else { return };
        let Some(real_col) = self
            .tab(tab_id)
            .and_then(|t| t.edit_cols.get(col).and_then(|o| o.clone()))
        else {
            return;
        };
        // PK must be read from the original row value before the optimistic update.
        let Some(pk) = self.row_pk(tab_id, row, &target) else { return };

        // Drop any earlier staged change for this same cell, so re-editing a cell
        // replaces (not stacks) its UPDATE and reverting clears it cleanly.
        self.remove_pending_update(tab_id, &pk, &real_col);

        let reverted = self
            .tab(tab_id)
            .and_then(|t| t.orig_rows.get(row).and_then(|r| r.get(col)))
            == Some(&value);
        if reverted {
            // Edited back to the original value: nothing to save. Drop the marker
            // and the staged edit (already removed above).
            if let Some(tab) = self.tab_mut(tab_id) {
                tab.edited_cells.remove(&(row, col));
            }
            self.status = "Reverted to original".into();
        } else {
            self.stage(
                tab_id,
                RowEdit::Update {
                    pk,
                    set: vec![(real_col, value.clone())],
                },
                &target,
                cx,
            );
            if let Some(tab) = self.tab_mut(tab_id) {
                tab.edited_cells.insert((row, col));
            }
        }
        // Reflect the value in the grid immediately. Cancel/Apply both reload the
        // authoritative rows.
        if let Some(tab) = self.tab_mut(tab_id) {
            if let Some(c) = tab.rows.get_mut(row).and_then(|r| r.get_mut(col)) {
                *c = value;
            }
        }
        self.refresh_table(tab_id, cx);
        cx.notify();
    }

    /// Remove any staged `Update` for the given PK that sets `col` (used to
    /// dedup re-edits and to drop an edit reverted to its original value).
    fn remove_pending_update(&mut self, tab_id: u64, pk: &[(String, CellValue)], col: &str) {
        let Some(tab) = self.tab_mut(tab_id) else { return };
        tab.pending.retain_mut(|e| match e {
            RowEdit::Update { pk: p, set } if p.as_slice() == pk => {
                set.retain(|(c, _)| c != col);
                !set.is_empty()
            }
            _ => true,
        });
    }

    /// Append a blank editable row; cells are filled by double-clicking.
    fn add_row(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        {
            let Some(tab) = self.tab_mut(tab_id) else { return };
            if tab.edit_target.is_none() {
                return;
            }
            let blank = vec![CellValue::Null; tab.headers.len()];
            tab.rows.push(blank);
            let idx = tab.rows.len() - 1;
            tab.new_row_idx = Some(idx);
            tab.current_row = Some(idx);
        }
        self.status = "New row — double-click cells to fill, then Save row.".into();
        self.refresh_table(tab_id, cx);
        cx.notify();
    }

    /// Stage an INSERT from the new row's filled (non-null) columns.
    fn save_new_row(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let (target, values) = {
            let Some(tab) = self.tab(tab_id) else { return };
            let (Some(idx), Some(target)) = (tab.new_row_idx, tab.edit_target.clone()) else {
                return;
            };
            let Some(row) = tab.rows.get(idx).cloned() else { return };
            let values: Vec<(String, CellValue)> = tab
                .edit_cols
                .iter()
                .enumerate()
                .filter_map(|(i, real)| {
                    let name = real.clone()?;
                    match row.get(i) {
                        Some(CellValue::Text(s)) => Some((name, CellValue::Text(s.clone()))),
                        _ => None, // skip nulls so DB defaults apply
                    }
                })
                .collect();
            (target, values)
        };
        if values.is_empty() {
            self.status = "Fill at least one column".into();
            cx.notify();
            return;
        }
        self.stage(tab_id, RowEdit::Insert { values }, &target, cx);
    }

    /// Stage a DELETE for the selected row, or discard the unsaved new row.
    fn delete_current_row(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(row) = self.tab(tab_id).and_then(|t| t.current_row) else { return };
        if Some(row) == self.tab(tab_id).and_then(|t| t.new_row_idx) {
            if let Some(tab) = self.tab_mut(tab_id) {
                if row < tab.rows.len() {
                    tab.rows.remove(row);
                }
                tab.new_row_idx = None;
                tab.current_row = None;
            }
            self.status = "New row discarded".into();
            self.refresh_table(tab_id, cx);
            cx.notify();
            return;
        }
        let Some(target) = self.tab(tab_id).and_then(|t| t.edit_target.clone()) else { return };
        let Some(pk) = self.row_pk(tab_id, row, &target) else { return };
        self.stage(tab_id, RowEdit::Delete { pk }, &target, cx);
    }

    fn refresh_table(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some((headers, rows, table)) = self
            .tab(tab_id)
            .map(|t| (t.headers.clone(), t.rows.clone(), t.table.clone()))
        else {
            return;
        };
        table.update(cx, |ts, cx| {
            ts.delegate_mut().set(&headers, rows);
            ts.refresh(cx);
        });
    }

    /// Add an edit to the tab's staged batch. Edits accumulate across the table
    /// and are all applied in one transaction on Apply.
    fn stage(&mut self, tab_id: u64, edit: RowEdit, target: &EditTarget, cx: &mut Context<Self>) {
        // Validate the statement builds before queueing it.
        if let Err(e) = edit.to_sql(target) {
            self.status = format!("Cannot build statement: {e}");
            cx.notify();
            return;
        }
        let n = {
            let Some(tab) = self.tab_mut(tab_id) else { return };
            tab.pending.push(edit);
            tab.pending.len()
        };
        self.status = format!(
            "{n} change{} staged — review, then Apply.",
            if n == 1 { "" } else { "s" }
        );
        cx.notify();
    }

    /// Combined SQL for a tab's staged edits (shown in the review pane).
    fn pending_sql(&self, tab_id: u64) -> Option<String> {
        let tab = self.tab(tab_id)?;
        let target = tab.edit_target.as_ref()?;
        zdb_db::build_batch(target, &tab.pending).ok().flatten()
    }

    fn cancel_pending(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let reload = {
            let Some(tab) = self.tab_mut(tab_id) else { return };
            tab.pending.clear();
            tab.edited_cells.clear();
            tab.last_sql.clone()
        };
        self.status = "Edits discarded".into();
        // Discard optimistic in-grid changes by reloading the original rows.
        if let Some(s) = reload {
            self.run_sql(tab_id, s, cx);
        }
        cx.notify();
    }

    /// Execute a tab's staged edits in a single transaction, then reload it.
    fn apply_pending(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let (target, edits, reload) = {
            let Some(tab) = self.tab_mut(tab_id) else { return };
            if tab.pending.is_empty() {
                return;
            }
            let Some(target) = tab.edit_target.clone() else { return };
            let edits = std::mem::take(&mut tab.pending);
            (target, edits, tab.last_sql.clone())
        };
        let Some(conn) = self.conn else {
            // No connection: put the edits back so they aren't lost.
            if let Some(tab) = self.tab_mut(tab_id) {
                tab.pending = edits;
            }
            return;
        };
        let sql = zdb_db::build_batch(&target, &edits)
            .ok()
            .flatten()
            .unwrap_or_default();
        self.status = "Applying…".into();
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let res = db.apply_edits(conn, &target, &edits).await;
            this.update(cx, |this, cx| {
                let status = match res {
                    Ok(()) => {
                        this.push_log(&sql, true);
                        if let Some(tab) = this.tab_mut(tab_id) {
                            tab.edited_cells.clear();
                        }
                        if let Some(s) = reload {
                            this.run_sql(tab_id, s, cx);
                        }
                        format!("Applied {} change(s)", edits.len())
                    }
                    Err(e) => {
                        // Keep the edits staged so the user can fix and retry.
                        if let Some(tab) = this.tab_mut(tab_id) {
                            tab.pending = edits;
                        }
                        this.push_log(&sql, false);
                        format!("Apply failed: {e}")
                    }
                };
                this.status = status;
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Primary-key (name, original value) pairs for a result row in `tab_id`.
    fn row_pk(
        &self,
        tab_id: u64,
        row: usize,
        target: &EditTarget,
    ) -> Option<Vec<(String, CellValue)>> {
        let tab = self.tab(tab_id)?;
        let r = tab.rows.get(row)?;
        target
            .pk_columns
            .iter()
            .map(|pk| {
                let idx = tab.edit_cols.iter().position(|c| c.as_deref() == Some(pk.as_str()))?;
                Some((pk.clone(), r.get(idx).cloned().unwrap_or(CellValue::Null)))
            })
            .collect()
    }

    // ---- selftest / palette / terminal -----------------------------------

    fn selftest(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = self.conn else { return };
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            for sql in [
                "DROP TABLE IF EXISTS public.zdb_selftest",
                "CREATE TABLE public.zdb_selftest (id int primary key, name text)",
                "INSERT INTO public.zdb_selftest VALUES (1,'alpha'),(2,NULL)",
            ] {
                let mut rx = db.query(conn, sql);
                while rx.recv().await.is_some() {}
            }
            this.update(cx, |this, cx| {
                // Opening a tab needs the window; defer to the next render (which
                // has it) via `pending_open`.
                this.pending_open = Some(("public".into(), "zdb_selftest".into()));
                log("selftest: table tab requested");
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_palette(&mut self, cx: &mut Context<Self>) {
        self.palette_open = !self.palette_open;
        cx.notify();
    }

    fn close_palette(&mut self, cx: &mut Context<Self>) {
        if self.palette_open {
            self.palette_open = false;
            cx.notify();
        }
    }

    fn run_command(&mut self, cmd: PaletteCmd, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = false;
        match cmd {
            PaletteCmd::Run => self.run_active_tab(cx),
            PaletteCmd::Cancel => self.cancel(cx),
            PaletteCmd::Refresh => self.connect_or_refresh(cx),
            PaletteCmd::Terminal => self.toggle_terminal(window, cx),
            PaletteCmd::Connections => self.toggle_connections(cx),
            PaletteCmd::Settings => self.toggle_settings(cx),
        }
    }

    fn toggle_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.terminal.is_none() {
            match crate::terminal::spawn(cx) {
                Ok(term) => {
                    term.view.read(cx).focus_handle().focus(window);
                    self.terminal = Some(term);
                    self.terminal_open = true;
                    self.status = "Terminal opened".into();
                }
                Err(e) => self.status = format!("Terminal failed: {e}"),
            }
        } else {
            self.terminal_open = !self.terminal_open;
            if self.terminal_open {
                if let Some(t) = &self.terminal {
                    t.view.read(cx).focus_handle().focus(window);
                }
            }
        }
        cx.notify();
    }

}

#[cfg(test)]
mod tests {
    use super::util::ssl_from_str;
    use super::*;
    use gpui::TestAppContext;
    use gpui_component::{Theme, ThemeMode};
    use zdb_db::SslMode;

    #[gpui::test]
    fn theme_switch_changes_palette(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_component::init(cx);
            Theme::change(ThemeMode::Light, None, cx);
        });
        let light = cx.read(palette);
        cx.update(|cx| Theme::change(ThemeMode::Dark, None, cx));
        let dark = cx.read(palette);
        assert_ne!(light.center, dark.center);
        assert_ne!(light.fg, dark.fg);
    }

    fn new_workspace(cx: &mut TestAppContext) -> gpui::WindowHandle<Workspace> {
        cx.update(|cx| gpui_component::init(cx));
        let db = DbHandle::spawn();
        cx.add_window(|window, cx| Workspace::new(db, Settings::default(), None, window, cx))
    }

    #[test]
    fn ssl_parsing() {
        assert_eq!(ssl_from_str("disable"), SslMode::Disable);
        assert_eq!(ssl_from_str("verify-full"), SslMode::VerifyFull);
        assert_eq!(ssl_from_str("whatever"), SslMode::Prefer);
    }

    #[test]
    fn oneline_collapses_and_truncates() {
        assert_eq!(oneline("SELECT\n  1"), "SELECT 1");
        assert_eq!(oneline(&"x ".repeat(200)).chars().last(), Some('…'));
    }

    #[gpui::test]
    fn palette_opens_and_closes(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                assert!(!ws.palette_open);
                ws.toggle_palette(cx);
                assert!(ws.palette_open);
                ws.close_palette(cx);
                assert!(!ws.palette_open);
            })
            .unwrap();
    }

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

    /// Open a blank Query tab and return its id (for tests needing a result grid).
    fn seed_query_tab(
        ws: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> u64 {
        ws.open_query_tab(window, cx);
        ws.active_id().expect("active tab")
    }

    /// Make `tab_id` an editable result over `public.widget(id pk, …)`.
    fn editable_tab(ws: &mut Workspace, tab_id: u64, headers: Vec<&str>) {
        let tab = ws.tab_mut(tab_id).expect("tab");
        tab.headers = headers.iter().map(|s| s.to_string()).collect();
        tab.edit_target = Some(EditTarget {
            schema: "public".into(),
            table: "widget".into(),
            pk_columns: vec!["id".into()],
        });
        tab.edit_cols = headers.iter().map(|s| Some(s.to_string())).collect();
    }

    #[gpui::test]
    fn header_click_cycles_sort(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                {
                    let tab = ws.tab_mut(id).unwrap();
                    tab.base_sql = Some("SELECT * FROM t".into());
                    tab.headers = vec!["qty".into()];
                }
                assert_eq!(ws.tab(id).unwrap().sort_state, None);
                ws.toggle_sort(id, 0, cx);
                assert_eq!(ws.tab(id).unwrap().sort_state, Some((0, false))); // ascending
                ws.toggle_sort(id, 0, cx);
                assert_eq!(ws.tab(id).unwrap().sort_state, Some((0, true))); // descending
                ws.toggle_sort(id, 0, cx);
                assert_eq!(ws.tab(id).unwrap().sort_state, None); // cleared
            })
            .unwrap();
    }

    #[gpui::test]
    fn table_query_builds_where(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.open_table_tab("public".into(), "users".into(), window, cx);
                let id = ws.active_id().unwrap();
                let input = ws.tab(id).unwrap().where_input.clone();
                input.update(cx, |i, cx| i.set_value("id > 5", window, cx));
                assert_eq!(
                    ws.table_query(id, cx),
                    "SELECT * FROM \"public\".\"users\" WHERE id > 5 LIMIT 500"
                );
                input.update(cx, |i, cx| i.set_value("", window, cx));
                assert_eq!(
                    ws.table_query(id, cx),
                    "SELECT * FROM \"public\".\"users\" LIMIT 500"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn new_query_tab_titles_increment(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                assert!(ws.active.is_none());
                ws.open_query_tab(window, cx);
                ws.open_query_tab(window, cx);
                assert_eq!(ws.tabs.len(), 2);
                assert_eq!(ws.tabs[0].title, "Query 1");
                assert_eq!(ws.tabs[1].title, "Query 2");
                assert_eq!(ws.active, Some(1));
            })
            .unwrap();
    }

    #[gpui::test]
    fn close_last_tab_shows_welcome(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                ws.close_tab(id, cx);
                assert!(ws.tabs.is_empty());
                assert_eq!(ws.active, None);
            })
            .unwrap();
    }

    #[gpui::test]
    fn scratch_tab_is_singleton(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.focus_scratch_tab(window, cx);
                ws.focus_scratch_tab(window, cx);
                let n = ws
                    .tabs
                    .iter()
                    .filter(|t| matches!(t.kind, TabKind::Scratch))
                    .count();
                assert_eq!(n, 1);
            })
            .unwrap();
    }

    #[gpui::test]
    fn single_click_selects_row(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                ws.set_current_row(id, 3, cx);
                assert_eq!(ws.tab(id).unwrap().current_row, Some(3));
            })
            .unwrap();
    }

    #[gpui::test]
    fn switching_tables_preserves_state(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                // Open table A and select a row (both the widget's own highlight
                // and our `current_row`).
                ws.open_table_tab("public".into(), "aaa".into(), window, cx);
                let a = ws.active_id().unwrap();
                let a_table = ws.tab(a).unwrap().table.clone();
                a_table.update(cx, |ts, cx| ts.set_selected_row(4, cx));
                ws.set_current_row(a, 4, cx);
                assert_eq!(ws.tab(a).unwrap().current_row, Some(4));

                // Open table B: a brand-new tab with its own (empty) selection.
                ws.open_table_tab("public".into(), "bbb".into(), window, cx);
                let b = ws.active_id().unwrap();
                assert_ne!(a, b);
                assert_eq!(ws.tab(b).unwrap().current_row, None);
                assert_eq!(ws.tab(b).unwrap().table.read(cx).selected_row(), None);

                // Switch back to A: its selection is intact (tabs keep their state).
                let a_idx = ws.tabs.iter().position(|t| t.id == a).unwrap();
                ws.activate_tab(a_idx, window, cx);
                assert_eq!(ws.tab(a).unwrap().current_row, Some(4));
                assert_eq!(ws.tab(a).unwrap().table.read(cx).selected_row(), Some(4));

                // Re-opening A's table focuses the SAME tab (focus-if-open, no dup).
                ws.open_table_tab("public".into(), "aaa".into(), window, cx);
                assert_eq!(ws.active_id(), Some(a));
                let n = ws
                    .tabs
                    .iter()
                    .filter(|t| matches!(&t.kind, TabKind::Table { table, .. } if table == "aaa"))
                    .count();
                assert_eq!(n, 1);
            })
            .unwrap();
    }

    #[gpui::test]
    fn add_row_then_save_stages_insert(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                editable_tab(ws, id, vec!["id", "name"]);
                ws.add_row(id, cx);
                let idx = ws.tab(id).unwrap().new_row_idx.expect("new row");

                // Editing the new row updates memory, not a staged statement.
                ws.begin_edit(id, idx, 0, window, cx);
                ws.cell_input.update(cx, |i, cx| i.set_value("7", window, cx));
                ws.commit_cell_edit(cx);
                assert!(ws.tab(id).unwrap().pending.is_empty());

                ws.begin_edit(id, idx, 1, window, cx);
                ws.cell_input.update(cx, |i, cx| i.set_value("zed", window, cx));
                ws.commit_cell_edit(cx);

                ws.save_new_row(id, cx);
                assert_eq!(ws.tab(id).unwrap().pending.len(), 1);
                let target = ws.tab(id).unwrap().edit_target.clone().unwrap();
                let sql = ws.tab(id).unwrap().pending[0].to_sql(&target).unwrap();
                assert_eq!(
                    sql,
                    r#"INSERT INTO "public"."widget" ("id", "name") VALUES ('7', 'zed')"#
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn discard_unsaved_new_row(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                editable_tab(ws, id, vec!["id"]);
                ws.tab_mut(id).unwrap().rows = vec![vec![CellValue::Text("1".into())]];
                ws.add_row(id, cx);
                assert_eq!(ws.tab(id).unwrap().rows.len(), 2);
                ws.delete_current_row(id, cx); // current row is the new one → discard
                assert_eq!(ws.tab(id).unwrap().rows.len(), 1);
                assert!(ws.tab(id).unwrap().new_row_idx.is_none());
                assert!(ws.tab(id).unwrap().pending.is_empty());
            })
            .unwrap();
    }

    #[gpui::test]
    fn run_without_connection_is_guarded(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                ws.run_sql(id, "SELECT 1".into(), cx);
                assert_eq!(ws.status, "Not connected");
                assert!(!ws.tab(id).unwrap().running);
            })
            .unwrap();
    }

    #[gpui::test]
    fn new_query_resets_editability(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                {
                    let tab = ws.tab_mut(id).unwrap();
                    tab.edit_target = Some(EditTarget {
                        schema: "public".into(),
                        table: "t".into(),
                        pk_columns: vec!["id".into()],
                    });
                    tab.edit_cols = vec![Some("id".into())];
                }
                ws.run_new_query(id, "SELECT 1".into(), cx);
                assert!(ws.tab(id).unwrap().edit_target.is_none());
                assert!(ws.tab(id).unwrap().edit_cols.is_empty());
            })
            .unwrap();
    }

    #[test]
    fn order_by_wraps_query() {
        assert_eq!(
            order_by_sql("SELECT * FROM t", "qty", ColumnSort::Descending),
            r#"SELECT * FROM (SELECT * FROM t) AS _zdb ORDER BY "qty" DESC"#
        );
        // Default clears the sort.
        assert_eq!(order_by_sql("SELECT 1;", "x", ColumnSort::Default), "SELECT 1;");
    }

    #[gpui::test]
    fn inline_edit_stages_update_sql(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                {
                    let tab = ws.tab_mut(id).unwrap();
                    tab.headers = vec!["id".into(), "name".into()];
                    tab.rows = vec![vec![
                        CellValue::Text("1".into()),
                        CellValue::Text("alpha".into()),
                    ]];
                    tab.edit_target = Some(EditTarget {
                        schema: "public".into(),
                        table: "widget".into(),
                        pk_columns: vec!["id".into()],
                    });
                    tab.edit_cols = vec![Some("id".into()), Some("name".into())];
                }

                ws.begin_edit(id, 0, 1, window, cx);
                assert_eq!(ws.tab(id).unwrap().editing, Some((0, 1)));
                ws.cell_input.update(cx, |inp, cx| inp.set_value("beta", window, cx));
                ws.commit_cell_edit(cx);

                assert_eq!(ws.tab(id).unwrap().pending.len(), 1);
                let target = ws.tab(id).unwrap().edit_target.clone().unwrap();
                let sql = ws.tab(id).unwrap().pending[0].to_sql(&target).unwrap();
                assert_eq!(
                    sql,
                    r#"UPDATE "public"."widget" SET "name" = 'beta' WHERE "id" = '1'"#
                );
                // Optimistic update reflects in the grid before Apply.
                assert_eq!(ws.tab(id).unwrap().rows[0][1], CellValue::Text("beta".into()));
            })
            .unwrap();
    }

    #[gpui::test]
    fn multiple_edits_accumulate_into_one_batch(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                {
                    let tab = ws.tab_mut(id).unwrap();
                    tab.headers = vec!["id".into(), "name".into()];
                    tab.rows = vec![
                        vec![CellValue::Text("1".into()), CellValue::Text("a".into())],
                        vec![CellValue::Text("2".into()), CellValue::Text("b".into())],
                    ];
                    tab.edit_target = Some(EditTarget {
                        schema: "public".into(),
                        table: "widget".into(),
                        pk_columns: vec!["id".into()],
                    });
                    tab.edit_cols = vec![Some("id".into()), Some("name".into())];
                }

                // Edit two different rows; both stage without replacing each other.
                ws.begin_edit(id, 0, 1, window, cx);
                ws.cell_input.update(cx, |i, cx| i.set_value("x", window, cx));
                ws.commit_cell_edit(cx);
                ws.begin_edit(id, 1, 1, window, cx);
                ws.cell_input.update(cx, |i, cx| i.set_value("y", window, cx));
                ws.commit_cell_edit(cx);

                assert_eq!(ws.tab(id).unwrap().pending.len(), 2);
                let batch = ws.pending_sql(id).expect("combined sql");
                assert!(batch.starts_with("BEGIN;"));
                assert!(batch.trim_end().ends_with("COMMIT;"));
                assert!(batch.contains(r#"SET "name" = 'x' WHERE "id" = '1'"#));
                assert!(batch.contains(r#"SET "name" = 'y' WHERE "id" = '2'"#));
            })
            .unwrap();
    }

    #[gpui::test]
    fn revert_to_original_drops_edit(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                {
                    let tab = ws.tab_mut(id).unwrap();
                    tab.headers = vec!["id".into(), "name".into()];
                    tab.rows = vec![vec![
                        CellValue::Text("1".into()),
                        CellValue::Text("alpha".into()),
                    ]];
                    tab.orig_rows = tab.rows.clone();
                    tab.edit_target = Some(EditTarget {
                        schema: "public".into(),
                        table: "widget".into(),
                        pk_columns: vec!["id".into()],
                    });
                    tab.edit_cols = vec![Some("id".into()), Some("name".into())];
                }

                // Edit to a new value: stages + marks the cell.
                ws.begin_edit(id, 0, 1, window, cx);
                ws.cell_input.update(cx, |i, cx| i.set_value("beta", window, cx));
                ws.commit_cell_edit(cx);
                assert_eq!(ws.tab(id).unwrap().pending.len(), 1);
                assert!(ws.tab(id).unwrap().edited_cells.contains(&(0, 1)));

                // Edit back to the original value: the staged edit is dropped and
                // the cell is no longer marked.
                ws.begin_edit(id, 0, 1, window, cx);
                ws.cell_input.update(cx, |i, cx| i.set_value("alpha", window, cx));
                ws.commit_cell_edit(cx);
                assert!(ws.tab(id).unwrap().pending.is_empty(), "edit reverted → no pending");
                assert!(!ws.tab(id).unwrap().edited_cells.contains(&(0, 1)));
                assert_eq!(ws.tab(id).unwrap().rows[0][1], CellValue::Text("alpha".into()));
            })
            .unwrap();
    }

    #[gpui::test]
    fn query_log_records_runs(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, _cx| {
                assert!(ws.log_entries.is_empty());
                ws.push_log("SELECT 1", true);
                ws.push_log("BAD", false);
                assert_eq!(ws.log_entries.len(), 2);
                assert!(!ws.log_entries[1].ok);
            })
            .unwrap();
    }
}
