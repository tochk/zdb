//! The results grid (`ResultDelegate` / `TableDelegate`) and the per-tab state
//! that owns it (`Tab`), so switching tabs preserves each tab's rows, selection,
//! and staged edits.

use gpui::{
    div, prelude::FluentBuilder as _, px, rgba, App, AppContext, ClickEvent, Context, Entity,
    InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement,
    Styled, WeakEntity, Window,
};
use gpui_component::{
    input::{Input, InputEvent, InputState},
    table::{Column, TableDelegate, TableState},
    Sizable as _,
};
use std::collections::HashSet;
use std::rc::Rc;
use zdb_db::{CellValue, EditTarget, RowEdit};

use super::colors::{palette, Colors};
use super::Workspace;
use crate::lsp::{LspSlot, SqlCompletion};

/// Translucent blue fill for a cell with a staged (unsaved) edit, plus a solid
/// blue left bar. Fixed so it reads on both light and dark themes.
const EDITED_BG: u32 = 0x3b82f666;
const EDITED_BAR: u32 = 0x2563ebff;
const EDITED_HOVER: u32 = 0x2563eb99;

// ---- results table delegate ---------------------------------------------

/// Renders the results grid and bridges interactions (cell edit, sort) back to
/// the `Workspace`, which owns all editing state.
pub(crate) struct ResultDelegate {
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

    pub(crate) fn set(&mut self, headers: &[String], rows: Vec<Vec<CellValue>>) {
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
pub(crate) enum TabKind {
    /// Ad-hoc SQL editor + results.
    Query,
    /// The single auto-saved scratch editor + results.
    Scratch,
    /// A table browsed from the schema tree: grid + WHERE filter.
    Table { schema: String, table: String },
}

/// One center tab. Owns its editor, results grid, and all editing state, so
/// switching tabs preserves each tab's rows, selection, and staged edits.
pub(crate) struct Tab {
    pub(crate) id: u64,
    pub(crate) kind: TabKind,
    pub(crate) title: String,
    /// SQL editor (Query / Scratch tabs); allocated-but-hidden for Table tabs.
    pub(crate) editor: Entity<InputState>,
    /// WHERE filter input (Table tabs).
    pub(crate) where_input: Entity<InputState>,
    pub(crate) table: Entity<TableState<ResultDelegate>>,

    // Current result.
    pub(crate) headers: Vec<String>,
    pub(crate) rows: Vec<Vec<CellValue>>,
    pub(crate) orig_rows: Vec<Vec<CellValue>>,
    pub(crate) base_sql: Option<String>,
    pub(crate) last_sql: Option<String>,
    pub(crate) sort_state: Option<(usize, bool)>,

    // Editability (from DbHandle::describe of `base_sql`).
    pub(crate) edit_target: Option<EditTarget>,
    pub(crate) edit_cols: Vec<Option<String>>,

    // Inline editing.
    pub(crate) editing: Option<(usize, usize)>,
    pub(crate) current_row: Option<usize>,
    pub(crate) new_row_idx: Option<usize>,
    pub(crate) pending: Vec<RowEdit>,
    pub(crate) edited_cells: HashSet<(usize, usize)>,

    pub(crate) running: bool,

    /// When set, an EXPLAIN plan to show in its own text view instead of the grid.
    pub(crate) plan: Option<String>,
}

pub(crate) fn make_grid(
    weak: WeakEntity<Workspace>,
    id: u64,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<TableState<ResultDelegate>> {
    cx.new(|cx| TableState::new(ResultDelegate::new(weak, id), window, cx).row_selectable(true))
}

pub(crate) fn make_sql_editor(
    placeholder: &str,
    slot: LspSlot,
    tab_id: u64,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<InputState> {
    let ph = placeholder.to_string();
    cx.new(|cx| {
        let mut state = InputState::new(window, cx)
            .code_editor("sql")
            .line_number(true)
            .placeholder(ph);
        // Schema-aware completion via the shared `sqls` handle (no-op until a
        // server is running).
        state.lsp.completion_provider = Some(Rc::new(SqlCompletion::new(slot, tab_id)));
        state
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
            plan: None,
        }
    }

    /// A blank ad-hoc query tab ("Query N").
    pub(crate) fn query(
        id: u64,
        n: u64,
        slot: LspSlot,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let weak = cx.weak_entity();
        let editor = make_sql_editor("SELECT * FROM ... ;  (press Run)", slot, id, window, cx);
        let where_input = cx.new(|cx| InputState::new(window, cx));
        let table = make_grid(weak, id, window, cx);
        Tab::base(id, TabKind::Query, format!("Query {n}"), editor, where_input, table)
    }

    /// The singleton scratch tab: editor seeded from disk and auto-saved on edit.
    pub(crate) fn scratch(
        id: u64,
        slot: LspSlot,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let weak = cx.weak_entity();
        let editor = cx.new(|cx| {
            let mut state = InputState::new(window, cx)
                .code_editor("sql")
                .line_number(true)
                .placeholder("Scratch query — auto-saved")
                .default_value(zdb_config::load_scratch());
            state.lsp.completion_provider = Some(Rc::new(SqlCompletion::new(slot, id)));
            state
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
    pub(crate) fn table(
        id: u64,
        schema: String,
        table_name: String,
        slot: LspSlot,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let weak = cx.weak_entity();
        let editor = make_sql_editor("", slot, id, window, cx);
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
