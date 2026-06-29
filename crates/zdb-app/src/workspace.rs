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
    /// The tab this grid belongs to; render reads that tab's editing state.
    tab_id: u64,
}

impl ResultDelegate {
    fn new(ws: WeakEntity<Workspace>, tab_id: u64) -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            ws,
            tab_id,
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
        let tab_id = self.tab_id;
        let arrow = self
            .ws
            .upgrade()
            .and_then(|w| match w.read(cx).tab(tab_id).and_then(|t| t.sort_state) {
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
                weak.update(app, |w, cx| w.toggle_sort(tab_id, col_ix, cx)).ok();
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

        let tab_id = self.tab_id;
        let Some(ws_entity) = self.ws.upgrade() else {
            return td_text(cell, c).into_any_element();
        };
        let ws = ws_entity.read(cx);
        let Some(tab) = ws.tab(tab_id) else {
            return td_text(cell, c).into_any_element();
        };
        let editing = tab.editing == Some((row_ix, col_ix));
        let editable = tab.edit_cols.get(col_ix).is_some_and(|o| o.is_some());
        let edited = tab.edited_cells.contains(&(row_ix, col_ix));
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
                            w.begin_edit(tab_id, row_ix, col_ix, window, cx);
                        } else {
                            w.set_current_row(tab_id, row_ix, cx);
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

// ---- tabs ----------------------------------------------------------------

/// What a center tab shows.
enum TabKind {
    /// Ad-hoc SQL editor + results.
    Query,
    /// The single auto-saved scratch editor + results.
    Scratch,
    /// A table browsed from the schema tree: grid + WHERE filter.
    Table { schema: String, table: String },
}

/// One center tab. Owns its editor, results grid, and all editing state, so
/// switching tabs preserves each tab's rows, selection, and staged edits.
struct Tab {
    id: u64,
    kind: TabKind,
    title: String,
    /// SQL editor (Query / Scratch tabs); allocated-but-hidden for Table tabs.
    editor: Entity<InputState>,
    /// WHERE filter input (Table tabs).
    where_input: Entity<InputState>,
    table: Entity<TableState<ResultDelegate>>,

    // Current result.
    headers: Vec<String>,
    rows: Vec<Vec<CellValue>>,
    orig_rows: Vec<Vec<CellValue>>,
    base_sql: Option<String>,
    last_sql: Option<String>,
    sort_state: Option<(usize, bool)>,

    // Editability (from DbHandle::describe of `base_sql`).
    edit_target: Option<EditTarget>,
    edit_cols: Vec<Option<String>>,

    // Inline editing.
    editing: Option<(usize, usize)>,
    current_row: Option<usize>,
    new_row_idx: Option<usize>,
    pending: Vec<RowEdit>,
    edited_cells: HashSet<(usize, usize)>,

    running: bool,
}

fn make_grid(
    weak: WeakEntity<Workspace>,
    id: u64,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<TableState<ResultDelegate>> {
    cx.new(|cx| TableState::new(ResultDelegate::new(weak, id), window, cx).row_selectable(true))
}

fn make_sql_editor(
    placeholder: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<InputState> {
    let ph = placeholder.to_string();
    cx.new(|cx| {
        InputState::new(window, cx)
            .code_editor("sql")
            .line_number(true)
            .placeholder(ph)
    })
}

impl Tab {
    fn base(
        id: u64,
        kind: TabKind,
        title: String,
        editor: Entity<InputState>,
        where_input: Entity<InputState>,
        table: Entity<TableState<ResultDelegate>>,
    ) -> Self {
        Self {
            id,
            kind,
            title,
            editor,
            where_input,
            table,
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
            pending: Vec::new(),
            edited_cells: HashSet::new(),
            running: false,
        }
    }

    /// A blank ad-hoc query tab ("Query N").
    fn query(id: u64, n: u64, window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let weak = cx.weak_entity();
        let editor = make_sql_editor("SELECT * FROM ... ;  (press Run)", window, cx);
        let where_input = cx.new(|cx| InputState::new(window, cx));
        let table = make_grid(weak, id, window, cx);
        Tab::base(id, TabKind::Query, format!("Query {n}"), editor, where_input, table)
    }

    /// The singleton scratch tab: editor seeded from disk and auto-saved on edit.
    fn scratch(id: u64, window: &mut Window, cx: &mut Context<Workspace>) -> Self {
        let weak = cx.weak_entity();
        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("sql")
                .line_number(true)
                .placeholder("Scratch query — auto-saved")
                .default_value(zdb_config::load_scratch())
        });
        // Persist scratch text to disk on every change.
        cx.subscribe(&editor, |_this, emitter, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                zdb_config::save_scratch(&emitter.read(cx).value());
            }
        })
        .detach();
        let where_input = cx.new(|cx| InputState::new(window, cx));
        let table = make_grid(weak, id, window, cx);
        Tab::base(id, TabKind::Scratch, "Scratch".into(), editor, where_input, table)
    }

    /// A table-browse tab; the WHERE input re-runs the query on Enter.
    fn table(
        id: u64,
        schema: String,
        table_name: String,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let weak = cx.weak_entity();
        let editor = make_sql_editor("", window, cx);
        let where_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("WHERE … (Enter to filter)"));
        cx.subscribe(&where_input, move |this, _i, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.apply_where(id, cx);
            }
        })
        .detach();
        let table = make_grid(weak, id, window, cx);
        let title = format!("{schema}.{table_name}");
        Tab::base(
            id,
            TabKind::Table { schema, table: table_name },
            title,
            editor,
            where_input,
            table,
        )
    }
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
                                        this.open_table_tab(s.clone(), t.clone(), window, cx)
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
        let strip = self.render_tab_strip(c, cx);
        let body = match self.active {
            Some(idx) if idx < self.tabs.len() => {
                Self::render_tab_body(&self.tabs[idx], c, cx).into_any_element()
            }
            _ => welcome_pane(c).into_any_element(),
        };
        v_flex().size_full().bg(c.center).child(strip).child(body)
    }

    /// The Zed-style tab strip: one chip per open tab + `+` (new query) and a
    /// scratch button.
    fn render_tab_strip(&self, c: Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let mut strip = h_flex()
            .h(px(34.))
            .flex_shrink_0()
            .items_center()
            .bg(c.header)
            .border_b_1()
            .border_color(c.border)
            .overflow_hidden();
        for (idx, tab) in self.tabs.iter().enumerate() {
            let is_active = self.active == Some(idx);
            let id = tab.id;
            let kind_icon = match tab.kind {
                TabKind::Table { .. } => "icons/table.svg",
                _ => "icons/file.svg",
            };
            let tint = if is_active { c.fg } else { c.fg_dim };
            // Separate clickable regions (title = activate, x = close) so a close
            // click doesn't also re-activate a just-removed tab.
            let label = h_flex()
                .id(SharedString::from(format!("tab-{id}")))
                .h_full()
                .pl_2()
                .pr_1()
                .gap_1p5()
                .items_center()
                .cursor_pointer()
                .text_sm()
                .text_color(tint)
                .when(!is_active, |d| d.hover(|s| s.bg(c.hover)))
                .child(tree_icon(kind_icon, tint))
                .child(div().max_w(px(160.)).truncate().child(tab.title.clone()))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.activate_tab(idx, window, cx)
                }));
            let close = div()
                .id(SharedString::from(format!("tabx-{id}")))
                .h_full()
                .px_1()
                .flex()
                .items_center()
                .cursor_pointer()
                .hover(|s| s.bg(c.hover))
                .child(tree_icon("icons/close.svg", c.fg_dim))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.close_tab(id, cx)));
            strip = strip.child(
                h_flex()
                    .h_full()
                    .items_center()
                    .flex_shrink_0()
                    .border_r_1()
                    .border_color(c.border)
                    .when(is_active, |d| d.bg(c.center))
                    .child(label)
                    .child(close),
            );
        }
        strip
            .child(
                Button::new("tab-add")
                    .icon(IconName::Plus)
                    .tooltip("New query")
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.open_query_tab(window, cx)),
                    ),
            )
            .child(
                Button::new("tab-scratch")
                    .icon(IconName::File)
                    .tooltip("Scratch (Ctrl/Cmd+Shift+E)")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.focus_scratch_tab(window, cx)
                    })),
            )
    }

    /// The body of one tab: the action toolbar + (editor split | table grid).
    fn render_tab_body(tab: &Tab, c: Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = tab.id;
        let editable = tab.edit_target.is_some();

        // Run toggles to Stop while a query is in flight.
        let run_btn = if tab.running {
            Button::new("run")
                .icon(Icon::empty().path("icons/circle-x.svg").text_color(rgba(0xef4444ff)))
                .tooltip("Stop")
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel(cx)))
        } else {
            Button::new("run")
                .icon(Icon::empty().path("icons/play.svg").text_color(rgba(0x22c55eff)))
                .tooltip("Run (Ctrl/Cmd+Enter)")
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.run_active_tab(cx)))
        };

        // +/- always visible; disabled when the result isn't editable / no row is
        // selected. Save-row appears only mid-insert.
        let del_tip = if tab.current_row.is_some() && tab.current_row == tab.new_row_idx {
            "Discard row"
        } else {
            "Delete row"
        };
        // Red only when actionable; neutral (theme fg) when disabled.
        let del_enabled = editable && tab.current_row.is_some();
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
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.add_row(tab_id, cx))),
            )
            .child(
                Button::new("del-row")
                    .icon(Icon::empty().path("icons/minus.svg").text_color(del_color))
                    .tooltip(del_tip)
                    .disabled(!del_enabled)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.delete_current_row(tab_id, cx)
                    })),
            )
            .child(
                Button::new("reload")
                    .icon(Icon::empty().path("icons/refresh-cw.svg").text_color(c.fg))
                    .tooltip("Refresh data")
                    .disabled(tab.base_sql.is_none())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.reload_data(tab_id, cx)
                    })),
            );
        if tab.new_row_idx.is_some() {
            toolbar = toolbar.child(
                Button::new("save-row")
                    .icon(IconName::Check)
                    .tooltip("Save row")
                    .primary()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.save_new_row(tab_id, cx)
                    })),
            );
        }

        let mut results = v_flex().size_full();
        if let TabKind::Table { schema, table } = &tab.kind {
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
                            .child(format!("{schema}.{table}  WHERE")),
                    )
                    .child(div().flex_grow().child(Input::new(&tab.where_input))),
            );
        }
        results = results.child(
            div()
                .size_full()
                .child(Table::new(&tab.table).stripe(true).bordered(true)),
        );

        // Table tabs hide the redundant generated `SELECT *` editor and let the
        // rows fill the pane; Query / Scratch keep the editor above the results.
        let body = if matches!(tab.kind, TabKind::Table { .. }) {
            results.into_any_element()
        } else {
            v_resizable(SharedString::from(format!("center-{tab_id}")))
                .child(
                    resizable_panel()
                        .size(px(170.))
                        .size_range(px(60.)..px(600.))
                        .child(div().size_full().child(Input::new(&tab.editor).h_full())),
                )
                .child(resizable_panel().child(results))
                .into_any_element()
        };

        v_flex().size_full().bg(c.center).child(toolbar).child(body)
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

        // When the active tab has staged edits, the bottom-right pane shows the
        // combined SQL for review; otherwise it shows status messages.
        let pending_tab = self
            .active_tab()
            .filter(|t| !t.pending.is_empty())
            .map(|t| (t.id, t.pending.len()));
        let right = if let Some((tab_id, n)) = pending_tab {
            let sql = self.pending_sql(tab_id).unwrap_or_default();
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
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.apply_pending(tab_id, cx)
                                })),
                        )
                        .child(Button::new("cancel-edit").label("Cancel").on_click(
                            cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.cancel_pending(tab_id, cx)
                            }),
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
        // A table requested from a windowless context (e.g. the selftest) opens
        // here, where the window is available.
        if let Some((schema, table)) = self.pending_open.take() {
            self.open_table_tab(schema, table, window, cx);
        }
        let center = self.render_center(cx).into_any_element();
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
            .on_action(cx.listener(|this, _: &RunQuery, _, cx| this.run_active_tab(cx)))
            .on_action(cx.listener(|this, _: &CancelQuery, _, cx| this.cancel(cx)))
            .on_action(cx.listener(|this, _: &ToggleScratch, window, cx| {
                this.focus_scratch_tab(window, cx)
            }))
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

/// Centered placeholder shown when no tab is open.
fn welcome_pane(c: Colors) -> impl IntoElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .bg(c.center)
        .child(div().text_sm().text_color(c.fg_dim).child("No tab open"))
        .child(
            div()
                .text_xs()
                .text_color(c.fg_dim)
                .child("Open a table from the sidebar, or press + for a new query"),
        )
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
