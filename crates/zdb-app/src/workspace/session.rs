//! Per-connection session: which tabs are open (order, active one, each tab's
//! SQL / WHERE / ORDER BY) is saved under the connection's id and reopened on
//! the next connect to it. Rows are never stored — a restored Table tab
//! re-queries the first time it is shown, a Query tab waits for Run.

use super::*;
use zdb_config::{SavedTab, SavedTabKind, SavedTabs};

/// Session key of the env-var (`ZDB_HOST`) connection, which has no saved
/// `ConnectionEntry` and so no id of its own.
pub(super) const ENV_CONN_KEY: &str = "env";

/// The session files. Tests run against the dev machine's real config
/// directory, so there they get an in-memory stand-in instead.
#[cfg(not(test))]
pub(super) mod store {
    use zdb_config::SavedTabs;

    pub(crate) fn load_tabs(key: &str) -> Option<SavedTabs> {
        zdb_config::load_tabs(key).unwrap_or_else(|e| {
            crate::workspace::log(format!("saved tabs ignored: {e:#}"));
            None
        })
    }

    pub(crate) fn save_tabs(key: &str, tabs: &SavedTabs) {
        if let Err(e) = zdb_config::save_tabs(key, tabs) {
            crate::workspace::log(format!("saving tabs failed: {e:#}"));
        }
    }

    pub(crate) fn load_scratch(key: Option<&str>) -> String {
        zdb_config::load_scratch_for(key)
    }

    pub(crate) fn save_scratch(key: Option<&str>, text: &str) {
        zdb_config::save_scratch_for(key, text);
    }
}

#[cfg(test)]
pub(super) mod store {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use zdb_config::SavedTabs;

    thread_local! {
        static TABS: RefCell<HashMap<String, SavedTabs>> = RefCell::default();
        static SCRATCH: RefCell<HashMap<Option<String>, String>> = RefCell::default();
    }

    pub(crate) fn load_tabs(key: &str) -> Option<SavedTabs> {
        TABS.with_borrow(|m| m.get(key).cloned())
    }

    pub(crate) fn save_tabs(key: &str, tabs: &SavedTabs) {
        TABS.with_borrow_mut(|m| m.insert(key.to_string(), tabs.clone()));
    }

    pub(crate) fn load_scratch(key: Option<&str>) -> String {
        SCRATCH.with_borrow(|m| {
            m.get(&key.map(str::to_string))
                .or_else(|| m.get(&None))
                .cloned()
                .unwrap_or_default()
        })
    }

    pub(crate) fn save_scratch(key: Option<&str>, text: &str) {
        SCRATCH.with_borrow_mut(|m| m.insert(key.map(str::to_string), text.to_string()));
    }
}

/// The number for the next "Query N" tab: one past the highest in use, so a
/// restored "Query 3" is never joined by a second one.
pub(super) fn next_query_n<'a>(titles: impl Iterator<Item = &'a str>) -> u64 {
    titles
        .filter_map(|t| t.strip_prefix("Query ")?.parse::<u64>().ok())
        .max()
        .unwrap_or(0)
        + 1
}

impl Workspace {
    /// The open tabs as they would be written to disk.
    pub(super) fn snapshot_session(&self, cx: &App) -> SavedTabs {
        let tabs = self
            .tabs
            .iter()
            .map(|tab| SavedTab {
                title: tab.title.clone(),
                kind: match &tab.kind {
                    TabKind::Query => SavedTabKind::Query {
                        sql: tab.editor.read(cx).value().to_string(),
                    },
                    TabKind::Scratch => SavedTabKind::Scratch,
                    TabKind::Table { schema, table } => SavedTabKind::Table {
                        schema: schema.clone(),
                        table: table.clone(),
                        filter: tab.where_input.read(cx).value().to_string(),
                        order_by: tab.order_input.read(cx).value().to_string(),
                        limit: Some(tab.limit_input.read(cx).value().to_string()),
                    },
                },
            })
            .collect();
        SavedTabs::new(self.active, tabs)
    }

    /// Something a saved session records has changed: schedule one write half a
    /// second out (typing fires this per keystroke), so the set also survives an
    /// exit that never reaches a close hook.
    pub(super) fn mark_session_dirty(&mut self, cx: &mut Context<Self>) {
        if !self.session_ready || self.session_flush {
            return;
        }
        self.session_flush = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(500))
                .await;
            let _ = this.update(cx, |this, cx| {
                this.session_flush = false;
                this.flush_session(cx);
            });
        })
        .detach();
    }

    /// Write the current tab set now, if it differs from what is on disk.
    /// A no-op until this connection's saved set has been restored — otherwise
    /// a failed connect (no tabs open) would overwrite it with nothing.
    pub(super) fn flush_session(&mut self, cx: &App) {
        if !self.session_ready || !self.settings.restore_tabs {
            return;
        }
        let Some(key) = self.conn_key.clone() else {
            return;
        };
        let snapshot = self.snapshot_session(cx);
        if self.last_saved.as_ref() == Some(&snapshot) {
            return;
        }
        store::save_tabs(&key, &snapshot);
        self.last_saved = Some(snapshot);
    }

    /// Reopen the connection's saved tabs (once per connect; called from
    /// `render`, which has the window). Tabs opened while not connected belong
    /// to no connection and are replaced.
    pub(super) fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The selftest opens its own tab and must start from a known state.
        if self.session_ready || std::env::var_os("ZDB_SELFTEST").is_some() {
            return;
        }
        let Some(key) = self.conn_key.clone() else {
            return;
        };
        if !self.settings.restore_tabs {
            return;
        }
        self.close_all_tabs(cx);
        let saved = store::load_tabs(&key);
        for tab in saved.iter().flat_map(|s| &s.tabs) {
            let id = self.next_tab_id;
            let slot = self.lsp_slot.clone();
            let restored = match &tab.kind {
                SavedTabKind::Query { sql } => {
                    let mut t = Tab::query(id, 0, slot, window, cx);
                    t.title = tab.title.clone();
                    t.editor
                        .update(cx, |i, cx| i.set_value(sql.clone(), window, cx));
                    t
                }
                SavedTabKind::Scratch => {
                    if self.tabs.iter().any(|t| matches!(t.kind, TabKind::Scratch)) {
                        continue;
                    }
                    Tab::scratch(id, Some(key.clone()), slot, window, cx)
                }
                SavedTabKind::Table {
                    schema,
                    table,
                    filter,
                    order_by,
                    limit,
                } => {
                    if self.tabs.iter().any(|t| {
                        matches!(&t.kind, TabKind::Table { schema: s, table: tt }
                            if s == schema && tt == table)
                    }) {
                        continue;
                    }
                    let mut t = Tab::table(id, schema.clone(), table.clone(), slot, window, cx);
                    t.where_input
                        .update(cx, |i, cx| i.set_value(filter.clone(), window, cx));
                    t.order_input
                        .update(cx, |i, cx| i.set_value(order_by.clone(), window, cx));
                    // Files from before the LIMIT box keep its default.
                    if let Some(limit) = limit {
                        t.limit_input
                            .update(cx, |i, cx| i.set_value(limit.clone(), window, cx));
                    }
                    t.needs_load = true;
                    t
                }
            };
            self.next_tab_id += 1;
            self.tabs.push(restored);
        }
        self.session_ready = true;
        let active = saved
            .as_ref()
            .and_then(|s| s.active)
            .filter(|&i| i < self.tabs.len())
            .or((!self.tabs.is_empty()).then_some(0));
        self.last_saved = saved;
        if let Some(idx) = active {
            self.activate_tab(idx, window, cx);
        }
    }

    /// Run a restored Table tab's query the first time it becomes the active
    /// tab (called from `render`, so every way of switching tabs is covered).
    pub(super) fn load_restored_tab(&mut self, cx: &mut Context<Self>) {
        if self.conn.is_none() {
            return;
        }
        let Some(tab) = self.active_tab_mut().filter(|t| t.needs_load) else {
            return;
        };
        tab.needs_load = false;
        let id = tab.id;
        self.apply_where(id, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::test_support::*;
    use gpui::TestAppContext;

    #[test]
    fn next_query_number_skips_past_the_highest() {
        assert_eq!(next_query_n(std::iter::empty()), 1);
        assert_eq!(next_query_n(["Query 1", "Query 2"].into_iter()), 3);
        // A gap is not reused, and unrelated titles are ignored.
        assert_eq!(next_query_n(["Query 4", "Query 1", "notes"].into_iter()), 5);
    }

    /// Open a Query tab, a filtered Table tab and the scratch tab under `key`,
    /// with the Table tab active.
    fn seed_session(
        ws: &mut Workspace,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        ws.conn_key = Some(key.into());
        ws.session_ready = true;
        ws.open_query_tab(window, cx);
        ws.open_query_tab(window, cx);
        let first = ws.tabs[0].id;
        ws.close_tab(first, cx);
        ws.active_tab()
            .unwrap()
            .editor
            .clone()
            .update(cx, |i, cx| i.set_value("select 42", window, cx));
        ws.open_table_tab("public".into(), "users".into(), window, cx);
        let tab = ws.active_tab().unwrap();
        let (filter, order) = (tab.where_input.clone(), tab.order_input.clone());
        let limit = tab.limit_input.clone();
        filter.update(cx, |i, cx| i.set_value("id > 5", window, cx));
        order.update(cx, |i, cx| i.set_value("\"name\" DESC", window, cx));
        limit.update(cx, |i, cx| i.set_value("100", window, cx));
        ws.focus_scratch_tab(window, cx);
        ws.activate_tab(1, window, cx);
    }

    #[gpui::test]
    fn session_round_trips_through_a_connection_switch(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                seed_session(ws, "conn-a", window, cx);
                let before = ws.snapshot_session(cx);
                assert_eq!(before.tabs.len(), 3);

                // Switching away saves A's set and leaves B with none of it.
                let cfg = ConnectionConfig::new("b", "127.0.0.1", "d", "u");
                ws.switch_connection(cfg.clone(), Some("conn-b".into()), cx);
                assert!(ws.tabs.is_empty());
                assert_eq!(store::load_tabs("conn-a"), Some(before.clone()));
                ws.restore_session(window, cx);
                assert!(ws.tabs.is_empty());

                // Back on A: same tabs, order, texts and active tab.
                ws.switch_connection(cfg, Some("conn-a".into()), cx);
                ws.restore_session(window, cx);
                assert_eq!(ws.snapshot_session(cx), before);
                assert_eq!(ws.active, Some(1));
                assert_eq!(ws.tabs[0].title, "Query 2");
                // Nothing was queried: the Table tab waits to be shown, the
                // Query tab for Run.
                assert!(ws.tabs[1].needs_load);
                assert!(ws.tabs.iter().all(|t| t.last_sql.is_none()));
                assert_eq!(
                    ws.table_query(ws.tabs[1].id, cx),
                    "SELECT * FROM \"public\".\"users\" WHERE id > 5 ORDER BY \"name\" DESC LIMIT 100"
                );
                // A new tab doesn't reuse the restored tab's number.
                ws.open_query_tab(window, cx);
                assert_eq!(ws.active_tab().unwrap().title, "Query 3");
            })
            .unwrap();
    }

    #[gpui::test]
    fn failed_connect_keeps_the_saved_set(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                seed_session(ws, "conn-a", window, cx);
                ws.flush_session(cx);
                let saved = store::load_tabs("conn-a").expect("saved");

                // Reconnect to A, but the restore never runs (connect failed).
                // Whatever happens to the tabs meanwhile must not reach disk.
                let cfg = ConnectionConfig::new("a", "127.0.0.1", "d", "u");
                ws.switch_connection(cfg, Some("conn-a".into()), cx);
                ws.open_query_tab(window, cx);
                ws.flush_session(cx);
                ws.close_all_tabs(cx);
                ws.flush_session(cx);
                assert_eq!(store::load_tabs("conn-a"), Some(saved));
            })
            .unwrap();
    }

    #[gpui::test]
    fn setting_off_neither_restores_nor_saves(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                seed_session(ws, "conn-a", window, cx);
                ws.flush_session(cx);
                let saved = store::load_tabs("conn-a").expect("saved");

                ws.settings.restore_tabs = false;
                let cfg = ConnectionConfig::new("a", "127.0.0.1", "d", "u");
                ws.switch_connection(cfg.clone(), Some("conn-a".into()), cx);
                ws.restore_session(window, cx);
                assert!(ws.tabs.is_empty());
                ws.open_query_tab(window, cx);
                ws.flush_session(cx);
                // Turning it back on mid-session doesn't start saving over the
                // old set either; that is back on the next connect.
                ws.settings.restore_tabs = true;
                ws.flush_session(cx);
                assert_eq!(store::load_tabs("conn-a"), Some(saved.clone()));
                ws.switch_connection(cfg, Some("conn-a".into()), cx);
                ws.restore_session(window, cx);
                assert_eq!(ws.snapshot_session(cx), saved);
            })
            .unwrap();
    }

    #[gpui::test]
    fn scratch_text_is_per_connection(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        store::save_scratch(None, "global");
        store::save_scratch(Some("conn-a"), "for a");
        window
            .update(cx, |ws, window, cx| {
                let text = |ws: &Workspace, cx: &App| {
                    ws.active_tab().unwrap().editor.read(cx).value().to_string()
                };
                // Not connected: the global scratch.
                ws.focus_scratch_tab(window, cx);
                assert_eq!(text(ws, cx), "global");
                ws.close_all_tabs(cx);
                ws.conn_key = Some("conn-a".into());
                ws.focus_scratch_tab(window, cx);
                assert_eq!(text(ws, cx), "for a");
                // A connection without its own scratch starts from the global one.
                ws.close_all_tabs(cx);
                ws.conn_key = Some("conn-b".into());
                ws.focus_scratch_tab(window, cx);
                assert_eq!(text(ws, cx), "global");
            })
            .unwrap();
    }
}
