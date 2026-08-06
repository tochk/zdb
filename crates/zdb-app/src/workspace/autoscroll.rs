//! Middle-click autoscroll for the result grid (the Windows convention).
//!
//! Pressing the wheel button over a grid drops an anchor and latches the mode;
//! from then on the grid scrolls continuously in *both* axes at a speed
//! proportional to how far the cursor sits from that anchor. Any click exits,
//! and so does releasing the wheel after an actual drag (press-drag-release
//! reads as a one-shot pan, exactly like Explorer).
//!
//! The scroll itself pokes the table widget's two scroll handles directly —
//! `TableState::{vertical,horizontal}_scroll_handle` are public, and both wrap a
//! gpui `ScrollHandle` whose offset is a shared `Rc`. `ScrollHandle::set_offset`
//! does NOT clamp, so we clamp against `max_offset()` ourselves (offsets run
//! from `-max` at the far end to `0` at the top/left).

use super::*;
use gpui::{point, Pixels, Point, Task};

/// No scrolling until the cursor leaves this radius around the anchor, so the
/// mode is usable without a rock-steady hand.
const DEAD_ZONE: f32 = 12.0;
/// Pixels scrolled per tick, per pixel of cursor distance beyond the dead zone.
const SPEED: f32 = 0.18;
/// Ceiling per tick, so a flick to the window edge stays controllable.
const MAX_SPEED: f32 = 60.0;
/// Movement past this (while the button is held) makes the gesture a drag: the
/// release then ends it instead of latching the mode.
const DRAG_THRESHOLD: f32 = 8.0;
/// Tick period — one animation frame at 60Hz.
const TICK_MS: u64 = 16;

pub(crate) struct AutoScroll {
    /// The grid being scrolled (looked up by id every tick — it can be closed).
    tab_id: u64,
    /// Window coordinates of the wheel press.
    pub(crate) anchor: Point<Pixels>,
    /// Latest cursor position; the scroll speed is derived from `cursor - anchor`.
    cursor: Point<Pixels>,
    /// Whether the cursor left `DRAG_THRESHOLD` while the button was held.
    dragged: bool,
    /// The repeating tick; dropping it stops the scrolling.
    _task: Task<()>,
}

/// Scroll amount for one tick along one axis, given the cursor's distance from
/// the anchor on that axis. Pure so the feel is testable without a window.
fn axis_speed(distance: f32) -> f32 {
    let mag = distance.abs();
    if mag <= DEAD_ZONE {
        return 0.0;
    }
    let speed = ((mag - DEAD_ZONE) * SPEED).min(MAX_SPEED);
    speed * distance.signum()
}

impl Workspace {
    /// Wheel press over a grid: latch autoscroll, or exit if it's already on.
    pub(super) fn toggle_autoscroll(
        &mut self,
        tab_id: u64,
        at: Point<Pixels>,
        cx: &mut Context<Self>,
    ) {
        if self.autoscroll.is_some() {
            self.stop_autoscroll(cx);
            return;
        }
        let task = cx.spawn(async move |this, cx| loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(TICK_MS))
                .await;
            let alive = this
                .update(cx, |this, cx| this.autoscroll_tick(cx))
                .unwrap_or(false);
            if !alive {
                break;
            }
        });
        self.autoscroll = Some(AutoScroll {
            tab_id,
            anchor: at,
            cursor: at,
            dragged: false,
            _task: task,
        });
        cx.notify();
    }

    pub(super) fn stop_autoscroll(&mut self, cx: &mut Context<Self>) {
        if self.autoscroll.take().is_some() {
            cx.notify();
        }
    }

    /// Track the cursor while the mode is on (bound on the workspace root, so it
    /// keeps working when the pointer leaves the grid).
    pub(super) fn autoscroll_move(&mut self, at: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(auto) = self.autoscroll.as_mut() else {
            return;
        };
        auto.cursor = at;
        let dx = f32::from(at.x - auto.anchor.x);
        let dy = f32::from(at.y - auto.anchor.y);
        if !auto.dragged && (dx * dx + dy * dy).sqrt() > DRAG_THRESHOLD {
            auto.dragged = true;
        }
        cx.notify();
    }

    /// Wheel released: a press-drag-release pan ends here; a plain click leaves
    /// the mode latched.
    pub(super) fn autoscroll_release(&mut self, cx: &mut Context<Self>) {
        if self.autoscroll.as_ref().is_some_and(|a| a.dragged) {
            self.stop_autoscroll(cx);
        }
    }

    pub(super) fn autoscroll_anchor(&self) -> Option<Point<Pixels>> {
        self.autoscroll.as_ref().map(|a| a.anchor)
    }

    /// One tick of scrolling. Returns false once the mode is over (or its tab is
    /// gone), which ends the tick loop.
    fn autoscroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(auto) = self.autoscroll.as_ref() else {
            return false;
        };
        let (tab_id, dx, dy) = (
            auto.tab_id,
            axis_speed(f32::from(auto.cursor.x - auto.anchor.x)),
            axis_speed(f32::from(auto.cursor.y - auto.anchor.y)),
        );
        let Some(table) = self.tab(tab_id).map(|t| t.table.clone()) else {
            self.stop_autoscroll(cx);
            return false;
        };
        if dx == 0.0 && dy == 0.0 {
            return true;
        }
        table.update(cx, |state, cx| {
            // Horizontal handle derefs to the base `ScrollHandle`; the vertical
            // (uniform-list) one keeps its base handle inside a RefCell.
            let h = state.horizontal_scroll_handle.clone();
            let v = state.vertical_scroll_handle.0.borrow().base_handle.clone();
            let (ho, vo) = (h.offset(), v.offset());
            let x = clamp_offset(ho.x - px(dx), h.max_offset().width);
            let y = clamp_offset(vo.y - px(dy), v.max_offset().height);
            if x != ho.x {
                h.set_offset(point(x, ho.y));
            }
            if y != vo.y {
                v.set_offset(point(vo.x, y));
            }
            cx.notify();
        });
        true
    }
}

/// Scroll offsets go from `-max` (scrolled to the end) up to `0` (at the start);
/// `ScrollHandle::set_offset` stores whatever it's given, so clamp here.
fn clamp_offset(value: Pixels, max: Pixels) -> Pixels {
    value.clamp(-max.max(px(0.)), px(0.))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::test_support::*;
    use gpui::TestAppContext;

    #[test]
    fn dead_zone_then_proportional_speed_capped() {
        assert_eq!(axis_speed(0.0), 0.0);
        assert_eq!(axis_speed(DEAD_ZONE), 0.0);
        assert_eq!(axis_speed(-DEAD_ZONE), 0.0);
        // Past the dead zone, speed grows with distance and keeps the sign.
        assert!(axis_speed(DEAD_ZONE + 10.0) > 0.0);
        assert_eq!(axis_speed(-(DEAD_ZONE + 10.0)), -axis_speed(DEAD_ZONE + 10.0));
        assert!(axis_speed(DEAD_ZONE + 100.0) > axis_speed(DEAD_ZONE + 10.0));
        // And is capped.
        assert_eq!(axis_speed(100_000.0), MAX_SPEED);
    }

    #[test]
    fn offsets_clamp_to_the_scrollable_range() {
        assert_eq!(clamp_offset(px(20.), px(100.)), px(0.));
        assert_eq!(clamp_offset(px(-40.), px(100.)), px(-40.));
        assert_eq!(clamp_offset(px(-140.), px(100.)), px(-100.));
        // Nothing to scroll: pinned at 0.
        assert_eq!(clamp_offset(px(-10.), px(0.)), px(0.));
    }

    #[gpui::test]
    fn wheel_press_latches_and_a_second_press_exits(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                ws.toggle_autoscroll(id, point(px(100.), px(100.)), cx);
                assert_eq!(ws.autoscroll_anchor(), Some(point(px(100.), px(100.))));

                // A small move stays a click; releasing keeps the mode latched.
                ws.autoscroll_move(point(px(103.), px(102.)), cx);
                ws.autoscroll_release(cx);
                assert!(ws.autoscroll.is_some());

                // A second press exits.
                ws.toggle_autoscroll(id, point(px(100.), px(100.)), cx);
                assert!(ws.autoscroll.is_none());
            })
            .unwrap();
    }

    #[gpui::test]
    fn releasing_after_a_drag_exits(cx: &mut TestAppContext) {
        let window = new_workspace(cx);
        window
            .update(cx, |ws, window, cx| {
                let id = seed_query_tab(ws, window, cx);
                ws.toggle_autoscroll(id, point(px(10.), px(10.)), cx);
                ws.autoscroll_move(point(px(200.), px(180.)), cx);
                ws.autoscroll_release(cx);
                assert!(ws.autoscroll.is_none());
            })
            .unwrap();
    }
}
