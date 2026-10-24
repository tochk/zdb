//! Center-tab management: accessors, tab lifecycle, and the table-tab
//! query builders (`table_query` / `apply_where`).

use super::*;

impl Workspace {
    // ---- tab management --------------------------------------------------

    pub(super) fn tab(&self, id: u64) -> Option<&Tab> {
        self.tabs.iter().find(|t| t.id == id)
    }

    pub(super) fn tab_mut(&mut self, id: u64) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.id == id)
    }

    pub(super) fn active_tab(&self) -> Option<&Tab> {
        self.active.and_then(|i| self.tabs.get(i))
    }

    pub(super) fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.active.and_then(|i| self.tabs.get_mut(i))
    }

    pub(super) fn active_id(&self) -> Option<u64> {
        self.active_tab().map(|t| t.id)
    }

    /// Make tab `idx` active: stage any in-flight cell edit (leaving a cell any
    /// way but Esc keeps the edit) and focus its input.
    pub(super) fn activate_tab(&mut self, idx: usize, window: &mut Window, cx: &mut Context<Self>) {
        if idx >= self.tabs.len() {
            return;
        }
        // Before `active` moves: the edit belongs to the tab being left.
        self.commit_cell_edit(cx);
        self.active = Some(idx);
        // Auto-reveal: a Table tab selects + scrolls to its node in the sidebar.
        if let TabKind::Table { schema, table } = &self.tabs[idx].kind {
            let (s, t) = (schema.clone(), table.clone());
            self.reveal_relation(&s, &t, cx);
        }
        self.focus_editor(window, cx);
        self.mark_session_dirty(cx);
        cx.notify();
    }

    /// Focus the active tab's text input: the SQL editor, or the WHERE filter on
    /// a Table tab (whose generated editor is hidden).
    pub(super) fn focus_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.active_tab() else {
            return;
        };
        let focus = match tab.kind {
            TabKind::Table { .. } => tab.where_input.clone(),
            _ => tab.editor.clone(),
        };
        // Focusing a gpui-component input touches the window's `Root` layer, which
        // only exists under a real app (not in headless `#[gpui::test]` windows).
        if window.root::<gpui_component::Root>().flatten().is_some() {
            focus.read(cx).focus_handle(cx).focus(window);
        }
    }

    /// Open a fresh blank "Query N" tab and focus it.
    pub(super) fn open_query_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let n = session::next_query_n(
            self.tabs
                .iter()
                .filter(|t| matches!(t.kind, TabKind::Query))
                .map(|t| t.title.as_str()),
        );
        let tab = Tab::query(id, n, self.lsp_slot.clone(), window, cx);
        self.tabs.push(tab);
        self.activate_tab(self.tabs.len() - 1, window, cx);
    }

    /// Open (or focus, if already open) a table-browse tab and run its query.
    pub(super) fn open_table_tab(
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
        let tab = Tab::table(id, schema, table, self.lsp_slot.clone(), window, cx);
        self.tabs.push(tab);
        self.activate_tab(self.tabs.len() - 1, window, cx);
        let sql = self.table_query(id, cx);
        self.run_new_query(id, sql, cx);
    }

    /// Focus the singleton scratch tab, opening it if absent.
    pub(super) fn focus_scratch_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(idx) = self
            .tabs
            .iter()
            .position(|t| matches!(t.kind, TabKind::Scratch))
        {
            self.activate_tab(idx, window, cx);
            return;
        }
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let tab = Tab::scratch(id, self.conn_key.clone(), self.lsp_slot.clone(), window, cx);
        self.tabs.push(tab);
        self.activate_tab(self.tabs.len() - 1, window, cx);
    }

    pub(super) fn close_tab(&mut self, id: u64, cx: &mut Context<Self>) {
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
        self.mark_session_dirty(cx);
        cx.notify();
    }

    /// Keep only `id` (tab-strip context menu "Close Others").
    pub(super) fn close_other_tabs(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.tabs.iter().any(|t| t.id == id) {
            return;
        }
        self.tabs.retain(|t| t.id == id);
        self.active = Some(0);
        self.mark_session_dirty(cx);
        cx.notify();
    }

    /// Drop every tab after `id` (tab-strip context menu "Close to the Right").
    pub(super) fn close_tabs_right(&mut self, id: u64, cx: &mut Context<Self>) {
        let Some(idx) = self.tabs.iter().position(|t| t.id == id) else {
            return;
        };
        if idx + 1 >= self.tabs.len() {
            return;
        }
        self.tabs.truncate(idx + 1);
        // Only move focus if the active tab was one of the closed ones.
        if self.active.is_none_or(|a| a > idx) {
            self.active = Some(idx);
        }
        self.mark_session_dirty(cx);
        cx.notify();
    }

    /// Drop every tab (e.g. on connection switch — old results are invalid).
    pub(super) fn close_all_tabs(&mut self, cx: &mut Context<Self>) {
        self.tabs.clear();
        self.active = None;
        self.mark_session_dirty(cx);
        cx.notify();
    }

    /// `SELECT * FROM <table> [WHERE …] [ORDER BY …] LIMIT n` for a table tab.
    /// The whole statement is rebuilt from the tab's own parts (filter + sort),
    /// so sorting never has to rewrite previously generated SQL.
    pub(super) fn table_query(&self, tab_id: u64, cx: &App) -> String {
        self.table_sql(tab_id, cx)
            .map(|q| q.sql)
            .unwrap_or_default()
    }

    fn table_sql(&self, tab_id: u64, cx: &App) -> Option<TableSql> {
        let tab = self.tab(tab_id)?;
        let TabKind::Table { schema, table } = &tab.kind else {
            return None;
        };
        let w = tab.where_input.read(cx).value();
        let o = tab.order_input.read(cx).value();
        Some(TableSql::new(schema, table, w.trim(), o.trim()))
    }

    /// Which of a Table tab's boxes a failure of `sql` at 1-based character
    /// `pos` points into. An error just past a box's text (`WHERE id >` fails
    /// at the next keyword) belongs to that box too. `None` if the boxes no
    /// longer produce `sql` (edited since) or the error is in the fixed part.
    pub(super) fn clause_at(&self, tab_id: u64, sql: &str, pos: u32, cx: &App) -> Option<Clause> {
        let q = self.table_sql(tab_id, cx).filter(|q| q.sql == sql)?;
        let at = pos.checked_sub(1)? as usize;
        match (q.where_at, q.order_at) {
            (_, Some(o)) if at >= o => Some(Clause::OrderBy),
            (Some(w), _) if at >= w => Some(Clause::Where),
            _ => None,
        }
    }

    /// Re-run a table tab from its WHERE / ORDER BY inputs. A hand-typed ORDER BY
    /// replaces whatever a header click had set, so the header arrows are cleared.
    pub(super) fn apply_where(&mut self, tab_id: u64, cx: &mut Context<Self>) {
        if !matches!(
            self.tab(tab_id).map(|t| &t.kind),
            Some(TabKind::Table { .. })
        ) {
            return;
        }
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.sort_state = None;
        }
        let sql = self.table_query(tab_id, cx);
        self.run_new_query(tab_id, sql, cx);
    }
}

/// A Table tab's generated `SELECT`, and where (in characters, as Postgres
/// counts error positions) its WHERE / ORDER BY text starts.
pub(super) struct TableSql {
    pub(super) sql: String,
    where_at: Option<usize>,
    order_at: Option<usize>,
}

impl TableSql {
    fn new(schema: &str, table: &str, filter: &str, order: &str) -> Self {
        let mut sql = format!(
            "SELECT * FROM \"{}\".\"{}\"",
            schema.replace('"', "\"\""),
            table.replace('"', "\"\"")
        );
        let mut clause = |keyword: &str, text: &str| {
            (!text.is_empty()).then(|| {
                sql.push_str(keyword);
                let at = sql.chars().count();
                sql.push_str(text);
                at
            })
        };
        let where_at = clause(" WHERE ", filter);
        let order_at = clause(" ORDER BY ", order);
        sql.push_str(&format!(" LIMIT {ROW_LIMIT}"));
        Self {
            sql,
            where_at,
            order_at,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::test_support::*;
    use gpui::TestAppContext;

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

    /// A header click on a table tab produces a plain `ORDER BY` in the generated
    /// query (no subquery wrapper) and fills the ORDER BY input.
    #[gpui::test]
    fn table_query_builds_order_by(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.open_table_tab("public".into(), "users".into(), window, cx);
                let id = ws.active_id().unwrap();
                ws.tab_mut(id).unwrap().headers = vec!["id".into(), "name".into()];
                ws.tab(id)
                    .unwrap()
                    .where_input
                    .update(cx, |i, cx| i.set_value("id > 5", window, cx));

                ws.toggle_sort(id, 1, window, cx);
                assert_eq!(
                    ws.table_query(id, cx),
                    "SELECT * FROM \"public\".\"users\" WHERE id > 5 ORDER BY \"name\" ASC LIMIT 500"
                );
                ws.toggle_sort(id, 1, window, cx);
                assert_eq!(
                    ws.table_query(id, cx),
                    "SELECT * FROM \"public\".\"users\" WHERE id > 5 ORDER BY \"name\" DESC LIMIT 500"
                );
                // Third click clears the sort and empties the input.
                ws.toggle_sort(id, 1, window, cx);
                assert_eq!(
                    ws.table_query(id, cx),
                    "SELECT * FROM \"public\".\"users\" WHERE id > 5 LIMIT 500"
                );

                // A hand-typed clause is used verbatim.
                ws.tab(id)
                    .unwrap()
                    .order_input
                    .update(cx, |i, cx| i.set_value("name DESC NULLS LAST", window, cx));
                assert_eq!(
                    ws.table_query(id, cx),
                    "SELECT * FROM \"public\".\"users\" WHERE id > 5 ORDER BY name DESC NULLS LAST LIMIT 500"
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
    fn close_other_tabs_keeps_one(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                seed_query_tab(ws, window, cx);
                let mid = seed_query_tab(ws, window, cx);
                seed_query_tab(ws, window, cx);
                ws.close_other_tabs(mid, cx);
                assert_eq!(ws.tabs.len(), 1);
                assert_eq!(ws.tabs[0].id, mid);
                assert_eq!(ws.active, Some(0));
            })
            .unwrap();
    }

    #[gpui::test]
    fn close_tabs_right_truncates(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let first = seed_query_tab(ws, window, cx);
                seed_query_tab(ws, window, cx);
                seed_query_tab(ws, window, cx);
                ws.close_tabs_right(first, cx);
                assert_eq!(ws.tabs.len(), 1);
                assert_eq!(ws.active, Some(0));
            })
            .unwrap();
    }

    #[gpui::test]
    fn close_tabs_right_keeps_active_left_of_anchor(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                seed_query_tab(ws, window, cx);
                let mid = seed_query_tab(ws, window, cx);
                seed_query_tab(ws, window, cx);
                ws.activate_tab(0, window, cx);
                ws.close_tabs_right(mid, cx);
                assert_eq!(ws.tabs.len(), 2);
                assert_eq!(ws.active, Some(0));
            })
            .unwrap();
    }

    #[gpui::test]
    fn close_tabs_right_on_last_is_noop(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                seed_query_tab(ws, window, cx);
                let last = seed_query_tab(ws, window, cx);
                ws.close_tabs_right(last, cx);
                assert_eq!(ws.tabs.len(), 2);
                assert_eq!(ws.active, Some(1));
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
    fn switching_tables_preserves_state(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                // Open table A and select a row (both the widget's own highlight
                // and our `current_row`).
                ws.open_table_tab("public".into(), "aaa".into(), window, cx);
                let a = ws.active_id().unwrap();
                let a_table = ws.tab(a).unwrap().table.clone();
                seed_rows(ws, a, vec!["id"], 5, cx);
                a_table.update(cx, |ts, cx| ts.set_selected_row(4, cx));
                ws.set_cursor(a, 4, 0, cx);
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
}
