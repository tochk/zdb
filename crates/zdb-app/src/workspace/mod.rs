//! The main application view: schema sidebar, center (SQL editor + results
//! table), and a bottom area (query log + pending-edit review).
//!
//! Editing works for any query whose result maps to a single table with a known
//! primary key (resolved via `DbHandle::describe`). Cells are edited inline in
//! the grid; an edit is staged and its SQL shown before you Apply it. Clicking a
//! column header re-runs the query ordered by that column. Every executed query
//! is recorded in the query log.

use crate::lsp::{self, LspSlot};
use gpui::ClipboardItem;
use gpui::{
    actions, div, prelude::FluentBuilder as _, px, rgba, App, AppContext, ClickEvent, Context,
    Entity, Focusable, FontWeight, Hsla, InteractiveElement, IntoElement, ParentElement, Render,
    ScrollStrategy, SharedString, StatefulInteractiveElement, Styled, Window, WindowBounds,
    WindowControlArea,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputEvent, InputState},
    resizable::{h_resizable, resizable_panel, v_resizable},
    table::{ColumnSort, Table},
    tree::{TreeItem, TreeState},
    v_flex, Disableable, Icon, IconName, Sizable,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use zdb_config::{ConnectionEntry, Settings};
use zdb_db::{
    CellValue, ConnId, ConnectionConfig, DbHandle, EditTarget, ExportFormat, QueryEvent,
    RelationDetail, RelationKind, RowEdit, SchemaObjects,
};

mod autoscroll;
mod colors;
mod conn;
mod edit;
mod export;
mod grid;
mod keynav;
mod modal;
mod query;
mod session;
mod tabs;
mod tree;
mod util;
mod view;

use autoscroll::AutoScroll;
use colors::{palette, Colors};
use conn::ConnForm;
use grid::{Clause, Tab, TabKind};
pub use keynav::bind_nav_keys;
use keynav::*;
use modal::{Modal, ModalFocus};
use query::QueryLog;
use tree::{NodeMeta, SchemaTree};
use util::{entry_to_config, oneline, order_by_sql, rel_icon, window_title};

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
        ToggleSettings,
        TreeOpenSelected
    ]
);

const ROW_LIMIT: usize = 500;

/// One command-palette entry.
#[derive(Clone, Copy)]
enum PaletteCmd {
    Run,
    Explain,
    ExplainAnalyze,
    Cancel,
    Refresh,
    Terminal,
    Connections,
    Settings,
    Shortcuts,
    FocusTree,
    FocusEditor,
    FocusGrid,
    NextTab,
    PrevTab,
    CloseTab,
    ApplyEdits,
    ExportCsv,
    ExportJson,
    ExportInserts,
    CopyTsv,
    Format,
}

const PALETTE_COMMANDS: &[(&str, PaletteCmd)] = &[
    ("Run query", PaletteCmd::Run),
    ("Explain query", PaletteCmd::Explain),
    ("Explain analyze (runs query)", PaletteCmd::ExplainAnalyze),
    ("Format SQL", PaletteCmd::Format),
    ("Export result as CSV", PaletteCmd::ExportCsv),
    ("Export result as JSON", PaletteCmd::ExportJson),
    ("Export result as SQL INSERTs", PaletteCmd::ExportInserts),
    ("Copy result (TSV) to clipboard", PaletteCmd::CopyTsv),
    ("Cancel query", PaletteCmd::Cancel),
    ("Refresh schemas", PaletteCmd::Refresh),
    ("Manage connections", PaletteCmd::Connections),
    ("Open settings", PaletteCmd::Settings),
    ("Keyboard shortcuts", PaletteCmd::Shortcuts),
    ("Toggle terminal", PaletteCmd::Terminal),
    ("Focus schema tree", PaletteCmd::FocusTree),
    ("Focus editor / filter", PaletteCmd::FocusEditor),
    ("Focus result grid", PaletteCmd::FocusGrid),
    ("Next tab", PaletteCmd::NextTab),
    ("Previous tab", PaletteCmd::PrevTab),
    ("Close tab", PaletteCmd::CloseTab),
    ("Apply staged edits", PaletteCmd::ApplyEdits),
];

/// Lightweight milestone logging to stderr, enabled by setting `ZDB_LOG`.
fn log(msg: impl AsRef<str>) {
    if std::env::var_os("ZDB_LOG").is_some() {
        eprintln!("[zdb] {}", msg.as_ref());
    }
}

// ---- workspace ----------------------------------------------------------

pub struct Workspace {
    db: DbHandle,
    cfg: Option<ConnectionConfig>,
    conn: Option<ConnId>,
    status: String,

    /// Sidebar schema tree (model + widget state; see `tree.rs`).
    tree: SchemaTree,

    /// Open center tabs and the index of the active one (None = welcome pane).
    tabs: Vec<Tab>,
    active: Option<usize>,
    /// Monotonic id source for tabs + the "Query N" counter.
    next_tab_id: u64,
    /// A table to open on the next render (set from windowless async contexts,
    /// e.g. the selftest, where no `&mut Window` is available).
    pending_open: Option<(String, String)>,

    /// What the current connection's session files are keyed by: the saved
    /// connection's id, or `session::ENV_CONN_KEY`. `None` = nothing persisted.
    conn_key: Option<String>,
    /// The connection's saved tabs have been restored, so the open tabs are now
    /// its set and may be written back (see `session.rs`).
    session_ready: bool,
    /// Connected: restore the saved tabs on the next render (needs the window).
    pending_restore: bool,
    /// Whether a debounced session write is already scheduled.
    session_flush: bool,
    /// The tab set last read from / written to disk, to skip identical writes.
    last_saved: Option<zdb_config::SavedTabs>,
    /// Last title pushed to the OS window (taskbar / alt-tab), so `render` only
    /// calls into the platform when it actually changes.
    window_title: String,

    /// Shared inline-cell editor (only the active tab edits a cell at a time).
    cell_input: Entity<InputState>,
    /// Full-value editor for cells whose text spans multiple lines.
    cell_multi: Entity<InputState>,

    /// Running `sqls` LSP handle (schema-aware completion), filled on connect.
    /// Shared with every SQL editor's completion provider.
    lsp_slot: LspSlot,

    log: QueryLog,

    /// The dialog showing, if any (see `modal.rs`).
    modal: Option<Modal>,
    modal_focus: ModalFocus,
    palette_input: Entity<InputState>,

    // Embedded terminal.
    terminal: Option<crate::terminal::Terminal>,
    terminal_open: bool,

    // Connections.
    settings: Settings,
    passwords: HashMap<String, String>,
    /// Whether the connection manager is showing the add form.
    conn_adding: bool,
    form: ConnForm,

    /// Latest window geometry seen in `render`, and whether a debounced write of
    /// it is already scheduled (resizing renders every frame — one write wins).
    pending_geom: Option<zdb_config::WindowState>,
    geom_flush: bool,

    /// Middle-click autoscroll, while it is running (see `autoscroll.rs`).
    autoscroll: Option<AutoScroll>,

    /// Focus the active grid on the next render (see `flush_pending_focus`).
    pending_grid_focus: bool,
    /// Fallback focus for the workspace root, taken whenever nothing else holds
    /// focus (no tab open, the focused input just unmounted). gpui dispatches
    /// keys along the focus path, so with nothing focused no binding would
    /// reach our handlers and the keyboard would be dead.
    root_focus: gpui::FocusHandle,
}

/// The window's geometry as stored in settings. A maximized window reports its
/// *restore* bounds, so unmaximizing after a restart lands on the size the user
/// last dragged.
fn geom_state(bounds: WindowBounds, is_maximized: bool) -> zdb_config::WindowState {
    let b = bounds.get_bounds();
    zdb_config::WindowState {
        x: f32::from(b.origin.x),
        y: f32::from(b.origin.y),
        width: f32::from(b.size.width),
        height: f32::from(b.size.height),
        maximized: matches!(bounds, WindowBounds::Maximized(_)) || is_maximized,
    }
}

fn window_geom(window: &Window) -> zdb_config::WindowState {
    geom_state(window.window_bounds(), window.is_maximized())
}

/// Persist the window's current geometry (best effort). Called on the close
/// paths; `track_window_geometry` covers everything else.
pub(super) fn save_window_geometry(window: &Window) {
    save_geom(window_geom(window));
}

/// The one place geometry reaches the disk. Tests run with real settings on the
/// dev machine, so they must never write.
fn save_geom(state: zdb_config::WindowState) {
    #[cfg(not(test))]
    zdb_config::save_window_state(state);
    #[cfg(test)]
    let _ = state;
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
        // A value with newlines CANNOT go into the single-line `cell_input`: gpui's
        // text system panics ("text argument should not contain newlines") when it
        // shapes such a line. Multi-line values are edited in full in this second
        // input, shown as a strip under the grid (`render_value_editor`).
        let cell_multi = cx.new(|cx| InputState::new(window, cx).multi_line(true));

        // Commit inline edits on Enter *and* on focus loss — clicking away from a
        // cell stages the edit exactly like pressing Enter (Esc is the only way to
        // discard, handled by the `ClosePalette`/escape action, which clears
        // `editing` first so the resulting blur commits nothing). Enter also
        // steps the cursor down a row and keeps the keyboard in the grid.
        cx.subscribe(
            &cell_input,
            |this, _input, event: &InputEvent, cx| match event {
                InputEvent::PressEnter { .. } => this.commit_and_advance(cx),
                InputEvent::Blur => this.commit_cell_edit(cx),
                _ => {}
            },
        )
        .detach();

        // The multi-line editor keeps Enter for newlines, so it commits only on
        // Ctrl/Cmd+Enter (or the Save button / moving to another cell). It must NOT
        // commit on blur either: clicking its own Cancel button blurs it first.
        cx.subscribe(&cell_multi, |this, _input, event: &InputEvent, cx| {
            if let InputEvent::PressEnter { secondary: true } = event {
                this.pending_grid_focus |= this.active_tab().is_some_and(|t| t.editing.is_some());
                this.commit_cell_edit(cx);
            }
        })
        .detach();

        let form = ConnForm::new(window, cx);
        let tree = SchemaTree::new(window, cx);

        // Remember the window geometry so the next launch reopens the same size
        // and position (otherwise gpui picks its default bounds every start).
        // Fires on the OS close path (Windows close button / Alt+F4); the Linux
        // title-bar button calls `save_window_geometry` itself before removing
        // the window.
        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            save_window_geometry(window);
            let _ = weak.update(cx, |this, cx| this.flush_session(cx));
            true
        });

        let mut this = Self {
            db,
            cfg: auto.clone(),
            conn: None,
            status: "Not connected".into(),
            tree,
            tabs: Vec::new(),
            active: None,
            next_tab_id: 1,
            pending_open: None,
            conn_key: auto.as_ref().map(|_| session::ENV_CONN_KEY.to_string()),
            session_ready: false,
            pending_restore: false,
            session_flush: false,
            last_saved: None,
            window_title: "zdb".into(),
            cell_input,
            cell_multi,
            lsp_slot: lsp::new_slot(),
            log: QueryLog::default(),
            modal: None,
            modal_focus: ModalFocus::new(cx),
            palette_input,
            terminal: None,
            terminal_open: false,
            settings,
            passwords: HashMap::new(),
            conn_adding: false,
            form,
            pending_geom: None,
            geom_flush: false,
            autoscroll: None,
            pending_grid_focus: false,
            root_focus: cx.focus_handle(),
        };
        if let Some(cfg) = auto {
            if let Some(pw) = &cfg.password {
                this.passwords.insert(cfg.name.clone(), pw.clone());
            }
            this.start_connect(cfg, cx);
        } else {
            this.modal = Some(Modal::Connections);
            this.conn_adding = this.settings.connections.is_empty();
        }
        this
    }

    /// Note the window's geometry (called from `render`, which runs on every
    /// resize) and schedule one write half a second after it stops changing —
    /// so a drag doesn't hammer the settings file, and a geometry change is
    /// persisted even if the app never reaches its close path.
    pub(super) fn track_window_geometry(&mut self, window: &Window, cx: &mut Context<Self>) {
        let state = window_geom(window);
        if self.pending_geom == Some(state) {
            return;
        }
        self.pending_geom = Some(state);
        if self.geom_flush {
            return;
        }
        self.geom_flush = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
            let _ = this.update(cx, |this, _cx| {
                this.geom_flush = false;
                if let Some(state) = this.pending_geom {
                    save_geom(state);
                }
            });
        })
        .detach();
    }

    /// Run `fut` to completion, then apply its output to the workspace and
    /// notify. The single home for the async-result plumbing every DB call
    /// needs; the tab/conn may be gone by the time `apply` runs, so it must
    /// re-look-up whatever it touches.
    fn spawn_db<T: 'static>(
        &self,
        cx: &mut Context<Self>,
        fut: impl std::future::Future<Output = T> + 'static,
        apply: impl FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    ) {
        cx.spawn(async move |this, cx| {
            let out = fut.await;
            this.update(cx, |this, cx| {
                apply(this, out, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---- selftest / palette / terminal -----------------------------------

    fn selftest(&mut self, cx: &mut Context<Self>) {
        let Some(conn) = self.conn else { return };
        let db = self.db.clone();
        let fut = async move {
            for sql in [
                "DROP TABLE IF EXISTS public.zdb_selftest",
                "CREATE TABLE public.zdb_selftest (id int primary key, name text)",
                "INSERT INTO public.zdb_selftest VALUES (1,'alpha'),(2,NULL)",
            ] {
                let mut rx = db.query(conn, sql);
                while rx.recv().await.is_some() {}
            }
        };
        self.spawn_db(cx, fut, |this, (), _cx| {
            // Opening a tab needs the window; defer to the next render (which
            // has it) via `pending_open`.
            this.pending_open = Some(("public".into(), "zdb_selftest".into()));
            log("selftest: table tab requested");
        });
    }

    fn toggle_palette(&mut self, cx: &mut Context<Self>) {
        self.toggle_modal(Modal::Palette, cx);
    }

    fn run_command(&mut self, cmd: PaletteCmd, window: &mut Window, cx: &mut Context<Self>) {
        self.modal = None;
        match cmd {
            PaletteCmd::Run => self.run_active_tab(cx),
            PaletteCmd::Explain => self.explain_active(false, cx),
            PaletteCmd::ExplainAnalyze => self.explain_active(true, cx),
            PaletteCmd::Format => self.format_active(window, cx),
            PaletteCmd::ExportCsv => self.export_active(ExportFormat::Csv, cx),
            PaletteCmd::ExportJson => self.export_active(ExportFormat::Json, cx),
            PaletteCmd::ExportInserts => self.export_active(ExportFormat::Inserts, cx),
            PaletteCmd::CopyTsv => self.copy_active_tsv(cx),
            PaletteCmd::Cancel => self.cancel(cx),
            PaletteCmd::Refresh => self.connect_or_refresh(cx),
            PaletteCmd::Terminal => self.toggle_terminal(window, cx),
            PaletteCmd::Connections => self.toggle_connections(cx),
            PaletteCmd::Settings => self.toggle_settings(cx),
            PaletteCmd::Shortcuts => self.toggle_shortcuts(cx),
            PaletteCmd::FocusTree => self.focus_tree(window, cx),
            PaletteCmd::FocusEditor => self.focus_editor(window, cx),
            PaletteCmd::FocusGrid => self.focus_grid(window, cx),
            PaletteCmd::NextTab => self.cycle_tab(true, window, cx),
            PaletteCmd::PrevTab => self.cycle_tab(false, window, cx),
            PaletteCmd::CloseTab => self.close_active_tab(window, cx),
            PaletteCmd::ApplyEdits => self.apply_active(cx),
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
mod test_support;

#[cfg(test)]
mod tests {
    use super::{geom_state, Modal};
    use crate::workspace::test_support::*;
    use gpui::{point, px, size, Bounds, TestAppContext, WindowBounds};

    #[test]
    fn geometry_keeps_restore_bounds_when_maximized() {
        let bounds = Bounds {
            origin: point(px(30.), px(40.)),
            size: size(px(1000.), px(700.)),
        };
        let windowed = geom_state(WindowBounds::Windowed(bounds), false);
        assert_eq!((windowed.x, windowed.width), (30., 1000.));
        assert!(!windowed.maximized);

        // Maximized stores the *restore* size, flagged so it reopens maximized.
        let max = geom_state(WindowBounds::Maximized(bounds), true);
        assert_eq!((max.width, max.height), (1000., 700.));
        assert!(max.maximized);
    }

    #[gpui::test]
    fn palette_opens_and_closes(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                ws.toggle_palette(cx);
                assert!(ws.is_modal(Modal::Palette));
                ws.toggle_palette(cx);
                assert!(!ws.modal_open());
            })
            .unwrap();
    }
}
