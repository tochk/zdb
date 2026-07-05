//! Shared `#[cfg(test)]` helpers used by the per-module workspace tests.

use super::*;
use gpui::TestAppContext;

pub(super) fn new_workspace(cx: &mut TestAppContext) -> gpui::WindowHandle<Workspace> {
    cx.update(|cx| gpui_component::init(cx));
    let db = DbHandle::spawn();
    cx.add_window(|window, cx| Workspace::new(db, Settings::default(), None, window, cx))
}

/// Open a blank Query tab and return its id (for tests needing a result grid).
pub(super) fn seed_query_tab(
    ws: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) -> u64 {
    ws.open_query_tab(window, cx);
    ws.active_id().expect("active tab")
}

/// Make `tab_id` an editable result over `public.widget(id pk, …)`.
pub(super) fn editable_tab(ws: &mut Workspace, tab_id: u64, headers: Vec<&str>) {
    let tab = ws.tab_mut(tab_id).expect("tab");
    tab.headers = headers.iter().map(|s| s.to_string()).collect();
    tab.edit_target = Some(EditTarget {
        schema: "public".into(),
        table: "widget".into(),
        pk_columns: vec!["id".into()],
    });
    tab.edit_cols = headers.iter().map(|s| Some(s.to_string())).collect();
}
