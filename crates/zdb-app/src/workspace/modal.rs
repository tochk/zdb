//! Dialogs (connections, settings, shortcut reference, command palette). At
//! most one is open, and while it is it owns the mouse and the keyboard:
//! - Mouse: the backdrop `occlude()`s every pane under it, and a press on it
//!   closes the dialog. It starts BELOW the title bar, so the window can still
//!   be moved / minimized / closed (an occluded title bar would swallow those).
//! - Keyboard: the panel is a focus target. `sync_modal_focus` (render) pulls
//!   focus into it when it opens and hands it back when it closes, Tab cycles
//!   only its own controls (`step_focus`), and the pane / tab / query chords
//!   are swallowed while it is up (`modal_open`, checked by their handlers).

use super::*;
use gpui::{FocusHandle, MouseButton, MouseDownEvent, Pixels, WeakFocusHandle};

/// Height of the custom title bar (`view.rs::render_titlebar`).
pub(super) const TITLEBAR_H: Pixels = px(34.);

/// Upper bound on tab stops walked looking for one inside the dialog.
const MAX_TAB_STEPS: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Modal {
    Connections,
    Settings,
    Shortcuts,
    Palette,
    /// "Cancel the running query?" — a click on a tab's loader asks it.
    ConfirmCancel(u64),
    /// A tab's staged changes as statements, to review before they run.
    ConfirmApply(u64),
}

/// Dialog focus bookkeeping (see the module docs).
pub(super) struct ModalFocus {
    /// Tracked by the open dialog's panel.
    pub(super) panel: FocusHandle,
    /// Where focus was before the dialog took it.
    restore: Option<WeakFocusHandle>,
}

impl ModalFocus {
    pub(super) fn new(cx: &mut App) -> Self {
        Self {
            panel: cx.focus_handle(),
            restore: None,
        }
    }
}

impl Workspace {
    pub(super) fn modal_open(&self) -> bool {
        self.modal.is_some()
    }

    pub(super) fn is_modal(&self, m: Modal) -> bool {
        self.modal == Some(m)
    }

    /// Show `m` in place of whatever dialog is up, or close it if it already is.
    pub(super) fn toggle_modal(&mut self, m: Modal, cx: &mut Context<Self>) {
        self.modal = (self.modal != Some(m)).then_some(m);
        cx.notify();
    }

    pub(super) fn close_modal(&mut self, cx: &mut Context<Self>) {
        if self.modal.take().is_some() {
            cx.notify();
        }
    }

    /// Called from `render`: keep the keyboard inside an open dialog, and give
    /// it back to where it was once the dialog closes.
    pub(super) fn sync_modal_focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let inside = self.modal_focus.panel.contains_focused(window, cx);
        if self.modal.is_none() {
            let restore = self.modal_focus.restore.take();
            // A palette command may have moved focus on purpose — only undo
            // focus the dialog itself was holding.
            if inside || window.focused(cx).is_none() {
                if let Some(handle) = restore.and_then(|h| h.upgrade()) {
                    handle.focus(window);
                }
            }
            return;
        }
        if inside {
            return;
        }
        if self.modal_focus.restore.is_none() {
            self.modal_focus.restore = window.focused(cx).map(|h| h.downgrade());
        }
        // The palette is typed into straight away. (Focusing an input needs the
        // window's `Root`, which headless test windows don't have.)
        if self.is_modal(Modal::Palette)
            && window.root::<gpui_component::Root>().flatten().is_some()
        {
            self.palette_input.read(cx).focus_handle(cx).focus(window);
        } else {
            self.modal_focus.panel.focus(window);
        }
    }

    /// Tab / Shift+Tab outside the panes that use the key themselves (grid
    /// cells, code-editor indent, the terminal). gpui's tab order spans the whole
    /// window, so with a dialog up we keep stepping until focus lands on one of
    /// ITS controls instead of wandering into the panes behind it.
    pub(super) fn step_focus(
        &mut self,
        forward: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let step = |window: &mut Window| {
            if forward {
                window.focus_next()
            } else {
                window.focus_prev()
            }
        };
        if self.modal_open() {
            for _ in 0..MAX_TAB_STEPS {
                step(window);
                if self.modal_focus.panel.contains_focused(window, cx) {
                    return;
                }
            }
            // Nothing in it takes focus: keep the keyboard on the panel.
            self.modal_focus.panel.focus(window);
            return;
        }
        step(window);
        // Tabbing into the grid puts the cursor on a cell, so there's
        // something to see (and to move from).
        if let Some((id, true)) = self
            .active_tab()
            .map(|t| (t.id, t.cursor.is_none() && t.grid_focus.is_focused(window)))
        {
            self.set_cursor(id, 0, 0, cx);
        }
    }

    /// The shared dialog shell: a dimmed, occluding backdrop under the title
    /// bar, with `panel` (sized and placed by the caller) styled as a dialog
    /// card on top of it.
    pub(super) fn modal_layer(&self, panel: gpui::Div, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        div()
            .absolute()
            .top(TITLEBAR_H)
            .left_0()
            .right_0()
            .bottom_0()
            .flex()
            .justify_center()
            .items_start()
            .bg(rgba(0x00000066))
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| this.close_modal(cx)),
            )
            .child(
                panel
                    .track_focus(&self.modal_focus.panel)
                    // Occluding too: a press on the card must not reach the
                    // backdrop's close handler.
                    .occlude()
                    .bg(c.panel)
                    .border_1()
                    .border_color(c.border)
                    .rounded_md()
                    .shadow_lg()
                    .overflow_hidden(),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::test_support::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn one_dialog_at_a_time(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                // No saved connections and no env connection: the manager opens.
                assert!(ws.is_modal(Modal::Connections));
                ws.toggle_settings(cx);
                assert!(ws.is_modal(Modal::Settings));
                // F1 from settings swaps dialogs instead of stacking them.
                ws.toggle_shortcuts(cx);
                assert!(ws.is_modal(Modal::Shortcuts));
                ws.toggle_shortcuts(cx);
                assert!(!ws.modal_open());
                ws.toggle_palette(cx);
                ws.close_modal(cx);
                assert!(!ws.modal_open());
            })
            .unwrap();
    }

    /// Settings → connections per database stays within its range.
    #[gpui::test]
    fn pool_size_setting_stays_in_range(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, _w, cx| {
                ws.set_pool_size(0, cx);
                assert_eq!(ws.settings.pool_size, 1);
                ws.set_pool_size(99, cx);
                assert_eq!(ws.settings.pool_size, zdb_config::POOL_SIZE_MAX);
                ws.set_pool_size(6, cx);
                assert_eq!(ws.settings.pool_size, 6);
            })
            .unwrap();
    }

    /// The pane / tab chords do nothing behind a dialog.
    #[gpui::test]
    fn chords_are_ignored_behind_a_dialog(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                ws.close_modal(cx);
                ws.open_query_tab(window, cx);
                ws.open_query_tab(window, cx);
                assert_eq!(ws.active, Some(1));
                ws.toggle_settings(cx);
                window.dispatch_action(Box::new(NextTab), cx);
                window.dispatch_action(Box::new(CloseTab), cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |ws, window, cx| {
                assert_eq!((ws.tabs.len(), ws.active), (2, Some(1)));
                ws.close_modal(cx);
                window.dispatch_action(Box::new(NextTab), cx);
            })
            .unwrap();
        cx.run_until_parked();
        window
            .update(cx, |ws, _w, _cx| assert_eq!(ws.active, Some(0)))
            .unwrap();
    }
}
