//! Query execution: run/explain the active tab, sorting, SQL formatting,
//! and the query log.

use super::*;
use gpui_component::highlighter::{Diagnostic, DiagnosticSeverity};
use gpui_component::RopeExt as _;
use grid::TabSession;
use zdb_db::{DbError, Session};

const LOG_CAP: usize = 500;

pub(super) struct LogEntry {
    pub(super) sql: String,
    pub(super) ok: bool,
}

/// The query log (most recent last, capped at `LOG_CAP`).
#[derive(Default)]
pub(super) struct QueryLog {
    pub(super) entries: Vec<LogEntry>,
}

impl QueryLog {
    pub(super) fn push(&mut self, sql: &str, ok: bool) {
        self.entries.push(LogEntry {
            sql: sql.to_string(),
            ok,
        });
        if self.entries.len() > LOG_CAP {
            self.entries.remove(0);
        }
    }
}

/// Everything a finished query produced, drained from its event stream.
#[derive(Default)]
struct QueryOutcome {
    headers: Vec<String>,
    rows: Vec<Vec<CellValue>>,
    affected: u64,
    elapsed: std::time::Duration,
    error: Option<DbError>,
    /// The tab's session is left inside a transaction.
    in_transaction: bool,
}

/// Collect a query's whole event stream into one `QueryOutcome`.
async fn drain_query(mut rx: zdb_db::QueryStream) -> QueryOutcome {
    let mut out = QueryOutcome::default();
    while let Some(ev) = rx.recv().await {
        match ev {
            QueryEvent::Columns(c) => out.headers = c.iter().map(|m| m.name.clone()).collect(),
            QueryEvent::Rows(mut r) => out.rows.append(&mut r),
            QueryEvent::Done { affected, elapsed } => {
                out.affected = affected;
                out.elapsed = elapsed;
            }
            QueryEvent::Failed(e) => out.error = Some(e),
            QueryEvent::Transaction(t) => out.in_transaction = t,
        }
    }
    out
}

impl Workspace {
    // ---- query execution -------------------------------------------------

    /// `tab_id`'s own database session: a connection only that tab uses, so
    /// its work never queues behind another tab's and Stop cancels only its
    /// own. Dialed by the first job given to it; closed with the tab.
    pub(super) fn tab_session(&mut self, tab_id: u64) -> Option<Session> {
        let session = Session::new(self.conn?, tab_id);
        let db = self.db.clone();
        let tab = self.tab_mut(tab_id)?;
        if tab.db_session.as_ref().map(|s| s.session) != Some(session) {
            tab.db_session = Some(TabSession { db, session });
        }
        Some(session)
    }

    /// Start a run of `tab_id`: whatever it still has in flight is stale from
    /// here on. Returns the run's number for `current_run`.
    pub(super) fn begin_run(&mut self, tab_id: u64) -> Option<u64> {
        let tab = self.tab_mut(tab_id)?;
        tab.run_gen += 1;
        tab.running = true;
        Some(tab.run_gen)
    }

    /// `tab_id`, if run `gen` is still its latest (not stopped or superseded).
    pub(super) fn current_run(&mut self, tab_id: u64, gen: u64) -> Option<&mut Tab> {
        self.tab_mut(tab_id).filter(|t| t.run_gen == gen)
    }

    /// Record whether a finished query left the tab's session in a
    /// transaction — for a stopped run too: the session is in that state
    /// either way (a tab's jobs run in order, so a later run reports later).
    fn note_transaction(&mut self, tab_id: u64, in_transaction: bool) {
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.in_transaction = in_transaction;
        }
    }

    /// Run the active tab: a Table tab rebuilds its query from the CURRENT
    /// filter input (not the stale `base_sql`, which still holds the last
    /// applied WHERE); a Query/Scratch tab runs the single statement under the
    /// cursor (or the whole editor if there's one statement).
    pub(super) fn run_active_tab(&mut self, cx: &mut Context<Self>) {
        // Ctrl/Cmd+Enter is also the multi-line cell editor's "save": while that
        // strip is open it saves the value instead of re-running the query (which
        // would reload the rows and drop the edit in progress).
        if self.active_tab().is_some_and(|t| t.editing_multi) {
            self.commit_cell_edit(cx);
            return;
        }
        let (id, is_table, full, cursor) = {
            let Some(tab) = self.active_tab() else { return };
            let ed = tab.editor.read(cx);
            (
                tab.id,
                matches!(tab.kind, TabKind::Table { .. }),
                ed.value().to_string(),
                ed.cursor(),
            )
        };
        if is_table {
            self.apply_where(id, cx);
        } else {
            let range = util::statement_range(&full, cursor);
            let stmt = full[range.clone()].to_string();
            if let Some(tab) = self.tab_mut(id) {
                tab.editor_run = Some((stmt.clone(), range.start));
            }
            self.run_new_query(id, stmt, cx);
        }
    }

    /// The SQL the active tab would run: a Table tab's generated `SELECT *`
    /// (with WHERE filter), else the editor text.
    pub(super) fn active_sql(&self, cx: &App) -> String {
        let Some(tab) = self.active_tab() else {
            return String::new();
        };
        if matches!(tab.kind, TabKind::Table { .. }) {
            self.table_query(tab.id, cx)
        } else {
            tab.editor.read(cx).value().to_string()
        }
    }

    /// Run `EXPLAIN` (or `EXPLAIN (ANALYZE, BUFFERS)`) on the active tab's SQL and
    /// show the text plan in a dedicated plan view (not the results grid). Plain
    /// EXPLAIN does not execute the query; ANALYZE does. A toggle: asked again
    /// while its own plan is up, it closes the plan instead (the other kind
    /// replaces it).
    pub(super) fn explain_active(&mut self, analyze: bool, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_tab() {
            if tab.plan.is_some() && tab.plan_analyze == analyze && !tab.running {
                let id = tab.id;
                return self.close_plan(id, cx);
            }
        }
        let sql = self.active_sql(cx).trim().trim_end_matches(';').to_string();
        if sql.is_empty() {
            self.status = "Nothing to explain".into();
            cx.notify();
            return;
        }
        let Some(id) = self.active_id() else { return };
        let prefix = if analyze {
            "EXPLAIN (ANALYZE, BUFFERS) "
        } else {
            "EXPLAIN "
        };
        self.run_explain(id, format!("{prefix}{sql}"), analyze, cx);
    }

    /// Execute an `EXPLAIN …` query and collect the `QUERY PLAN` text into the
    /// tab's plan view (leaves the grid untouched).
    pub(super) fn run_explain(
        &mut self,
        tab_id: u64,
        sql: String,
        analyze: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.tab_session(tab_id) else {
            self.status = "Not connected".into();
            cx.notify();
            return;
        };
        let Some(gen) = self.begin_run(tab_id) else {
            return;
        };
        self.status = "Explaining…".into();
        log(format!("explain: {sql}"));
        let db = self.db.clone();
        let logged = sql.clone();
        let fut = async move { drain_query(db.query(session, sql)).await };
        self.spawn_db(cx, fut, move |this, out, _cx| {
            this.note_transaction(tab_id, out.in_transaction);
            let Some(tab) = this.current_run(tab_id, gen) else {
                return;
            };
            tab.running = false;
            tab.plan_analyze = analyze;
            match out.error {
                Some(e) => {
                    tab.plan = Some(format!("Error: {e}"));
                    this.log.push(&logged, false);
                    this.status = format!("Error: {e}");
                }
                None => {
                    // Single "QUERY PLAN" column of text per row.
                    let lines: Vec<String> = out
                        .rows
                        .into_iter()
                        .filter_map(|row| match row.into_iter().next() {
                            Some(CellValue::Text(s)) => Some(s),
                            _ => None,
                        })
                        .collect();
                    let n = lines.len();
                    tab.plan = Some(lines.join("\n"));
                    this.log.push(&logged, true);
                    this.status = format!("Plan: {n} line(s)");
                }
            }
        });
        cx.notify();
    }

    /// Dismiss the plan view, back to the tab's normal editor / results.
    pub(super) fn close_plan(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.plan = None;
        }
        cx.notify();
    }

    /// Re-run a tab's current query/table from the DB (discards staged edits),
    /// keeping its filter and sort.
    pub(super) fn reload_data(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(tab) = self.tab(tab_id) else {
            return;
        };
        let sort = tab.sort_state;
        if matches!(tab.kind, TabKind::Table { .. }) {
            // A Table tab's query IS its WHERE / ORDER BY boxes, so rebuild it
            // from them as Run does — `base_sql` predates any header sort. The
            // header arrow stays while the box still holds what its click wrote.
            let order = tab.order_input.read(cx).value().trim().to_string();
            let keep = sort.filter(|_| order == sort_clause(&tab.headers, sort));
            self.with_table_pk(tab_id, cx, move |this, cx| {
                let sql = this.table_query(tab_id, cx);
                this.start_query(tab_id, sql.clone(), sql, keep, cx);
            });
            return;
        }
        let Some(base) = tab.base_sql.clone() else {
            return;
        };
        let shown = sorted_sql(&base, &tab.headers, sort);
        self.start_query(tab_id, base, shown, sort, cx);
    }

    /// The pager: show another page (`LIMIT` rows) of a Table tab, keeping its
    /// WHERE, ORDER BY and header sort. The last page needs the row count;
    /// without one yet it is counted first (`count_rows`).
    pub(super) fn go_page(&mut self, tab_id: u64, to: PageMove, cx: &mut Context<Self>) {
        let Some(size) = self
            .tab(tab_id)
            .and_then(|t| tabs::page_size(&t.limit_input.read(cx).value()))
        else {
            return;
        };
        let Some(tab) = self.tab_mut(tab_id) else {
            return;
        };
        tab.page = match to {
            PageMove::First => 0,
            PageMove::Prev => tab.page.saturating_sub(1),
            PageMove::Next => tab.page + 1,
            PageMove::Last => match tab.total_rows {
                Some(total) => total.saturating_sub(1) / size,
                None => return self.count_rows(tab_id, cx),
            },
        };
        self.reload_data(tab_id, cx);
    }

    /// `count(*)` a Table tab under its WHERE, then go to its last page.
    fn count_rows(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(sql) = self.table_count_query(tab_id, cx) else {
            return;
        };
        let Some(session) = self.tab_session(tab_id) else {
            return;
        };
        let Some(gen) = self.begin_run(tab_id) else {
            return;
        };
        self.status = "Counting rows…".into();
        let db = self.db.clone();
        let logged = sql.clone();
        let fut = async move { drain_query(db.query(session, sql)).await };
        self.spawn_db(cx, fut, move |this, out, cx| {
            this.note_transaction(tab_id, out.in_transaction);
            let Some(tab) = this.current_run(tab_id, gen) else {
                return;
            };
            tab.running = false;
            let total = out.rows.first().and_then(|row| match row.first() {
                Some(CellValue::Text(n)) => n.parse::<usize>().ok(),
                _ => None,
            });
            match (out.error, total) {
                (None, Some(total)) => {
                    tab.total_rows = Some(total);
                    this.log.push(&logged, true);
                    this.go_page(tab_id, PageMove::Last, cx);
                }
                (error, _) => {
                    this.log.push(&logged, false);
                    this.status = match error {
                        Some(e) => format!("Error: {e}"),
                        None => "Couldn't count the rows".into(),
                    };
                }
            }
        });
        cx.notify();
    }

    /// Start a new query for `tab_id`: remember it as the sort base, drop stale
    /// editability, then describe (for editability) and execute it.
    pub(super) fn run_new_query(&mut self, tab_id: u64, base: String, cx: &mut Context<Self>) {
        self.start_query(tab_id, base.clone(), base, None, cx);
    }

    /// `run_new_query`, showing the rows as `shown` (`base` under the header
    /// `sort`) — what a refresh needs to keep the order on screen.
    fn start_query(
        &mut self,
        tab_id: u64,
        base: String,
        shown: String,
        sort: Option<(usize, bool)>,
        cx: &mut Context<Self>,
    ) {
        {
            let Some(tab) = self.tab_mut(tab_id) else {
                return;
            };
            tab.base_sql = Some(base.clone());
            tab.sort_state = sort;
            tab.edit_target = None;
            tab.edit_cols.clear();
            tab.editing = None;
            tab.pending.clear();
            tab.edited_cells.clear();
            tab.plan = None;
        }
        // The cell cursor is deliberately left alone: `run_sql` clamps it to the
        // rows that come back, so a refresh / re-sort keeps your place.
        self.run_sql(tab_id, shown, cx);
        self.describe_async(tab_id, base, cx);
    }

    /// Find out whether `sql`'s rows are editable, as part of the run just
    /// started (it queues behind that run on the tab's session, and is
    /// dropped with it).
    fn describe_async(&mut self, tab_id: u64, sql: String, cx: &mut Context<Self>) {
        let Some(session) = self.tab_session(tab_id) else {
            return;
        };
        let Some(gen) = self.tab(tab_id).map(|t| t.run_gen) else {
            return;
        };
        let db = self.db.clone();
        self.spawn_db(
            cx,
            async move { db.describe(session, sql).await },
            move |this, res, _cx| {
                let Some(tab) = this.current_run(tab_id, gen) else {
                    return;
                };
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
            },
        );
    }

    /// Execute `sql` for display in `tab_id` (does not change editability or sort base).
    pub(super) fn run_sql(&mut self, tab_id: u64, sql: String, cx: &mut Context<Self>) {
        if self.conn.is_none() {
            self.status = "Not connected".into();
            cx.notify();
            return;
        }
        if sql.trim().is_empty() {
            if let Some(tab) = self.tab_mut(tab_id) {
                tab.running = false;
            }
            self.status = "Nothing to run".into();
            cx.notify();
            return;
        }
        let Some(session) = self.tab_session(tab_id) else {
            return;
        };
        let Some(gen) = self.begin_run(tab_id) else {
            return;
        };
        // Where these rows start in the table (the OFFSET), as of the request,
        // and whether they come back end-first (a page read from the far end).
        let page = self.table_page_of(tab_id, &sql, cx);
        let (first_row, reversed) = page;
        {
            let Some(tab) = self.tab_mut(tab_id) else {
                return;
            };
            tab.editing = None;
            tab.new_row_idx = None;
            tab.last_sql = Some(sql.clone());
            tab.last_page = page;
        }
        self.status = "Running…".into();
        log(format!("run: {sql}"));
        let db = self.db.clone();
        let logged = sql.clone();
        let fut = async move { drain_query(db.query(session, sql)).await };
        self.spawn_db(cx, fut, move |this, out, cx| {
            this.note_transaction(tab_id, out.in_transaction);
            // The tab may have been closed or stopped while the query ran (or
            // run again).
            let Some(tab) = this.current_run(tab_id, gen) else {
                this.log.push(&logged, out.error.is_none());
                return;
            };
            tab.running = false;
            // Marks from an earlier failure go, whatever this run did.
            tab.clause_error = None;
            let editor = tab.editor.clone();
            grid::clear_marks(&editor, cx);
            let QueryOutcome {
                headers,
                mut rows,
                affected,
                elapsed,
                error,
                ..
            } = out;
            if reversed {
                rows.reverse();
            }
            let status = match error {
                Some(e) => {
                    let s = format!("Error: {e}");
                    this.log.push(&logged, false);
                    this.mark_failure(tab_id, &logged, &e, cx);
                    s
                }
                None => {
                    let is_select = !headers.is_empty();
                    let n = rows.len();
                    let (table, cursor) = {
                        let Some(tab) = this.tab_mut(tab_id) else {
                            return;
                        };
                        tab.headers = headers.clone();
                        tab.rows = rows.clone();
                        tab.orig_rows = rows.clone();
                        tab.first_row = first_row;
                        // Fresh rows just landed: keep the cell cursor where it
                        // was, clamped to the new result (none if it is empty).
                        // Single point every result passes through, so the
                        // cursor, `current_row` and the widget's row highlight
                        // can't drift apart — or point past the data.
                        let start = tab
                            .cursor
                            .or(std::mem::take(&mut tab.cursor_on_load).then_some((0, 0)));
                        tab.cursor = keynav::clamp_cursor(start, n, headers.len());
                        tab.current_row = tab.cursor.map(|(r, _)| r);
                        (tab.table.clone(), tab.cursor)
                    };
                    table.update(cx, |ts, cx| {
                        ts.delegate_mut().set(&headers, rows);
                        ts.delegate_mut().fit_gutter(first_row + n);
                        ts.refresh(cx);
                        match cursor {
                            Some((r, _)) => ts.set_selected_row(r, cx),
                            None => ts.clear_selection(cx),
                        }
                    });
                    this.log.push(&logged, true);
                    if is_select {
                        format!("{n} row(s) in {elapsed:?}")
                    } else {
                        format!("{affected} row(s) affected in {elapsed:?}")
                    }
                }
            };
            this.status = status;
        });
        cx.notify();
    }

    /// Stop the active tab's run. The tab is free again at once — whatever
    /// comes back for the stopped run is dropped — while the server cancels it
    /// in the background (only this tab's session: other tabs' work goes on).
    pub(super) fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(id) = self.active_id() {
            self.cancel_tab(id, cx);
        }
    }

    /// A click on a running tab's loader: ask before stopping its query —
    /// unless Settings (or an earlier "Don't ask again") says not to.
    pub(super) fn ask_cancel(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if !self.tab(tab_id).is_some_and(|t| t.running) {
            return;
        }
        if !self.settings.confirm_query_cancel {
            return self.cancel_tab(tab_id, cx);
        }
        self.cancel_dont_ask = false;
        self.modal = Some(Modal::ConfirmCancel(tab_id));
        cx.notify();
    }

    /// The confirmation's "Cancel query": stop the query, close the dialog,
    /// and stop asking if "Don't ask again" is ticked. ("Keep running" / Esc
    /// leave the setting alone, ticked or not.)
    pub(super) fn confirm_cancel(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.cancel_dont_ask) {
            self.set_confirm_query_cancel(false, cx);
        }
        self.close_modal(cx);
        self.cancel_tab(tab_id, cx);
    }

    /// Stop `tab_id`'s run (see `cancel`).
    pub(super) fn cancel_tab(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        let Some(tab) = self.tab_mut(tab_id) else {
            return;
        };
        if !tab.running {
            return;
        }
        tab.running = false;
        tab.run_gen += 1;
        if let Some(s) = &tab.db_session {
            s.db.cancel(s.session);
        }
        self.status = "Cancelled".into();
        cx.notify();
    }

    // ---- formatting ------------------------------------------------------

    /// Pretty-print the active editor's SQL in place (Query / Scratch tabs).
    pub(super) fn format_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else { return };
        if matches!(tab.kind, TabKind::Table { .. }) {
            self.status = "Nothing to format".into();
            cx.notify();
            return;
        }
        let editor = tab.editor.clone();
        let sql = editor.read(cx).value().to_string();
        if sql.trim().is_empty() {
            return;
        }
        let pretty = util::format_sql(&sql);
        editor.update(cx, |state, cx| state.set_value(pretty, window, cx));
        cx.notify();
    }

    /// Put a failed run's error back where it happened: underline the token
    /// Postgres points at in the editor (hovering shows the message), or flag
    /// the Table tab box it falls in. Either clears on the next edit or run.
    fn mark_failure(&mut self, tab_id: u64, sql: &str, err: &DbError, cx: &mut Context<Self>) {
        let DbError::Postgres {
            message,
            position: Some(pos),
            detail,
            hint,
            ..
        } = err
        else {
            return;
        };
        let Some(tab) = self.tab(tab_id) else {
            return;
        };
        if matches!(tab.kind, TabKind::Table { .. }) {
            let clause = self.clause_at(tab_id, sql, *pos, cx);
            if let Some(tab) = self.tab_mut(tab_id) {
                tab.clause_error = clause;
            }
            return;
        }
        // Only a statement run from the editor maps back onto its text (not a
        // re-sort's wrapper, say).
        let Some(start) = tab
            .editor_run
            .as_ref()
            .filter(|(run, _)| run == sql)
            .map(|(_, start)| *start)
        else {
            return;
        };
        let span = util::error_span(sql, *pos);
        if span.is_empty() {
            return;
        }
        let mut text = message.clone();
        for (label, extra) in [("DETAIL", detail), ("HINT", hint)] {
            if let Some(extra) = extra {
                text.push_str(&format!("\n{label}: {extra}"));
            }
        }
        tab.editor.clone().update(cx, |state, cx| {
            let rope = state.text().clone();
            // Edited while it ran: the offsets no longer line up.
            if rope.to_string().get(start..start + sql.len()) != Some(sql) {
                return;
            }
            let range = rope.offset_to_position(start + span.start)
                ..rope.offset_to_position(start + span.end);
            if let Some(marks) = state.diagnostics_mut() {
                marks.reset(&rope);
                marks.push(Diagnostic {
                    range,
                    severity: DiagnosticSeverity::Error,
                    message: text.into(),
                    source: Some("postgres".into()),
                    ..Default::default()
                });
            }
            cx.notify();
        });
    }

    // ---- sorting ---------------------------------------------------------

    /// Clicking a header cycles its sort: none → ascending → descending → none,
    /// re-running the query ordered by that column.
    pub(super) fn toggle_sort(
        &mut self,
        tab_id: u64,
        col_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (next, base, headers, is_table) = {
            let Some(tab) = self.tab_mut(tab_id) else {
                return;
            };
            let next = match tab.sort_state {
                Some((c, false)) if c == col_ix => Some((col_ix, true)),
                Some((c, true)) if c == col_ix => None,
                _ => Some((col_ix, false)),
            };
            tab.sort_state = next;
            let is_table = matches!(tab.kind, TabKind::Table { .. });
            (next, tab.base_sql.clone(), tab.headers.clone(), is_table)
        };
        // A table tab owns all the parts of its query: push the new sort into the
        // ORDER BY input and rebuild, instead of rewriting the SQL it produced
        // last time.
        if is_table {
            // A new order starts from the first page.
            if let Some(tab) = self.tab_mut(tab_id) {
                tab.page = 0;
            }
            let clause = sort_clause(&headers, next);
            if let Some(input) = self.tab(tab_id).map(|t| t.order_input.clone()) {
                input.update(cx, |state, cx| state.set_value(clause, window, cx));
            }
            let sql = self.table_query(tab_id, cx);
            log(format!("sort: {sql}"));
            self.run_sql(tab_id, sql, cx);
            return;
        }
        let Some(base) = base else {
            log("sort: no base query");
            return;
        };
        let sql = sorted_sql(&base, &headers, next);
        log(format!("sort: {sql}"));
        self.run_sql(tab_id, sql, cx);
    }
}

/// What a header sort writes into a Table tab's ORDER BY box: `"col" ASC|DESC`
/// (empty for no sort, or a column that's gone).
fn sort_clause(headers: &[String], sort: Option<(usize, bool)>) -> String {
    let Some((name, desc)) = sort.and_then(|(c, desc)| Some((headers.get(c)?, desc))) else {
        return String::new();
    };
    format!(
        "\"{}\" {}",
        name.replace('"', "\"\""),
        if desc { "DESC" } else { "ASC" }
    )
}

/// A Query tab's `base` under a header sort (unchanged for no sort).
fn sorted_sql(base: &str, headers: &[String], sort: Option<(usize, bool)>) -> String {
    match sort.and_then(|(c, desc)| Some((headers.get(c)?, desc))) {
        Some((col, desc)) => {
            let dir = if desc {
                ColumnSort::Descending
            } else {
                ColumnSort::Ascending
            };
            order_by_sql(base, col, dir)
        }
        None => base.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::test_support::*;
    use gpui::TestAppContext;

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
                ws.toggle_sort(id, 0, window, cx);
                assert_eq!(ws.tab(id).unwrap().sort_state, Some((0, false))); // ascending
                ws.toggle_sort(id, 0, window, cx);
                assert_eq!(ws.tab(id).unwrap().sort_state, Some((0, true))); // descending
                ws.toggle_sort(id, 0, window, cx);
                assert_eq!(ws.tab(id).unwrap().sort_state, None); // cleared
            })
            .unwrap();
    }

    /// Explain / Explain Analyze are toggles: clicked while their own plan is
    /// up they close it; the other kind doesn't (it would replace the plan).
    #[gpui::test]
    fn explain_again_closes_its_plan(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                let show = |ws: &mut Workspace, analyze: bool| {
                    let tab = ws.tab_mut(id).unwrap();
                    tab.plan = Some("Seq Scan on t".into());
                    tab.plan_analyze = analyze;
                };
                let plan_shown = |ws: &Workspace| ws.tab(id).unwrap().plan.is_some();

                show(ws, false);
                ws.explain_active(true, cx);
                assert!(plan_shown(ws), "Analyze isn't the plain plan's toggle");
                ws.explain_active(false, cx);
                assert!(!plan_shown(ws));

                show(ws, true);
                ws.explain_active(false, cx);
                assert!(plan_shown(ws));
                ws.explain_active(true, cx);
                assert!(!plan_shown(ws));
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

    #[gpui::test]
    fn run_button_rereads_table_filter(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.open_table_tab("public".into(), "users".into(), window, cx);
                let id = ws.active_id().unwrap();
                let input = ws.tab(id).unwrap().where_input.clone();
                input.update(cx, |i, cx| i.set_value("id > 5", window, cx));
                ws.apply_where(id, cx);
                let base = ws.tab(id).unwrap().base_sql.clone().unwrap();
                assert!(base.contains("WHERE id > 5"));

                // Clearing the filter and pressing Run must drop the WHERE —
                // without requiring Enter in the filter box first.
                input.update(cx, |i, cx| i.set_value("", window, cx));
                ws.run_active_tab(cx);
                let base = ws.tab(id).unwrap().base_sql.clone().unwrap();
                assert!(!base.contains("WHERE"));
            })
            .unwrap();
    }

    /// Refresh re-runs a Table tab from its WHERE / ORDER BY boxes — including
    /// a header sort, which never touched `base_sql`.
    #[gpui::test]
    fn refresh_keeps_table_filter_and_sort(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.open_table_tab("public".into(), "users".into(), window, cx);
                let id = ws.active_id().unwrap();
                let (filter, order) = {
                    let tab = ws.tab(id).unwrap();
                    (tab.where_input.clone(), tab.order_input.clone())
                };
                filter.update(cx, |i, cx| i.set_value("id > 5", window, cx));
                ws.apply_where(id, cx);
                ws.tab_mut(id).unwrap().headers = vec!["id".into(), "name".into()];
                ws.toggle_sort(id, 1, window, cx);

                ws.reload_data(id, cx);
                let tab = ws.tab(id).unwrap();
                let base = tab.base_sql.clone().unwrap();
                assert!(
                    base.contains("WHERE id > 5 ORDER BY \"name\" ASC"),
                    "{base}"
                );
                assert_eq!(tab.sort_state, Some((1, false)));

                // Typed over the header's clause: the text wins, the arrow goes.
                order.update(cx, |i, cx| i.set_value("id DESC", window, cx));
                ws.reload_data(id, cx);
                let tab = ws.tab(id).unwrap();
                assert!(tab
                    .base_sql
                    .clone()
                    .unwrap()
                    .ends_with("ORDER BY id DESC LIMIT 500"));
                assert_eq!(tab.sort_state, None);
            })
            .unwrap();
    }

    /// A Query tab's header sort survives a refresh; the base stays unsorted.
    #[gpui::test]
    fn refresh_keeps_query_tab_sort(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                {
                    let tab = ws.tab_mut(id).unwrap();
                    tab.base_sql = Some("SELECT * FROM t".into());
                    tab.headers = vec!["qty".into()];
                }
                ws.toggle_sort(id, 0, window, cx);
                ws.reload_data(id, cx);
                let tab = ws.tab(id).unwrap();
                assert_eq!(tab.base_sql.as_deref(), Some("SELECT * FROM t"));
                assert_eq!(tab.sort_state, Some((0, false)));
                assert_eq!(
                    sorted_sql("SELECT * FROM t", &tab.headers, tab.sort_state),
                    order_by_sql("SELECT * FROM t", "qty", ColumnSort::Ascending)
                );
            })
            .unwrap();
    }

    /// Stop frees the tab at once; whatever the stopped run sends back later
    /// is dropped.
    #[gpui::test]
    fn stop_frees_the_tab_and_drops_the_late_result(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.close_modal(cx);
                ws.open_query_tab(window, cx);
                let id = ws.active_id().unwrap();
                let gen = ws.begin_run(id).unwrap();
                assert!(ws.tab(id).unwrap().running);
                ws.cancel(cx);
                assert!(!ws.tab(id).unwrap().running);
                assert!(ws.current_run(id, gen).is_none());
                // Nothing running: Stop does nothing.
                let now = ws.tab(id).unwrap().run_gen;
                ws.cancel(cx);
                assert_eq!(ws.tab(id).unwrap().run_gen, now);
            })
            .unwrap();
    }

    /// The loader shows as soon as a run starts: on a blank pane, or over the
    /// rows from before, which take no input until the run is over.
    #[gpui::test]
    fn loader_shows_while_a_run_goes_on(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.close_modal(cx);
                let id = seed_query_tab(ws, window, cx);
                assert_eq!(ws.tab(id).unwrap().loader(), None);
                ws.begin_run(id).unwrap();
                assert_eq!(ws.tab(id).unwrap().loader(), Some(Loader::Blank));

                seed_rows(ws, id, vec!["id", "name"], 3, cx);
                editable_tab(ws, id, vec!["id", "name"]);
                ws.set_cursor(id, 0, 0, cx);
                assert_eq!(ws.tab(id).unwrap().loader(), Some(Loader::OverData));
                ws.grid_motion(Motion::Down, cx);
                ws.begin_edit(id, 0, 1, window, cx);
                let tab = ws.tab(id).unwrap();
                assert_eq!((tab.cursor, tab.editing), (Some((0, 0)), None));

                // A click on the loader asks first; keeping it running
                // changes nothing, confirming stops it.
                ws.ask_cancel(id, cx);
                assert!(ws.is_modal(Modal::ConfirmCancel(id)));
                ws.close_modal(cx);
                assert!(ws.tab(id).unwrap().running);
                ws.ask_cancel(id, cx);
                ws.confirm_cancel(id, cx);
                assert!(!ws.modal_open());
                assert!(
                    ws.settings.confirm_query_cancel,
                    "box not ticked: still asks"
                );
                // Stopped: no loader, and the grid answers again.
                assert_eq!(ws.tab(id).unwrap().loader(), None);
                ws.grid_motion(Motion::Down, cx);
                assert_eq!(ws.tab(id).unwrap().cursor, Some((1, 0)));
            })
            .unwrap();
    }

    /// "Don't ask again" counts only with "Cancel query"; after that a click
    /// on the loader cancels straight away.
    #[gpui::test]
    fn dont_ask_again_only_on_cancel(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.close_modal(cx);
                let id = seed_query_tab(ws, window, cx);
                ws.begin_run(id).unwrap();

                ws.ask_cancel(id, cx);
                ws.cancel_dont_ask = true;
                ws.close_modal(cx); // Keep running
                assert!(ws.settings.confirm_query_cancel);
                assert!(ws.tab(id).unwrap().running);

                ws.ask_cancel(id, cx);
                assert!(!ws.cancel_dont_ask, "the box starts unticked each time");
                ws.cancel_dont_ask = true;
                ws.confirm_cancel(id, cx);
                assert!(!ws.settings.confirm_query_cancel);

                ws.begin_run(id).unwrap();
                ws.ask_cancel(id, cx);
                assert!(!ws.modal_open());
                assert!(!ws.tab(id).unwrap().running);
            })
            .unwrap();
    }

    fn pg_error(position: u32) -> DbError {
        DbError::Postgres {
            message: "column \"nme\" does not exist".into(),
            code: Some("42703".into()),
            position: Some(position),
            detail: None,
            hint: Some("Perhaps you meant \"name\".".into()),
        }
    }

    /// A failed editor run is marked on the statement's own text (not where
    /// the same offset would land in the whole editor), and editing clears it.
    #[gpui::test]
    fn failed_run_is_marked_in_the_editor(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        let stmt = "select nme from t";
        let editor = window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                let editor = ws.tab(id).unwrap().editor.clone();
                editor.update(cx, |e, cx| {
                    e.set_value(format!("select 1;\n{stmt}"), window, cx)
                });
                ws.tab_mut(id).unwrap().editor_run = Some((stmt.into(), 10));
                ws.mark_failure(id, stmt, &pg_error(8), cx);
                assert_eq!(editor.read(cx).diagnostics().map(|d| d.len()), Some(1));

                // The run's text no longer where it was: nothing is marked.
                ws.tab_mut(id).unwrap().editor_run = Some((stmt.into(), 3));
                grid::clear_marks(&editor, cx);
                ws.mark_failure(id, stmt, &pg_error(8), cx);
                assert_eq!(editor.read(cx).diagnostics().map(|d| d.len()), Some(0));

                ws.tab_mut(id).unwrap().editor_run = Some((stmt.into(), 10));
                ws.mark_failure(id, stmt, &pg_error(8), cx);
                editor.update(cx, |e, cx| e.set_value("select 2", window, cx));
                editor
            })
            .unwrap();
        cx.run_until_parked();
        let left = cx.read(|cx| editor.read(cx).diagnostics().map(|d| d.len()));
        assert_eq!(left, Some(0));
    }

    /// A Table tab error is pinned on the box it falls in — including one
    /// reported just past the box's text, at the next keyword.
    #[gpui::test]
    fn table_errors_point_at_their_box(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.open_table_tab("public".into(), "users".into(), window, cx);
                let id = ws.active_id().unwrap();
                let (filter, order) = {
                    let tab = ws.tab(id).unwrap();
                    (tab.where_input.clone(), tab.order_input.clone())
                };
                filter.update(cx, |i, cx| i.set_value("id >", window, cx));
                order.update(cx, |i, cx| i.set_value("nme", window, cx));
                let sql = ws.table_query(id, cx);
                let at = |s: &str| sql.find(s).unwrap() as u32 + 1;
                assert_eq!(ws.clause_at(id, &sql, at("id >"), cx), Some(Clause::Where));
                assert_eq!(
                    ws.clause_at(id, &sql, at(" ORDER"), cx),
                    Some(Clause::Where)
                );
                assert_eq!(ws.clause_at(id, &sql, at("nme"), cx), Some(Clause::OrderBy));
                assert_eq!(
                    ws.clause_at(id, &sql, at("LIMIT"), cx),
                    Some(Clause::OrderBy)
                );
                assert_eq!(ws.clause_at(id, &sql, at("users"), cx), None);
                // SQL the boxes no longer produce isn't attributed.
                assert_eq!(ws.clause_at(id, "SELECT 1", 1, cx), None);

                ws.mark_failure(id, &sql, &pg_error(at("nme")), cx);
                assert_eq!(ws.tab(id).unwrap().clause_error, Some(Clause::OrderBy));
            })
            .unwrap();
    }

    #[gpui::test]
    fn query_log_records_runs(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, _cx| {
                assert!(ws.log.entries.is_empty());
                ws.log.push("SELECT 1", true);
                ws.log.push("BAD", false);
                assert_eq!(ws.log.entries.len(), 2);
                assert!(!ws.log.entries[1].ok);
            })
            .unwrap();
    }
}
