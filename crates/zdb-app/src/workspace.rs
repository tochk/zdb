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
    SharedString, StatefulInteractiveElement, Styled, WeakEntity, Window, WindowControlArea,
};
// Used only for the Linux title-bar drag (Windows drags natively via hit-test).
#[cfg(not(target_os = "windows"))]
use gpui::MouseButton;
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputEvent, InputState},
    resizable::{h_resizable, resizable_panel, v_resizable},
    table::{Column, ColumnSort, Table, TableDelegate, TableState},
    v_flex, ActiveTheme, Disableable, Icon, IconName, Sizable,
};
use std::collections::{HashMap, HashSet};
use zdb_config::{ConnectionEntry, Settings};
use zdb_db::{
    CellValue, ConnId, ConnectionConfig, DbHandle, EditTarget, QueryEvent, RelationKind, RowEdit,
    SslMode,
};

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

/// Chrome colors pulled from the active gpui-component theme.
#[derive(Clone, Copy, PartialEq)]
struct Colors {
    sidebar: Hsla,
    center: Hsla,
    messages: Hsla,
    header: Hsla,
    border: Hsla,
    fg: Hsla,
    fg_dim: Hsla,
    fg_null: Hsla,
    panel: Hsla,
    accent: Hsla,
    /// Row/list hover background.
    hover: Hsla,
    /// Selected/active row background.
    active: Hsla,
}

fn palette(cx: &App) -> Colors {
    let t = cx.theme();
    Colors {
        sidebar: t.sidebar,
        center: t.background,
        messages: t.sidebar,
        header: t.secondary,
        border: t.border,
        fg: t.foreground,
        fg_dim: t.muted_foreground,
        fg_null: t.accent,
        panel: t.popover,
        accent: t.accent,
        hover: t.list_hover,
        active: t.list_active,
    }
}

/// Translucent blue fill for a cell with a staged (unsaved) edit, plus a solid
/// blue left bar. Fixed so it reads on both light and dark themes.
const EDITED_BG: u32 = 0x3b82f666;
const EDITED_BAR: u32 = 0x2563ebff;
/// Hover fill for an already-edited cell: same blue, a bit darker/stronger (so a
/// changed cell stays visibly "changed" on hover instead of going neutral).
const EDITED_HOVER: u32 = 0x2563eb99;

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

// ---- results table delegate ---------------------------------------------

/// Renders the results grid and bridges interactions (cell edit, sort) back to
/// the `Workspace`, which owns all editing state.
struct ResultDelegate {
    columns: Vec<Column>,
    rows: Vec<Vec<CellValue>>,
    ws: WeakEntity<Workspace>,
}

impl ResultDelegate {
    fn new(ws: WeakEntity<Workspace>) -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            ws,
        }
    }

    fn set(&mut self, headers: &[String], rows: Vec<Vec<CellValue>>) {
        self.columns = headers
            .iter()
            .enumerate()
            .map(|(i, h)| {
                Column::new(SharedString::from(format!("c{i}")), SharedString::from(h.clone()))
                    .width(px(180.))
            })
            .collect();
        self.rows = rows;
    }
}

impl TableDelegate for ResultDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> &Column {
        &self.columns[col_ix]
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let name = self
            .columns
            .get(col_ix)
            .map(|c| c.name.to_string())
            .unwrap_or_default();
        // Sort arrow for the currently-sorted column.
        let arrow = self
            .ws
            .upgrade()
            .and_then(|w| match w.read(cx).sort_state {
                Some((ci, desc)) if ci == col_ix => Some(if desc { " ▼" } else { " ▲" }),
                _ => None,
            })
            .unwrap_or("");
        let weak = self.ws.clone();
        div()
            .id(SharedString::from(format!("th-{col_ix}")))
            .size_full()
            .cursor_pointer()
            .child(format!("{name}{arrow}"))
            .on_click(move |_: &ClickEvent, _window, app| {
                weak.update(app, |w, cx| w.toggle_sort(col_ix, cx)).ok();
            })
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let c = palette(cx);
        let cell = self.rows.get(row_ix).and_then(|r| r.get(col_ix)).cloned();

        let Some(ws_entity) = self.ws.upgrade() else {
            return td_text(cell, c).into_any_element();
        };
        let ws = ws_entity.read(cx);
        let editing = ws.editing == Some((row_ix, col_ix));
        let editable = ws.edit_cols.get(col_ix).is_some_and(|o| o.is_some());
        let edited = ws.edited_cells.contains(&(row_ix, col_ix));
        let cell_input = ws.cell_input.clone();

        if editing {
            // Seamless inline edit: `appearance(false)` drops the input's border /
            // background, and `.small()` sets its horizontal padding to 8px — the
            // same as `td_text`'s `px_2` — so the text sits exactly where the
            // display text was (no shift right/down). The cell just gets a subtle
            // active background to signal edit mode. No wrapper padding/border (that
            // was the original "text shifts right and down" problem).
            return div()
                .size_full()
                // Small top padding nudges the vertically-centered input text down
                // to line up with the display cells (which are top-aligned + py_1).
                .pt(px(3.))
                .bg(c.active)
                .text_sm()
                .child(Input::new(&cell_input).appearance(false).small())
                .into_any_element();
        }

        let base = td_text(cell, c);
        if editable {
            let weak = self.ws.clone();
            div()
                .id(SharedString::from(format!("cell-{row_ix}-{col_ix}")))
                .size_full()
                // Staged-edit marker: blue fill + a solid blue left bar (the bar
                // stays visible even over the row-selection tint, like a dirty
                // gutter in DataGrip / Zed). Hover keeps the blue identity (a bit
                // darker) rather than reverting to the neutral hover color.
                .when(edited, |d| {
                    d.bg(rgba(EDITED_BG))
                        .border_l_2()
                        .border_color(rgba(EDITED_BAR))
                        .hover(|s| s.bg(rgba(EDITED_HOVER)))
                })
                .when(!edited, |d| d.hover(|s| s.bg(c.hover)))
                .cursor_pointer()
                .child(base)
                .on_click(move |ev: &ClickEvent, window, app| {
                    let double = ev.click_count() >= 2;
                    weak.update(app, |w, cx| {
                        if double {
                            w.begin_edit(row_ix, col_ix, window, cx);
                        } else {
                            w.set_current_row(row_ix, cx);
                        }
                    })
                    .ok();
                })
                .into_any_element()
        } else {
            base.into_any_element()
        }
    }

}

fn td_text(cell: Option<CellValue>, c: Colors) -> gpui::Div {
    let base = div().px_2().py_1().text_sm();
    match cell {
        Some(CellValue::Text(s)) => base.text_color(c.fg).child(s),
        Some(CellValue::Null) => base.text_color(c.fg_null).child("NULL"),
        None => base.child(""),
    }
}

// ---- workspace ----------------------------------------------------------

pub struct Workspace {
    db: DbHandle,
    cfg: Option<ConnectionConfig>,
    conn: Option<ConnId>,
    status: String,
    running: bool,
    tree: Vec<SchemaNode>,
    editor: Entity<InputState>,
    table: Entity<TableState<ResultDelegate>>,

    /// When browsing a table (opened from the tree): (schema, table) + WHERE input.
    table_view: Option<(String, String)>,
    where_input: Entity<InputState>,

    /// Auto-saved scratch query editor (separate view).
    scratch: Entity<InputState>,
    scratch_open: bool,

    // Current result.
    headers: Vec<String>,
    rows: Vec<Vec<CellValue>>,
    /// Snapshot of the rows as loaded, to detect when a cell is edited back to
    /// its original value (then the staged edit is dropped).
    orig_rows: Vec<Vec<CellValue>>,
    /// Original user query (without any added ORDER BY), for re-sorting/reload.
    base_sql: Option<String>,
    /// Effective query last executed (with sort), for reload after an edit.
    last_sql: Option<String>,
    /// Current sort: (result column index, descending?).
    sort_state: Option<(usize, bool)>,

    // Editability (from DbHandle::describe of `base_sql`).
    edit_target: Option<EditTarget>,
    /// Result column -> real table column name (None = not editable).
    edit_cols: Vec<Option<String>>,

    // Inline editing.
    editing: Option<(usize, usize)>,
    current_row: Option<usize>,
    /// Index of an unsaved new row appended to the grid, if any.
    new_row_idx: Option<usize>,
    cell_input: Entity<InputState>,
    /// Staged edits, accumulated across the table; all applied in one
    /// transaction when Apply is pressed. The combined SQL is shown for review.
    pending: Vec<RowEdit>,
    /// Grid (row, col) coordinates with a staged edit, for highlighting.
    edited_cells: HashSet<(usize, usize)>,

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
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("sql")
                .line_number(true)
                .placeholder("SELECT * FROM ... ;  (press Run)")
        });
        let weak = cx.weak_entity();
        let table =
            cx.new(|cx| TableState::new(ResultDelegate::new(weak), window, cx).row_selectable(true));
        let palette_input = cx.new(|cx| InputState::new(window, cx).placeholder("Type a command…"));
        let cell_input = cx.new(|cx| InputState::new(window, cx));
        let where_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("WHERE … (Enter to filter)"));
        let scratch = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("sql")
                .line_number(true)
                .placeholder("Scratch query — auto-saved")
                .default_value(zdb_config::load_scratch())
        });

        // Commit/cancel inline edits on Enter / focus loss.
        cx.subscribe(&cell_input, |this, _input, event: &InputEvent, cx| match event {
            InputEvent::PressEnter { .. } => this.commit_cell_edit(cx),
            InputEvent::Blur => this.cancel_cell_edit(cx),
            _ => {}
        })
        .detach();
        // Re-run the table query when the WHERE filter is submitted.
        cx.subscribe(&where_input, |this, _i, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.apply_where(cx);
            }
        })
        .detach();
        // Auto-save the scratch query to disk on every change.
        cx.subscribe(&scratch, |this, _i, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                zdb_config::save_scratch(&this.scratch.read(cx).value());
            }
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
            running: false,
            tree: Vec::new(),
            editor,
            table,
            table_view: None,
            where_input,
            scratch,
            scratch_open: false,
            headers: Vec::new(),
            rows: Vec::new(),
            orig_rows: Vec::new(),
            base_sql: None,
            last_sql: None,
            sort_state: None,
            edit_target: None,
            edit_cols: Vec::new(),
            editing: None,
            current_row: None,
            new_row_idx: None,
            cell_input,
            pending: Vec::new(),
            edited_cells: HashSet::new(),
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
        self.clear_result(cx);
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

    fn open_relation(
        &mut self,
        schema: String,
        table: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.table_view = Some((schema, table));
        self.where_input.update(cx, |i, cx| i.set_value("", window, cx));
        let sql = self.table_query(cx);
        self.editor.update(cx, |ed, cx| ed.set_value(sql.clone(), window, cx));
        self.run_new_query(sql, cx);
    }

    /// `SELECT * FROM <table> [WHERE …] LIMIT n` for the open table view.
    fn table_query(&self, cx: &App) -> String {
        let Some((s, t)) = &self.table_view else {
            return String::new();
        };
        let w = self.where_input.read(cx).value().trim().to_string();
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

    fn apply_where(&mut self, cx: &mut Context<Self>) {
        if self.table_view.is_none() {
            return;
        }
        let sql = self.table_query(cx);
        self.run_new_query(sql, cx);
    }

    // ---- query execution -------------------------------------------------

    /// Run the editor's SQL as a brand-new ad-hoc query (clears the table view).
    fn run_current(&mut self, cx: &mut Context<Self>) {
        self.table_view = None;
        let sql = self.editor.read(cx).value().to_string();
        self.run_new_query(sql, cx);
    }

    fn toggle_scratch(&mut self, cx: &mut Context<Self>) {
        self.scratch_open = !self.scratch_open;
        cx.notify();
    }

    /// Re-run the current query/table from the DB (discards staged edits).
    fn reload_data(&mut self, cx: &mut Context<Self>) {
        if let Some(sql) = self.base_sql.clone() {
            self.run_new_query(sql, cx);
        }
    }

    /// Run the scratch query and return to the results view.
    fn run_scratch(&mut self, cx: &mut Context<Self>) {
        let sql = self.scratch.read(cx).value().to_string();
        self.table_view = None;
        self.scratch_open = false;
        self.run_new_query(sql, cx);
    }

    /// Start a new query: remember it as the sort base, drop stale editability,
    /// then describe (for editability) and execute it.
    fn run_new_query(&mut self, base: String, cx: &mut Context<Self>) {
        self.base_sql = Some(base.clone());
        self.sort_state = None;
        self.edit_target = None;
        self.edit_cols.clear();
        self.editing = None;
        self.current_row = None;
        // Drop the Table widget's own row highlight; otherwise the previously
        // selected index stays lit when switching to a different table.
        self.table.update(cx, |ts, cx| ts.clear_selection(cx));
        self.pending.clear();
        self.edited_cells.clear();
        self.describe_async(base.clone(), cx);
        self.run_sql(base, cx);
    }

    fn describe_async(&mut self, sql: String, cx: &mut Context<Self>) {
        let Some(conn) = self.conn else { return };
        let db = self.db.clone();
        cx.spawn(async move |this, cx| {
            let res = db.describe(conn, sql).await;
            this.update(cx, |this, cx| {
                match res {
                    Ok(Some(d)) => {
                        this.edit_target = Some(d.target);
                        this.edit_cols = d.columns;
                    }
                    _ => {
                        this.edit_target = None;
                        this.edit_cols.clear();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Execute `sql` for display (does not change editability or the sort base).
    fn run_sql(&mut self, sql: String, cx: &mut Context<Self>) {
        let Some(conn) = self.conn else {
            self.status = "Not connected".into();
            cx.notify();
            return;
        };
        if sql.trim().is_empty() {
            self.status = "Nothing to run".into();
            cx.notify();
            return;
        }
        self.running = true;
        self.editing = None;
        self.new_row_idx = None;
        self.last_sql = Some(sql.clone());
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
                this.running = false;
                match error {
                    Some(e) => {
                        this.status = format!("Error: {e}");
                        this.push_log(&logged, false);
                    }
                    None => {
                        let is_select = !headers.is_empty();
                        let n = rows.len();
                        this.headers = headers.clone();
                        this.rows = rows.clone();
                        this.orig_rows = rows.clone();
                        // Fresh rows just landed: drop any prior selection so a
                        // previously-selected index doesn't stay lit on the new
                        // data (e.g. switching tables, or re-sorting). This is the
                        // single point every result passes through.
                        this.current_row = None;
                        this.table.update(cx, |ts, cx| {
                            ts.clear_selection(cx);
                            ts.delegate_mut().set(&headers, rows);
                            ts.refresh(cx);
                        });
                        this.status = if is_select {
                            format!("{n} row(s) in {elapsed:?}")
                        } else {
                            format!("{affected} row(s) affected in {elapsed:?}")
                        };
                        this.push_log(&logged, true);
                    }
                }
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

    fn clear_result(&mut self, cx: &mut Context<Self>) {
        self.headers.clear();
        self.rows.clear();
        self.orig_rows.clear();
        self.base_sql = None;
        self.last_sql = None;
        self.sort_state = None;
        self.edit_target = None;
        self.edit_cols.clear();
        self.editing = None;
        self.current_row = None;
        self.new_row_idx = None;
        self.pending.clear();
        self.edited_cells.clear();
        self.table.update(cx, |ts, cx| {
            ts.clear_selection(cx);
            ts.delegate_mut().set(&[], Vec::new());
            ts.refresh(cx);
        });
    }

    // ---- sorting ---------------------------------------------------------

    /// Clicking a header cycles its sort: none → ascending → descending → none,
    /// re-running the query ordered by that column.
    fn toggle_sort(&mut self, col_ix: usize, cx: &mut Context<Self>) {
        let next = match self.sort_state {
            Some((c, false)) if c == col_ix => Some((col_ix, true)),
            Some((c, true)) if c == col_ix => None,
            _ => Some((col_ix, false)),
        };
        self.sort_state = next;
        let Some(base) = self.base_sql.clone() else {
            log("sort: no base query");
            return;
        };
        let sql = match next {
            Some((c, desc)) => {
                let col = self.headers.get(c).cloned().unwrap_or_default();
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
        self.run_sql(sql, cx);
    }

    fn set_current_row(&mut self, row: usize, cx: &mut Context<Self>) {
        self.current_row = Some(row);
        cx.notify();
    }

    // ---- inline editing --------------------------------------------------

    fn begin_edit(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit_cols.get(col).map_or(true, |o| o.is_none()) {
            return;
        }
        let text = match self.rows.get(row).and_then(|r| r.get(col)) {
            Some(CellValue::Text(s)) => s.clone(),
            _ => String::new(),
        };
        self.editing = Some((row, col));
        self.current_row = Some(row);
        self.cell_input.update(cx, |inp, cx| inp.set_value(text, window, cx));
        let handle = self.cell_input.read(cx).focus_handle(cx);
        handle.focus(window);
        cx.notify();
    }

    fn cancel_cell_edit(&mut self, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            cx.notify();
        }
    }

    /// Stage the cell edit (build the UPDATE and show it for review).
    fn commit_cell_edit(&mut self, cx: &mut Context<Self>) {
        let Some((row, col)) = self.editing.take() else { return };
        let text = self.cell_input.read(cx).value().to_string();
        let value = if text.is_empty() {
            CellValue::Null
        } else {
            CellValue::Text(text)
        };

        // The unsaved new row accumulates in memory until "Save row".
        if Some(row) == self.new_row_idx {
            if let Some(c) = self.rows.get_mut(row).and_then(|r| r.get_mut(col)) {
                *c = value;
            }
            self.refresh_table(cx);
            cx.notify();
            return;
        }

        let Some(target) = self.edit_target.clone() else { return };
        let Some(real_col) = self.edit_cols.get(col).and_then(|o| o.clone()) else { return };
        // PK must be read from the original row value before the optimistic update.
        let Some(pk) = self.row_pk(row, &target) else { return };

        // Drop any earlier staged change for this same cell, so re-editing a cell
        // replaces (not stacks) its UPDATE and reverting clears it cleanly.
        self.remove_pending_update(&pk, &real_col);

        let original = self.orig_rows.get(row).and_then(|r| r.get(col));
        if original == Some(&value) {
            // Edited back to the original value: nothing to save. Drop the marker
            // and the staged edit (already removed above).
            self.edited_cells.remove(&(row, col));
            self.status = "Reverted to original".into();
        } else {
            self.stage(
                RowEdit::Update {
                    pk,
                    set: vec![(real_col, value.clone())],
                },
                &target,
                cx,
            );
            self.edited_cells.insert((row, col));
        }
        // Reflect the value in the grid immediately. Cancel/Apply both reload the
        // authoritative rows.
        if let Some(c) = self.rows.get_mut(row).and_then(|r| r.get_mut(col)) {
            *c = value;
        }
        self.refresh_table(cx);
        cx.notify();
    }

    /// Remove any staged `Update` for the given PK that sets `col` (used to
    /// dedup re-edits and to drop an edit reverted to its original value).
    fn remove_pending_update(&mut self, pk: &[(String, CellValue)], col: &str) {
        self.pending.retain_mut(|e| match e {
            RowEdit::Update { pk: p, set } if p.as_slice() == pk => {
                set.retain(|(c, _)| c != col);
                !set.is_empty()
            }
            _ => true,
        });
    }

    /// Append a blank editable row; cells are filled by double-clicking.
    fn add_row(&mut self, cx: &mut Context<Self>) {
        if self.edit_target.is_none() {
            return;
        }
        let blank = vec![CellValue::Null; self.headers.len()];
        self.rows.push(blank);
        let idx = self.rows.len() - 1;
        self.new_row_idx = Some(idx);
        self.current_row = Some(idx);
        self.status = "New row — double-click cells to fill, then Save row.".into();
        self.refresh_table(cx);
        cx.notify();
    }

    /// Stage an INSERT from the new row's filled (non-null) columns.
    fn save_new_row(&mut self, cx: &mut Context<Self>) {
        let (Some(idx), Some(target)) = (self.new_row_idx, self.edit_target.clone()) else {
            return;
        };
        let Some(row) = self.rows.get(idx).cloned() else { return };
        let values: Vec<(String, CellValue)> = self
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
        if values.is_empty() {
            self.status = "Fill at least one column".into();
            cx.notify();
            return;
        }
        self.stage(RowEdit::Insert { values }, &target, cx);
    }

    /// Stage a DELETE for the selected row, or discard the unsaved new row.
    fn delete_current_row(&mut self, cx: &mut Context<Self>) {
        let Some(row) = self.current_row else { return };
        if Some(row) == self.new_row_idx {
            if row < self.rows.len() {
                self.rows.remove(row);
            }
            self.new_row_idx = None;
            self.current_row = None;
            self.status = "New row discarded".into();
            self.refresh_table(cx);
            cx.notify();
            return;
        }
        let Some(target) = self.edit_target.clone() else { return };
        let Some(pk) = self.row_pk(row, &target) else { return };
        self.stage(RowEdit::Delete { pk }, &target, cx);
    }

    fn refresh_table(&mut self, cx: &mut Context<Self>) {
        let headers = self.headers.clone();
        let rows = self.rows.clone();
        self.table.update(cx, |ts, cx| {
            ts.delegate_mut().set(&headers, rows);
            ts.refresh(cx);
        });
    }

    /// Add an edit to the staged batch. Edits accumulate across the table and
    /// are all applied in one transaction on Apply.
    fn stage(&mut self, edit: RowEdit, target: &EditTarget, cx: &mut Context<Self>) {
        // Validate the statement builds before queueing it.
        if let Err(e) = edit.to_sql(target) {
            self.status = format!("Cannot build statement: {e}");
            cx.notify();
            return;
        }
        self.pending.push(edit);
        let n = self.pending.len();
        self.status = format!(
            "{n} change{} staged — review, then Apply.",
            if n == 1 { "" } else { "s" }
        );
        cx.notify();
    }

    /// Combined SQL for all staged edits (shown in the review pane).
    fn pending_sql(&self) -> Option<String> {
        let target = self.edit_target.as_ref()?;
        zdb_db::build_batch(target, &self.pending).ok().flatten()
    }

    fn cancel_pending(&mut self, cx: &mut Context<Self>) {
        self.pending.clear();
        self.edited_cells.clear();
        self.status = "Edits discarded".into();
        // Discard optimistic in-grid changes by reloading the original rows.
        if let Some(s) = self.last_sql.clone() {
            self.run_sql(s, cx);
        }
        cx.notify();
    }

    /// Execute all staged edits in a single transaction, then reload the query.
    fn apply_pending(&mut self, cx: &mut Context<Self>) {
        if self.pending.is_empty() {
            return;
        }
        let (Some(target), Some(conn)) = (self.edit_target.clone(), self.conn) else {
            return;
        };
        let edits = std::mem::take(&mut self.pending);
        let sql = zdb_db::build_batch(&target, &edits)
            .ok()
            .flatten()
            .unwrap_or_default();
        self.status = "Applying…".into();
        let db = self.db.clone();
        let reload = self.last_sql.clone();
        cx.spawn(async move |this, cx| {
            let res = db.apply_edits(conn, &target, &edits).await;
            this.update(cx, |this, cx| {
                match res {
                    Ok(()) => {
                        this.push_log(&sql, true);
                        this.status = format!("Applied {} change(s)", edits.len());
                        this.edited_cells.clear();
                        if let Some(s) = reload {
                            this.run_sql(s, cx);
                        }
                    }
                    Err(e) => {
                        // Keep the edits staged so the user can fix and retry.
                        this.pending = edits;
                        this.push_log(&sql, false);
                        this.status = format!("Apply failed: {e}");
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Primary-key (name, original value) pairs for a result row.
    fn row_pk(&self, row: usize, target: &EditTarget) -> Option<Vec<(String, CellValue)>> {
        let r = self.rows.get(row)?;
        target
            .pk_columns
            .iter()
            .map(|pk| {
                let idx = self.edit_cols.iter().position(|c| c.as_deref() == Some(pk.as_str()))?;
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
                this.table_view = Some(("public".into(), "zdb_selftest".into()));
                let sql = this.table_query(cx);
                this.run_new_query(sql, cx);
                log("selftest: table view opened");
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
            PaletteCmd::Run => self.run_current(cx),
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

    // ---- rendering -------------------------------------------------------

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let mut list = v_flex().p_1().gap(px(1.));
        for (ix, node) in self.tree.iter().enumerate() {
            let chevron = if node.expanded {
                "icons/chevron-down.svg"
            } else {
                "icons/chevron-right.svg"
            };
            list = list.child(
                div()
                    .id(SharedString::from(format!("schema-{ix}")))
                    .flex()
                    .items_center()
                    .px_1()
                    .py(px(3.))
                    .gap_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(c.fg)
                    .text_sm()
                    .hover(|s| s.bg(c.hover))
                    .child(tree_icon(chevron, c.fg_dim))
                    .child(tree_icon("icons/database.svg", c.accent))
                    .child(node.name.clone())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.toggle_schema(ix, cx)
                    })),
            );
            if node.expanded {
                let children = match &node.relations {
                    None => v_flex()
                        .child(tree_leaf_text("loading…", c))
                        .into_any_element(),
                    Some(rels) if rels.is_empty() => v_flex()
                        .child(tree_leaf_text("(no tables)", c))
                        .into_any_element(),
                    Some(rels) => {
                        let schema = node.name.clone();
                        let mut kids = v_flex().gap(px(1.));
                        for (rix, rel) in rels.iter().enumerate() {
                            let s = schema.clone();
                            let t = rel.name.clone();
                            kids = kids.child(
                                div()
                                    .id(SharedString::from(format!("rel-{ix}-{rix}")))
                                    .flex()
                                    .items_center()
                                    .pl_2()
                                    .pr_1()
                                    .py(px(3.))
                                    .gap_1p5()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(c.fg)
                                    .hover(|s| s.bg(c.hover))
                                    .child(tree_icon(rel_icon(rel.kind), c.fg_dim))
                                    .child(rel.name.clone())
                                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                        this.open_relation(s.clone(), t.clone(), window, cx)
                                    })),
                            );
                        }
                        kids.into_any_element()
                    }
                };
                // Indent + vertical guide line, like Zed's file explorer.
                list = list.child(
                    div()
                        .pl(px(10.))
                        .child(
                            div()
                                .pl(px(8.))
                                .border_l_1()
                                .border_color(c.border)
                                .child(children),
                        ),
                );
            }
        }

        let connected = self.conn.is_some();
        let title = self
            .cfg
            .as_ref()
            .filter(|_| connected)
            .map(|cfg| cfg.name.clone())
            .unwrap_or_else(|| "DATABASE".into());
        let mut actions = h_flex()
            .gap_1()
            .items_center()
            .child(
                Button::new("connections")
                    .icon(IconName::Globe)
                    .tooltip("Connections")
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_connections(cx)),
                    ),
            )
            .child(
                Button::new("settings")
                    .icon(Icon::empty().path("icons/settings.svg"))
                    .tooltip("Settings")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_settings(cx))),
            );
        if connected {
            actions = actions.child(
                Button::new("refresh")
                    .icon(Icon::empty().path("icons/refresh-cw.svg"))
                    .tooltip("Refresh schemas")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.connect_or_refresh(cx))),
            );
        }
        let header = h_flex()
            .px_3()
            .py_1()
            .bg(c.header)
            .border_b_1()
            .border_color(c.border)
            .justify_between()
            .items_center()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.fg_dim)
                    .truncate()
                    .child(title),
            )
            .child(actions);

        v_flex()
            .size_full()
            .bg(c.sidebar)
            .border_r_1()
            .border_color(c.border)
            .child(header)
            .child(
                div()
                    .id("schema-scroll")
                    .flex_grow()
                    .overflow_y_scroll()
                    .child(list),
            )
    }

    fn render_center(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let editable = self.edit_target.is_some();

        // Run toggles to Stop while a query is in flight.
        let run_btn = if self.running {
            Button::new("run")
                .icon(Icon::empty().path("icons/circle-x.svg").text_color(rgba(0xef4444ff)))
                .tooltip("Stop")
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel(cx)))
        } else {
            Button::new("run")
                .icon(Icon::empty().path("icons/play.svg").text_color(rgba(0x22c55eff)))
                .tooltip("Run (Ctrl/Cmd+Enter)")
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.run_current(cx)))
        };

        // +/- live on the top toolbar, always visible; disabled when the result
        // isn't editable / no row is selected. Save-row appears only mid-insert.
        let del_tip = if self.current_row.is_some() && self.current_row == self.new_row_idx {
            "Discard row"
        } else {
            "Delete row"
        };
        // Red only when actionable; neutral (theme fg) when disabled.
        let del_enabled = editable && self.current_row.is_some();
        let del_color: Hsla = if del_enabled {
            rgba(0xef4444ff).into()
        } else {
            c.fg
        };
        let mut toolbar = h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .bg(c.header)
            .border_b_1()
            .border_color(c.border)
            .child(run_btn)
            .child(
                Button::new("add-row")
                    .icon(IconName::Plus)
                    .tooltip("Add row")
                    .disabled(!editable)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.add_row(cx))),
            )
            .child(
                Button::new("del-row")
                    .icon(Icon::empty().path("icons/minus.svg").text_color(del_color))
                    .tooltip(del_tip)
                    .disabled(!del_enabled)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.delete_current_row(cx)
                    })),
            )
            .child(
                Button::new("reload")
                    .icon(Icon::empty().path("icons/refresh-cw.svg").text_color(c.fg))
                    .tooltip("Refresh data")
                    .disabled(self.base_sql.is_none())
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.reload_data(cx))),
            );
        if self.new_row_idx.is_some() {
            toolbar = toolbar.child(
                Button::new("save-row")
                    .icon(IconName::Check)
                    .tooltip("Save row")
                    .primary()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_new_row(cx))),
            );
        }
        let toolbar = toolbar.child(
            Button::new("scratch")
                .icon(IconName::File)
                .tooltip("Scratch editor")
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_scratch(cx))),
        );

        let mut results = v_flex().size_full();
        if let Some((s, t)) = &self.table_view {
            results = results.child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .bg(c.header)
                    .border_b_1()
                    .border_color(c.border)
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(c.fg_dim)
                            .child(format!("{s}.{t}  WHERE")),
                    )
                    .child(div().flex_grow().child(Input::new(&self.where_input))),
            );
        }
        results = results.child(
            div()
                .size_full()
                .child(Table::new(&self.table).stripe(true).bordered(true)),
        );

        // When browsing a table (opened from the schema tree) the generated
        // `SELECT * FROM …` is redundant, so the SQL editor is hidden and the rows
        // fill the pane. Ad-hoc query mode keeps the editor above the results.
        let body = if self.table_view.is_some() {
            results.into_any_element()
        } else {
            v_resizable("zdb-center")
                .child(
                    resizable_panel()
                        .size(px(170.))
                        .size_range(px(60.)..px(600.))
                        .child(div().size_full().child(Input::new(&self.editor).h_full())),
                )
                .child(resizable_panel().child(results))
                .into_any_element()
        };

        v_flex().size_full().bg(c.center).child(toolbar).child(body)
    }

    /// The separate, auto-saved scratch query view (replaces the center).
    fn render_scratch(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let toolbar = h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .bg(c.header)
            .border_b_1()
            .border_color(c.border)
            .child(
                Button::new("scratch-run")
                    .icon(Icon::empty().path("icons/play.svg").text_color(rgba(0x22c55eff)))
                    .tooltip("Run scratch (Ctrl/Cmd+Enter)")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.run_scratch(cx))),
            )
            .child(
                Button::new("scratch-close")
                    .icon(IconName::Close)
                    .tooltip("Close scratch")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_scratch(cx))),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(c.fg_dim)
                    .child("scratch · auto-saved"),
            );
        v_flex()
            .size_full()
            .bg(c.center)
            .child(toolbar)
            .child(
                // The Input element defaults to content height (one line); `.h_full()`
                // (multi-line only) makes the code editor fill its container.
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .child(Input::new(&self.scratch).h_full()),
            )
    }

    fn render_bottom(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);

        let mut log_list = v_flex().p_1().gap(px(1.));
        for e in self.log_entries.iter().rev() {
            let glyph = if e.ok { "✓" } else { "✗" };
            log_list = log_list.child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(if e.ok { c.fg } else { c.fg_null })
                    .truncate()
                    .child(format!("{glyph}  {}", oneline(&e.sql))),
            );
        }
        let log_panel = v_flex()
            .w(px(520.))
            .flex_none()
            .h_full()
            .overflow_hidden()
            .border_r_1()
            .border_color(c.border)
            .child(section_header("QUERY LOG", c))
            .child(
                div()
                    .id("log-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .child(log_list),
            );

        // When edits are staged, the bottom-right pane shows the combined SQL for
        // review (readable fg color — the old accent color was nearly invisible);
        // otherwise it shows status messages.
        let right = if !self.pending.is_empty() {
            let n = self.pending.len();
            let sql = self.pending_sql().unwrap_or_default();
            v_flex()
                .flex_grow()
                .h_full()
                .overflow_hidden()
                .child(section_header(
                    format!("PENDING — {n} CHANGE(S), REVIEW THEN APPLY"),
                    c,
                ))
                .child(
                    div()
                        .id("pending-scroll")
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(c.fg)
                        .child(sql),
                )
                .child(
                    h_flex()
                        .px_2()
                        .pb_2()
                        .gap_2()
                        .child(
                            Button::new("apply")
                                .label("Apply")
                                .primary()
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.apply_pending(cx)
                                })),
                        )
                        .child(Button::new("cancel-edit").label("Cancel").on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| this.cancel_pending(cx)),
                        )),
                )
                .into_any_element()
        } else {
            v_flex()
                .flex_grow()
                .h_full()
                .overflow_hidden()
                .child(section_header("MESSAGES", c))
                .child(
                    div()
                        .id("messages-scroll")
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(c.fg)
                        .child(self.status.clone()),
                )
                .into_any_element()
        };

        h_flex()
            .size_full()
            .overflow_hidden()
            .bg(c.messages)
            .border_t_1()
            .border_color(c.border)
            .child(log_panel)
            .child(right)
    }

    fn render_conn_manager(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let show_form = self.conn_adding || self.settings.connections.is_empty();

        let body = if show_form {
            let field = |label: &'static str, input: &Entity<InputState>| {
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(96.))
                            .flex_none()
                            .text_xs()
                            .text_color(c.fg_dim)
                            .child(label),
                    )
                    .child(div().w(px(320.)).child(Input::new(input)))
            };
            let mut buttons = h_flex().gap_2().pt_1().child(
                Button::new("add-conn")
                    .label("Save & Connect")
                    .primary()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.add_connection(cx))),
            );
            // Allow returning to the list if there are saved connections.
            buttons = if self.settings.connections.is_empty() {
                buttons.child(Button::new("close-form").label("Close").on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_connections(cx)),
                ))
            } else {
                buttons.child(Button::new("back-list").label("Back").on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| this.show_conn_list(cx),
                )))
            };
            v_flex()
                .child(section_header("ADD CONNECTION", c))
                .child(
                    v_flex()
                        .p_2()
                        .gap_1()
                        .child(field("Name", &self.f_name))
                        .child(field("Host", &self.f_host))
                        .child(field("Port", &self.f_port))
                        .child(field("User", &self.f_user))
                        .child(field("Database", &self.f_db))
                        .child(field("Password", &self.f_password))
                        .child(field("SSL mode", &self.f_ssl))
                        .child(buttons),
                )
                .into_any_element()
        } else {
            let mut list = v_flex().p_1().gap(px(2.));
            for (i, e) in self.settings.connections.iter().enumerate() {
                let active =
                    self.conn.is_some() && self.cfg.as_ref().is_some_and(|cur| cur.name == e.name);
                let subtitle = format!("{}@{}:{}/{}", e.user, e.host, e.port, e.dbname);
                list = list.child(
                    div()
                        .id(SharedString::from(format!("conn-{i}")))
                        .px_2()
                        .py(px(6.))
                        .gap_2()
                        .flex()
                        .items_center()
                        .rounded_md()
                        .cursor_pointer()
                        .when(active, |d| d.bg(c.active))
                        .hover(|s| s.bg(c.hover))
                        .child(
                            div()
                                .size(px(7.))
                                .rounded_full()
                                .when(active, |d| d.bg(rgba(0x22c55eff)))
                                .when(!active, |d| d.border_1().border_color(c.fg_dim)),
                        )
                        .child(tree_icon("icons/database.svg", c.fg_dim))
                        .child(
                            v_flex()
                                .gap(px(1.))
                                .child(div().text_sm().text_color(c.fg).child(e.name.clone()))
                                .child(div().text_xs().text_color(c.fg_dim).child(subtitle)),
                        )
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.connect_saved(i, cx)
                        })),
                );
            }
            v_flex()
                .child(section_header("CONNECTIONS", c))
                .child(
                    div()
                        .id("conn-list")
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .child(list),
                )
                .child(
                    h_flex()
                        .p_2()
                        .gap_2()
                        .border_t_1()
                        .border_color(c.border)
                        .child(
                            Button::new("add-new")
                                .label("Add")
                                .primary()
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.show_add_form(cx)
                                })),
                        )
                        .child(Button::new("close-conn").label("Close").on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| this.toggle_connections(cx),
                        ))),
                )
                .into_any_element()
        };

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .justify_center()
            .items_start()
            .bg(rgba(0x000000aa))
            .child(
                v_flex()
                    .mt(px(60.))
                    .w(px(520.))
                    .bg(c.panel)
                    .border_1()
                    .border_color(c.border)
                    .rounded_md()
                    .child(body),
            )
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let theme = self.settings.theme;

        let theme_btn = |label: &'static str, val: zdb_config::Theme| {
            let selected = theme == val;
            let mut b = Button::new(SharedString::from(format!("theme-{label}"))).label(label);
            if selected {
                b = b.primary();
            }
            b.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.set_theme(val, window, cx)
            }))
        };

        let setting_row = |label: &'static str, control: gpui::AnyElement| {
            h_flex()
                .px_3()
                .py_2()
                .items_center()
                .justify_between()
                .border_b_1()
                .border_color(c.border)
                .child(div().text_sm().text_color(c.fg).child(label))
                .child(control)
        };

        // Keybinding reference (display-only).
        let keys: &[(&str, &str)] = &[
            ("Run query", "Ctrl+Enter"),
            ("Command palette", "Ctrl+Shift+P"),
            ("Connections", "Ctrl+Shift+O"),
            ("Scratch editor", "Ctrl+Shift+E"),
            ("Terminal", "Ctrl+`"),
            ("Cancel query", "Esc"),
        ];
        let mut keys_list = v_flex().px_3().py_1().gap(px(2.));
        for (action, key) in keys.iter().copied() {
            keys_list = keys_list.child(
                h_flex()
                    .py(px(2.))
                    .justify_between()
                    .child(div().text_sm().text_color(c.fg).child(action))
                    .child(
                        div()
                            .px_1p5()
                            .rounded_md()
                            .bg(c.header)
                            .text_xs()
                            .text_color(c.fg_dim)
                            .child(key),
                    ),
            );
        }

        let path = zdb_config::Settings::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unavailable)".into());

        let body = v_flex()
            .child(section_header("SETTINGS", c))
            .child(setting_row(
                "Theme",
                h_flex()
                    .gap_1()
                    .child(theme_btn("Light", zdb_config::Theme::Light))
                    .child(theme_btn("Dark", zdb_config::Theme::Dark))
                    .into_any_element(),
            ))
            .child(section_header("KEYBINDINGS", c))
            .child(keys_list)
            .child(section_header("CONFIG FILE", c))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(c.fg_dim)
                    .child(path),
            )
            .child(
                h_flex().p_2().gap_2().justify_end().child(
                    Button::new("close-settings")
                        .label("Close")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.toggle_settings(cx)
                        })),
                ),
            );

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .justify_center()
            .items_start()
            .bg(rgba(0x000000aa))
            .child(
                v_flex()
                    .mt(px(60.))
                    .w(px(520.))
                    .bg(c.panel)
                    .border_1()
                    .border_color(c.border)
                    .rounded_md()
                    .child(body),
            )
    }

    fn render_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let query = self.palette_input.read(cx).value().to_lowercase();
        let mut list = v_flex().p_1().gap(px(1.));
        for (label, cmd) in PALETTE_COMMANDS.iter().copied() {
            if !query.is_empty() && !label.to_lowercase().contains(&query) {
                continue;
            }
            list = list.child(
                div()
                    .id(SharedString::from(format!("cmd-{label}")))
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .text_sm()
                    .text_color(c.fg)
                    .child(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.run_command(cmd, window, cx)
                    })),
            );
        }

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .justify_center()
            .items_start()
            .bg(rgba(0x000000aa))
            .child(
                v_flex()
                    .mt(px(80.))
                    .w(px(540.))
                    .bg(c.panel)
                    .border_1()
                    .border_color(c.border)
                    .rounded_md()
                    .child(
                        div()
                            .p_2()
                            .border_b_1()
                            .border_color(c.border)
                            .child(Input::new(&self.palette_input)),
                    )
                    .child(list),
            )
    }

    /// Zed-style custom title bar: replaces the OS title bar (the window is
    /// opened with `appears_transparent`). Shows the app name + a connection
    /// status dot + the active connection on the left, a native drag region in
    /// the middle, and our own minimize / maximize-restore / close controls on
    /// the right. We draw the controls ourselves (rather than using
    /// `gpui_component::TitleBar`) because that component's control glyphs take
    /// their color from the ambient `window.text_style()`, which an ancestor
    /// `.text_color` does not reach — so they render invisible. Ours set an
    /// explicit color. On Windows the OS handles drag/click via the
    /// `window_control_area` hit-test hints; on Linux we wire the actions.
    fn render_titlebar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let connected = self.conn.is_some();
        let conn_name = self
            .cfg
            .as_ref()
            .filter(|_| connected)
            .map(|cfg| cfg.name.clone())
            .unwrap_or_else(|| "Not connected".into());
        let dot = if connected {
            rgba(0x22c55eff)
        } else {
            rgba(0x9ca3afff)
        };

        let (max_icon, max_area) = if window.is_maximized() {
            ("icons/window-restore.svg", WindowControlArea::Max)
        } else {
            ("icons/window-maximize.svg", WindowControlArea::Max)
        };

        // Left: app name + connection status dot + active connection.
        let left = h_flex()
            .flex_shrink_0()
            .h_full()
            .items_center()
            .gap_2()
            .pl_3()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.fg)
                    .child("zdb"),
            )
            .child(div().w(px(1.)).h(px(14.)).bg(c.border))
            .child(div().size(px(7.)).rounded_full().bg(dot))
            .child(div().text_xs().text_color(c.fg_dim).child(conn_name));

        let controls = h_flex()
            .flex_shrink_0()
            .h_full()
            .items_center()
            .child(self.window_control(
                "win-min",
                "icons/window-minimize.svg",
                c.fg,
                c.header,
                WindowControlArea::Min,
                cx,
            ))
            .child(self.window_control(
                "win-max",
                max_icon,
                c.fg,
                c.header,
                max_area,
                cx,
            ))
            .child(self.window_control(
                "win-close",
                "icons/window-close.svg",
                c.fg,
                rgba(0xef4444ff),
                WindowControlArea::Close,
                cx,
            ));

        // The DRAG region must wrap ONLY the left content, not the control
        // buttons. gpui's window-control hit test (events.rs → on_hit_test_window
        // _control) returns the FIRST area in paint order whose hitbox is under
        // the cursor. A parent paints before its children, so tagging the whole
        // bar `Drag` makes every button resolve to `Drag` (the bar wins) and the
        // min/max/close clicks do nothing. Keeping the controls OUTSIDE the drag
        // area — as gpui-component's own TitleBar does — lets each button claim
        // its own Min/Max/Close region. The flex_1 drag region also pushes the
        // controls to the right edge (no `justify_between` needed); `min_w(0)` +
        // `overflow_hidden` stop a long connection name from inflating the bar.
        let drag = h_flex()
            .flex_1()
            .h_full()
            .items_center()
            .min_w(px(0.))
            .overflow_hidden()
            .window_control_area(WindowControlArea::Drag)
            .child(left);

        // On Linux there's no native non-client drag; start a window move when the
        // drag region is pressed.
        #[cfg(not(target_os = "windows"))]
        let drag = drag.id("titlebar").on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _, window, _| window.start_window_move()),
        );

        // The bar width is set EXPLICITLY to the window's logical width. `w_full`
        // does NOT work: the results table's min-content width inflates the layout
        // width past the window, pushing the controls off-screen right. A definite
        // width pins the right edge.
        let bar = h_flex()
            .w(window.viewport_size().width)
            .h(px(34.))
            .flex_shrink_0()
            .items_center()
            .bg(c.header)
            .border_b_1()
            .border_color(c.border)
            .child(drag)
            .child(controls);

        bar
    }

    /// One window-control button (minimize / maximize / close). `hover_bg` is the
    /// background shown on hover (red for close). The icon color is explicit so it
    /// renders regardless of the ambient text style.
    fn window_control(
        &self,
        id: &'static str,
        icon_path: &'static str,
        color: impl Into<Hsla>,
        hover_bg: impl Into<Hsla>,
        area: WindowControlArea,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // `area` is used only on Windows (native hit-test); `cx` only elsewhere
        // (click handlers). Discard the unused one per platform without moving cx.
        #[cfg(target_os = "windows")]
        let _ = cx;
        #[cfg(not(target_os = "windows"))]
        let _ = area;
        let color: Hsla = color.into();
        let hover_bg: Hsla = hover_bg.into();
        let btn = div()
            .id(id)
            .w(px(46.))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|s| s.bg(hover_bg))
            .child(Icon::empty().path(icon_path).text_color(color));

        #[cfg(target_os = "windows")]
        let btn = btn.window_control_area(area);
        #[cfg(not(target_os = "windows"))]
        let btn = btn.on_click(cx.listener(move |_, _: &ClickEvent, window, _| match id {
            "win-min" => window.minimize_window(),
            "win-max" => window.zoom_window(),
            _ => window.remove_window(),
        }));

        btn
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let center = if self.scratch_open {
            self.render_scratch(cx).into_any_element()
        } else {
            self.render_center(cx).into_any_element()
        };
        let mut top = h_resizable("zdb-top")
            .child(
                resizable_panel()
                    .size(px(280.))
                    .size_range(px(180.)..px(560.))
                    .child(self.render_sidebar(cx)),
            )
            .child(resizable_panel().child(center));
        if self.terminal_open {
            if let Some(t) = &self.terminal {
                top = top.child(
                    resizable_panel()
                        .size(px(480.))
                        .size_range(px(200.)..px(900.))
                        .child(div().size_full().child(t.view.clone())),
                );
            }
        }

        // Both panels need an explicit size: gpui-component's resizable seeds any
        // panel without one to PANEL_MIN_SIZE, which squeezed the bottom panel
        // (query-review / log) off-screen. Sizes are ratio-scaled to the window.
        let main = v_resizable("zdb-root")
            .child(
                resizable_panel()
                    .size(px(560.))
                    .size_range(px(200.)..px(1200.))
                    .child(top),
            )
            .child(
                resizable_panel()
                    .size(px(200.))
                    .size_range(px(80.)..px(460.))
                    .child(self.render_bottom(cx)),
            );

        // `min_w(0)` + `overflow_hidden` stop a wide descendant (the results table's
        // min-content width) from stretching the column past the window width.
        // Without this the title bar (`w_full`) followed the overflow and its
        // right-aligned controls landed off-screen.
        let content = v_flex()
            .size_full()
            .min_w(px(0.))
            .overflow_hidden()
            .bg(c.center)
            .child(self.render_titlebar(window, cx))
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(main),
            );

        let mut root = div()
            .relative()
            .size_full()
            // Window-wide default text color (the ambient `window.text_style()`
            // is otherwise transparent).
            .text_color(c.fg)
            .key_context("zdb")
            .on_action(cx.listener(|this, _: &RunQuery, _, cx| {
                if this.scratch_open {
                    this.run_scratch(cx)
                } else {
                    this.run_current(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &CancelQuery, _, cx| this.cancel(cx)))
            .on_action(cx.listener(|this, _: &ToggleScratch, _, cx| this.toggle_scratch(cx)))
            .on_action(cx.listener(|this, _: &TogglePalette, _, cx| this.toggle_palette(cx)))
            .on_action(cx.listener(|this, _: &ClosePalette, _, cx| {
                this.close_palette(cx);
                if this.conn_manager_open {
                    this.conn_manager_open = false;
                    cx.notify();
                }
                if this.settings_open {
                    this.settings_open = false;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                this.toggle_terminal(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleConnections, _, cx| {
                this.toggle_connections(cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleSettings, _, cx| this.toggle_settings(cx)))
            .child(content);

        if self.conn_manager_open {
            root = root.child(self.render_conn_manager(cx));
        }
        if self.settings_open {
            root = root.child(self.render_settings(cx));
        }
        if self.palette_open {
            root = root.child(self.render_palette(cx));
        }
        root
    }
}

// ---- helpers -------------------------------------------------------------

fn section_header(label: impl Into<SharedString>, c: Colors) -> impl IntoElement {
    h_flex()
        .px_3()
        .py_1()
        .bg(c.header)
        .border_b_1()
        .border_color(c.border)
        .text_color(c.fg_dim)
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .child(label.into())
}

/// A small tree icon (chevron / database / table / view) tinted to `color`.
fn tree_icon(path: &'static str, color: Hsla) -> impl IntoElement {
    Icon::empty().path(path).text_color(color).small()
}

/// Muted placeholder leaf text ("loading…", "(no tables)") inside a schema.
fn tree_leaf_text(s: &'static str, c: Colors) -> impl IntoElement {
    div().pl_2().py(px(2.)).text_sm().text_color(c.fg_dim).child(s)
}

/// Wrap a query so it is ordered by one of its result columns. `Default` clears
/// the sort (returns the base query unchanged).
fn order_by_sql(base: &str, col: &str, sort: ColumnSort) -> String {
    let q = base.trim().trim_end_matches(';');
    let ident = col.replace('"', "\"\"");
    match sort {
        ColumnSort::Default => base.to_string(),
        ColumnSort::Ascending => format!("SELECT * FROM ({q}) AS _zdb ORDER BY \"{ident}\" ASC"),
        ColumnSort::Descending => format!("SELECT * FROM ({q}) AS _zdb ORDER BY \"{ident}\" DESC"),
    }
}

/// Collapse a (possibly multi-line) SQL string to a single trimmed line.
fn oneline(sql: &str) -> String {
    let s: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.len() > 120 {
        format!("{}…", &s[..120])
    } else {
        s
    }
}

fn ssl_from_str(s: &str) -> SslMode {
    match s.trim() {
        "disable" => SslMode::Disable,
        "require" => SslMode::Require,
        "verify-ca" => SslMode::VerifyCa,
        "verify-full" => SslMode::VerifyFull,
        _ => SslMode::Prefer,
    }
}

fn entry_to_config(entry: &ConnectionEntry, password: Option<String>) -> ConnectionConfig {
    let mut cfg = ConnectionConfig::new(
        entry.name.clone(),
        entry.host.clone(),
        entry.dbname.clone(),
        entry.user.clone(),
    );
    cfg.port = entry.port;
    cfg.ssl_mode = ssl_from_str(&entry.ssl_mode);
    cfg.password = password;
    cfg
}

fn rel_icon(kind: RelationKind) -> &'static str {
    match kind {
        RelationKind::Table => "icons/table.svg",
        RelationKind::View | RelationKind::MaterializedView => "icons/eye.svg",
        RelationKind::ForeignTable => "icons/database.svg",
        RelationKind::Other => "icons/table.svg",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use gpui_component::{Theme, ThemeMode};

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

    #[gpui::test]
    fn header_click_cycles_sort(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                ws.base_sql = Some("SELECT * FROM t".into());
                ws.headers = vec!["qty".into()];
                assert_eq!(ws.sort_state, None);
                ws.toggle_sort(0, cx);
                assert_eq!(ws.sort_state, Some((0, false))); // ascending
                ws.toggle_sort(0, cx);
                assert_eq!(ws.sort_state, Some((0, true))); // descending
                ws.toggle_sort(0, cx);
                assert_eq!(ws.sort_state, None); // cleared
            })
            .unwrap();
    }

    #[gpui::test]
    fn table_query_builds_where(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.table_view = Some(("public".into(), "users".into()));
                ws.where_input.update(cx, |i, cx| i.set_value("id > 5", window, cx));
                assert_eq!(
                    ws.table_query(cx),
                    "SELECT * FROM \"public\".\"users\" WHERE id > 5 LIMIT 500"
                );
                ws.where_input.update(cx, |i, cx| i.set_value("", window, cx));
                assert_eq!(
                    ws.table_query(cx),
                    "SELECT * FROM \"public\".\"users\" LIMIT 500"
                );
            })
            .unwrap();
    }

    #[gpui::test]
    fn ad_hoc_run_clears_table_view(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                ws.table_view = Some(("a".into(), "b".into()));
                ws.run_current(cx);
                assert!(ws.table_view.is_none());
            })
            .unwrap();
    }

    #[gpui::test]
    fn scratch_view_toggles(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                assert!(!ws.scratch_open);
                ws.toggle_scratch(cx);
                assert!(ws.scratch_open);
                ws.toggle_scratch(cx);
                assert!(!ws.scratch_open);
            })
            .unwrap();
    }

    #[gpui::test]
    fn single_click_selects_row(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                ws.set_current_row(3, cx);
                assert_eq!(ws.current_row, Some(3));
            })
            .unwrap();
    }

    #[gpui::test]
    fn switching_table_clears_row_selection(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, w, cx| {
                // The blue row highlight is the Table *widget's* own selection
                // (set by its internal click handler), separate from our
                // `current_row`. Simulate both being set on the open table.
                ws.table.update(cx, |ts, cx| ts.set_selected_row(4, cx));
                ws.set_current_row(4, cx);
                assert_eq!(ws.table.read(cx).selected_row(), Some(4));
                assert_eq!(ws.current_row, Some(4));
                // Open a different table via the real UI entry point. It must
                // drop BOTH the widget highlight and our selection, so row 5 of
                // the new table isn't left lit.
                ws.open_relation("public".into(), "other".into(), w, cx);
                assert_eq!(ws.current_row, None);
                assert_eq!(ws.table.read(cx).selected_row(), None);
            })
            .unwrap();
    }

    fn editable_ws(ws: &mut Workspace, headers: Vec<&str>) {
        ws.headers = headers.iter().map(|s| s.to_string()).collect();
        ws.edit_target = Some(EditTarget {
            schema: "public".into(),
            table: "widget".into(),
            pk_columns: vec!["id".into()],
        });
        ws.edit_cols = headers.iter().map(|s| Some(s.to_string())).collect();
    }

    #[gpui::test]
    fn add_row_then_save_stages_insert(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                editable_ws(ws, vec!["id", "name"]);
                ws.add_row(cx);
                let idx = ws.new_row_idx.expect("new row");

                // Editing the new row updates memory, not a staged statement.
                ws.begin_edit(idx, 0, window, cx);
                ws.cell_input.update(cx, |i, cx| i.set_value("7", window, cx));
                ws.commit_cell_edit(cx);
                assert!(ws.pending.is_empty());

                ws.begin_edit(idx, 1, window, cx);
                ws.cell_input.update(cx, |i, cx| i.set_value("zed", window, cx));
                ws.commit_cell_edit(cx);

                ws.save_new_row(cx);
                assert_eq!(ws.pending.len(), 1);
                let sql = ws.pending[0].to_sql(ws.edit_target.as_ref().unwrap()).unwrap();
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
            .update(cx, |ws, _w, cx| {
                editable_ws(ws, vec!["id"]);
                ws.rows = vec![vec![CellValue::Text("1".into())]];
                ws.add_row(cx);
                assert_eq!(ws.rows.len(), 2);
                ws.delete_current_row(cx); // current row is the new one → discard
                assert_eq!(ws.rows.len(), 1);
                assert!(ws.new_row_idx.is_none());
                assert!(ws.pending.is_empty());
            })
            .unwrap();
    }

    #[gpui::test]
    fn run_without_connection_is_guarded(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                ws.run_sql("SELECT 1".into(), cx);
                assert_eq!(ws.status, "Not connected");
                assert!(!ws.running);
            })
            .unwrap();
    }

    #[gpui::test]
    fn new_query_resets_editability(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                ws.edit_target = Some(EditTarget {
                    schema: "public".into(),
                    table: "t".into(),
                    pk_columns: vec!["id".into()],
                });
                ws.edit_cols = vec![Some("id".into())];
                ws.run_new_query("SELECT 1".into(), cx);
                assert!(ws.edit_target.is_none());
                assert!(ws.edit_cols.is_empty());
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
                ws.headers = vec!["id".into(), "name".into()];
                ws.rows = vec![vec![
                    CellValue::Text("1".into()),
                    CellValue::Text("alpha".into()),
                ]];
                ws.edit_target = Some(EditTarget {
                    schema: "public".into(),
                    table: "widget".into(),
                    pk_columns: vec!["id".into()],
                });
                ws.edit_cols = vec![Some("id".into()), Some("name".into())];

                ws.begin_edit(0, 1, window, cx);
                assert_eq!(ws.editing, Some((0, 1)));
                ws.cell_input.update(cx, |inp, cx| inp.set_value("beta", window, cx));
                ws.commit_cell_edit(cx);

                assert_eq!(ws.pending.len(), 1);
                let sql = ws.pending[0].to_sql(ws.edit_target.as_ref().unwrap()).unwrap();
                assert_eq!(
                    sql,
                    r#"UPDATE "public"."widget" SET "name" = 'beta' WHERE "id" = '1'"#
                );
                // Optimistic update reflects in the grid before Apply.
                assert_eq!(ws.rows[0][1], CellValue::Text("beta".into()));
            })
            .unwrap();
    }

    #[gpui::test]
    fn multiple_edits_accumulate_into_one_batch(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.headers = vec!["id".into(), "name".into()];
                ws.rows = vec![
                    vec![CellValue::Text("1".into()), CellValue::Text("a".into())],
                    vec![CellValue::Text("2".into()), CellValue::Text("b".into())],
                ];
                ws.edit_target = Some(EditTarget {
                    schema: "public".into(),
                    table: "widget".into(),
                    pk_columns: vec!["id".into()],
                });
                ws.edit_cols = vec![Some("id".into()), Some("name".into())];

                // Edit two different rows; both stage without replacing each other.
                ws.begin_edit(0, 1, window, cx);
                ws.cell_input.update(cx, |i, cx| i.set_value("x", window, cx));
                ws.commit_cell_edit(cx);
                ws.begin_edit(1, 1, window, cx);
                ws.cell_input.update(cx, |i, cx| i.set_value("y", window, cx));
                ws.commit_cell_edit(cx);

                assert_eq!(ws.pending.len(), 2);
                let batch = ws.pending_sql().expect("combined sql");
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
                ws.headers = vec!["id".into(), "name".into()];
                ws.rows = vec![vec![
                    CellValue::Text("1".into()),
                    CellValue::Text("alpha".into()),
                ]];
                ws.orig_rows = ws.rows.clone();
                ws.edit_target = Some(EditTarget {
                    schema: "public".into(),
                    table: "widget".into(),
                    pk_columns: vec!["id".into()],
                });
                ws.edit_cols = vec![Some("id".into()), Some("name".into())];

                // Edit to a new value: stages + marks the cell.
                ws.begin_edit(0, 1, window, cx);
                ws.cell_input.update(cx, |i, cx| i.set_value("beta", window, cx));
                ws.commit_cell_edit(cx);
                assert_eq!(ws.pending.len(), 1);
                assert!(ws.edited_cells.contains(&(0, 1)));

                // Edit back to the original value: the staged edit is dropped and
                // the cell is no longer marked.
                ws.begin_edit(0, 1, window, cx);
                ws.cell_input.update(cx, |i, cx| i.set_value("alpha", window, cx));
                ws.commit_cell_edit(cx);
                assert!(ws.pending.is_empty(), "edit reverted → no pending");
                assert!(!ws.edited_cells.contains(&(0, 1)));
                assert_eq!(ws.rows[0][1], CellValue::Text("alpha".into()));
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
