//! Inline editing: cell edits, row add/delete, and the staged-edit batch
//! (review, apply-in-one-transaction, cancel).

use super::*;

impl Workspace {
    pub(super) fn set_current_row(&mut self, tab_id: u64, row: usize, cx: &mut Context<Self>) {
        if let Some(tab) = self.tab_mut(tab_id) {
            tab.current_row = Some(row);
        }
        cx.notify();
    }

    // ---- inline editing --------------------------------------------------

    pub(super) fn begin_edit(
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

    pub(super) fn cancel_cell_edit(&mut self, cx: &mut Context<Self>) {
        if let Some(tab) = self.active_tab_mut() {
            if tab.editing.take().is_some() {
                cx.notify();
            }
        }
    }

    /// Stage the active tab's cell edit (build the UPDATE and show it for review).
    pub(super) fn commit_cell_edit(&mut self, cx: &mut Context<Self>) {
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
    pub(super) fn remove_pending_update(&mut self, tab_id: u64, pk: &[(String, CellValue)], col: &str) {
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
    pub(super) fn add_row(&mut self, tab_id: u64, cx: &mut Context<Self>) {
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
    pub(super) fn save_new_row(&mut self, tab_id: u64, cx: &mut Context<Self>) {
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
    pub(super) fn delete_current_row(&mut self, tab_id: u64, cx: &mut Context<Self>) {
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

    pub(super) fn refresh_table(&mut self, tab_id: u64, cx: &mut Context<Self>) {
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
    pub(super) fn stage(&mut self, tab_id: u64, edit: RowEdit, target: &EditTarget, cx: &mut Context<Self>) {
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
    pub(super) fn pending_sql(&self, tab_id: u64) -> Option<String> {
        let tab = self.tab(tab_id)?;
        let target = tab.edit_target.as_ref()?;
        zdb_db::build_batch(target, &tab.pending).ok().flatten()
    }

    pub(super) fn cancel_pending(&mut self, tab_id: u64, cx: &mut Context<Self>) {
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
    pub(super) fn apply_pending(&mut self, tab_id: u64, cx: &mut Context<Self>) {
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
        // The future owns the edits and hands them back for retry-on-failure.
        let fut = async move {
            let res = db.apply_edits(conn, &target, &edits).await;
            (res, edits)
        };
        self.spawn_db(cx, fut, move |this, (res, edits), cx| {
            let status = match res {
                Ok(()) => {
                    this.log.push(&sql, true);
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
                    this.log.push(&sql, false);
                    format!("Apply failed: {e}")
                }
            };
            this.status = status;
        });
        cx.notify();
    }

    /// Primary-key (name, original value) pairs for a result row in `tab_id`.
    pub(super) fn row_pk(
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::test_support::*;
    use gpui::TestAppContext;

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
    fn inline_edit_stages_update_sql(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                editable_tab(ws, id, vec!["id", "name"]);
                ws.tab_mut(id).unwrap().rows = vec![vec![
                    CellValue::Text("1".into()),
                    CellValue::Text("alpha".into()),
                ]];

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
                editable_tab(ws, id, vec!["id", "name"]);
                ws.tab_mut(id).unwrap().rows = vec![
                    vec![CellValue::Text("1".into()), CellValue::Text("a".into())],
                    vec![CellValue::Text("2".into()), CellValue::Text("b".into())],
                ];

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
                editable_tab(ws, id, vec!["id", "name"]);
                {
                    let tab = ws.tab_mut(id).unwrap();
                    tab.rows = vec![vec![
                        CellValue::Text("1".into()),
                        CellValue::Text("alpha".into()),
                    ]];
                    tab.orig_rows = tab.rows.clone();
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
}
