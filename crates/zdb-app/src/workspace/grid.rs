//! The results grid (`ResultDelegate` / `TableDelegate`) and the per-tab state
//! that owns it (`Tab`), so switching tabs preserves each tab's rows, selection,
//! and staged edits.

use gpui::{
    div, prelude::FluentBuilder as _, px, rgba, App, AppContext, ClickEvent, Context,
    DragMoveEvent, Entity, EntityId, FocusHandle, Focusable as _, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, SharedString, StatefulInteractiveElement, Styled, WeakEntity,
    Window,
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
use super::session;
use super::Workspace;
use crate::lsp::{self, LspSlot, SqlCompletion};

/// Translucent blue fill for a cell with a staged (unsaved) edit, plus a solid
/// blue left bar. Fixed so it reads on both light and dark themes.
const EDITED_BG: u32 = 0x3b82f666;
const EDITED_BAR: u32 = 0x2563ebff;
const EDITED_HOVER: u32 = 0x2563eb99;
/// Outline of the keyboard cell cursor (drawn only while the grid has focus).
const CURSOR_RING: u32 = 0x2563ebff;

/// Grid column 0 is a row-number gutter, so grid column `i+1` renders result
/// column `i`. Everything the `Workspace` stores (sort state, `edit_cols`,
/// `editing`, `edited_cells`) is in RESULT-column space — this file is the only
/// place that maps between the two.
pub(crate) const ROWNUM_COL: usize = 0;

/// Default (and minimum) column widths. A drag can widen a column but never
/// shrink it below the width it starts at.
const ROWNUM_W: Pixels = px(56.);
const COL_W: Pixels = px(180.);
/// Grab zone for our own resize handle. The widget's built-in one is 2px wide
/// (`col_resizable(false)` in `make_grid` turns it off) — nobody can hit that.
const HANDLE_W: Pixels = px(9.);
/// The widget wraps our `render_th` in a flex that gets `pr(cell_padding.right -
/// column padding.right)` — 8px, since our columns are `p_0()` — to leave room
/// for its sort icon. So the box we render into ends 8px SHORT of the column's
/// real right edge, and the handle has to be pushed back out by that much to sit
/// on the boundary the user actually sees (it stays inside the cell, so the
/// cell's `overflow_hidden` doesn't clip it away).
const HEADER_PR: Pixels = px(8.);

fn min_col_width(col_ix: usize) -> Pixels {
    if col_ix == ROWNUM_COL {
        ROWNUM_W
    } else {
        COL_W
    }
}

/// Drag payload for a column resize: which table, which column.
#[derive(Clone, Copy)]
struct ResizeCol(EntityId, usize);

impl Render for ResizeCol {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        // The drag preview is invisible; the column width itself is the feedback.
        div()
    }
}

// ---- results table delegate ---------------------------------------------

/// Renders the results grid and bridges interactions (cell edit, sort) back to
/// the `Workspace`, which owns all editing state.
pub(crate) struct ResultDelegate {
    columns: Vec<Column>,
    rows: Vec<Vec<CellValue>>,
    ws: WeakEntity<Workspace>,
    /// The tab this grid belongs to; render reads that tab's editing state.
    tab_id: u64,
    /// Where inside the resize handle the current drag was grabbed (column +
    /// offset from the column's right edge), so the column doesn't jump by the
    /// grab offset on the first mouse move.
    resize_grab: Option<(usize, Pixels)>,
}

impl ResultDelegate {
    fn new(ws: WeakEntity<Workspace>, tab_id: u64) -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
            ws,
            tab_id,
            resize_grab: None,
        }
    }

    pub(crate) fn set(&mut self, headers: &[String], rows: Vec<Vec<CellValue>>) {
        // `p_0()` zeroes the widget's own cell padding (`table_cell_padding`,
        // 8px/4px) so OUR td element fills the whole cell box — otherwise the
        // edit/edited highlight paints inside that padding and reads visibly
        // smaller than the cell. We re-add the padding ourselves in `td_text`
        // / `render_th` so the text lands in the same place as before.
        // Re-running the same query (sort, refresh, reload after an edit) must not
        // throw away the widths the user dragged, so carry them over per column
        // name; a genuinely different result set just won't match.
        let old: std::collections::HashMap<SharedString, Pixels> = self
            .columns
            .iter()
            .skip(1)
            .map(|c| (c.name.clone(), c.width))
            .collect();
        let rownum_w = self.columns.first().map_or(ROWNUM_W, |c| c.width);
        let rownum = Column::new(SharedString::from("rownum"), SharedString::from(""))
            .width(rownum_w.max(ROWNUM_W))
            .fixed_left()
            .p_0();
        self.columns = std::iter::once(rownum)
            .chain(headers.iter().enumerate().map(|(i, h)| {
                let name = SharedString::from(h.clone());
                let w = old.get(&name).copied().unwrap_or(COL_W).max(COL_W);
                Column::new(SharedString::from(format!("c{i}")), name)
                    .width(w)
                    .p_0()
            }))
            .collect();
        self.rows = rows;
        self.resize_grab = None;
    }

    /// Left edge (within the scrollable columns, i.e. past the pinned row-number
    /// gutter) and width of RESULT column `col`.
    pub(crate) fn col_span(&self, col: usize) -> Option<(Pixels, Pixels)> {
        let width = self.columns.get(col + 1)?.width;
        let left = self.columns[1..=col]
            .iter()
            .fold(px(0.), |sum, c| sum + c.width);
        Some((left, width))
    }

    /// Applies a dragged width (clamped to the column's minimum). Returns
    /// whether anything changed — the caller only refreshes when it did.
    fn set_col_width(&mut self, col_ix: usize, width: Pixels) -> bool {
        let width = width.max(min_col_width(col_ix)).floor();
        let Some(col) = self.columns.get_mut(col_ix) else {
            return false;
        };
        if (col.width - width).abs() < px(1.) {
            return false;
        }
        col.width = width;
        true
    }
}

/// A fat, draggable grab zone on the right edge of a header cell.
///
/// The widget's own handle is disabled (see `make_grid`) because it is 2px wide.
/// This one is [`HANDLE_W`] and does the resize itself: `on_drag_move` fires for
/// the whole window while a drag is live (gpui checks only the payload type, not
/// the bounds), and `e.bounds` is this handle's own box — its right edge IS the
/// column's right edge, so the new width follows the cursor with no drift.
fn resize_handle(col_ix: usize, cx: &mut Context<TableState<ResultDelegate>>) -> impl IntoElement {
    let entity_id = cx.entity_id();
    div()
        .id(SharedString::from(format!("colresize-{col_ix}")))
        // Keeps the header's sort click (and the widget's column-move drag) from
        // firing when the user is aiming at the handle.
        .occlude()
        .absolute()
        .top_0()
        .bottom_0()
        .right(-HEADER_PR)
        .w(HANDLE_W)
        .cursor_col_resize()
        .on_drag(ResizeCol(entity_id, col_ix), |drag, _, _, cx| {
            cx.stop_propagation();
            cx.new(|_| drag.clone())
        })
        .on_drag_move(cx.listener(
            move |ts: &mut TableState<ResultDelegate>, e: &DragMoveEvent<ResizeCol>, _, cx| {
                let ResizeCol(id, ix) = *e.drag(cx);
                if id != cx.entity_id() || ix != col_ix {
                    return;
                }
                let offset = e.event.position.x - e.bounds.right();
                let grab = match ts.delegate().resize_grab {
                    Some((c, grab)) if c == ix => grab,
                    // First move of this drag: remember where inside the handle
                    // the cursor grabbed, so the column doesn't jump.
                    _ => {
                        ts.delegate_mut().resize_grab = Some((ix, offset));
                        offset
                    }
                };
                let width = ts
                    .delegate()
                    .columns
                    .get(ix)
                    .map(|c| c.width)
                    .unwrap_or(COL_W)
                    + offset
                    - grab;
                if ts.delegate_mut().set_col_width(ix, width) {
                    // Rebuilds the widget's cached `col_groups` from our columns.
                    ts.refresh(cx);
                }
            },
        ))
        .on_mouse_up_out(
            gpui::MouseButton::Left,
            cx.listener(|ts: &mut TableState<ResultDelegate>, _, _, _| {
                ts.delegate_mut().resize_grab = None;
            }),
        )
        .on_mouse_up(
            gpui::MouseButton::Left,
            cx.listener(|ts: &mut TableState<ResultDelegate>, _, _, _| {
                ts.delegate_mut().resize_grab = None;
            }),
        )
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
        // Sort arrow for the currently-sorted column (in result-column space).
        let tab_id = self.tab_id;
        let data_ix = col_ix.wrapping_sub(1);
        let arrow = self
            .ws
            .upgrade()
            .and_then(
                |w| match w.read(cx).tab(tab_id).and_then(|t| t.sort_state) {
                    Some((ci, desc)) if ci == data_ix => Some(if desc { " ▼" } else { " ▲" }),
                    _ => None,
                },
            )
            .unwrap_or("");
        let weak = self.ws.clone();
        div()
            .id(SharedString::from(format!("th-{col_ix}")))
            .size_full()
            .flex()
            .items_center()
            // Columns are `p_0()`, so the header supplies its own padding (matches
            // `td_text`'s `px_2` → header and cell text share one left edge).
            .px_2()
            .relative()
            .children(col_sep(col_ix, palette(cx)))
            .child(format!("{name}{arrow}"))
            // The row-number gutter has no data to sort by.
            .when(col_ix != ROWNUM_COL, |d| {
                d.cursor_pointer()
                    .on_click(move |_: &ClickEvent, window, app| {
                        weak.update(app, |w, cx| w.toggle_sort(tab_id, data_ix, window, cx))
                            .ok();
                    })
            })
            .child(resize_handle(col_ix, cx))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let c = palette(cx);
        let tab_id = self.tab_id;

        // Column 0 is the row-number gutter: not data, not editable, but clicking
        // it selects the row like any other cell.
        if col_ix == ROWNUM_COL {
            let weak = self.ws.clone();
            let first = weak
                .upgrade()
                .and_then(|ws| ws.read(cx).tab(tab_id).map(|t| t.first_row))
                .unwrap_or(0);
            return div()
                .id(SharedString::from(format!("rownum-{row_ix}")))
                .size_full()
                .flex()
                .items_center()
                .justify_end()
                .px_2()
                .text_sm()
                .text_color(c.fg_dim)
                .cursor_pointer()
                .child((first + row_ix + 1).to_string())
                .on_click(move |_: &ClickEvent, _window, app| {
                    weak.update(app, |w, cx| w.set_cursor_row(tab_id, row_ix, cx))
                        .ok();
                })
                .into_any_element();
        }

        // Grid column i+1 renders result column i. From here on `col_ix` is a
        // RESULT column index (what the tab's edit/sort state is keyed by); every
        // data cell sits right of the gutter, so all of them get a separator.
        let col_ix = col_ix - 1;
        let cell = self.rows.get(row_ix).and_then(|r| r.get(col_ix)).cloned();

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
        // The keyboard cursor: an outline, and only while the grid itself holds
        // focus (the row highlight is what stays when focus moves elsewhere).
        let cursor = (tab.cursor == Some((row_ix, col_ix)) && tab.grid_focus.is_focused(window))
            .then(cursor_ring);

        if editing && tab.editing_multi {
            // A multi-line value is edited in the strip under the grid (see
            // `render_value_editor`); the cell only marks itself as the target.
            return div()
                .size_full()
                .relative()
                .bg(c.active)
                .child(sep_line(c))
                .child(td_text(cell, c))
                .into_any_element();
        }
        if editing {
            // Seamless inline edit: `appearance(false)` drops the input's border /
            // background, and `.small()` sets its horizontal padding to 8px — the
            // same as `td_text`'s `px_2` — so the text sits exactly where the
            // display text was (no shift right/down). The wrapper carries NO padding
            // or border of its own (that was the original "text shifts right and
            // down" problem); it fills the whole cell (columns are `p_0()`) and
            // centers the input vertically, so the highlight matches the cell box.
            return div()
                .size_full()
                .flex()
                .items_center()
                .relative()
                .bg(c.active)
                .text_sm()
                .child(sep_line(c))
                .child(Input::new(&cell_input).appearance(false).small())
                .into_any_element();
        }

        let base = td_text(cell, c);
        if editable {
            let weak = self.ws.clone();
            div()
                .id(SharedString::from(format!("cell-{row_ix}-{col_ix}")))
                .size_full()
                .relative()
                // Column separator goes in FIRST so the dirty gutter below paints
                // over it (both sit on the left edge).
                .child(sep_line(c))
                // Staged-edit marker: blue fill + a solid blue left bar (the bar
                // stays visible even over the row-selection tint, like a dirty
                // gutter in DataGrip / Zed). Hover keeps the blue identity (a bit
                // darker) rather than reverting to the neutral hover color.
                // The bar is an ABSOLUTE overlay, not `border_l_2`: a real border
                // takes part in layout and shifted the cell text 2px right, which
                // was plainly visible next to unedited rows.
                .when(edited, |d| {
                    d.bg(rgba(EDITED_BG))
                        .hover(|s| s.bg(rgba(EDITED_HOVER)))
                        .child(
                            div()
                                .absolute()
                                .left_0()
                                .top_0()
                                .bottom_0()
                                .w(px(2.))
                                .bg(rgba(EDITED_BAR)),
                        )
                })
                .when(!edited, |d| d.hover(|s| s.bg(c.hover)))
                .cursor_pointer()
                .child(base)
                .children(cursor)
                .on_click(move |ev: &ClickEvent, window, app| {
                    let double = ev.click_count() >= 2;
                    weak.update(app, |w, cx| {
                        if double {
                            w.begin_edit(tab_id, row_ix, col_ix, window, cx);
                        } else {
                            w.set_cursor(tab_id, row_ix, col_ix, cx);
                        }
                    })
                    .ok();
                })
                .into_any_element()
        } else {
            // Read-only cells still take the cursor (navigation and copy work on
            // any result, not just editable ones).
            let weak = self.ws.clone();
            base.id(SharedString::from(format!("cell-{row_ix}-{col_ix}")))
                .relative()
                .child(sep_line(c))
                .children(cursor)
                .on_click(move |_: &ClickEvent, _window, app| {
                    weak.update(app, |w, cx| w.set_cursor(tab_id, row_ix, col_ix, cx))
                        .ok();
                })
                .into_any_element()
        }
    }
}

/// A 1px rule on a cell's LEFT edge, separating it from the previous column
/// (skipped for column 0 so no line hugs the table's left edge).
///
/// It is an absolute overlay rather than `border_l_1`/`border_r_1` on purpose:
/// a real border takes part in layout and would nudge the cell text sideways —
/// and it must be the left edge in BOTH the header and the body, because the
/// widget pads the header's inner flex on the right (`offset_pr`) so a
/// right-edge rule would not line up between the two.
fn col_sep(col_ix: usize, c: Colors) -> Option<gpui::Div> {
    (col_ix > 0).then(|| sep_line(c))
}

fn sep_line(c: Colors) -> gpui::Div {
    div()
        .absolute()
        .left_0()
        .top_0()
        .bottom_0()
        .w(px(1.))
        .bg(c.grid)
}

/// The cell-cursor outline. An absolute overlay like the dirty gutter — a real
/// border would take part in layout and nudge the text.
fn cursor_ring() -> gpui::Div {
    div()
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .border_1()
        .border_color(rgba(CURSOR_RING))
}

fn td_text(cell: Option<CellValue>, c: Colors) -> gpui::Div {
    // Fills the cell (columns are `p_0()`) and carries the padding the widget used
    // to add, so highlights cover the full cell while the text stays put. Vertically
    // centered, matching the inline edit input. `whitespace_nowrap` + clipping keep
    // a multi-line value one row tall: without them the text WRAPS inside the fixed
    // 32px row, so a 3-line value shows only its middle line and a tall one bleeds
    // over its neighbours.
    let base = div()
        .size_full()
        .flex()
        .items_center()
        .px_2()
        .text_sm()
        .whitespace_nowrap()
        .overflow_hidden()
        .text_ellipsis();
    match cell {
        Some(CellValue::Text(s)) => base.text_color(c.fg).child(display_text(&s)),
        Some(CellValue::Null) => base.text_color(c.fg_null).child("NULL"),
        None => base.child(""),
    }
}

/// How many characters of a cell value the grid draws. Past this the text is far
/// wider than any column, and shaping a whole document per cell is wasted work.
const CELL_MAX_CHARS: usize = 300;

/// Flattens a value for DISPLAY only — the inline editor loads the raw string
/// from `tab.rows`, and export/copy use the raw rows too, so a newline survives
/// a round trip through the grid. Line breaks become `⏎` (as in Zed / DataGrip)
/// rather than vanishing, which would make two different values look identical.
fn display_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(CELL_MAX_CHARS));
    for ch in s.chars().take(CELL_MAX_CHARS) {
        match ch {
            '\n' => out.push('⏎'),
            '\r' => {}
            '\t' => out.push(' '),
            c => out.push(c),
        }
    }
    if s.chars().nth(CELL_MAX_CHARS).is_some() {
        out.push('…');
    }
    out
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
    /// ORDER BY input (Table tabs). Header clicks write into it, so the text is
    /// the single source of truth for the generated `ORDER BY`.
    pub(crate) order_input: Entity<InputState>,
    /// LIMIT input (Table tabs; empty = no limit). Also the page size.
    pub(crate) limit_input: Entity<InputState>,
    /// Table tabs: which page of `limit` rows is shown (`OFFSET page * limit`).
    pub(crate) page: usize,
    /// Number (0-based) of the result's first row in the whole table — the
    /// page's OFFSET when the rows were fetched. The row gutter counts from it.
    pub(crate) first_row: usize,
    /// Table tabs: rows under the current WHERE, once "last page" has counted
    /// them (forgotten when the boxes are applied again).
    pub(crate) total_rows: Option<usize>,
    pub(crate) table: Entity<TableState<ResultDelegate>>,
    /// The grid's focus handle (a clone of the `TableState`'s). Kept here because
    /// `render_td` runs while that entity is being rendered and can't read it.
    pub(crate) grid_focus: FocusHandle,

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
    /// Whether the cell being edited holds a multi-line value, which is edited in
    /// the `cell_multi` strip under the grid instead of inline (a single-line
    /// input panics gpui's text shaper on a newline).
    pub(crate) editing_multi: bool,
    pub(crate) current_row: Option<usize>,
    /// Keyboard cell cursor, `(row, result column)`. `current_row` always equals
    /// its row; go through `Workspace::set_cursor` to keep the two (and the
    /// widget's row highlight) in step.
    pub(crate) cursor: Option<(usize, usize)>,
    /// Put the cursor on the first cell when the next result lands (the tab was
    /// opened from the keyboard, before it had any rows to stand on).
    pub(crate) cursor_on_load: bool,
    pub(crate) new_row_idx: Option<usize>,
    pub(crate) pending: Vec<RowEdit>,
    pub(crate) edited_cells: HashSet<(usize, usize)>,

    pub(crate) running: bool,
    /// A Table tab restored from a saved session that hasn't run its query yet
    /// (it does the first time it is shown).
    pub(crate) needs_load: bool,

    /// When set, an EXPLAIN plan to show in its own text view instead of the grid.
    pub(crate) plan: Option<String>,

    /// The statement last run from the editor, and its byte offset in the
    /// editor text: where a failure's error position goes back as a mark.
    pub(crate) editor_run: Option<(String, usize)>,
    /// Table tabs: the box the last failed run's error points into.
    pub(crate) clause_error: Option<Clause>,
}

/// A Table tab's WHERE / ORDER BY / LIMIT box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Clause {
    Where,
    OrderBy,
    Limit,
}

/// Where the pager goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PageMove {
    First,
    Prev,
    Next,
    Last,
}

/// Drop an editor's error marks. They are byte offsets into the text they
/// were made for, so any edit invalidates them (and one past the new end would
/// trip gpui's text layout).
pub(crate) fn clear_marks(input: &Entity<InputState>, cx: &mut App) {
    input.update(cx, |state, _| {
        if let Some(marks) = state.diagnostics_mut() {
            marks.clear();
        }
    });
}

pub(crate) fn make_grid(
    weak: WeakEntity<Workspace>,
    id: u64,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> Entity<TableState<ResultDelegate>> {
    cx.new(|cx| {
        TableState::new(ResultDelegate::new(weak, id), window, cx)
            .row_selectable(true)
            // We keep our own cell cursor; the widget's whole-column selection
            // (its left/right keys, header clicks) would only fight it.
            .col_selectable(false)
            // The widget's own resize handle is 2px wide and unhittable; we draw
            // a fat one in `render_th` and do the resize ourselves.
            .col_resizable(false)
            // Column moves reorder the widget's `col_groups` only — our rows and
            // the row-number gutter keep their order, so a move would desync the
            // grid-column → result-column mapping.
            .col_movable(false)
    })
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
        order_input: Entity<InputState>,
        limit_input: Entity<InputState>,
        table: Entity<TableState<ResultDelegate>>,
        cx: &App,
    ) -> Self {
        // A tab stop, so Tab from the filter / editor lands in the grid. The
        // flag lives on the shared focus entry, so the widget's own copy (which
        // it tracks) picks it up too.
        let grid_focus = table.read(cx).focus_handle(cx).tab_stop(true);
        Self {
            id,
            kind,
            title,
            editor,
            where_input,
            order_input,
            limit_input,
            page: 0,
            first_row: 0,
            total_rows: None,
            table,
            grid_focus,
            headers: Vec::new(),
            rows: Vec::new(),
            orig_rows: Vec::new(),
            base_sql: None,
            last_sql: None,
            sort_state: None,
            edit_target: None,
            edit_cols: Vec::new(),
            editing: None,
            editing_multi: false,
            current_row: None,
            cursor: None,
            cursor_on_load: false,
            new_row_idx: None,
            pending: Vec::new(),
            edited_cells: HashSet::new(),
            running: false,
            needs_load: false,
            plan: None,
            editor_run: None,
            clause_error: None,
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
        cx.subscribe(&editor, |this, input, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                clear_marks(&input, cx);
                this.mark_session_dirty(cx);
            }
        })
        .detach();
        let where_input = cx.new(|cx| InputState::new(window, cx));
        let order_input = cx.new(|cx| InputState::new(window, cx));
        let limit_input = cx.new(|cx| InputState::new(window, cx));
        let table = make_grid(weak, id, window, cx);
        Tab::base(
            id,
            TabKind::Query,
            format!("Query {n}"),
            editor,
            where_input,
            order_input,
            limit_input,
            table,
            cx,
        )
    }

    /// The singleton scratch tab: editor seeded from disk and auto-saved on edit.
    /// `key` is the connection the text belongs to (`None` = the global scratch,
    /// used while not connected).
    pub(crate) fn scratch(
        id: u64,
        key: Option<String>,
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
                .default_value(session::store::load_scratch(key.as_deref()));
            state.lsp.completion_provider = Some(Rc::new(SqlCompletion::new(slot, id)));
            state
        });
        // Persist scratch text to disk on every change.
        cx.subscribe(&editor, move |_this, emitter, event: &InputEvent, cx| {
            if let InputEvent::Change = event {
                clear_marks(&emitter, cx);
                session::store::save_scratch(key.as_deref(), &emitter.read(cx).value());
            }
        })
        .detach();
        let where_input = cx.new(|cx| InputState::new(window, cx));
        let order_input = cx.new(|cx| InputState::new(window, cx));
        let limit_input = cx.new(|cx| InputState::new(window, cx));
        let table = make_grid(weak, id, window, cx);
        Tab::base(
            id,
            TabKind::Scratch,
            "Scratch".into(),
            editor,
            where_input,
            order_input,
            limit_input,
            table,
            cx,
        )
    }

    /// A table-browse tab; the WHERE / ORDER BY / LIMIT inputs re-run the query
    /// on Enter.
    pub(crate) fn table(
        id: u64,
        schema: String,
        table_name: String,
        slot: LspSlot,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Self {
        let weak = cx.weak_entity();
        let editor = make_sql_editor("", slot.clone(), id, window, cx);
        // The bar labels each field with its keyword, so the placeholder only
        // shows what goes after it. Both complete the table's columns.
        let clause_input = |key: &str,
                            keyword: &str,
                            example: &str,
                            window: &mut Window,
                            cx: &mut Context<Workspace>| {
            let completion = SqlCompletion::clause(
                slot.clone(),
                &format!("tab-{id}-{key}"),
                lsp::clause_head(&schema, &table_name, keyword),
            );
            let example = example.to_string();
            cx.new(|cx| {
                let mut state = InputState::new(window, cx).placeholder(example);
                state.lsp.completion_provider = Some(Rc::new(completion));
                state
            })
        };
        let where_input = clause_input(
            "where",
            "WHERE",
            "e.g. id > 10 AND name ILIKE 'a%'",
            window,
            cx,
        );
        let order_input = clause_input("order", "ORDER BY", "e.g. created_at DESC", window, cx);
        let limit_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("none")
                .default_value(super::ROW_LIMIT.to_string())
        });
        for input in [&where_input, &order_input, &limit_input] {
            cx.subscribe(input, move |this, _i, event: &InputEvent, cx| match event {
                InputEvent::PressEnter { .. } => this.apply_where(id, cx),
                InputEvent::Change => {
                    if let Some(tab) = this.tab_mut(id) {
                        tab.clause_error = None;
                    }
                    this.mark_session_dirty(cx);
                }
                _ => {}
            })
            .detach();
        }
        let table = make_grid(weak, id, window, cx);
        let title = format!("{schema}.{table_name}");
        Tab::base(
            id,
            TabKind::Table {
                schema,
                table: table_name,
            },
            title,
            editor,
            where_input,
            order_input,
            limit_input,
            table,
            cx,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::test_support::*;
    use gpui::TestAppContext;

    /// Display flattening is DISPLAY-only: newlines become `⏎`, tabs a space,
    /// and only the drawn text is capped (the editor gets `tab.rows` in full).
    #[test]
    fn display_text_flattens_and_caps() {
        assert_eq!(display_text("a\r\nb\tc"), "a⏎b c");
        assert_eq!(display_text("plain"), "plain");
        let long = "x".repeat(CELL_MAX_CHARS + 10);
        let shown = display_text(&long);
        assert_eq!(shown.chars().count(), CELL_MAX_CHARS + 1);
        assert_eq!(shown.chars().last(), Some('…'));
        // Exactly at the cap: nothing is added.
        let exact = "y".repeat(CELL_MAX_CHARS);
        assert_eq!(display_text(&exact), exact);
    }

    /// The grid has one more column than the result (the row-number gutter),
    /// dragged widths survive a re-run of the same query, and a column never
    /// shrinks below its default width.
    #[gpui::test]
    fn gutter_column_and_resize_rules(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                let table = ws.tab(id).unwrap().table.clone();
                let headers = vec!["id".to_string(), "name".to_string()];
                table.update(cx, |ts, _cx| {
                    let d = ts.delegate_mut();
                    d.set(&headers, vec![vec![CellValue::Null, CellValue::Null]]);
                    assert_eq!(d.columns.len(), headers.len() + 1);
                    assert_eq!(d.columns[ROWNUM_COL].width, ROWNUM_W);
                    assert_eq!(d.columns[1].name.as_ref(), "id");

                    // Widen "name", then re-run the same query: the width sticks.
                    assert!(d.set_col_width(2, px(300.)));
                    d.set(&headers, vec![]);
                    assert_eq!(d.columns[2].width, px(300.));

                    // Below the default is clamped (and reported as no change).
                    assert!(!d.set_col_width(1, px(20.)));
                    assert_eq!(d.columns[1].width, COL_W);
                    assert!(!d.set_col_width(ROWNUM_COL, px(1.)));
                    assert_eq!(d.columns[ROWNUM_COL].width, ROWNUM_W);
                });
            })
            .unwrap();
    }
}
