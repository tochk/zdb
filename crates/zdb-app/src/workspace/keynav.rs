//! Keyboard navigation: the result grid's cell cursor, the schema tree, pane
//! focus (`ctrl-1/2/3`), tab-strip keys, and the bindings for all of them.
//!
//! Where a binding lives matters, because gpui prefers the DEEPEST matching key
//! context and, at one depth, the binding registered LAST:
//! - Arrows in the grid are bound in the widget's own `"Table"` context (after
//!   `gpui_component::init`, so ours win) — the widget binds them there itself,
//!   and anything bound on an outer context would lose to it.
//! - Every other grid key sits on `"Grid"`, a context the grid's wrapper div
//!   carries only while NO cell is being edited. Bare-letter (vim) bindings on
//!   `"Table"` would also match while the inline input is focused inside it and
//!   eat the typed character; a context that is simply absent during an edit
//!   can't.
//! - Keys the single-line inline input binds but doesn't handle (`tab`, `up`,
//!   `down`) fall through to the next match — that is how `tab` reaches
//!   `"GridEdit"` and `down` in the filter box reaches `"TreeFilter"`.
//! - Tab everywhere else is ours (`FocusNextField` on `"zdb"`), not
//!   gpui-component's `Root` binding: that one walks EVERY tab stop in the
//!   window, behind an open dialog too. Only the filter box, the tree, the
//!   editor / WHERE / ORDER BY and the grid are tab stops (the panes' buttons
//!   opt out), so Tab walks those fields in order; with a dialog up it stays in
//!   the dialog (`modal.rs`). Panes that use Tab themselves win by depth: grid
//!   cells, code-editor indent, and the terminal, where `NoAction` masks every
//!   outer binding so the key reaches the shell.

use super::*;
use gpui::{DismissEvent, KeyBinding, KeyDownEvent, NoAction, Pixels, Point};
use gpui_component::menu::PopupMenu;

actions!(
    zdb,
    [
        ToggleShortcuts,
        FocusTree,
        FocusEditor,
        FocusGrid,
        NextTab,
        PrevTab,
        CloseTab,
        ApplyEdits,
        GridUp,
        GridDown,
        GridLeft,
        GridRight,
        GridRowStart,
        GridRowEnd,
        GridFirst,
        GridLast,
        GridTop,
        GridBottom,
        GridPageUp,
        GridPageDown,
        GridHalfPageUp,
        GridHalfPageDown,
        GridNextCell,
        GridPrevCell,
        GridEdit,
        GridCopyCell,
        TreeUp,
        TreeDown,
        TreeLeft,
        TreeRight,
        TreeFirst,
        TreeLast,
        TreePageUp,
        TreePageDown,
        TreeEscape,
        TreeMenu,
        TreeFocusFilter,
        FilterToTree,
        FocusNextField,
        FocusPrevField,
        AcceptCompletion,
    ]
);

/// Rows a page key moves in the tree (its scroll handle is private, so the
/// real viewport height isn't available).
const TREE_PAGE: usize = 15;

/// Register the navigation bindings. Must run AFTER `gpui_component::init`:
/// the arrow keys override the widgets' own bindings in the same contexts.
pub fn bind_nav_keys(cx: &mut App) {
    const TABLE: Option<&str> = Some("Table");
    const GRID: Option<&str> = Some("Grid");
    const GRID_EDIT: Option<&str> = Some("GridEdit");
    const GRID_VIM: Option<&str> = Some("Grid && vim");
    const TREE: Option<&str> = Some("SchemaTree");
    const TREE_VIM: Option<&str> = Some("SchemaTree && vim");

    let mut keys = vec![
        // Panes, tabs, apply. Handled on the workspace root, which hands them
        // back to the terminal when that has focus (they are shell keys there).
        KeyBinding::new("f1", ToggleShortcuts, None),
        KeyBinding::new("ctrl-1", FocusTree, None),
        KeyBinding::new("cmd-1", FocusTree, None),
        KeyBinding::new("ctrl-2", FocusEditor, None),
        KeyBinding::new("cmd-2", FocusEditor, None),
        KeyBinding::new("ctrl-3", FocusGrid, None),
        KeyBinding::new("cmd-3", FocusGrid, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevTab, None),
        KeyBinding::new("ctrl-w", CloseTab, None),
        KeyBinding::new("cmd-w", CloseTab, None),
        KeyBinding::new("ctrl-s", ApplyEdits, None),
        KeyBinding::new("cmd-s", ApplyEdits, None),
        // Field to field (see the module docs).
        KeyBinding::new("tab", FocusNextField, Some("zdb")),
        // Tab takes the highlighted completion, as in Zed. Same context as the
        // input's own `tab` (indent), so registering later puts it first.
        KeyBinding::new("tab", AcceptCompletion, Some("Input")),
        KeyBinding::new("shift-tab", FocusPrevField, Some("zdb")),
        // The terminal's own keys: mask every outer binding so they reach the
        // shell (completion, vi's Esc) instead of moving focus / closing things.
        KeyBinding::new("tab", NoAction, Some("Terminal")),
        KeyBinding::new("shift-tab", NoAction, Some("Terminal")),
        KeyBinding::new("escape", NoAction, Some("Terminal")),
        // Grid: the keys the Table widget binds itself. Its `escape` would
        // clear the row highlight out from under the cursor.
        KeyBinding::new("up", GridUp, TABLE),
        KeyBinding::new("down", GridDown, TABLE),
        KeyBinding::new("left", GridLeft, TABLE),
        KeyBinding::new("right", GridRight, TABLE),
        KeyBinding::new("escape", ClosePalette, TABLE),
        // Grid: everything else, only while no cell is being edited.
        KeyBinding::new("home", GridRowStart, GRID),
        KeyBinding::new("end", GridRowEnd, GRID),
        KeyBinding::new("ctrl-home", GridFirst, GRID),
        KeyBinding::new("cmd-home", GridFirst, GRID),
        KeyBinding::new("ctrl-end", GridLast, GRID),
        KeyBinding::new("cmd-end", GridLast, GRID),
        KeyBinding::new("pageup", GridPageUp, GRID),
        KeyBinding::new("pagedown", GridPageDown, GRID),
        KeyBinding::new("tab", GridNextCell, GRID),
        KeyBinding::new("shift-tab", GridPrevCell, GRID),
        KeyBinding::new("enter", GridEdit, GRID),
        KeyBinding::new("f2", GridEdit, GRID),
        KeyBinding::new("ctrl-c", GridCopyCell, GRID),
        KeyBinding::new("cmd-c", GridCopyCell, GRID),
        // While the inline input is open: it declines `tab` (single-line), so
        // the key lands here and stages the edit before moving on.
        KeyBinding::new("tab", GridNextCell, GRID_EDIT),
        KeyBinding::new("shift-tab", GridPrevCell, GRID_EDIT),
        // Grid, vim navigation (settings toggle).
        KeyBinding::new("h", GridLeft, GRID_VIM),
        KeyBinding::new("j", GridDown, GRID_VIM),
        KeyBinding::new("k", GridUp, GRID_VIM),
        KeyBinding::new("l", GridRight, GRID_VIM),
        KeyBinding::new("g g", GridTop, GRID_VIM),
        KeyBinding::new("shift-g", GridBottom, GRID_VIM),
        KeyBinding::new("0", GridRowStart, GRID_VIM),
        KeyBinding::new("$", GridRowEnd, GRID_VIM),
        KeyBinding::new("shift-4", GridRowEnd, GRID_VIM),
        KeyBinding::new("ctrl-d", GridHalfPageDown, GRID_VIM),
        KeyBinding::new("ctrl-u", GridHalfPageUp, GRID_VIM),
        KeyBinding::new("i", GridEdit, GRID_VIM),
        KeyBinding::new("y", GridCopyCell, GRID_VIM),
        // Schema tree, extras that only our wrapper knows.
        KeyBinding::new("home", TreeFirst, TREE),
        KeyBinding::new("end", TreeLast, TREE),
        KeyBinding::new("pageup", TreePageUp, TREE),
        KeyBinding::new("pagedown", TreePageDown, TREE),
        KeyBinding::new("escape", TreeEscape, TREE),
        KeyBinding::new("shift-f10", TreeMenu, TREE),
        KeyBinding::new("menu", TreeMenu, TREE),
        KeyBinding::new("h", TreeLeft, TREE_VIM),
        KeyBinding::new("j", TreeDown, TREE_VIM),
        KeyBinding::new("k", TreeUp, TREE_VIM),
        KeyBinding::new("l", TreeRight, TREE_VIM),
        KeyBinding::new("g g", TreeFirst, TREE_VIM),
        KeyBinding::new("shift-g", TreeLast, TREE_VIM),
        KeyBinding::new("/", TreeFocusFilter, TREE_VIM),
        KeyBinding::new("down", FilterToTree, Some("TreeFilter")),
    ];
    // Arrows + Enter in the tree, twice: `"SchemaTree"` is our wrapper (focused
    // from the keyboard), `"Tree"` the widget inside it (focused by a click),
    // whose own arrow bindings would otherwise win there.
    for ctx in [TREE, Some("Tree")] {
        keys.extend([
            KeyBinding::new("up", TreeUp, ctx),
            KeyBinding::new("down", TreeDown, ctx),
            KeyBinding::new("left", TreeLeft, ctx),
            KeyBinding::new("right", TreeRight, ctx),
            KeyBinding::new("enter", TreeOpenSelected, ctx),
        ]);
    }
    cx.bind_keys(keys);
}

/// A titled group of `(what it does, keys)` rows in the shortcut reference.
pub(super) type ShortcutSection = (&'static str, &'static [(&'static str, &'static str)]);

/// The keyboard-shortcut reference (F1), by section. Display-only: keep it in
/// step with `bind_nav_keys` and the bindings in `main.rs` by hand. `Ctrl` is
/// `Cmd` on macOS.
pub(super) const SHORTCUTS: &[ShortcutSection] = &[
    (
        "GENERAL",
        &[
            ("Keyboard shortcuts", "F1"),
            ("Command palette", "Ctrl+Shift+P"),
            ("Settings", "Ctrl+,"),
            ("Connections", "Ctrl+Shift+O"),
            ("Terminal", "Ctrl+`"),
            ("Close dialog / cancel", "Esc"),
        ],
    ),
    (
        "PANES AND TABS",
        &[
            ("Focus schema tree", "Ctrl+1"),
            ("Focus editor / filter", "Ctrl+2"),
            ("Focus result grid", "Ctrl+3"),
            ("Next / previous field", "Tab / Shift+Tab"),
            ("Next / previous tab", "Ctrl+Tab / Ctrl+Shift+Tab"),
            ("Close tab", "Ctrl+W"),
            ("Scratch editor", "Ctrl+Shift+E"),
        ],
    ),
    (
        "QUERY",
        &[
            ("Run statement under cursor", "Ctrl+Enter"),
            ("Accept completion", "Enter / Tab"),
            ("Apply staged edits", "Ctrl+S"),
        ],
    ),
    (
        "RESULT GRID",
        &[
            ("Move cell", "Arrows"),
            ("Next / previous cell", "Tab / Shift+Tab"),
            ("Row start / end", "Home / End"),
            ("First / last cell", "Ctrl+Home / Ctrl+End"),
            ("Page up / down", "PgUp / PgDn"),
            ("Copy cell", "Ctrl+C"),
        ],
    ),
    (
        "CELL EDITING",
        &[
            ("Edit cell", "Enter / F2 / type"),
            ("Save and move down", "Enter"),
            ("Save and move sideways", "Tab / Shift+Tab"),
            ("Discard edit", "Esc"),
            ("Save multi-line value", "Ctrl+Enter"),
        ],
    ),
    (
        "SCHEMA TREE",
        &[
            ("Move", "Up / Down"),
            ("Collapse / expand", "Left / Right"),
            ("Top / bottom", "Home / End"),
            ("Page up / down", "PgUp / PgDn"),
            ("Open table", "Enter"),
            ("Context menu", "Shift+F10"),
            ("Filter box to tree", "Down"),
            ("Clear filter / to filter box", "Esc"),
        ],
    ),
    (
        "VIM NAVIGATION (IF ENABLED)",
        &[
            ("Move", "h j k l"),
            ("Top / bottom", "g g / G"),
            ("Row start / end (grid)", "0 / $"),
            ("Half page down / up (grid)", "Ctrl+D / Ctrl+U"),
            ("Edit cell", "i"),
            ("Copy cell", "y"),
            ("Tree filter", "/"),
        ],
    ),
];

// ---- cell-cursor math -----------------------------------------------------

/// One cursor movement in the grid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Motion {
    Up,
    Down,
    Left,
    Right,
    /// First / last column of the current row.
    RowStart,
    RowEnd,
    /// The grid's first / last cell.
    First,
    Last,
    /// First / last row, same column.
    Top,
    Bottom,
    PageUp(usize),
    PageDown(usize),
    /// Tab order: sideways, wrapping onto the next / previous row.
    Next,
    Prev,
}

/// Where `m` takes the cursor in a `rows` × `cols` grid. Everything stops at the
/// edges; only the Tab order wraps (and it too stops at the grid's corners).
pub(super) fn step((r, c): (usize, usize), rows: usize, cols: usize, m: Motion) -> (usize, usize) {
    let (last_r, last_c) = (rows.saturating_sub(1), cols.saturating_sub(1));
    let (r, c) = (r.min(last_r), c.min(last_c));
    match m {
        Motion::Up => (r.saturating_sub(1), c),
        Motion::Down => ((r + 1).min(last_r), c),
        Motion::Left => (r, c.saturating_sub(1)),
        Motion::Right => (r, (c + 1).min(last_c)),
        Motion::RowStart => (r, 0),
        Motion::RowEnd => (r, last_c),
        Motion::First => (0, 0),
        Motion::Last => (last_r, last_c),
        Motion::Top => (0, c),
        Motion::Bottom => (last_r, c),
        Motion::PageUp(n) => (r.saturating_sub(n), c),
        Motion::PageDown(n) => ((r + n).min(last_r), c),
        Motion::Next if c < last_c => (r, c + 1),
        Motion::Next if r < last_r => (r + 1, 0),
        Motion::Prev if c > 0 => (r, c - 1),
        Motion::Prev if r > 0 => (r - 1, last_c),
        Motion::Next | Motion::Prev => (r, c),
    }
}

/// Pull a cursor back inside a `rows` × `cols` result; an empty result has
/// nowhere to stand.
pub(super) fn clamp_cursor(
    cursor: Option<(usize, usize)>,
    rows: usize,
    cols: usize,
) -> Option<(usize, usize)> {
    let (r, c) = cursor?;
    (rows > 0 && cols > 0).then(|| (r.min(rows - 1), c.min(cols - 1)))
}

/// The horizontal scroll offset (0 = start, negative = scrolled right) that
/// brings the span `left..left + width` into a `viewport`-wide view, moving no
/// further than it has to. A span wider than the view shows its left edge.
pub(super) fn reveal_offset(
    offset: Pixels,
    left: Pixels,
    width: Pixels,
    viewport: Pixels,
) -> Pixels {
    if viewport <= px(0.) {
        // Not laid out yet; nothing to measure against.
        return offset;
    }
    if left + offset < px(0.) || width >= viewport {
        -left
    } else if left + width + offset > viewport {
        viewport - (left + width)
    } else {
        offset
    }
}

// ---- tree flattening ------------------------------------------------------

/// A visible tree row, in the widget's own order (see `flat_index_of`).
pub(super) struct FlatNode {
    pub(super) id: SharedString,
    pub(super) depth: usize,
    pub(super) folder: bool,
    pub(super) expanded: bool,
}

pub(super) fn flatten(items: &[TreeItem]) -> Vec<FlatNode> {
    fn walk(item: &TreeItem, depth: usize, out: &mut Vec<FlatNode>) {
        out.push(FlatNode {
            id: item.id.clone(),
            depth,
            folder: item.is_folder(),
            expanded: item.is_expanded(),
        });
        if item.is_expanded() {
            for child in &item.children {
                walk(child, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    for item in items {
        walk(item, 0, &mut out);
    }
    out
}

/// A selection movement in the tree.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum TreeMove {
    Up,
    Down,
    First,
    Last,
    PageUp,
    PageDown,
}

/// The row `m` selects in a tree of `len` visible rows. Up/Down wrap around
/// (as the widget's own arrows do); the page keys stop at the ends.
pub(super) fn tree_step(cur: Option<usize>, len: usize, m: TreeMove) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let last = len - 1;
    let Some(cur) = cur.map(|c| c.min(last)) else {
        return Some(if matches!(m, TreeMove::Up | TreeMove::Last) {
            last
        } else {
            0
        });
    };
    Some(match m {
        TreeMove::Up if cur == 0 => last,
        TreeMove::Up => cur - 1,
        TreeMove::Down if cur == last => 0,
        TreeMove::Down => cur + 1,
        TreeMove::First => 0,
        TreeMove::Last => last,
        TreeMove::PageUp => cur.saturating_sub(TREE_PAGE),
        TreeMove::PageDown => (cur + TREE_PAGE).min(last),
    })
}

/// Nearest row above `ix` that is one level shallower.
pub(super) fn parent_index(nodes: &[FlatNode], ix: usize) -> Option<usize> {
    let depth = nodes.get(ix)?.depth;
    (0..ix).rev().find(|&i| nodes[i].depth < depth)
}

impl Workspace {
    // ---- panes / tabs ------------------------------------------------------

    /// Whether the embedded terminal holds keyboard focus — the pane/tab chords
    /// are ordinary shell keys there and must be left to it.
    pub(super) fn terminal_focused(&self, window: &Window, cx: &App) -> bool {
        self.terminal_open
            && self
                .terminal
                .as_ref()
                .is_some_and(|t| t.view.read(cx).focus_handle().is_focused(window))
    }

    pub(super) fn toggle_shortcuts(&mut self, cx: &mut Context<Self>) {
        self.toggle_modal(Modal::Shortcuts, cx);
    }

    pub(super) fn focus_tree(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.conn.is_none() {
            return;
        }
        self.tree.focus.focus(window);
        if self.tree.state.read(cx).selected_index().is_none() {
            self.tree_move(TreeMove::First, cx);
        }
        cx.notify();
    }

    /// `ctrl-3`: focus the active tab's grid (nothing to stand on without rows).
    pub(super) fn focus_grid(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.rows.is_empty() || tab.headers.is_empty() || tab.plan.is_some() {
            return;
        }
        let (id, cursor, handle) = (tab.id, tab.cursor, tab.grid_focus.clone());
        if cursor.is_none() {
            self.set_cursor(id, 0, 0, cx);
        }
        handle.focus(window);
        cx.notify();
    }

    /// Give the grid focus on the next render — for the paths that end a cell
    /// edit without a `&mut Window` at hand (the input's own events). The input
    /// unmounts with the edit, and focus would otherwise fall off the grid.
    pub(super) fn flush_pending_focus(&mut self, window: &mut Window) {
        if !std::mem::take(&mut self.pending_grid_focus) {
            return;
        }
        if let Some(tab) = self.active_tab().filter(|t| t.plan.is_none()) {
            tab.grid_focus.focus(window);
        }
    }

    pub(super) fn cycle_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.tabs.len();
        let Some(active) = self.active.filter(|_| n > 1) else {
            return;
        };
        let next = if forward {
            (active + 1) % n
        } else {
            (active + n - 1) % n
        };
        self.activate_tab(next, window, cx);
    }

    pub(super) fn close_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active_id() else {
            return;
        };
        self.cancel_cell_edit(cx);
        self.close_tab(id, cx);
        // Land the keyboard in the tab that took its place.
        if let Some(idx) = self.active {
            self.activate_tab(idx, window, cx);
        }
    }

    /// `ctrl-s`: apply the active tab's staged batch (staging the cell being
    /// edited first, so it is part of it).
    pub(super) fn apply_active(&mut self, cx: &mut Context<Self>) {
        self.commit_cell_edit(cx);
        if let Some(id) = self.active_id() {
            self.apply_pending(id, cx);
        }
    }

    // ---- settings toggles --------------------------------------------------

    pub(super) fn set_esc_saves(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.esc_saves_edit = on;
        self.save_settings();
        cx.notify();
    }

    pub(super) fn set_vim_navigation(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.vim_navigation = on;
        self.save_settings();
        cx.notify();
    }

    pub(super) fn set_restore_tabs(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.restore_tabs = on;
        self.save_settings();
        cx.notify();
    }

    /// Tests run against the dev machine's real settings file — never write it.
    fn save_settings(&mut self) {
        #[cfg(not(test))]
        if let Err(e) = self.settings.save() {
            self.status = format!("Setting changed (disk write failed: {e})");
        }
    }

    // ---- grid --------------------------------------------------------------

    /// Rows a page key moves: what the grid currently shows, less one for context.
    fn grid_page(&self, cx: &App) -> usize {
        self.active_tab()
            .map(|t| t.table.read(cx).visible_range().rows().len())
            .unwrap_or(0)
            .saturating_sub(1)
            .max(1)
    }

    /// Move the active tab's cell cursor. A no-op mid-edit: the single-line cell
    /// input doesn't handle up/down, so those keys fall through to here and must
    /// not walk off the cell being edited.
    pub(super) fn grid_motion(&mut self, m: Motion, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        if tab.editing.is_some() {
            return;
        }
        self.move_cursor(m, cx);
    }

    fn move_cursor(&mut self, m: Motion, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let (id, rows, cols) = (tab.id, tab.rows.len(), tab.headers.len());
        if rows == 0 || cols == 0 {
            return;
        }
        let (r, c) = tab.cursor.map_or((0, 0), |cur| step(cur, rows, cols, m));
        self.set_cursor(id, r, c, cx);
    }

    pub(super) fn grid_page_motion(&mut self, down: bool, half: bool, cx: &mut Context<Self>) {
        let page = self.grid_page(cx);
        let n = if half { (page / 2).max(1) } else { page };
        self.grid_motion(
            if down {
                Motion::PageDown(n)
            } else {
                Motion::PageUp(n)
            },
            cx,
        );
    }

    /// Tab / Shift+Tab: step sideways. Unlike the arrows this also works from
    /// inside the inline editor, where it stages the edit first.
    pub(super) fn grid_tab(&mut self, forward: bool, cx: &mut Context<Self>) {
        let was_editing = self.active_tab().is_some_and(|t| t.editing.is_some());
        // `set_cursor` stages the edit in progress.
        self.move_cursor(if forward { Motion::Next } else { Motion::Prev }, cx);
        self.pending_grid_focus |= was_editing;
    }

    /// Tab with a completion menu open (SQL editor, WHERE, ORDER BY) takes the
    /// highlighted item, as Enter does; otherwise the key goes on to indent or
    /// to the next field.
    pub(super) fn accept_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let enter = gpui_component::input::Enter { secondary: false };
        if !self.completion_menu_action(Box::new(enter), window, cx) {
            cx.propagate();
        }
    }

    /// Offer `action` (Enter / Escape) to the completion menu of the focused
    /// SQL input, if one is open; true when the menu took it. Needed for Esc
    /// because a binding with NO context matches at the deepest level in gpui,
    /// so the global `escape` → `ClosePalette` outranks the input's own escape
    /// and the menu would never see it (it could not be closed at all).
    pub(super) fn completion_menu_action(
        &mut self,
        action: Box<dyn gpui::Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let focused = self.active_tab().and_then(|t| {
            [&t.editor, &t.where_input, &t.order_input]
                .into_iter()
                .find(|i| i.read(cx).focus_handle(cx).is_focused(window))
                .cloned()
        });
        let handled = focused.is_some_and(|input| {
            input.update(cx, |state, cx| {
                state.handle_action_for_context_menu(action, window, cx)
            })
        });
        // The menu marks the event to propagate even when it handles it; the
        // key must not go on to indent / change field / close something too.
        if handled {
            cx.stop_propagation();
        }
        handled
    }

    /// Enter / F2 / `i`: edit the cell under the cursor.
    pub(super) fn grid_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((id, (row, col))) = self.active_tab().and_then(|t| Some((t.id, t.cursor?))) else {
            return;
        };
        self.begin_edit(id, row, col, window, cx);
    }

    /// Copy the cursor cell's raw value (never the truncated display text).
    pub(super) fn grid_copy_cell(&mut self, cx: &mut Context<Self>) {
        let Some(text) = self.active_tab().and_then(|t| {
            let (row, col) = t.cursor?;
            Some(match t.rows.get(row)?.get(col)? {
                CellValue::Text(s) => s.clone(),
                CellValue::Null => String::new(),
            })
        }) else {
            return;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        self.status = "Cell copied".into();
        cx.notify();
    }

    /// Type-to-edit: a printable key on the cursor cell opens the editor with
    /// that character replacing the value. Off under vim navigation, where bare
    /// letters are commands.
    pub(super) fn grid_key_down(
        &mut self,
        tab_id: u64,
        ev: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings.vim_navigation {
            return;
        }
        let m = ev.keystroke.modifiers;
        if m.control || m.alt || m.platform || m.function {
            return;
        }
        let Some(typed) = ev
            .keystroke
            .key_char
            .clone()
            .filter(|s| !s.is_empty() && !s.chars().any(char::is_control))
        else {
            return;
        };
        let Some((row, col)) = self.tab(tab_id).and_then(|t| {
            (t.editing.is_none() && t.grid_focus.is_focused(window))
                .then_some(t.cursor)
                .flatten()
        }) else {
            return;
        };
        self.begin_edit_with(tab_id, row, col, Some(typed), window, cx);
        cx.stop_propagation();
    }

    /// Enter in the inline input: stage the edit and step down a row.
    pub(super) fn commit_and_advance(&mut self, cx: &mut Context<Self>) {
        if self.active_tab().is_none_or(|t| t.editing.is_none()) {
            return;
        }
        self.commit_cell_edit(cx);
        self.move_cursor(Motion::Down, cx);
        self.pending_grid_focus = true;
    }

    /// Esc on a cell edit: discard it — or stage it, when the user asked for
    /// that in settings. Either way the keyboard goes back to the grid.
    pub(super) fn escape_cell_edit(&mut self, cx: &mut Context<Self>) {
        if self.active_tab().is_none_or(|t| t.editing.is_none()) {
            return;
        }
        if self.settings.esc_saves_edit {
            self.commit_cell_edit(cx);
        } else {
            self.cancel_cell_edit(cx);
        }
        self.pending_grid_focus = true;
    }

    // ---- schema tree -------------------------------------------------------

    fn tree_select(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.tree.state.update(cx, |ts, cx| {
            // Same strategies as the widget's own arrows: reveal at the edge the
            // selection is travelling toward.
            let strategy = if ts.selected_index().is_some_and(|old| ix < old) {
                ScrollStrategy::Top
            } else {
                ScrollStrategy::Bottom
            };
            ts.set_selected_index(Some(ix), cx);
            ts.scroll_to_item(ix, strategy);
        });
    }

    pub(super) fn tree_move(&mut self, m: TreeMove, cx: &mut Context<Self>) {
        let len = flatten(&self.tree.items).len();
        let cur = self.tree.state.read(cx).selected_index();
        if let Some(ix) = tree_step(cur, len, m) {
            self.tree_select(ix, cx);
        }
    }

    /// Left: collapse the selected node, or step out to its parent. Right:
    /// expand it, or step into its first child.
    pub(super) fn tree_expand(&mut self, expand: bool, cx: &mut Context<Self>) {
        let nodes = flatten(&self.tree.items);
        let Some(ix) = self.tree.state.read(cx).selected_index() else {
            return;
        };
        let Some(node) = nodes.get(ix) else {
            return;
        };
        if node.folder && node.expanded != expand {
            // While filtering, db/schema expansion is forced by the filter and
            // isn't ours to record (see `sync_expansion`).
            let filtering = !self.tree.filter.trim().is_empty();
            let id = node.id.clone();
            if filtering && (id.as_ref() == "db" || id.as_ref().starts_with(tree::SCH_PREFIX)) {
                return;
            }
            if expand {
                self.tree.expanded.insert(id.clone());
            } else {
                self.tree.expanded.remove(&id);
            }
            // `sync_tree` restores the selection from this across its rebuild.
            self.tree.sel = Some(id);
            self.sync_tree(cx);
        } else if expand {
            if node.folder && ix + 1 < nodes.len() {
                self.tree_select(ix + 1, cx);
            }
        } else if let Some(parent) = parent_index(&nodes, ix) {
            self.tree_select(parent, cx);
        }
    }

    /// Esc in the tree: drop the filter if there is one, else step back to the
    /// filter box.
    pub(super) fn tree_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tree.filter.is_empty() {
            self.focus_tree_filter(window, cx);
            return;
        }
        self.tree.filter.clear();
        self.tree
            .filter_input
            .update(cx, |i, cx| i.set_value("", window, cx));
        self.sync_tree(cx);
    }

    pub(super) fn focus_tree_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.conn.is_none() || window.root::<gpui_component::Root>().flatten().is_none() {
            return;
        }
        self.tree
            .filter_input
            .update(cx, |i, cx| i.focus(window, cx));
    }

    /// Shift+F10 / Menu key: the selected node's context menu, anchored under
    /// its row.
    pub(super) fn open_tree_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(meta) = self
            .tree
            .state
            .read(cx)
            .selected_entry()
            .and_then(|e| self.tree.meta.get(&e.item().id).cloned())
            .filter(view::has_node_menu)
        else {
            return;
        };
        let Some(bounds) = self.tree.sel_bounds.get() else {
            return;
        };
        let at: Point<Pixels> = gpui::point(bounds.left() + px(24.), bounds.bottom());
        let weak = cx.weak_entity();
        let focus = self.tree.focus.clone();
        let menu = PopupMenu::build(window, cx, |menu, _, _| {
            // Dismissing hands focus back to the tree.
            view::node_menu(menu.action_context(focus), &meta, &weak)
        });
        cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.tree.menu = None;
            cx.notify();
        })
        .detach();
        menu.focus_handle(cx).focus(window);
        self.tree.menu = Some((menu, at));
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::test_support::*;
    use gpui::TestAppContext;

    #[test]
    fn arrows_stop_at_the_edges() {
        // 3 rows × 4 cols.
        assert_eq!(step((0, 0), 3, 4, Motion::Up), (0, 0));
        assert_eq!(step((0, 0), 3, 4, Motion::Left), (0, 0));
        assert_eq!(step((2, 3), 3, 4, Motion::Down), (2, 3));
        assert_eq!(step((2, 3), 3, 4, Motion::Right), (2, 3));
        assert_eq!(step((1, 1), 3, 4, Motion::Down), (2, 1));
        assert_eq!(step((1, 1), 3, 4, Motion::Right), (1, 2));
    }

    #[test]
    fn home_end_and_corners() {
        assert_eq!(step((1, 2), 3, 4, Motion::RowStart), (1, 0));
        assert_eq!(step((1, 2), 3, 4, Motion::RowEnd), (1, 3));
        assert_eq!(step((1, 2), 3, 4, Motion::First), (0, 0));
        assert_eq!(step((1, 2), 3, 4, Motion::Last), (2, 3));
        // vim gg / G keep the column.
        assert_eq!(step((1, 2), 3, 4, Motion::Top), (0, 2));
        assert_eq!(step((1, 2), 3, 4, Motion::Bottom), (2, 2));
    }

    #[test]
    fn pages_clamp() {
        assert_eq!(step((5, 1), 100, 4, Motion::PageDown(20)), (25, 1));
        assert_eq!(step((95, 1), 100, 4, Motion::PageDown(20)), (99, 1));
        assert_eq!(step((5, 1), 100, 4, Motion::PageUp(20)), (0, 1));
    }

    #[test]
    fn tab_order_wraps_rows_but_not_the_grid() {
        assert_eq!(step((0, 2), 3, 4, Motion::Next), (0, 3));
        assert_eq!(step((0, 3), 3, 4, Motion::Next), (1, 0));
        assert_eq!(step((2, 3), 3, 4, Motion::Next), (2, 3));
        assert_eq!(step((1, 0), 3, 4, Motion::Prev), (0, 3));
        assert_eq!(step((0, 0), 3, 4, Motion::Prev), (0, 0));
    }

    #[test]
    fn cursor_clamps_to_the_result() {
        assert_eq!(clamp_cursor(None, 5, 5), None);
        assert_eq!(clamp_cursor(Some((9, 9)), 3, 4), Some((2, 3)));
        assert_eq!(clamp_cursor(Some((1, 1)), 3, 4), Some((1, 1)));
        assert_eq!(clamp_cursor(Some((1, 1)), 0, 4), None);
        assert_eq!(clamp_cursor(Some((1, 1)), 3, 0), None);
        // A stale cursor is pulled in before it moves.
        assert_eq!(step((9, 9), 3, 4, Motion::Left), (2, 2));
    }

    #[test]
    fn reveal_scrolls_only_as_far_as_needed() {
        let p = |v: f32| px(v);
        // Already in view: untouched.
        assert_eq!(reveal_offset(p(-100.), p(180.), p(180.), p(500.)), p(-100.));
        // Off the left edge: its left edge comes to the view's left.
        assert_eq!(reveal_offset(p(-400.), p(180.), p(180.), p(500.)), p(-180.));
        // Off the right edge: its right edge comes to the view's right.
        assert_eq!(reveal_offset(p(0.), p(900.), p(180.), p(500.)), p(-580.));
        // Wider than the view: show where it starts.
        assert_eq!(reveal_offset(p(0.), p(900.), p(800.), p(500.)), p(-900.));
        // Before the first layout there is no viewport to measure against.
        assert_eq!(reveal_offset(p(-40.), p(900.), p(180.), p(0.)), p(-40.));
    }

    #[test]
    fn tree_steps() {
        assert_eq!(tree_step(None, 0, TreeMove::Down), None);
        assert_eq!(tree_step(None, 5, TreeMove::Down), Some(0));
        assert_eq!(tree_step(None, 5, TreeMove::Up), Some(4));
        assert_eq!(tree_step(Some(4), 5, TreeMove::Down), Some(0));
        assert_eq!(tree_step(Some(0), 5, TreeMove::Up), Some(4));
        assert_eq!(tree_step(Some(2), 5, TreeMove::First), Some(0));
        assert_eq!(tree_step(Some(2), 5, TreeMove::Last), Some(4));
        assert_eq!(
            tree_step(Some(2), 50, TreeMove::PageDown),
            Some(2 + TREE_PAGE)
        );
        assert_eq!(tree_step(Some(2), 5, TreeMove::PageDown), Some(4));
        assert_eq!(tree_step(Some(2), 5, TreeMove::PageUp), Some(0));
    }

    #[test]
    fn flatten_follows_expansion_and_finds_parents() {
        let items = vec![TreeItem::new("db", "db").expanded(true).children([
            TreeItem::new("a", "a")
                .expanded(true)
                .children([TreeItem::new("a1", "a1"), TreeItem::new("a2", "a2")]),
            TreeItem::new("b", "b").children([TreeItem::new("b1", "b1")]),
        ])];
        let nodes = flatten(&items);
        let ids: Vec<&str> = nodes.iter().map(|n| n.id.as_ref()).collect();
        // `b` is collapsed, so its child isn't a visible row.
        assert_eq!(ids, ["db", "a", "a1", "a2", "b"]);
        assert_eq!(parent_index(&nodes, 3), Some(1));
        assert_eq!(parent_index(&nodes, 4), Some(0));
        assert_eq!(parent_index(&nodes, 0), None);
        assert!(nodes[4].folder && !nodes[4].expanded);
    }

    /// A 3×2 editable result (`r{row}c{col}` cells) on a fresh Query tab.
    fn seed_grid(ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) -> u64 {
        let id = seed_query_tab(ws, window, cx);
        editable_tab(ws, id, vec!["id", "name"]);
        seed_rows(ws, id, vec!["id", "name"], 3, cx);
        id
    }

    #[gpui::test]
    fn cursor_drives_row_selection(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_grid(ws, window, cx);
                ws.set_cursor(id, 1, 1, cx);
                let tab = ws.tab(id).unwrap();
                assert_eq!(tab.cursor, Some((1, 1)));
                assert_eq!(tab.current_row, Some(1));
                assert_eq!(tab.table.read(cx).selected_row(), Some(1));

                ws.grid_motion(Motion::Down, cx);
                ws.grid_motion(Motion::Down, cx); // stops at the last row
                ws.grid_motion(Motion::Left, cx);
                let tab = ws.tab(id).unwrap();
                assert_eq!(tab.cursor, Some((2, 0)));
                assert_eq!(tab.table.read(cx).selected_row(), Some(2));

                // Out-of-range requests are clamped, never stored.
                ws.set_cursor(id, 99, 99, cx);
                assert_eq!(ws.tab(id).unwrap().cursor, Some((2, 1)));
            })
            .unwrap();
    }

    #[gpui::test]
    fn enter_stages_and_steps_down(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_grid(ws, window, cx);
                ws.set_cursor(id, 0, 1, cx);
                ws.grid_edit(window, cx);
                assert_eq!(ws.tab(id).unwrap().editing, Some((0, 1)));
                ws.cell_input
                    .update(cx, |i, cx| i.set_value("changed", window, cx));

                ws.commit_and_advance(cx);
                let tab = ws.tab(id).unwrap();
                assert_eq!(tab.editing, None);
                assert_eq!(tab.pending.len(), 1);
                assert_eq!(tab.cursor, Some((1, 1)));
                assert!(ws.pending_grid_focus);
            })
            .unwrap();
    }

    #[gpui::test]
    fn tab_stages_and_steps_sideways(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_grid(ws, window, cx);
                ws.begin_edit(id, 0, 1, window, cx);
                ws.cell_input
                    .update(cx, |i, cx| i.set_value("changed", window, cx));
                // Last column: Tab wraps onto the next row.
                ws.grid_tab(true, cx);
                let tab = ws.tab(id).unwrap();
                assert_eq!((tab.editing, tab.pending.len()), (None, 1));
                assert_eq!(tab.cursor, Some((1, 0)));

                // Arrows that fall out of the inline input must not move the cell.
                ws.begin_edit(id, 1, 1, window, cx);
                ws.grid_motion(Motion::Down, cx);
                assert_eq!(ws.tab(id).unwrap().editing, Some((1, 1)));
                ws.cancel_cell_edit(cx);
            })
            .unwrap();
    }

    #[gpui::test]
    fn escape_discards_unless_the_setting_says_save(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_grid(ws, window, cx);
                ws.begin_edit(id, 0, 1, window, cx);
                ws.cell_input
                    .update(cx, |i, cx| i.set_value("changed", window, cx));
                ws.escape_cell_edit(cx);
                let tab = ws.tab(id).unwrap();
                assert_eq!((tab.editing, tab.pending.len()), (None, 0));
                // The cursor stays on the cell that was being edited.
                assert_eq!(tab.cursor, Some((0, 1)));

                ws.set_esc_saves(true, cx);
                ws.begin_edit(id, 0, 1, window, cx);
                ws.cell_input
                    .update(cx, |i, cx| i.set_value("changed", window, cx));
                ws.escape_cell_edit(cx);
                let tab = ws.tab(id).unwrap();
                assert_eq!((tab.editing, tab.pending.len()), (None, 1));
                assert_eq!(tab.cursor, Some((0, 1)));
            })
            .unwrap();
    }

    #[gpui::test]
    fn typing_replaces_the_value(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_grid(ws, window, cx);
                ws.begin_edit_with(id, 2, 1, Some("x".into()), window, cx);
                assert_eq!(ws.tab(id).unwrap().editing, Some((2, 1)));
                assert_eq!(ws.cell_input.read(cx).value().as_ref(), "x");
                // Plain begin_edit loads the stored value instead.
                ws.cancel_cell_edit(cx);
                ws.begin_edit(id, 2, 1, window, cx);
                assert_eq!(ws.cell_input.read(cx).value().as_ref(), "r2c1");
                // (Headless windows have no `Root` to paint a live input into.)
                ws.cancel_cell_edit(cx);
            })
            .unwrap();
    }

    #[gpui::test]
    fn copy_cell_puts_the_raw_value_on_the_clipboard(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_grid(ws, window, cx);
                ws.set_cursor(id, 1, 1, cx);
                ws.grid_copy_cell(cx);
                let got = cx.read_from_clipboard().and_then(|c| c.text());
                assert_eq!(got.as_deref(), Some("r1c1"));
            })
            .unwrap();
    }

    #[gpui::test]
    fn tabs_cycle_and_close(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.open_query_tab(window, cx);
                ws.open_query_tab(window, cx);
                ws.open_query_tab(window, cx);
                assert_eq!(ws.active, Some(2));
                ws.cycle_tab(true, window, cx);
                assert_eq!(ws.active, Some(0)); // wraps
                ws.cycle_tab(false, window, cx);
                assert_eq!(ws.active, Some(2));
                ws.close_active_tab(window, cx);
                assert_eq!((ws.tabs.len(), ws.active), (2, Some(1)));
            })
            .unwrap();
    }

    /// Switching tabs mid-edit keeps the edit (only Esc discards one).
    #[gpui::test]
    fn switching_tabs_stages_the_edit(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_grid(ws, window, cx);
                ws.begin_edit(id, 0, 1, window, cx);
                ws.cell_input
                    .update(cx, |i, cx| i.set_value("changed", window, cx));
                ws.open_query_tab(window, cx);
                let tab = ws.tab(id).unwrap();
                assert_eq!((tab.editing, tab.pending.len()), (None, 1));
            })
            .unwrap();
    }
}
