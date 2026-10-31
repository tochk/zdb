//! The terminal's view, mouse and clipboard behaving like Zed's: drag / double /
//! triple click select (Shift+click extends), Ctrl+Shift+C or Ctrl+Insert (Cmd+C)
//! copy, the wheel scrolls back, a right click opens a menu. A program that asked
//! for the mouse gets it instead, unless Shift is held.
//!
//! The emulator is `alacritty_terminal`'s; this file feeds it the PTY's output and
//! paints its grid.

use crate::terminal::TerminalStyle;
use crate::workspace::{TerminalCopy, TerminalPaste, TerminalSelectAll};
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{point_to_viewport, viewport_to_point, Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape, Processor};
use gpui::prelude::*;
use gpui::{
    anchored, canvas, deferred, div, fill, outline, point, px, size, App, AsyncApp, BorderStyle,
    Bounds, ClipboardItem, Context, DismissEvent, DispatchPhase, Entity, FocusHandle, Focusable,
    Font, FontFeatures, FontStyle, FontWeight, Hitbox, HitboxBehavior, Hsla, KeyDownEvent,
    Keystroke, Modifiers, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    Render, ScrollEvent, SharedString, Size, Task, TextAlign, TextRun, UnderlineStyle, WeakEntity,
    Window,
};
use gpui_component::menu::PopupMenu;
use std::cell::{Cell, RefCell};
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::mpsc;

/// Space around the grid.
const PADDING: Pixels = px(6.);
const LINE_HEIGHT: f32 = 1.2;
/// A dim cell's foreground opacity.
const DIM_ALPHA: f32 = 0.7;

// The xterm mouse protocol's button codes.
const REPORT_MOTION: u8 = 32;
const REPORT_HOVER: u8 = 35;
const REPORT_WHEEL_UP: u8 = 64;
const REPORT_WHEEL_DOWN: u8 = 65;

/// The emulator's events we use: programs copy with OSC 52 (tmux, nvim over ssh).
/// Replies it wants written back (`PtyWrite`) are dropped, see `terminal::CursorReport`.
struct Events(mpsc::Sender<String>);

impl EventListener for Events {
    fn send_event(&self, event: Event) {
        if let Event::ClipboardStore(_, text) = event {
            let _ = self.0.send(text);
        }
    }
}

/// A grid size, as the emulator takes it.
struct GridSize {
    cols: usize,
    rows: usize,
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.rows
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// Where the grid was last painted: what maps the pointer to a cell.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Geometry {
    origin: Point<Pixels>,
    cell: Size<Pixels>,
    cols: usize,
    rows: usize,
}

impl Geometry {
    /// The viewport cell under `at`, clamped into the grid, and which half of
    /// it — a selection includes a cell only past its middle.
    fn cell_at(&self, at: Point<Pixels>) -> (GridPoint<usize>, Side) {
        let x = (at.x - self.origin.x) / self.cell.width;
        let y = (at.y - self.origin.y) / self.cell.height;
        let line = (y.max(0.) as usize).min(self.rows.saturating_sub(1));
        if x as usize >= self.cols {
            return (
                GridPoint::new(line, Column(self.cols.saturating_sub(1))),
                Side::Right,
            );
        }
        let x = x.max(0.);
        let side = if x.fract() > 0.5 {
            Side::Right
        } else {
            Side::Left
        };
        (GridPoint::new(line, Column(x as usize)), side)
    }

    /// Lines to scroll (positive = back) while a drag is above / below the grid.
    fn drag_scroll(&self, y: Pixels) -> i32 {
        let bottom = self.origin.y + self.cell.height * self.rows as f32;
        if y < self.origin.y {
            ((self.origin.y - y) / self.cell.height).ceil() as i32
        } else if y > bottom {
            -(((y - bottom) / self.cell.height).ceil() as i32)
        } else {
            0
        }
    }
}

pub struct TerminalView {
    term: Rc<RefCell<Term<Events>>>,
    parser: Processor,
    copied: mpsc::Receiver<String>,
    /// The shell's input.
    input: Box<dyn Write>,
    /// Tells the PTY the grid's size.
    on_resize: Box<dyn Fn(usize, usize)>,
    focus: FocusHandle,
    style: TerminalStyle,
    geometry: Rc<Cell<Option<Geometry>>>,
    /// A left drag is extending the selection.
    selecting: bool,
    /// The button a program was told went down, and hasn't heard go up.
    reported_button: Option<u8>,
    /// The cell last reported as hovered: a program hears about each cell once.
    reported_cell: Option<GridPoint<usize>>,
    /// Wheel movement short of a whole line.
    scroll_rest: f32,
    menu: Option<(Entity<PopupMenu>, Point<Pixels>)>,
    _reader: Task<()>,
}

impl TerminalView {
    pub fn new(
        input: impl Write + 'static,
        mut output: impl Read + Send + 'static,
        on_resize: impl Fn(usize, usize) + 'static,
        style: TerminalStyle,
        cx: &mut Context<Self>,
    ) -> Self {
        let (copied_tx, copied) = mpsc::channel();
        let (bytes_tx, bytes_rx) = flume::unbounded::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = output.read(&mut buf) {
                if n == 0 || bytes_tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        let reader = cx.spawn(async move |this: WeakEntity<Self>, cx: &mut AsyncApp| {
            while let Ok(bytes) = bytes_rx.recv_async().await {
                let alive = this.update(cx, |view, cx| {
                    view.process(&bytes);
                    while let Ok(text) = view.copied.try_recv() {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        Self {
            term: Rc::new(RefCell::new(Term::new(
                Config::default(),
                &GridSize { cols: 80, rows: 24 },
                Events(copied_tx),
            ))),
            parser: Processor::new(),
            copied,
            input: Box::new(input),
            on_resize: Box::new(on_resize),
            focus: cx.focus_handle(),
            style,
            geometry: Rc::new(Cell::new(None)),
            selecting: false,
            reported_button: None,
            reported_cell: None,
            scroll_rest: 0.,
            menu: None,
            _reader: reader,
        }
    }

    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Repaint in another palette / font (the theme was switched).
    pub fn set_style(&mut self, style: TerminalStyle, cx: &mut Context<Self>) {
        self.style = style;
        cx.notify();
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut *self.term.borrow_mut(), bytes);
    }

    fn mode(&self) -> TermMode {
        *self.term.borrow().mode()
    }

    fn resize(&mut self, cols: usize, rows: usize) {
        self.term.borrow_mut().resize(GridSize { cols, rows });
    }

    fn write(&mut self, bytes: &[u8]) {
        let _ = self.input.write_all(bytes);
        let _ = self.input.flush();
    }

    /// Input for the shell: it drops the selection and shows the prompt again.
    fn send_input(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        {
            let mut term = self.term.borrow_mut();
            term.selection = None;
            term.scroll_display(Scroll::Bottom);
        }
        self.write(bytes);
        cx.notify();
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // The menu sits in this element, so unclaimed keys bubble here. Esc doesn't
        // reach its own handler: `"Terminal"` masks that key to keep it the shell's.
        if self.menu.is_some() {
            if event.keystroke.key == "escape" {
                self.menu = None;
                self.focus.focus(window, cx);
                cx.notify();
            }
            return;
        }
        // Cmd / Win chords are the app's and the OS's, never the shell's.
        if event.keystroke.modifiers.platform {
            return;
        }
        if let Some(bytes) = keystroke_bytes(&event.keystroke, self.mode()) {
            self.send_input(&bytes, cx);
        }
    }

    fn selected_text(&self) -> Option<String> {
        self.term.borrow().selection_to_string()
    }

    fn copy(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.paste_text(&text, cx);
        }
    }

    fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let bytes = paste_bytes(text, self.mode());
        self.send_input(bytes.as_bytes(), cx);
    }

    /// The whole buffer, scrollback included.
    fn select_all(&mut self, cx: &mut Context<Self>) {
        {
            let mut term = self.term.borrow_mut();
            let mut all = Selection::new(
                SelectionType::Simple,
                GridPoint::new(term.topmost_line(), Column(0)),
                Side::Left,
            );
            let last: GridPoint<Line> = GridPoint::new(term.bottommost_line(), term.last_column());
            all.update(last, Side::Right);
            term.selection = Some(all);
        }
        self.publish_selection(cx);
        cx.notify();
    }

    /// Start a selection at a viewport cell: a drag by characters, a double
    /// click's word or a triple click's line; Shift extends the current one.
    fn begin_selection(&mut self, cell: GridPoint<usize>, side: Side, clicks: usize, extend: bool) {
        self.selecting = true;
        let mut term = self.term.borrow_mut();
        let at = viewport_to_point(term.grid().display_offset(), cell);
        match term.selection.as_mut().filter(|_| extend && clicks == 1) {
            Some(selection) => selection.update(at, side),
            None => {
                let ty = match clicks {
                    0 | 1 => SelectionType::Simple,
                    2 => SelectionType::Semantic,
                    _ => SelectionType::Lines,
                };
                term.selection = Some(Selection::new(ty, at, side));
            }
        }
    }

    fn extend_selection(&mut self, cell: GridPoint<usize>, side: Side, scroll: i32) {
        let mut term = self.term.borrow_mut();
        if scroll != 0 {
            term.scroll_display(Scroll::Delta(scroll));
        }
        let at = viewport_to_point(term.grid().display_offset(), cell);
        if let Some(selection) = term.selection.as_mut() {
            selection.update(at, side);
        }
    }

    /// A finished selection is the X11 / Wayland primary selection, what a
    /// middle click pastes anywhere.
    fn publish_selection(&self, cx: &mut Context<Self>) {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(text) = self.selected_text() {
            cx.write_to_primary(ClipboardItem::new_string(text));
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        let _ = cx;
    }

    /// Whether the mouse is the program's right now. Shift takes it back.
    fn program_has_mouse(&self, modifiers: Modifiers) -> bool {
        !modifiers.shift && self.mode().intersects(TermMode::MOUSE_MODE)
    }

    fn report_mouse(
        &mut self,
        code: u8,
        cell: GridPoint<usize>,
        pressed: bool,
        modifiers: Modifiers,
    ) {
        let mods = u8::from(modifiers.shift) * 4
            + u8::from(modifiers.alt) * 8
            + u8::from(modifiers.control) * 16;
        if let Some(bytes) = mouse_report(code | mods, cell, pressed, self.mode()) {
            self.write(&bytes);
        }
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
        let Some((cell, side)) = self
            .geometry
            .get()
            .map(|geometry| geometry.cell_at(event.position))
        else {
            return;
        };
        if self.program_has_mouse(event.modifiers) {
            let code = match event.button {
                MouseButton::Left => 0,
                MouseButton::Middle => 1,
                MouseButton::Right => 2,
                _ => return,
            };
            self.reported_button = Some(code);
            self.report_mouse(code, cell, true, event.modifiers);
            return;
        }
        match event.button {
            MouseButton::Left => {
                self.begin_selection(cell, side, event.click_count, event.modifiers.shift)
            }
            MouseButton::Right => self.open_menu(event.position, window, cx),
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            MouseButton::Middle => {
                if let Some(text) = cx.read_from_primary().and_then(|item| item.text()) {
                    self.paste_text(&text, cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn mouse_moved(&mut self, event: &MouseMoveEvent, hovered: bool, cx: &mut Context<Self>) {
        let Some(geometry) = self.geometry.get() else {
            return;
        };
        let (cell, side) = geometry.cell_at(event.position);
        if self.selecting {
            // The release happened outside the window.
            if event.pressed_button != Some(MouseButton::Left) {
                self.selecting = false;
                self.publish_selection(cx);
                return;
            }
            self.extend_selection(cell, side, geometry.drag_scroll(event.position.y));
            cx.notify();
            return;
        }
        let mode = self.mode();
        let code = match self.reported_button {
            Some(button) if mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG) => {
                button + REPORT_MOTION
            }
            None if hovered && mode.contains(TermMode::MOUSE_MOTION) => REPORT_HOVER,
            _ => return,
        };
        if self.reported_cell != Some(cell) {
            self.reported_cell = Some(cell);
            self.report_mouse(code, cell, true, event.modifiers);
        }
    }

    fn mouse_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        if std::mem::take(&mut self.selecting) {
            self.publish_selection(cx);
        }
        if let (Some(button), Some(geometry)) = (self.reported_button.take(), self.geometry.get()) {
            let (cell, _) = geometry.cell_at(event.position);
            self.report_mouse(button, cell, false, event.modifiers);
        }
    }

    /// The wheel goes to the program that asked for it, turns into arrow keys in
    /// a full-screen one (`less`), and otherwise scrolls back.
    fn scroll_wheel(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(geometry) = self.geometry.get() else {
            return;
        };
        let moved = event.delta.pixel_delta(geometry.cell.height).y / geometry.cell.height
            + self.scroll_rest;
        let lines = moved.trunc();
        self.scroll_rest = moved - lines;
        let lines = lines as i32;
        if lines == 0 {
            return;
        }
        let mode = self.mode();
        if self.program_has_mouse(event.modifiers) {
            let (cell, _) = geometry.cell_at(event.position);
            let code = if lines > 0 {
                REPORT_WHEEL_UP
            } else {
                REPORT_WHEEL_DOWN
            };
            for _ in 0..lines.unsigned_abs() {
                self.report_mouse(code, cell, true, event.modifiers);
            }
        } else if mode.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
            && !event.modifiers.shift
        {
            self.write(&wheel_as_arrows(lines, mode));
        } else {
            self.term.borrow_mut().scroll_display(Scroll::Delta(lines));
            cx.notify();
        }
    }

    fn open_menu(&mut self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let nothing_selected = self.selected_text().is_none();
        let focus = self.focus.clone();
        let menu = PopupMenu::build(window, cx, |menu, _, _| {
            // Dismissing hands focus back to the terminal.
            menu.action_context(focus)
                .menu_with_disabled("Copy", Box::new(TerminalCopy), nothing_selected)
                .menu("Paste", Box::new(TerminalPaste))
                .separator()
                .menu("Select All", Box::new(TerminalSelectAll))
        });
        cx.subscribe(&menu, |this, _, _: &DismissEvent, cx| {
            this.menu = None;
            cx.notify();
        })
        .detach();
        menu.focus_handle(cx).focus(window, cx);
        self.menu = Some((menu, at));
    }

    /// Fit the grid to `bounds` and return where it now is.
    fn lay_out(&mut self, bounds: Bounds<Pixels>, window: &mut Window) -> Geometry {
        let cell = cell_size(&self.style, window);
        let width = bounds.size.width - PADDING * 2.;
        let height = bounds.size.height - PADDING * 2.;
        let geometry = Geometry {
            origin: bounds.origin + point(PADDING, PADDING),
            cell,
            cols: ((width / cell.width) as usize).max(1),
            rows: ((height / cell.height) as usize).max(1),
        };
        let current = {
            let term = self.term.borrow();
            (term.columns(), term.screen_lines())
        };
        if (geometry.cols, geometry.rows) != current {
            (self.on_resize)(geometry.cols, geometry.rows);
            self.resize(geometry.cols, geometry.rows);
        }
        self.geometry.set(Some(geometry));
        geometry
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let this = cx.weak_entity();
        let term = self.term.clone();
        let style = self.style.clone();
        let focused = self.focus.is_focused(window);
        div()
            .size_full()
            .track_focus(&self.focus)
            // Tab / Esc are shell keys here (see `bind_nav_keys`).
            .key_context("Terminal")
            .cursor_text()
            .on_key_down(cx.listener(Self::key_down))
            .on_action(cx.listener(|this, _: &TerminalCopy, _, cx| this.copy(cx)))
            .on_action(cx.listener(|this, _: &TerminalPaste, _, cx| this.paste(cx)))
            .on_action(cx.listener(|this, _: &TerminalSelectAll, _, cx| this.select_all(cx)))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::mouse_down))
            .on_scroll(cx.listener(Self::scroll_wheel))
            .child(
                canvas(
                    |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                    move |bounds, hitbox: Hitbox, window, cx| {
                        let Some(geometry) =
                            this.update(cx, |view, _| view.lay_out(bounds, window)).ok()
                        else {
                            return;
                        };
                        paint_grid(
                            &mut term.borrow_mut(),
                            bounds,
                            geometry,
                            &style,
                            focused,
                            window,
                            cx,
                        );
                        // Window-wide, not on the element: a drag goes on past
                        // the terminal's edge (that's how it scrolls).
                        let moved = this.clone();
                        window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                            if phase == DispatchPhase::Bubble {
                                let hovered = hitbox.is_hovered(window);
                                let _ = moved.update(cx, |v, cx| v.mouse_moved(e, hovered, cx));
                            }
                        });
                        let up = this.clone();
                        window.on_mouse_event(move |e: &MouseUpEvent, phase, _, cx| {
                            if phase == DispatchPhase::Bubble {
                                let _ = up.update(cx, |v, cx| v.mouse_up(e, cx));
                            }
                        });
                    },
                )
                .size_full(),
            )
            .children(self.menu.as_ref().map(|(menu, at)| {
                deferred(
                    anchored()
                        .position(*at)
                        .snap_to_window_with_margin(px(8.))
                        .child(menu.clone()),
                )
                .with_priority(1)
            }))
    }
}

fn font(style: &TerminalStyle, bold: bool, italic: bool) -> Font {
    Font {
        family: style.font_family.clone(),
        features: FontFeatures::default(),
        fallbacks: None,
        weight: if bold {
            FontWeight::BOLD
        } else {
            FontWeight::NORMAL
        },
        style: if italic {
            FontStyle::Italic
        } else {
            FontStyle::Normal
        },
    }
}

/// A cell's size in the terminal's font.
fn cell_size(style: &TerminalStyle, window: &mut Window) -> Size<Pixels> {
    let run = TextRun {
        len: 1,
        font: font(style, false, false),
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    let m = window
        .text_system()
        .shape_line("M".into(), style.font_size, &[run], None);
    size(m.width, (m.ascent + m.descent) * LINE_HEIGHT)
}

/// A stretch of cells on one line in one color, as a single quad: side by side
/// quads leave hairline seams at fractional scales, and these can be translucent.
struct CellRun {
    line: usize,
    start: usize,
    end: usize,
    color: Hsla,
}

fn extend_runs(runs: &mut Vec<CellRun>, line: usize, col: usize, width: usize, color: Hsla) {
    match runs.last_mut() {
        Some(run) if run.line == line && run.end == col && run.color == color => {
            run.end = col + width;
        }
        _ => runs.push(CellRun {
            line,
            start: col,
            end: col + width,
            color,
        }),
    }
}

/// Backgrounds, then the selection, then the cursor, then the glyphs on top —
/// each at its exact cell, with the scrollback offset applied.
fn paint_grid(
    term: &mut Term<Events>,
    bounds: Bounds<Pixels>,
    geometry: Geometry,
    style: &TerminalStyle,
    focused: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Geometry { origin, cell, .. } = geometry;
    let cell_bounds = |line: usize, col: usize, width: usize| {
        Bounds::new(
            origin + point(cell.width * col as f32, cell.height * line as f32),
            size(cell.width * width as f32, cell.height),
        )
    };
    let palette = &style.colors;
    window.paint_quad(fill(bounds, palette.background));

    let content = term.renderable_content();
    let offset = content.display_offset;
    let colors = content.colors;
    let selection = content.selection;
    let cursor_shape = content.cursor.shape;
    let cursor_at = point_to_viewport(offset, content.cursor.point)
        .filter(|_| cursor_shape != CursorShape::Hidden);

    let mut backgrounds = Vec::new();
    let mut selected = Vec::new();
    let mut glyphs = Vec::new();
    for item in content.display_iter {
        let Some(at) = point_to_viewport(offset, item.point) else {
            continue;
        };
        let flags = item.cell.flags;
        if flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let (line, col) = (at.line, at.column.0);
        let width = if flags.contains(Flags::WIDE_CHAR) {
            2
        } else {
            1
        };
        let (mut fg, mut bg) = (
            palette.resolve(item.cell.fg, colors),
            palette.resolve(item.cell.bg, colors),
        );
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        if bg != palette.background {
            extend_runs(&mut backgrounds, line, col, width, bg);
        }
        if selection.is_some_and(|range| range.contains(item.point)) {
            extend_runs(&mut selected, line, col, width, style.selection);
        }
        if flags.contains(Flags::DIM) {
            fg.a *= DIM_ALPHA;
        }
        // The solid cursor is the one thing painted under a glyph that shares
        // its color: the character shows through in the cell's background.
        if focused && cursor_shape == CursorShape::Block && cursor_at == Some(at) {
            fg = bg;
        }
        let ch = item.cell.c;
        if !ch.is_whitespace() && !ch.is_control() && !flags.contains(Flags::HIDDEN) {
            glyphs.push((line, col, ch, fg, flags));
        }
    }

    for run in backgrounds.iter().chain(&selected) {
        window.paint_quad(fill(
            cell_bounds(run.line, run.start, run.end - run.start),
            run.color,
        ));
    }

    if let Some(at) = cursor_at {
        let block = cell_bounds(at.line, at.column.0, 1);
        let color = palette.cursor;
        let bar = px(2.);
        match cursor_shape {
            _ if !focused => window.paint_quad(outline(block, color, BorderStyle::Solid)),
            CursorShape::Block => window.paint_quad(fill(block, color)),
            CursorShape::HollowBlock => {
                window.paint_quad(outline(block, color, BorderStyle::Solid))
            }
            CursorShape::Beam => window.paint_quad(fill(
                Bounds::new(block.origin, size(bar, cell.height)),
                color,
            )),
            CursorShape::Underline => window.paint_quad(fill(
                Bounds::new(
                    block.origin + point(px(0.), cell.height - bar),
                    size(cell.width, bar),
                ),
                color,
            )),
            CursorShape::Hidden => {}
        }
    }

    for (line, col, ch, fg, flags) in glyphs {
        let text: SharedString = ch.to_string().into();
        let run = TextRun {
            len: text.len(),
            font: font(
                style,
                flags.contains(Flags::BOLD),
                flags.contains(Flags::ITALIC),
            ),
            color: fg,
            background_color: None,
            underline: flags.contains(Flags::UNDERLINE).then_some(UnderlineStyle {
                thickness: px(1.),
                color: Some(fg),
                wavy: false,
            }),
            strikethrough: None,
        };
        let shaped = window
            .text_system()
            .shape_line(text, style.font_size, &[run], None);
        let origin = cell_bounds(line, col, 1).origin;
        let _ = shaped.paint(origin, cell.height, TextAlign::Left, None, window, cx);
    }
}

/// The bytes a key sends to the shell, by xterm's conventions.
fn keystroke_bytes(key: &Keystroke, mode: TermMode) -> Option<Vec<u8>> {
    let mods = key.modifiers;
    let app_cursor = mode.contains(TermMode::APP_CURSOR);
    let arrow = |normal: &'static [u8], app: &'static [u8]| {
        if app_cursor {
            app
        } else {
            normal
        }
    };
    let named: Option<&[u8]> = match key.key.as_str() {
        "space" if mods.control => Some(b"\0"),
        "space" => Some(b" "),
        "enter" => Some(b"\r"),
        "escape" => Some(b"\x1b"),
        "backspace" => Some(b"\x7f"),
        "tab" if mods.shift => Some(b"\x1b[Z"),
        "tab" => Some(b"\t"),
        "up" => Some(arrow(b"\x1b[A", b"\x1bOA")),
        "down" => Some(arrow(b"\x1b[B", b"\x1bOB")),
        "right" => Some(arrow(b"\x1b[C", b"\x1bOC")),
        "left" => Some(arrow(b"\x1b[D", b"\x1bOD")),
        "home" => Some(b"\x1b[H"),
        "end" => Some(b"\x1b[F"),
        "pageup" => Some(b"\x1b[5~"),
        "pagedown" => Some(b"\x1b[6~"),
        "insert" => Some(b"\x1b[2~"),
        "delete" => Some(b"\x1b[3~"),
        "f1" => Some(b"\x1bOP"),
        "f2" => Some(b"\x1bOQ"),
        "f3" => Some(b"\x1bOR"),
        "f4" => Some(b"\x1bOS"),
        "f5" => Some(b"\x1b[15~"),
        "f6" => Some(b"\x1b[17~"),
        "f7" => Some(b"\x1b[18~"),
        "f8" => Some(b"\x1b[19~"),
        "f9" => Some(b"\x1b[20~"),
        "f10" => Some(b"\x1b[21~"),
        "f11" => Some(b"\x1b[23~"),
        "f12" => Some(b"\x1b[24~"),
        _ => None,
    };
    if let Some(bytes) = named {
        return Some(bytes.to_vec());
    }
    let single = {
        let mut chars = key.key.chars();
        chars.next().filter(|_| chars.next().is_none())
    };
    if mods.control {
        return match single? {
            c if c.is_ascii_alphabetic() => Some(vec![c.to_ascii_uppercase() as u8 - b'@']),
            '[' => Some(b"\x1b".to_vec()),
            '\\' => Some(b"\x1c".to_vec()),
            ']' => Some(b"\x1d".to_vec()),
            '^' => Some(b"\x1e".to_vec()),
            '_' => Some(b"\x1f".to_vec()),
            '?' => Some(b"\x7f".to_vec()),
            _ => None,
        };
    }
    if mods.alt {
        return single.filter(char::is_ascii).map(|c| vec![0x1b, c as u8]);
    }
    if let Some(typed) = &key.key_char {
        return Some(typed.as_bytes().to_vec());
    }
    single.map(|c| {
        let c = if mods.shift {
            c.to_ascii_uppercase()
        } else {
            c
        };
        c.to_string().into_bytes()
    })
}

/// Pasted text as the shell should get it: wrapped in the markers of a program
/// that asked for bracketed paste (with any ESC removed, so the text can't end
/// the paste itself and run as keystrokes), else with line breaks as Enter's CR.
fn paste_bytes(text: &str, mode: TermMode) -> String {
    if mode.contains(TermMode::BRACKETED_PASTE) {
        format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r")
    }
}

/// The mouse event a program asked to hear, in the encoding it asked for. `code`
/// is the button plus modifier bits.
fn mouse_report(
    code: u8,
    cell: GridPoint<usize>,
    pressed: bool,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let (col, line) = (cell.column.0, cell.line);
    if mode.contains(TermMode::SGR_MOUSE) {
        let end = if pressed { 'M' } else { 'm' };
        return Some(format!("\x1b[<{code};{};{}{end}", col + 1, line + 1).into_bytes());
    }
    // The legacy encoding has a byte per number (offset by 32), which ends at
    // cell 223, and one "released" code that keeps only the modifier bits.
    if col >= 223 || line >= 223 {
        return None;
    }
    let code = if pressed { code } else { 3 | (code & 0b1_1100) };
    Some(vec![
        0x1b,
        b'[',
        b'M',
        32 + code,
        33 + col as u8,
        33 + line as u8,
    ])
}

fn wheel_as_arrows(lines: i32, mode: TermMode) -> Vec<u8> {
    let arrow: &[u8] = match (lines > 0, mode.contains(TermMode::APP_CURSOR)) {
        (true, true) => b"\x1bOA",
        (true, false) => b"\x1b[A",
        (false, true) => b"\x1bOB",
        (false, false) => b"\x1b[B",
    };
    arrow.repeat(lines.unsigned_abs() as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A shell that says nothing and never exits: EOF would wake the view's task
    /// from the reader thread, which the test scheduler rejects.
    struct Silent;

    impl Read for Silent {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            loop {
                std::thread::park();
            }
        }
    }

    /// A view fed `output`, sized 20×4, with what it writes to the shell.
    fn view_with(output: &[u8], cx: &mut TestAppContext) -> (Entity<TerminalView>, Sink) {
        cx.update(gpui_component::init);
        let sink = Sink::default();
        let input = sink.clone();
        let view = cx.update(|cx| {
            let style = TerminalStyle::from_theme(cx);
            cx.new(|cx| TerminalView::new(input, Silent, |_, _| {}, style, cx))
        });
        let output = output.to_vec();
        view.update(cx, |view, _| {
            view.resize(20, 4);
            view.process(&output);
        });
        (view, sink)
    }

    #[test]
    fn keys_encode_like_xterm() {
        fn bytes(key: &'static str, mode: TermMode) -> Option<Vec<u8>> {
            keystroke_bytes(&Keystroke::parse(key).unwrap(), mode)
        }
        let plain = TermMode::empty();
        assert_eq!(bytes("enter", plain).unwrap(), b"\r");
        assert_eq!(bytes("shift-tab", plain).unwrap(), b"\x1b[Z");
        assert_eq!(bytes("up", plain).unwrap(), b"\x1b[A");
        assert_eq!(bytes("up", TermMode::APP_CURSOR).unwrap(), b"\x1bOA");
        assert_eq!(bytes("ctrl-c", plain).unwrap(), [3]);
        assert_eq!(bytes("ctrl-[", plain).unwrap(), [0x1b]);
        assert_eq!(bytes("alt-b", plain).unwrap(), b"\x1bb");
        assert_eq!(bytes("a", plain).unwrap(), b"a");
        assert_eq!(bytes("shift", plain), None);
    }

    fn cell(line: usize, col: usize) -> GridPoint<usize> {
        GridPoint::new(line, Column(col))
    }

    fn geometry() -> Geometry {
        Geometry {
            origin: point(px(10.), px(10.)),
            cell: size(px(10.), px(20.)),
            cols: 20,
            rows: 4,
        }
    }

    #[test]
    fn the_pointer_maps_to_a_cell_and_side() {
        let g = geometry();
        let at = |x, y| g.cell_at(point(px(x), px(y)));
        assert_eq!(at(10., 10.), (cell(0, 0), Side::Left));
        assert_eq!(at(14., 29.), (cell(0, 0), Side::Left));
        assert_eq!(at(16., 30.), (cell(1, 0), Side::Right));
        // Outside the grid it clamps: a drag keeps a usable cell.
        assert_eq!(at(0., 0.), (cell(0, 0), Side::Left));
        assert_eq!(at(9000., 9000.), (cell(3, 19), Side::Right));
    }

    #[test]
    fn a_drag_past_the_edge_scrolls() {
        let g = geometry();
        assert_eq!(g.drag_scroll(px(50.)), 0);
        assert_eq!(g.drag_scroll(px(5.)), 1);
        assert_eq!(g.drag_scroll(px(-35.)), 3);
        assert_eq!(g.drag_scroll(px(95.)), -1);
    }

    #[gpui::test]
    fn dragging_selects_characters(cx: &mut TestAppContext) {
        let (view, _) = view_with(b"hello world\r\nsecond line", cx);
        view.update(cx, |view, _| {
            view.begin_selection(cell(0, 6), Side::Left, 1, false);
            assert_eq!(view.selected_text(), None, "a plain click selects nothing");
            view.extend_selection(cell(0, 10), Side::Right, 0);
            assert_eq!(view.selected_text().as_deref(), Some("world"));
            view.extend_selection(cell(1, 5), Side::Right, 0);
            assert_eq!(view.selected_text().as_deref(), Some("world\nsecond"));
        });
    }

    #[gpui::test]
    fn clicks_select_words_and_lines(cx: &mut TestAppContext) {
        let (view, _) = view_with(b"select this word\r\nother", cx);
        view.update(cx, |view, _| {
            view.begin_selection(cell(0, 8), Side::Left, 2, false);
            assert_eq!(view.selected_text().as_deref(), Some("this"));
            view.begin_selection(cell(0, 8), Side::Left, 3, false);
            assert_eq!(view.selected_text().as_deref(), Some("select this word\n"));
        });
    }

    #[gpui::test]
    fn shift_click_extends(cx: &mut TestAppContext) {
        let (view, _) = view_with(b"one two three", cx);
        view.update(cx, |view, _| {
            view.begin_selection(cell(0, 0), Side::Left, 1, false);
            view.extend_selection(cell(0, 2), Side::Right, 0);
            view.begin_selection(cell(0, 6), Side::Right, 1, true);
            assert_eq!(view.selected_text().as_deref(), Some("one two"));
        });
    }

    #[gpui::test]
    fn a_selection_reaches_into_the_scrollback(cx: &mut TestAppContext) {
        let mut output = Vec::new();
        for n in 0..10 {
            output.extend_from_slice(format!("line {n}\r\n").as_bytes());
        }
        let (view, _) = view_with(&output, cx);
        view.update(cx, |view, _| {
            view.term.borrow_mut().scroll_display(Scroll::Delta(3));
            view.begin_selection(cell(0, 0), Side::Left, 3, false);
            assert_eq!(view.selected_text().as_deref(), Some("line 4\n"));
            // Dragging above the top keeps scrolling back.
            view.extend_selection(cell(0, 0), Side::Left, 2);
            assert_eq!(
                view.selected_text().as_deref(),
                Some("line 2\nline 3\nline 4\n")
            );
        });
    }

    #[gpui::test]
    fn select_all_takes_the_scrollback_too(cx: &mut TestAppContext) {
        let mut output = Vec::new();
        for n in 0..10 {
            output.extend_from_slice(format!("line {n}\r\n").as_bytes());
        }
        let (view, _) = view_with(&output, cx);
        view.update(cx, |view, cx| {
            view.select_all(cx);
            let text = view.selected_text().unwrap();
            assert!(text.starts_with("line 0\n"), "{text:?}");
            assert!(text.contains("line 9"), "{text:?}");
        });
    }

    #[gpui::test]
    fn copy_writes_the_selection_and_keeps_it(cx: &mut TestAppContext) {
        let (view, _) = view_with(b"copy me", cx);
        view.update(cx, |view, cx| {
            view.begin_selection(cell(0, 0), Side::Left, 1, false);
            view.extend_selection(cell(0, 3), Side::Right, 0);
            view.copy(cx);
            assert_eq!(view.selected_text().as_deref(), Some("copy"));
        });
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some("copy"));
    }

    #[gpui::test]
    fn copy_with_nothing_selected_leaves_the_clipboard(cx: &mut TestAppContext) {
        let (view, _) = view_with(b"text", cx);
        cx.write_to_clipboard(ClipboardItem::new_string("before".into()));
        view.update(cx, |view, cx| view.copy(cx));
        let copied = cx.read_from_clipboard().and_then(|item| item.text());
        assert_eq!(copied.as_deref(), Some("before"));
    }

    #[gpui::test]
    fn typing_drops_the_selection_and_returns_to_the_prompt(cx: &mut TestAppContext) {
        let mut output = Vec::new();
        for n in 0..10 {
            output.extend_from_slice(format!("line {n}\r\n").as_bytes());
        }
        let (view, sink) = view_with(&output, cx);
        view.update(cx, |view, cx| {
            view.term.borrow_mut().scroll_display(Scroll::Delta(3));
            view.begin_selection(cell(0, 0), Side::Left, 3, false);
            view.send_input(b"x", cx);
            assert_eq!(view.selected_text(), None);
            assert_eq!(view.term.borrow().grid().display_offset(), 0);
        });
        assert_eq!(*sink.0.lock().unwrap(), b"x");
    }

    #[gpui::test]
    fn pasting_follows_the_programs_mode(cx: &mut TestAppContext) {
        let (view, sink) = view_with(b"", cx);
        view.update(cx, |view, cx| view.paste_text("a\nb", cx));
        assert_eq!(*sink.0.lock().unwrap(), b"a\rb");
        sink.0.lock().unwrap().clear();
        view.update(cx, |view, cx| {
            view.process(b"\x1b[?2004h");
            view.paste_text("a\n\x1b[201~b", cx);
        });
        assert_eq!(
            String::from_utf8(sink.0.lock().unwrap().clone()).unwrap(),
            "\x1b[200~a\n[201~b\x1b[201~"
        );
    }

    #[gpui::test]
    fn a_program_that_asked_for_the_mouse_gets_it(cx: &mut TestAppContext) {
        let (view, _) = view_with(b"", cx);
        view.update(cx, |view, _| {
            let none = Modifiers::default();
            let shift = Modifiers {
                shift: true,
                ..Default::default()
            };
            assert!(!view.program_has_mouse(none));
            view.process(b"\x1b[?1000h");
            assert!(view.program_has_mouse(none));
            assert!(!view.program_has_mouse(shift), "Shift takes it back");
        });
    }

    #[test]
    fn mouse_reports_use_the_programs_encoding() {
        let sgr = TermMode::SGR_MOUSE;
        assert_eq!(
            mouse_report(0, cell(2, 4), true, sgr).unwrap(),
            b"\x1b[<0;5;3M"
        );
        assert_eq!(
            mouse_report(0, cell(2, 4), false, sgr).unwrap(),
            b"\x1b[<0;5;3m"
        );
        let legacy = TermMode::empty();
        assert_eq!(
            mouse_report(1, cell(2, 4), true, legacy).unwrap(),
            [0x1b, b'[', b'M', 33, 37, 35]
        );
        // Released keeps the modifiers (4 = Shift) but not the button.
        assert_eq!(
            mouse_report(1 | 4, cell(2, 4), false, legacy).unwrap(),
            [0x1b, b'[', b'M', 39, 37, 35]
        );
        assert_eq!(mouse_report(0, cell(0, 300), true, legacy), None);
        assert!(mouse_report(0, cell(0, 300), true, sgr).is_some());
    }

    #[test]
    fn the_wheel_scrolls_a_full_screen_program_with_arrows() {
        assert_eq!(wheel_as_arrows(2, TermMode::empty()), b"\x1b[A\x1b[A");
        assert_eq!(wheel_as_arrows(-1, TermMode::APP_CURSOR), b"\x1bOB");
    }
}
