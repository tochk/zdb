//! Query execution: run/explain the active tab, sorting, SQL formatting,
//! and the query log.

use super::*;

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
        self.entries.push(LogEntry { sql: sql.to_string(), ok });
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
    error: Option<String>,
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
            QueryEvent::Failed(e) => out.error = Some(e.to_string()),
        }
    }
    out
}

impl Workspace {
    // ---- query execution -------------------------------------------------

    /// Run the active tab: a Table tab rebuilds its query from the CURRENT
    /// filter input (not the stale `base_sql`, which still holds the last
    /// applied WHERE); a Query/Scratch tab runs the single statement under the
    /// cursor (or the whole editor if there's one statement).
    pub(super) fn run_active_tab(&mut self, cx: &mut Context<Self>) {
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
            self.run_new_query(id, util::statement_at(&full, cursor), cx);
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
    /// EXPLAIN does not execute the query; ANALYZE does.
    pub(super) fn explain_active(&mut self, analyze: bool, cx: &mut Context<Self>) {
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
        self.run_explain(id, format!("{prefix}{sql}"), cx);
    }

    /// Execute an `EXPLAIN …` query and collect the `QUERY PLAN` text into the
    /// tab's plan view (leaves the grid untouched).
    pub(super) fn run_explain(&mut self, tab_id: u64, sql: String, cx: &mut Context<Self>) {
        let Some(conn) = self.conn else {
            self.status = "Not connected".into();
            cx.notify();
            return;
        };
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.running = true;
        }
        self.status = "Explaining…".into();
        log(format!("explain: {sql}"));
        let db = self.db.clone();
        let logged = sql.clone();
        let fut = async move { drain_query(db.query(conn, sql)).await };
        self.spawn_db(cx, fut, move |this, out, _cx| {
            let Some(tab) = this.tab_mut(tab_id) else { return };
            tab.running = false;
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

    /// Re-run a tab's current query/table from the DB (discards staged edits).
    pub(super) fn reload_data(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if let Some(sql) = self.tab(tab_id).and_then(|t| t.base_sql.clone()) {
            self.run_new_query(tab_id, sql, cx);
        }
    }

    /// Start a new query for `tab_id`: remember it as the sort base, drop stale
    /// editability, then describe (for editability) and execute it.
    pub(super) fn run_new_query(&mut self, tab_id: u64, base: String, cx: &mut Context<Self>) {
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
            tab.plan = None;
            tab.table.clone()
        };
        // Drop the Table widget's own row highlight; otherwise the previously
        // selected index stays lit when switching to a different table / reloading.
        table.update(cx, |ts, cx| ts.clear_selection(cx));
        self.describe_async(tab_id, base.clone(), cx);
        self.run_sql(tab_id, base, cx);
    }

    pub(super) fn describe_async(&mut self, tab_id: u64, sql: String, cx: &mut Context<Self>) {
        let Some(conn) = self.conn else { return };
        let db = self.db.clone();
        self.spawn_db(cx, async move { db.describe(conn, sql).await }, move |this, res, _cx| {
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
        });
    }

    /// Execute `sql` for display in `tab_id` (does not change editability or sort base).
    pub(super) fn run_sql(&mut self, tab_id: u64, sql: String, cx: &mut Context<Self>) {
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
        let fut = async move { drain_query(db.query(conn, sql)).await };
        self.spawn_db(cx, fut, move |this, out, cx| {
            // The tab may have been closed while the query ran.
            let Some(tab) = this.tab_mut(tab_id) else { return };
            tab.running = false;
            let QueryOutcome { headers, rows, affected, elapsed, error } = out;
            let status = match error {
                Some(e) => {
                    let s = format!("Error: {e}");
                    this.log.push(&logged, false);
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

    pub(super) fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(conn) = self.conn {
            self.db.cancel(conn);
            self.status = "Cancel requested".into();
            cx.notify();
        }
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

    // ---- sorting ---------------------------------------------------------

    /// Clicking a header cycles its sort: none → ascending → descending → none,
    /// re-running the query ordered by that column.
    pub(super) fn toggle_sort(&mut self, tab_id: u64, col_ix: usize, cx: &mut Context<Self>) {
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
