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
    ScrollStrategy, SharedString, StatefulInteractiveElement, Styled, Window, WindowControlArea,
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
use crate::lsp::{self, LspSlot};
use gpui::ClipboardItem;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use zdb_config::{ConnectionEntry, Settings};
use zdb_db::{
    CellValue, ConnId, ConnectionConfig, DbHandle, EditTarget, ExportFormat, QueryEvent,
    RelationDetail, RelationKind, RowEdit, SchemaObjects,
};

mod colors;
mod conn;
mod edit;
mod export;
mod grid;
mod query;
mod tabs;
mod tree;
mod util;
mod view;

use colors::{palette, Colors};
use conn::ConnForm;
use grid::{Tab, TabKind};
use query::QueryLog;
use tree::{NodeMeta, SchemaTree};
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
    ("Toggle terminal", PaletteCmd::Terminal),
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

    /// Shared inline-cell editor (only the active tab edits a cell at a time).
    cell_input: Entity<InputState>,

    /// Running `sqls` LSP handle (schema-aware completion), filled on connect.
    /// Shared with every SQL editor's completion provider.
    lsp_slot: LspSlot,

    log: QueryLog,

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
    form: ConnForm,
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

        let form = ConnForm::new(window, cx);
        let tree = SchemaTree::new(window, cx);

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
            cell_input,
            lsp_slot: lsp::new_slot(),
            log: QueryLog::default(),
            palette_open: false,
            palette_input,
            terminal: None,
            terminal_open: false,
            settings,
            passwords: HashMap::new(),
            conn_manager_open: false,
            conn_adding: false,
            settings_open: false,
            form,
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
    use crate::workspace::test_support::*;
    use gpui::TestAppContext;

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
}
