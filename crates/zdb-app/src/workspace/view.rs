//! All view code for `Workspace`: the `render_*` panel/modal builders, the
//! `impl Render`, and small free view helpers. Split out of `mod.rs`. This is a
//! child module, so it reaches `Workspace`'s private fields and logic methods
//! directly — no visibility bumps needed.

use super::*;
use gpui::{
    anchored, canvas, deferred, relative, AnyElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, WeakEntity,
};
use gpui_component::checkbox::Checkbox;
use gpui_component::list::ListItem;
use gpui_component::menu::{ContextMenuExt, PopupMenu, PopupMenuItem};
use gpui_component::spinner::Spinner;
use gpui_component::tree::{tree, TreeEntry};
use gpui_component::{ActiveTheme, Selectable as _};

/// Amber of the "transaction open" markers (tab chip dot, toolbar badge).
const TX_AMBER: u32 = 0xf59e0bff;
/// Height of the status bar along the window's bottom edge.
const STATUS_BAR_H: gpui::Pixels = px(26.);
/// Width of the query log while Messages shares the bottom panel with it.
const LOG_W: gpui::Pixels = px(520.);

impl Workspace {
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let connected = self.conn.is_some();
        let title = self
            .cfg
            .as_ref()
            .filter(|_| connected)
            .map(|cfg| cfg.name.clone())
            .unwrap_or_else(|| "DATABASE".into());
        let mut actions = h_flex()
            .gap_1()
            .items_center()
            .child(
                Button::new("connections")
                    .tab_stop(false)
                    .icon(IconName::Globe)
                    .tooltip("Connections")
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_connections(cx)),
                    ),
            )
            .child(
                Button::new("settings")
                    .tab_stop(false)
                    .icon(Icon::empty().path("icons/settings.svg"))
                    .tooltip("Settings")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_settings(cx))),
            )
            .child(
                Button::new("shortcuts")
                    .tab_stop(false)
                    .icon(Icon::empty().path("icons/keyboard.svg"))
                    .tooltip("Keyboard shortcuts (F1)")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_shortcuts(cx))),
            );
        if connected {
            actions = actions.child(
                Button::new("refresh")
                    .tab_stop(false)
                    .icon(Icon::empty().path("icons/refresh-cw.svg"))
                    .tooltip("Refresh schemas")
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.connect_or_refresh(cx)),
                    ),
            );
        }
        let header = h_flex()
            .px_3()
            .py_1()
            .bg(c.header)
            .border_b_1()
            .border_color(c.border)
            .justify_between()
            .items_center()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.fg_dim)
                    .truncate()
                    .child(title),
            )
            .child(actions);

        // Zed-style filter box: narrows the tree to matching relations.
        let filter = connected.then(|| {
            h_flex()
                .px_2()
                .py_1()
                .gap_1p5()
                .items_center()
                .border_b_1()
                .border_color(c.border)
                .child(tree_icon("icons/search.svg", c.fg_dim))
                .child(
                    // `down` steps from the filter box into the tree (the single-
                    // line input declines the key, so it reaches this context).
                    div().flex_1().key_context("TreeFilter").child(
                        Input::new(&self.tree.filter_input)
                            .appearance(false)
                            .small(),
                    ),
                )
        });

        let body: AnyElement = if connected {
            let weak = cx.weak_entity();
            div()
                .flex_grow()
                .min_h(px(0.))
                // Our own focus target + key context around the widget: its focus
                // handle is private, so the keyboard reaches the tree through
                // this one (the handlers are on the workspace root).
                .track_focus(&self.tree.focus)
                .key_context(if self.settings.vim_navigation {
                    "SchemaTree vim"
                } else {
                    "SchemaTree"
                })
                .child(tree(
                    &self.tree.state,
                    move |ix, entry, selected, window, cx| {
                        schema_tree_row(ix, entry, selected, &weak, window, cx)
                    },
                ))
                .into_any_element()
        } else {
            div()
                .p_3()
                .text_sm()
                .text_color(c.fg_dim)
                .child("Not connected")
                .into_any_element()
        };

        v_flex()
            .size_full()
            .bg(c.sidebar)
            .border_r_1()
            .border_color(c.border)
            .child(header)
            .children(filter)
            .child(body)
    }

    fn render_center(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let strip = self.render_tab_strip(c, cx);
        let body = match self.active {
            Some(idx) if idx < self.tabs.len() => {
                let vim = self.settings.vim_navigation;
                Self::render_tab_body(&self.tabs[idx], &self.cell_multi, vim, window, c, cx)
                    .into_any_element()
            }
            _ => welcome_pane(c).into_any_element(),
        };
        v_flex()
            .size_full()
            .min_w(px(0.))
            .overflow_hidden()
            .bg(c.center)
            .child(strip)
            .child(body)
    }

    /// The Zed-style tab strip: one chip per open tab + `+` (new query) and a
    /// scratch button. The chips scroll sideways once they overflow (the wheel
    /// scrolls them — gpui turns vertical wheel into horizontal on an x-only
    /// scroller) and `render` keeps the active one in view; the buttons stay
    /// right after them, never scrolled away.
    fn render_tab_strip(&self, c: Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let mut chips = h_flex()
            .id("tab-strip")
            .h_full()
            .min_w(px(0.))
            .overflow_x_scroll()
            .track_scroll(&self.tab_strip);
        let weak = cx.weak_entity();
        let n_tabs = self.tabs.len();
        for (idx, tab) in self.tabs.iter().enumerate() {
            let is_active = self.active == Some(idx);
            let id = tab.id;
            let kind_icon = match tab.kind {
                TabKind::Table { .. } => "icons/table.svg",
                _ => "icons/file.svg",
            };
            let tint = if is_active { c.fg } else { c.fg_dim };
            // Separate clickable regions (title = activate, x = close) so a close
            // click doesn't also re-activate a just-removed tab.
            let label = h_flex()
                .id(SharedString::from(format!("tab-{id}")))
                .h_full()
                .pl_2()
                .pr_1()
                .gap_1p5()
                .items_center()
                .cursor_pointer()
                .text_sm()
                .text_color(tint)
                .when(!is_active, |d| d.hover(|s| s.bg(c.hover)))
                .child(tree_icon(kind_icon, tint))
                .child(div().max_w(px(160.)).truncate().child(tab.title.clone()))
                // Inside a transaction (see the toolbar badge).
                .when(tab.in_transaction, |d| {
                    d.child(div().size(px(6.)).rounded_full().bg(rgba(TX_AMBER)))
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.activate_tab(idx, window, cx)
                }));
            let close = div()
                .id(SharedString::from(format!("tabx-{id}")))
                .h_full()
                .px_1()
                .flex()
                .items_center()
                .cursor_pointer()
                .hover(|s| s.bg(c.hover))
                .child(tree_icon("icons/close.svg", c.fg_dim))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.close_tab(id, cx)));
            let weak = weak.clone();
            let has_others = n_tabs > 1;
            let has_right = idx + 1 < n_tabs;
            // `ContextMenuExt::context_menu` hardcodes the element id
            // ("context-menu"), so sibling chips would all share ONE element
            // state (menu open flag + full-window occluding overlay) — every
            // chip then paints the popup and the last overlay swallows the
            // clicks. Wrapping each chip in a UNIQUELY id'd div puts a distinct
            // id on the stack above the menu, giving each its own state.
            let chip = h_flex()
                .h_full()
                .items_center()
                .flex_shrink_0()
                .border_r_1()
                .border_color(c.border)
                .when(is_active, |d| d.bg(c.center))
                .child(label)
                .child(close)
                // Right-click anywhere on the chip: close variants.
                .context_menu(move |menu, _, _| {
                    let w = weak.clone();
                    let mut menu =
                        menu.item(PopupMenuItem::new("Close").on_click(move |_, _, cx| {
                            w.update(cx, |this, cx| this.close_tab(id, cx)).ok();
                        }));
                    if has_others {
                        let w = weak.clone();
                        menu = menu.item(PopupMenuItem::new("Close Others").on_click(
                            move |_, _, cx| {
                                w.update(cx, |this, cx| this.close_other_tabs(id, cx)).ok();
                            },
                        ));
                    }
                    if has_right {
                        let w = weak.clone();
                        menu = menu.item(PopupMenuItem::new("Close to the Right").on_click(
                            move |_, _, cx| {
                                w.update(cx, |this, cx| this.close_tabs_right(id, cx)).ok();
                            },
                        ));
                    }
                    let w = weak.clone();
                    menu.separator()
                        .item(PopupMenuItem::new("Close All").on_click(move |_, _, cx| {
                            w.update(cx, |this, cx| this.close_all_tabs(cx)).ok();
                        }))
                });
            chips = chips.child(
                div()
                    .id(SharedString::from(format!("tabchip-{id}")))
                    .h_full()
                    .flex()
                    .flex_shrink_0()
                    .child(chip),
            );
        }
        h_flex()
            .h(px(34.))
            .flex_shrink_0()
            .items_center()
            .bg(c.header)
            .border_b_1()
            .border_color(c.border)
            .overflow_hidden()
            .child(chips)
            .child(
                Button::new("tab-add")
                    .tab_stop(false)
                    .icon(IconName::Plus)
                    .tooltip("New query")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_query_tab(window, cx)
                    })),
            )
            .child(
                Button::new("tab-scratch")
                    .tab_stop(false)
                    .icon(IconName::File)
                    .tooltip("Scratch (Ctrl/Cmd+Shift+E)")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.focus_scratch_tab(window, cx)
                    })),
            )
    }

    /// The body of one tab: the action toolbar + (editor split | table grid).
    fn render_tab_body(
        tab: &Tab,
        cell_multi: &Entity<InputState>,
        vim: bool,
        window: &Window,
        c: Colors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tab_id = tab.id;
        let editable = tab.edit_target.is_some();
        // The grid's keys live on a context that exists only while no cell is
        // being edited — see `keynav.rs` for why it can't be the widget's own.
        let grid_context = match (tab.editing.is_some(), vim) {
            (true, _) => "GridEdit",
            (false, true) => "Grid vim",
            (false, false) => "Grid",
        };
        let is_editor = !matches!(tab.kind, TabKind::Table { .. });
        let has_result = !tab.headers.is_empty();
        let has_sql = match &tab.kind {
            TabKind::Table { .. } => true,
            _ => !tab.editor.read(cx).value().trim().is_empty(),
        };

        // Run toggles to Stop while a query is in flight.
        let run_btn = if tab.running {
            Button::new("run")
                .tab_stop(false)
                .icon(
                    Icon::empty()
                        .path("icons/circle-x.svg")
                        .text_color(rgba(0xef4444ff)),
                )
                .tooltip("Stop")
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel(cx)))
        } else {
            Button::new("run")
                .tab_stop(false)
                .icon(
                    Icon::empty()
                        .path("icons/play.svg")
                        .text_color(rgba(0x22c55eff)),
                )
                .tooltip("Run (Ctrl/Cmd+Enter)")
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.run_active_tab(cx)))
        };

        // +/- always visible; disabled when the result isn't editable / no row is
        // selected. Save-row appears only mid-insert.
        let del_tip = if tab.current_row.is_some() && tab.current_row == tab.new_row_idx {
            "Discard row"
        } else {
            "Delete row"
        };
        // Red only when actionable. Disabled, it takes the button's own (dimmed)
        // text color — a fixed one would make it look live.
        let del_enabled = editable && tab.current_row.is_some() && !tab.running;
        let del_icon = Icon::empty()
            .path("icons/minus.svg")
            .when(del_enabled, |i| i.text_color(rgba(0xef4444ff)));
        let mut toolbar = h_flex()
            .px_2()
            .py_1()
            .gap_2()
            .items_center()
            .bg(c.header)
            .border_b_1()
            .border_color(c.border)
            .child(run_btn)
            .child(
                Button::new("add-row")
                    .tab_stop(false)
                    .icon(IconName::Plus)
                    .tooltip("Add row")
                    .disabled(!editable || tab.running)
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| this.add_row(tab_id, cx)),
                    ),
            )
            .child(
                Button::new("del-row")
                    .tab_stop(false)
                    .icon(del_icon)
                    .tooltip(del_tip)
                    .disabled(!del_enabled)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.delete_current_row(tab_id, cx)
                    })),
            )
            .child(
                Button::new("reload")
                    .tab_stop(false)
                    .icon(Icon::empty().path("icons/refresh-cw.svg"))
                    .tooltip("Refresh data")
                    .disabled(tab.base_sql.is_none())
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.reload_data(tab_id, cx)
                        }),
                    ),
            );
        // Groups: row actions | pager (Table tabs) | what to do with the result.
        // The icons carry no color of their own, so a disabled one dims.
        let divider = || div().flex_none().w(px(1.)).h(px(16.)).bg(c.border);
        let action = |id: &'static str, icon: &'static str, tip: &'static str, enabled: bool| {
            Button::new(id)
                .tab_stop(false)
                .icon(Icon::empty().path(icon))
                .tooltip(tip)
                .disabled(!enabled)
        };
        if matches!(tab.kind, TabKind::Table { .. }) {
            toolbar = toolbar
                .child(divider())
                .child(Self::render_pager(tab, c, cx));
        }
        toolbar = toolbar
            .child(divider())
            // EXPLAIN the current statement (plan shown in the grid).
            .child(
                action(
                    "explain",
                    "icons/list-tree.svg",
                    "Explain (query plan)",
                    has_sql,
                )
                .on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| this.explain_active(false, cx)),
                ),
            )
            // Export / copy the current result.
            .child(
                action(
                    "export-csv",
                    "icons/download.svg",
                    "Export result to CSV (more formats in the palette)",
                    has_result,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.export_active(ExportFormat::Csv, cx)
                })),
            )
            .child(
                action(
                    "copy-tsv",
                    "icons/copy.svg",
                    "Copy result to clipboard (TSV)",
                    has_result,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.copy_active_tsv(cx))),
            );
        if is_editor {
            toolbar = toolbar.child(
                action("format-sql", "icons/braces.svg", "Format SQL", has_sql).on_click(
                    cx.listener(|this, _: &ClickEvent, window, cx| this.format_active(window, cx)),
                ),
            );
        }
        if tab.in_transaction {
            toolbar = toolbar.child(divider()).child(
                div()
                    .id("tx-badge")
                    .px_2()
                    .py(px(2.))
                    .rounded_md()
                    .bg(rgba(TX_AMBER & 0xffffff00 | 0x26))
                    .text_xs()
                    .text_color(rgba(TX_AMBER))
                    .child("Transaction open")
                    .tooltip(|window, cx| {
                        gpui_component::tooltip::Tooltip::new(
                            "This tab holds its own connection until COMMIT or ROLLBACK. \
                             Closing the tab rolls the transaction back.",
                        )
                        .build(window, cx)
                    }),
            );
        }
        if tab.new_row_idx.is_some() {
            toolbar = toolbar.child(
                Button::new("save-row")
                    .tab_stop(false)
                    .icon(IconName::Check)
                    .tooltip("Save row")
                    .primary()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.save_new_row(tab_id, cx)
                    })),
            );
        }

        // `min_w(0)` + `overflow_hidden` all the way down to the Table element:
        // a flex item's automatic minimum size is its MIN-CONTENT width, which for
        // a wide grid is the sum of all column widths. Without the clamp every
        // ancestor (incl. gpui-component's `resizable_panel`, which is `flex_grow`
        // + `size_full` with no min-width of its own) grows to that width, the
        // table believes it fits, its internal horizontal scroll never engages, and
        // the off-window columns are simply clipped by the root `overflow_hidden`.
        let mut results = v_flex().size_full().min_w(px(0.)).overflow_hidden();
        if matches!(tab.kind, TabKind::Table { .. }) {
            results = results.child(Self::render_filter_bar(tab, window, c, cx));
        }
        // The results area shows the grid, or — after EXPLAIN — the plan text in
        // that same place (query data vs. plan, same spot). A running query
        // puts its loader over it.
        let shown = if let Some(plan) = &tab.plan {
            Self::plan_view(tab_id, plan, c, cx).into_any_element()
        } else {
            div()
                .size_full()
                .min_w(px(0.))
                .overflow_hidden()
                .key_context(grid_context)
                // Type-to-edit: keys no binding claimed.
                .on_key_down(cx.listener(move |this, ev: &KeyDownEvent, window, cx| {
                    this.grid_key_down(tab_id, ev, window, cx)
                }))
                // Wheel press = autoscroll (see `autoscroll.rs`). Stopping
                // propagation keeps the workspace root's "any click exits"
                // handler from undoing the press that just started it.
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        this.toggle_autoscroll(tab_id, ev.position, cx);
                        cx.stop_propagation();
                    }),
                )
                .child(Table::new(&tab.table).stripe(true).bordered(true))
                .into_any_element()
        };
        let loader = tab.loader();
        results = results.child(
            div()
                .relative()
                .size_full()
                .min_w(px(0.))
                .overflow_hidden()
                .child(shown)
                .children(loader.map(|l| Self::loading_overlay(tab_id, l, c, cx))),
        );

        // Table tabs hide the redundant generated `SELECT *` editor and let the
        // rows fill the pane; Query / Scratch keep the editor above the results.
        let body = if matches!(tab.kind, TabKind::Table { .. }) {
            results.into_any_element()
        } else {
            v_resizable(SharedString::from(format!("center-{tab_id}")))
                .child(
                    resizable_panel()
                        .size(px(170.))
                        .size_range(px(60.)..px(600.))
                        .child(div().size_full().child(Input::new(&tab.editor).h_full())),
                )
                .child(resizable_panel().child(results))
                .into_any_element()
        };

        v_flex()
            .size_full()
            .min_w(px(0.))
            .overflow_hidden()
            .bg(c.center)
            .child(toolbar)
            .child(body)
            .children(Self::render_value_editor(tab, cell_multi, c, cx))
    }

    /// The loader over a running tab's results: an animated spinner, on a
    /// blank pane or over the previous rows, dimmed. Either way it
    /// `occlude()`s what's under it — no click, hover or wheel reaches the grid
    /// until the run is over (its keys are ignored meanwhile too) — and a
    /// click on it asks whether to cancel the query.
    fn loading_overlay(
        tab_id: u64,
        loader: Loader,
        c: Colors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let backdrop = match loader {
            Loader::Blank => c.center,
            Loader::OverData => c.center.opacity(0.6),
        };
        div()
            .id("loading-overlay")
            .absolute()
            .inset_0()
            .occlude()
            .bg(backdrop)
            .flex()
            .items_center()
            .justify_center()
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.ask_cancel(tab_id, cx)))
            .child(
                Spinner::new()
                    .icon(Icon::empty().path("icons/loader-circle.svg"))
                    .color(c.fg_dim)
                    .large(),
            )
    }

    /// "Cancel the running query?" — asked by a click on a tab's loader.
    fn render_confirm_cancel(&self, tab_id: u64, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let title = self
            .tab(tab_id)
            .map(|t| t.title.clone())
            .unwrap_or_default();
        let body =
            v_flex()
                .p_4()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(c.fg)
                        .child("Cancel the running query?"),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(c.fg_dim)
                        .child(format!("“{title}” is still waiting for the database.")),
                )
                .child(
                    h_flex()
                        .pt_1()
                        .gap_2()
                        .items_center()
                        .child(
                            div().flex_1().child(
                                Checkbox::new("cancel-dont-ask")
                                    .label("Don't ask again")
                                    .checked(self.cancel_dont_ask)
                                    .on_click(cx.listener(|this, on: &bool, _, cx| {
                                        this.cancel_dont_ask = *on;
                                        cx.notify();
                                    })),
                            ),
                        )
                        .child(Button::new("keep-running").label("Keep running").on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| this.close_modal(cx)),
                        ))
                        .child(
                            Button::new("confirm-cancel")
                                .label("Cancel query")
                                .danger()
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.confirm_cancel(tab_id, cx)
                                })),
                        ),
                );
        self.modal_layer(v_flex().mt(px(120.)).w(px(460.)).child(body), cx)
    }

    /// A Table tab's WHERE / ORDER BY / LIMIT strip, built like Zed's "Search
    /// threads…" box: one full-width bar with no field chrome — a muted icon and
    /// the SQL keyword, a borderless input that reads as the rest of the clause,
    /// and a clear button once there's something to clear (not on LIMIT, where
    /// empty means every row). The focused box lights up in place of a focus
    /// border, and turns red when the last run failed on its text (the error
    /// itself is in MESSAGES). Enter (or Run / refresh) applies them all.
    fn render_filter_bar(
        tab: &Tab,
        window: &Window,
        c: Colors,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tab_id = tab.id;
        let field = |id: &'static str,
                     icon: &'static str,
                     keyword: &'static str,
                     clause: Clause,
                     input: &Entity<InputState>,
                     clearable: bool,
                     cx: &mut Context<Self>| {
            let state = input.read(cx);
            let tint = if tab.clause_error == Some(clause) {
                c.danger
            } else if state.focus_handle(cx).is_focused(window) {
                c.fg
            } else {
                c.fg_dim
            };
            let clear = (clearable && !state.value().is_empty()).then(|| {
                let input = input.clone();
                Button::new(id)
                    .ghost()
                    .xsmall()
                    .tab_stop(false)
                    .icon(Icon::empty().path("icons/close.svg").text_color(c.fg_dim))
                    .tooltip("Clear")
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        input.update(cx, |i, cx| {
                            i.set_value("", window, cx);
                            i.focus(window, cx);
                        });
                        this.apply_where(tab_id, cx);
                    }))
            });
            h_flex()
                .h_full()
                .min_w(px(0.))
                .gap_1p5()
                .items_center()
                .child(tree_icon(icon, tint))
                .child(
                    div()
                        .flex_none()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(tint)
                        .child(keyword),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .child(Input::new(input).appearance(false).small()),
                )
                .children(clear)
        };
        let filter = field(
            "clear-where",
            "icons/list-filter.svg",
            "WHERE",
            Clause::Where,
            &tab.where_input,
            true,
            cx,
        );
        let order = field(
            "clear-order",
            "icons/arrow-up-down.svg",
            "ORDER BY",
            Clause::OrderBy,
            &tab.order_input,
            true,
            cx,
        );
        let limit = field(
            "clear-limit",
            "icons/hash.svg",
            "LIMIT",
            Clause::Limit,
            &tab.limit_input,
            false,
            cx,
        );
        let divider = || div().flex_none().w(px(1.)).h(px(16.)).bg(c.border);
        h_flex()
            .h(px(32.))
            .flex_none()
            .pl_3()
            .pr_2()
            .gap_3()
            .items_center()
            .bg(c.center)
            .border_b_1()
            .border_color(c.border)
            // The inputs hold SQL: set them (and their keywords) in the code font.
            .font_family(cx.theme().mono_font_family.clone())
            .child(filter.flex_1())
            .child(divider())
            .child(order.flex_none().w(relative(0.32)))
            .child(divider())
            .child(limit.flex_none().w(px(124.)))
    }

    /// A Table tab's pager: `« ‹ 501–1000 of 12000 › »`. A page is LIMIT rows,
    /// so without a LIMIT there's nothing to page; it also holds still while a
    /// query runs and over staged edits (turning the page re-reads the rows,
    /// which would drop them). There's more to see while a page comes back full
    /// — or, once the total is known, until it's reached. The total comes from
    /// the `count(*)` that "last page" runs (not one per load: it can be slow on
    /// a big table).
    fn render_pager(tab: &Tab, c: Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = tab.id;
        let size = tabs::page_size(&tab.limit_input.read(cx).value());
        let loaded = tab.orig_rows.len();
        let idle = size.is_some() && !tab.running && tab.pending.is_empty();
        let back = idle && tab.page > 0;
        let more = idle
            && match tab.total_rows {
                Some(total) => tab.first_row + loaded < total,
                None => size.is_some_and(|n| loaded >= n),
            };
        let mut label = if loaded == 0 {
            "No rows".to_string()
        } else {
            let (from, to) = (tab.first_row + 1, tab.first_row + loaded);
            format!("{}–{}", util::grouped(from), util::grouped(to))
        };
        if let Some(total) = tab.total_rows {
            label.push_str(&format!(" of {}", util::grouped(total)));
        }
        let step =
            |id: &'static str, icon: &'static str, tip: &'static str, to: PageMove, on: bool| {
                Button::new(id)
                    .tab_stop(false)
                    .icon(Icon::empty().path(icon))
                    .tooltip(tip)
                    .disabled(!on)
                    .on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.go_page(tab_id, to, cx)
                        }),
                    )
            };
        h_flex()
            .gap_1()
            .items_center()
            .child(step(
                "page-first",
                "icons/chevrons-left.svg",
                "First page",
                PageMove::First,
                back,
            ))
            .child(step(
                "page-prev",
                "icons/chevron-left.svg",
                "Previous page",
                PageMove::Prev,
                back,
            ))
            .child(
                div()
                    .min_w(px(72.))
                    .px_1()
                    .flex()
                    .justify_center()
                    .text_xs()
                    .text_color(c.fg_dim)
                    .child(label),
            )
            .child(step(
                "page-next",
                "icons/chevron-right.svg",
                "Next page",
                PageMove::Next,
                more,
            ))
            .child(step(
                "page-last",
                "icons/chevrons-right.svg",
                "Last page",
                PageMove::Last,
                more,
            ))
    }

    /// Full-value editor for a multi-line cell, docked under the grid.
    ///
    /// A cell is one row tall and the inline input is single-line (gpui's shaper
    /// panics on a newline), so a value with line breaks is edited here — in full,
    /// nothing truncated. Enter inserts a newline, so committing is Ctrl/Cmd+Enter
    /// or Save; Esc discards like any other cell edit.
    fn render_value_editor(
        tab: &Tab,
        cell_multi: &Entity<InputState>,
        c: Colors,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        if !tab.editing_multi {
            return None;
        }
        let (_, col) = tab.editing?;
        let name = tab
            .headers
            .get(col)
            .cloned()
            .unwrap_or_else(|| format!("column {}", col + 1));
        Some(
            v_flex()
                .h(px(160.))
                .flex_none()
                .border_t_1()
                .border_color(c.border)
                .bg(c.panel)
                .child(
                    h_flex()
                        .px_2()
                        .py_1()
                        .gap_2()
                        .items_center()
                        .bg(c.header)
                        .child(
                            div()
                                .flex_grow()
                                .text_xs()
                                .text_color(c.fg_dim)
                                .child(format!("{name} — multi-line value")),
                        )
                        .child(
                            Button::new("value-cancel")
                                .tab_stop(false)
                                .label("Cancel")
                                .small()
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.cancel_cell_edit(cx)
                                })),
                        )
                        .child(
                            Button::new("value-save")
                                .tab_stop(false)
                                .label("Save")
                                .primary()
                                .small()
                                .tooltip("Ctrl/Cmd+Enter")
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.commit_cell_edit(cx)
                                })),
                        ),
                )
                .child(
                    div()
                        .flex_grow()
                        .p_1()
                        .child(Input::new(cell_multi).h_full()),
                ),
        )
    }

    /// The EXPLAIN plan view: a header with a close button and a scrollable
    /// monospace body (one row per plan line, horizontally scrollable so long
    /// lines aren't clipped).
    fn plan_view(tab_id: u64, plan: &str, c: Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let mono = cx.theme().mono_font_family.clone();
        let mut lines = v_flex().p_2().gap(px(1.));
        for line in plan.lines() {
            lines = lines.child(
                div()
                    .whitespace_nowrap()
                    .text_color(c.fg)
                    .child(line.to_string()),
            );
        }
        v_flex()
            .size_full()
            .bg(c.center)
            .child(
                h_flex()
                    .px_2()
                    .py_1()
                    .items_center()
                    .justify_between()
                    .bg(c.header)
                    .border_b_1()
                    .border_color(c.border)
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(c.fg_dim)
                            .child("QUERY PLAN"),
                    )
                    .child(
                        Button::new("close-plan")
                            .tab_stop(false)
                            .icon(Icon::empty().path("icons/close.svg").text_color(c.fg))
                            .tooltip("Close plan")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.close_plan(tab_id, cx)
                            })),
                    ),
            )
            .child(
                div()
                    .id("plan-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_x_scroll()
                    .overflow_y_scroll()
                    .text_sm()
                    .font_family(mono)
                    .child(lines),
            )
    }

    /// The active tab and its staged-edit count, when it has edits waiting
    /// for review.
    fn pending_review(&self) -> Option<(u64, usize)> {
        self.active_tab()
            .filter(|t| !t.pending.is_empty())
            .map(|t| (t.id, t.pending.len()))
    }

    /// Whether the bottom panel shows at all: the query log or Messages is
    /// toggled on in the status bar, or staged edits wait for review (that
    /// pane holds their Apply / Cancel, so it never hides).
    fn bottom_visible(&self) -> bool {
        self.settings.show_query_log
            || self.settings.show_messages
            || self.pending_review().is_some()
    }

    fn render_bottom(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let pending_tab = self.pending_review();
        let show_right = pending_tab.is_some() || self.settings.show_messages;

        let mut log_list = v_flex().p_1().gap(px(1.));
        for e in self.log.entries.iter().rev() {
            let glyph = if e.ok { "✓" } else { "✗" };
            log_list = log_list.child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(if e.ok { c.fg } else { c.danger })
                    .truncate()
                    .child(format!("{glyph}  {}", oneline(&e.sql))),
            );
        }
        // Next to the right pane it keeps a fixed width; alone it takes the
        // whole panel.
        let log_panel = v_flex()
            .h_full()
            .overflow_hidden()
            .map(|d| {
                if show_right {
                    d.w(LOG_W).flex_none().border_r_1().border_color(c.border)
                } else {
                    d.flex_1().min_w(px(0.))
                }
            })
            .child(section_header("QUERY LOG", c))
            .child(
                div()
                    .id("log-scroll")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .child(log_list),
            );

        // When the active tab has staged edits, the bottom-right pane shows the
        // combined SQL for review; otherwise it shows status messages.
        let right = if let Some((tab_id, n)) = pending_tab {
            let sql = self.pending_sql(tab_id).unwrap_or_default();
            v_flex()
                .flex_grow()
                .h_full()
                .overflow_hidden()
                .child(section_header(
                    format!("PENDING — {n} CHANGE(S), REVIEW THEN APPLY"),
                    c,
                ))
                .child(
                    div()
                        .id("pending-scroll")
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(c.fg)
                        .child(sql),
                )
                .child(
                    h_flex()
                        .px_2()
                        .pb_2()
                        .gap_2()
                        .child(
                            Button::new("apply")
                                .tab_stop(false)
                                .label("Apply")
                                .primary()
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.apply_pending(tab_id, cx)
                                })),
                        )
                        .child(
                            Button::new("cancel-edit")
                                .tab_stop(false)
                                .label("Cancel")
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.cancel_pending(tab_id, cx)
                                })),
                        ),
                )
                .into_any_element()
        } else {
            v_flex()
                .flex_grow()
                .h_full()
                .overflow_hidden()
                .child(section_header("MESSAGES", c))
                .child(
                    div()
                        .id("messages-scroll")
                        .flex_1()
                        .min_h(px(0.))
                        .overflow_y_scroll()
                        .px_3()
                        .py_2()
                        .text_sm()
                        .text_color(c.fg)
                        .child(self.status.clone()),
                )
                .into_any_element()
        };

        h_flex()
            .size_full()
            .overflow_hidden()
            .bg(c.messages)
            .border_t_1()
            .border_color(c.border)
            .when(self.settings.show_query_log, |d| d.child(log_panel))
            .when(show_right, |d| d.child(right))
    }

    /// Zed-style status bar along the window's bottom edge: toggles for the
    /// bottom panels (lit while open) and — while Messages is hidden — the
    /// latest message, so hiding it doesn't hide what just happened.
    fn render_status_bar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        // An open panel's toggle turns blue, so "on" reads at a glance.
        let blue = cx.theme().blue;
        let toggle = |id: &'static str, icon: &'static str, tip: &'static str, on: bool| {
            Button::new(id)
                .ghost()
                .xsmall()
                .tab_stop(false)
                .icon(Icon::empty().path(icon).when(on, |i| i.text_color(blue)))
                .tooltip(tip)
                .selected(on)
        };
        // Pinned to the window width like the title bar (see there): a wide
        // grid's min-content width must not push anything off-screen.
        h_flex()
            .w(window.viewport_size().width)
            .h(STATUS_BAR_H)
            .flex_shrink_0()
            .px_1()
            .gap_1()
            .items_center()
            .bg(c.header)
            .border_t_1()
            .border_color(c.border)
            .child(
                toggle(
                    "toggle-log",
                    "icons/history.svg",
                    "Query log",
                    self.settings.show_query_log,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_query_log(cx))),
            )
            .child(
                toggle(
                    "toggle-messages",
                    "icons/message-square.svg",
                    "Messages",
                    self.settings.show_messages,
                )
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_messages(cx))),
            )
            .when(!self.settings.show_messages, |bar| {
                bar.child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .pl_2()
                        .text_xs()
                        .text_color(c.fg_dim)
                        .truncate()
                        .child(oneline(&self.status)),
                )
            })
    }

    fn render_conn_manager(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let show_form = self.conn_adding || self.settings.connections.is_empty();

        let body = if show_form {
            let label_w = px(96.);
            let label = |text: &'static str| {
                div()
                    .w(label_w)
                    .flex_none()
                    .text_xs()
                    .text_color(c.fg_dim)
                    .child(text)
            };
            let field = |text: &'static str, input: &Entity<InputState>| {
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(label(text))
                    .child(div().w(px(380.)).child(Input::new(input)))
            };
            // Multi-line fields: label at the top, a hint under the input.
            let lines = |text: &'static str, input: &Entity<InputState>, hint: &'static str| {
                h_flex()
                    .gap_2()
                    .items_start()
                    .child(div().pt(px(6.)).child(label(text)))
                    .child(
                        v_flex()
                            .w(px(380.))
                            .gap(px(2.))
                            .child(Input::new(input))
                            .child(div().text_xs().text_color(c.fg_dim).child(hint)),
                    )
            };
            let editing = self.form.editing.is_some();
            let save = Button::new("save-conn")
                .label(if editing { "Save" } else { "Save & Connect" })
                .primary()
                .loading(self.form.applying)
                .disabled(self.form.applying)
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_connection(cx)));
            let cancel = Button::new("cancel-conn")
                .label("Cancel")
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel_conn_form(cx)));
            v_flex()
                .child(section_header(
                    if editing {
                        "EDIT CONNECTION"
                    } else {
                        "ADD CONNECTION"
                    },
                    c,
                ))
                .child(
                    v_flex()
                        .p_2()
                        .gap_1()
                        .child(field("Name", &self.form.name))
                        .child(field("Host", &self.form.host))
                        .child(field("Port", &self.form.port))
                        .child(field("User", &self.form.user))
                        .child(field("Database", &self.form.db))
                        .child(field("Password", &self.form.password))
                        .child(field("SSL mode", &self.form.ssl))
                        .child(div().h(px(4.)))
                        .child(lines(
                            "Parameters",
                            &self.form.params,
                            "key=value per line. libpq keys (application_name, \
                             connect_timeout…) set up the connection, others are server settings.",
                        ))
                        .child(lines(
                            "On connect",
                            &self.form.init_sql,
                            "SQL run on every connection of the pool, e.g. SET statements.",
                        ))
                        .children(self.form.error.clone().map(|e| {
                            div()
                                .pl(label_w + px(8.))
                                .text_xs()
                                .text_color(c.danger)
                                .child(e)
                        }))
                        .child(
                            h_flex()
                                .pt_1()
                                .gap_2()
                                .justify_end()
                                .child(cancel)
                                .child(save),
                        ),
                )
                .into_any_element()
        } else {
            let mut list = v_flex().p_1().gap(px(2.));
            for (i, e) in self.settings.connections.iter().enumerate() {
                let active =
                    self.conn.is_some() && self.cfg.as_ref().is_some_and(|cur| cur.name == e.name);
                let subtitle = format!("{}@{}:{}/{}", e.user, e.host, e.port, e.dbname);
                // The edit button is a SIBLING of the row's click area, not
                // inside it: nested click handlers would both fire.
                list = list.child(
                    h_flex()
                        .pr_1()
                        .gap_1()
                        .items_center()
                        .rounded_md()
                        .when(active, |d| d.bg(c.active))
                        .hover(|s| s.bg(c.hover))
                        .child(
                            div()
                                .id(SharedString::from(format!("conn-{i}")))
                                .flex_1()
                                .min_w(px(0.))
                                .px_2()
                                .py(px(6.))
                                .gap_2()
                                .flex()
                                .items_center()
                                .cursor_pointer()
                                .child(
                                    div()
                                        .flex_none()
                                        .size(px(7.))
                                        .rounded_full()
                                        .when(active, |d| d.bg(rgba(0x22c55eff)))
                                        .when(!active, |d| d.border_1().border_color(c.fg_dim)),
                                )
                                .child(tree_icon("icons/database.svg", c.fg_dim))
                                .child(
                                    v_flex()
                                        .min_w(px(0.))
                                        .gap(px(1.))
                                        .child(
                                            div().text_sm().text_color(c.fg).child(e.name.clone()),
                                        )
                                        .child(
                                            div().text_xs().text_color(c.fg_dim).child(subtitle),
                                        ),
                                )
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.connect_saved(i, cx)
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("edit-conn-{i}")))
                                .ghost()
                                .small()
                                .icon(Icon::empty().path("icons/pencil.svg"))
                                .tooltip("Edit connection")
                                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                    this.show_edit_form(i, window, cx)
                                })),
                        ),
                );
            }
            v_flex()
                .child(section_header("CONNECTIONS", c))
                .child(
                    div()
                        .id("conn-list")
                        .max_h(px(240.))
                        .overflow_y_scroll()
                        .child(list),
                )
                .child(
                    h_flex()
                        .p_2()
                        .gap_2()
                        .justify_end()
                        .border_t_1()
                        .border_color(c.border)
                        .child(Button::new("close-conn").label("Close").on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_connections(cx)),
                        ))
                        .child(Button::new("add-new").label("Add").primary().on_click(
                            cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.show_add_form(window, cx)
                            }),
                        )),
                )
                .into_any_element()
        };

        self.modal_layer(v_flex().mt(px(26.)).w(px(520.)).child(body), cx)
    }

    fn render_settings(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let theme = self.settings.theme;

        let theme_btn =
            |label: &'static str, val: zdb_config::Theme| {
                let selected = theme == val;
                let mut b = Button::new(SharedString::from(format!("theme-{label}"))).label(label);
                if selected {
                    b = b.primary();
                }
                b.on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.set_theme(val, window, cx)
                }))
            };

        let setting_row = |label: &'static str, control: gpui::AnyElement| {
            h_flex()
                .px_3()
                .py_2()
                .items_center()
                .justify_between()
                .border_b_1()
                .border_color(c.border)
                .child(div().text_sm().text_color(c.fg).child(label))
                .child(control)
        };

        let path = zdb_config::Settings::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unavailable)".into());

        // Taller than a small window: it scrolls instead of running off it.
        let body = v_flex()
            .id("settings-body")
            .max_h(window.viewport_size().height - modal::TITLEBAR_H - px(52.))
            .overflow_y_scroll()
            .child(section_header("SETTINGS", c))
            .child(setting_row(
                "Theme",
                h_flex()
                    .gap_1()
                    .child(theme_btn("Light", zdb_config::Theme::Light))
                    .child(theme_btn("Dark", zdb_config::Theme::Dark))
                    .into_any_element(),
            ))
            .child(setting_row(
                "Esc saves cell edit",
                Checkbox::new("esc-saves")
                    .checked(self.settings.esc_saves_edit)
                    .on_click(cx.listener(|this, on: &bool, _, cx| this.set_esc_saves(*on, cx)))
                    .into_any_element(),
            ))
            .child(setting_row(
                "Vim navigation (hjkl, gg / G, 0 / $, i, y)",
                Checkbox::new("vim-nav")
                    .checked(self.settings.vim_navigation)
                    .on_click(
                        cx.listener(|this, on: &bool, _, cx| this.set_vim_navigation(*on, cx)),
                    )
                    .into_any_element(),
            ))
            .child(setting_row(
                "Restore tabs per connection",
                Checkbox::new("restore-tabs")
                    .checked(self.settings.restore_tabs)
                    .on_click(cx.listener(|this, on: &bool, _, cx| this.set_restore_tabs(*on, cx)))
                    .into_any_element(),
            ))
            .child(setting_row(
                "Ask before cancelling a query (click on its loader)",
                Checkbox::new("confirm-cancel")
                    .checked(self.settings.confirm_query_cancel)
                    .on_click(
                        cx.listener(|this, on: &bool, _, cx| {
                            this.set_confirm_query_cancel(*on, cx)
                        }),
                    )
                    .into_any_element(),
            ))
            .child(setting_row(
                "Connections per database",
                self.render_pool_size(c, cx).into_any_element(),
            ))
            .child(setting_row(
                "Keyboard shortcuts",
                Button::new("show-shortcuts")
                    .label("Show (F1)")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_shortcuts(cx)))
                    .into_any_element(),
            ))
            .child(section_header("CONFIG FILE", c))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(c.fg_dim)
                    .child(path),
            )
            .child(
                h_flex().p_2().gap_2().justify_end().child(
                    Button::new("close-settings").label("Close").on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_settings(cx)),
                    ),
                ),
            );

        self.modal_layer(v_flex().mt(px(26.)).w(px(520.)).child(body), cx)
    }

    /// `− N +` for the pool size. Tabs inside a transaction hold one more
    /// each (see `zdb_db::Session`).
    fn render_pool_size(&self, c: Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let n = self.settings.pool_size;
        let step = |id: &'static str, icon: &'static str, to: usize, on: bool| {
            Button::new(id)
                .small()
                .icon(Icon::empty().path(icon))
                .disabled(!on)
                .on_click(
                    cx.listener(move |this, _: &ClickEvent, _, cx| this.set_pool_size(to, cx)),
                )
        };
        h_flex()
            .gap_1()
            .items_center()
            .child(step(
                "pool-less",
                "icons/minus.svg",
                n.saturating_sub(1),
                n > 1,
            ))
            .child(
                div()
                    .w(px(28.))
                    .text_center()
                    .text_sm()
                    .text_color(c.fg)
                    .child(n.to_string()),
            )
            .child(step(
                "pool-more",
                "icons/plus.svg",
                n + 1,
                n < zdb_config::POOL_SIZE_MAX,
            ))
    }

    /// The keyboard-shortcut reference (F1): every section of
    /// `keynav::SHORTCUTS`, flowed into two columns.
    fn render_shortcuts(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let column = |sections: &'static [ShortcutSection]| {
            let mut col = v_flex().flex_1().min_w(px(0.));
            for (title, keys) in sections.iter().copied() {
                let mut list = v_flex().px_3().py_1().gap(px(2.));
                for (action, key) in keys.iter().copied() {
                    list = list.child(
                        h_flex()
                            .py(px(2.))
                            .gap_3()
                            .justify_between()
                            .child(div().text_sm().text_color(c.fg).child(action))
                            .child(
                                div()
                                    .flex_none()
                                    .px_1p5()
                                    .rounded_md()
                                    .bg(c.header)
                                    .text_xs()
                                    .text_color(c.fg_dim)
                                    .child(key),
                            ),
                    );
                }
                col = col.child(section_header(title, c)).child(list);
            }
            col
        };
        // Split so the two columns come out about the same height.
        let rows = |s: &[ShortcutSection]| s.iter().map(|(_, k)| k.len() + 1).sum::<usize>();
        let split = (1..SHORTCUTS.len())
            .min_by_key(|&i| rows(&SHORTCUTS[..i]).abs_diff(rows(&SHORTCUTS[i..])))
            .unwrap_or(SHORTCUTS.len());
        let viewport = window.viewport_size();
        self.modal_layer(
            v_flex()
                .mt(px(14.))
                .w(px(760.))
                .max_w(viewport.width - px(32.))
                .max_h(viewport.height - px(80.))
                .child(
                    h_flex()
                        .id("shortcuts-body")
                        .flex_1()
                        .min_h(px(0.))
                        .items_start()
                        .overflow_y_scroll()
                        .child(column(&SHORTCUTS[..split]))
                        .child(column(&SHORTCUTS[split..])),
                )
                .child(
                    h_flex()
                        .p_2()
                        .justify_between()
                        .items_center()
                        .border_t_1()
                        .border_color(c.border)
                        .child(
                            div()
                                .px_1()
                                .text_xs()
                                .text_color(c.fg_dim)
                                .child("Ctrl is Cmd on macOS"),
                        )
                        .child(Button::new("close-shortcuts").label("Close").on_click(
                            cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_shortcuts(cx)),
                        )),
                ),
            cx,
        )
    }

    fn render_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let query = self.palette_input.read(cx).value().to_lowercase();
        let mut list = v_flex().p_1().gap(px(1.));
        for (label, cmd) in PALETTE_COMMANDS.iter().copied() {
            if !query.is_empty() && !label.to_lowercase().contains(&query) {
                continue;
            }
            list = list.child(
                div()
                    .id(SharedString::from(format!("cmd-{label}")))
                    .px_3()
                    .py_2()
                    .cursor_pointer()
                    .text_sm()
                    .text_color(c.fg)
                    .child(label)
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.run_command(cmd, window, cx)
                    })),
            );
        }

        self.modal_layer(
            v_flex()
                .mt(px(46.))
                .w(px(540.))
                .child(
                    div()
                        .p_2()
                        .border_b_1()
                        .border_color(c.border)
                        .child(Input::new(&self.palette_input)),
                )
                .child(list),
            cx,
        )
    }

    /// Zed-style custom title bar: replaces the OS title bar (the window is
    /// opened with `appears_transparent`). Shows the app name + a connection
    /// status dot + the active connection on the left, a native drag region in
    /// the middle, and our own minimize / maximize-restore / close controls on
    /// the right. We draw the controls ourselves (rather than using
    /// `gpui_component::TitleBar`) because that component's control glyphs take
    /// their color from the ambient `window.text_style()`, which an ancestor
    /// `.text_color` does not reach — so they render invisible. Ours set an
    /// explicit color. On Windows the OS handles drag/click via the
    /// `window_control_area` hit-test hints; on Linux we wire the actions.
    fn render_titlebar(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let connected = self.conn.is_some();
        let conn_name = self
            .cfg
            .as_ref()
            .filter(|_| connected)
            .map(|cfg| cfg.name.clone())
            .unwrap_or_else(|| "Not connected".into());
        let dot = if connected {
            rgba(0x22c55eff)
        } else {
            rgba(0x9ca3afff)
        };

        let (max_icon, max_area) = if window.is_maximized() {
            ("icons/window-restore.svg", WindowControlArea::Max)
        } else {
            ("icons/window-maximize.svg", WindowControlArea::Max)
        };

        // Left: app name + connection status dot + active connection. On macOS
        // the native traffic lights are drawn over the client area at the
        // top-left (traffic_light_position in title_bar_options), so the
        // content starts past them.
        let left = h_flex()
            .flex_shrink_0()
            .h_full()
            .items_center()
            .gap_2()
            .pl(if cfg!(target_os = "macos") {
                px(80.)
            } else {
                px(12.)
            })
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(c.fg)
                    .child("zdb"),
            )
            .child(div().w(px(1.)).h(px(14.)).bg(c.border))
            .child(div().size(px(7.)).rounded_full().bg(dot))
            .child(div().text_xs().text_color(c.fg_dim).child(conn_name));

        let controls = h_flex()
            .flex_shrink_0()
            .h_full()
            .items_center()
            .child(self.window_control(
                "win-min",
                "icons/window-minimize.svg",
                c.fg,
                c.header,
                WindowControlArea::Min,
                cx,
            ))
            .child(self.window_control("win-max", max_icon, c.fg, c.header, max_area, cx))
            .child(self.window_control(
                "win-close",
                "icons/window-close.svg",
                c.fg,
                rgba(0xef4444ff),
                WindowControlArea::Close,
                cx,
            ));

        // The DRAG region must wrap ONLY the left content, not the control
        // buttons. gpui's window-control hit test (events.rs → on_hit_test_window
        // _control) returns the FIRST area in paint order whose hitbox is under
        // the cursor. A parent paints before its children, so tagging the whole
        // bar `Drag` makes every button resolve to `Drag` (the bar wins) and the
        // min/max/close clicks do nothing. Keeping the controls OUTSIDE the drag
        // area — as gpui-component's own TitleBar does — lets each button claim
        // its own Min/Max/Close region. The flex_1 drag region also pushes the
        // controls to the right edge (no `justify_between` needed); `min_w(0)` +
        // `overflow_hidden` stop a long connection name from inflating the bar.
        let drag = h_flex()
            .flex_1()
            .h_full()
            .items_center()
            .min_w(px(0.))
            .overflow_hidden()
            .window_control_area(WindowControlArea::Drag)
            .child(left);

        // On Linux there's no native non-client drag; start a window move when the
        // drag region is pressed.
        #[cfg(not(target_os = "windows"))]
        let drag = drag.id("titlebar").on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _, window, _| window.start_window_move()),
        );

        // The bar width is set EXPLICITLY to the window's logical width. `w_full`
        // does NOT work: the results table's min-content width inflates the layout
        // width past the window, pushing the controls off-screen right. A definite
        // width pins the right edge.
        let mut bar = h_flex()
            .w(window.viewport_size().width)
            .h(modal::TITLEBAR_H)
            .flex_shrink_0()
            .items_center()
            .bg(c.header)
            .border_b_1()
            .border_color(c.border)
            .child(drag);
        // macOS keeps its native traffic lights; drawing our own controls would
        // duplicate them (and ours would overlap the top-right corner).
        if !cfg!(target_os = "macos") {
            bar = bar.child(controls);
        }

        bar
    }

    /// One window-control button (minimize / maximize / close). `hover_bg` is the
    /// background shown on hover (red for close). The icon color is explicit so it
    /// renders regardless of the ambient text style.
    fn window_control(
        &self,
        id: &'static str,
        icon_path: &'static str,
        color: impl Into<Hsla>,
        hover_bg: impl Into<Hsla>,
        area: WindowControlArea,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // `area` is used only on Windows (native hit-test); `cx` only elsewhere
        // (click handlers). Discard the unused one per platform without moving cx.
        #[cfg(target_os = "windows")]
        let _ = cx;
        #[cfg(not(target_os = "windows"))]
        let _ = area;
        let color: Hsla = color.into();
        let hover_bg: Hsla = hover_bg.into();
        let btn = div()
            .id(id)
            .w(px(46.))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|s| s.bg(hover_bg))
            .child(Icon::empty().path(icon_path).text_color(color));

        #[cfg(target_os = "windows")]
        let btn = btn.window_control_area(area);
        #[cfg(not(target_os = "windows"))]
        let btn = btn.on_click(
            cx.listener(move |this, _: &ClickEvent, window, cx| match id {
                "win-min" => window.minimize_window(),
                "win-max" => window.zoom_window(),
                _ => {
                    // `remove_window` bypasses the OS close path, so the hook in
                    // `Workspace::new` never runs — save geometry and tabs here.
                    super::save_window_geometry(window);
                    this.flush_session(cx);
                    window.remove_window()
                }
            }),
        );

        btn
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        // The taskbar / alt-tab can't see our client-area title bar, so mirror
        // the connection label into the real window title. `render` is hot and
        // connections change from windowless contexts, so diff it here rather
        // than threading a `&mut Window` into `conn.rs`.
        let title = window_title(
            self.cfg
                .as_ref()
                .filter(|_| self.conn.is_some())
                .map(|cfg| cfg.name.as_str()),
        );
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
        // Remember size/position for the next launch (debounced; see the fn).
        self.track_window_geometry(window, cx);
        // A table requested from a windowless context (e.g. the selftest) opens
        // here, where the window is available.
        if let Some((schema, table)) = self.pending_open.take() {
            self.open_table_tab(schema, table, window, cx);
        }
        // Just connected: reopen this connection's saved tabs.
        if std::mem::take(&mut self.pending_restore) {
            self.restore_session(window, cx);
        }
        self.load_restored_tab(cx);
        // Filter cleared from a window-less context (connection switch).
        if self.tree.pending_clear_filter {
            self.tree.pending_clear_filter = false;
            self.tree
                .filter_input
                .update(cx, |i, cx| i.set_value("", window, cx));
        }
        self.flush_pending_focus(window);
        self.sync_modal_focus(window, cx);
        if window.focused(cx).is_none() {
            self.root_focus.focus(window);
        }
        // Keep the active tab's chip in view in the scrolling strip — only when
        // the active tab changes (so scrolling it by hand isn't undone), and a
        // frame later: chip positions come from the last layout, which has to
        // show the current tabs already.
        let active = self.active_id();
        if active != self.strip_revealed {
            if active != self.strip_seen {
                self.strip_seen = active;
                cx.notify();
            } else if self.reveal_active_chip() {
                self.strip_revealed = active;
            } else {
                cx.notify();
            }
        }
        let center = self.render_center(window, cx).into_any_element();
        let mut top = h_resizable("zdb-top")
            .child(
                resizable_panel()
                    .size(px(280.))
                    .size_range(px(180.)..px(560.))
                    .child(self.render_sidebar(cx)),
            )
            .child(resizable_panel().child(center));
        if self.terminal_open {
            if let Some(t) = &self.terminal {
                top = top.child(
                    resizable_panel()
                        .size(px(480.))
                        .size_range(px(200.)..px(900.))
                        // Tab / Esc are shell keys here (see `bind_nav_keys`).
                        .child(
                            div()
                                .size_full()
                                .key_context("Terminal")
                                .child(t.view.clone()),
                        ),
                );
            }
        }

        // Both panels need an explicit size: gpui-component's resizable seeds any
        // panel without one to PANEL_MIN_SIZE, which squeezed the bottom panel
        // (query-review / log) off-screen. Sizes are ratio-scaled to the window.
        // Hidden from the status bar, the bottom panel stays in the group as an
        // invisible panel (so its height is remembered for the next show) and
        // the top one, uncapped, fills the space.
        let bottom = self.bottom_visible();
        let main = v_resizable("zdb-root")
            .child(
                resizable_panel()
                    .size(px(560.))
                    .size_range(px(200.)..if bottom { px(1200.) } else { gpui::Pixels::MAX })
                    .child(top),
            )
            .child(
                resizable_panel()
                    .size(px(200.))
                    .size_range(px(80.)..px(460.))
                    .visible(bottom)
                    .when(bottom, |p| p.child(self.render_bottom(cx))),
            );

        // `min_w(0)` + `overflow_hidden` stop a wide descendant (the results table's
        // min-content width) from stretching the column past the window width.
        // Without this the title bar (`w_full`) followed the overflow and its
        // right-aligned controls landed off-screen.
        let content = v_flex()
            .size_full()
            .min_w(px(0.))
            .overflow_hidden()
            .bg(c.center)
            .child(self.render_titlebar(window, cx))
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .min_h(px(0.))
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(main),
            )
            .child(self.render_status_bar(window, cx));

        let root = div()
            .relative()
            .size_full()
            // Window-wide default text color (the ambient `window.text_style()`
            // is otherwise transparent).
            .text_color(c.fg)
            .key_context("zdb")
            // The query / tab chords act on the panes, which a dialog
            // covers: swallowed while one is open.
            .on_action(cx.listener(|this, _: &RunQuery, _, cx| {
                if !this.modal_open() {
                    this.run_active_tab(cx)
                }
            }))
            .on_action(cx.listener(|this, _: &CancelQuery, _, cx| this.cancel(cx)))
            .on_action(cx.listener(|this, _: &ToggleScratch, window, cx| {
                if !this.modal_open() {
                    this.focus_scratch_tab(window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &TogglePalette, _, cx| this.toggle_palette(cx)))
            .on_action(cx.listener(|this, _: &ClosePalette, window, cx| {
                // An open completion menu takes Esc first (see the fn).
                let escape = Box::new(gpui_component::input::Escape);
                if this.completion_menu_action(escape, window, cx) {
                    return;
                }
                // Esc closes the dialog; behind one, nothing else is live.
                if this.modal_open() {
                    this.close_modal(cx);
                    return;
                }
                this.stop_autoscroll(cx);
                // Esc discards the inline cell edit (or stages it, per the
                // setting). Clearing `editing` first means the blur that
                // follows (the input unmounts) finds nothing to commit.
                this.escape_cell_edit(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                if !this.modal_open() {
                    this.toggle_terminal(window, cx)
                }
            }))
            .on_action(
                cx.listener(|this, _: &ToggleConnections, _, cx| this.toggle_connections(cx)),
            )
            .on_action(cx.listener(|this, _: &ToggleSettings, _, cx| this.toggle_settings(cx)));
        let mut root = Self::nav_actions(root, cx)
            // The fallback focus target is a ZERO-SIZE child, not the root itself.
            // A focusable element claims every mouse-down inside it (focus +
            // `prevent_default`), and the root contains the title bar: on Windows
            // gpui then treats the press on min/max/close as handled and never
            // arms the button, so the window couldn't be closed. With no area it
            // is never under the pointer, yet keys still bubble from it to the
            // root's handlers.
            .child(div().absolute().size_0().track_focus(&self.root_focus))
            .child(content);

        // Middle-click autoscroll: the cursor tracking and the exits live on the
        // root, not on the grid, so they keep working when the pointer wanders
        // off the table (and off the window's panels entirely). They're only
        // bound while the mode runs — otherwise the root would swallow the very
        // press that starts it.
        if let Some(anchor) = self.autoscroll_anchor() {
            root = root
                .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _, cx| {
                    this.autoscroll_move(ev.position, cx)
                }))
                .on_mouse_up(
                    MouseButton::Middle,
                    cx.listener(|this, _: &MouseUpEvent, _, cx| this.autoscroll_release(cx)),
                )
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| this.stop_autoscroll(cx)),
                )
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| this.stop_autoscroll(cx)),
                )
                .on_mouse_down(
                    MouseButton::Middle,
                    cx.listener(|this, _: &MouseDownEvent, _, cx| this.stop_autoscroll(cx)),
                )
                .child(autoscroll_marker(anchor, c));
        }

        // The tree's keyboard-opened context menu, anchored under its row.
        if let Some((menu, at)) = &self.tree.menu {
            root = root.child(
                deferred(
                    anchored()
                        .position(*at)
                        .snap_to_window_with_margin(px(8.))
                        .child(menu.clone()),
                )
                .with_priority(1),
            );
        }
        // The query it asks about finished meanwhile: nothing left to cancel.
        if let Some(Modal::ConfirmCancel(id)) = self.modal {
            if !self.tab(id).is_some_and(|t| t.running) {
                self.modal = None;
            }
        }
        let dialog = match self.modal {
            Some(Modal::Connections) => Some(self.render_conn_manager(cx).into_any_element()),
            Some(Modal::Settings) => Some(self.render_settings(window, cx).into_any_element()),
            Some(Modal::Shortcuts) => Some(self.render_shortcuts(window, cx).into_any_element()),
            Some(Modal::Palette) => Some(self.render_palette(cx).into_any_element()),
            Some(Modal::ConfirmCancel(id)) => {
                Some(self.render_confirm_cancel(id, cx).into_any_element())
            }
            None => None,
        };
        root.children(dialog)
    }
}

impl Workspace {
    /// Keyboard-navigation handlers, all on the workspace root: every action is
    /// bound to a key context (see `keynav::bind_nav_keys`), so it only ever
    /// arrives from the pane it belongs to.
    fn nav_actions(root: gpui::Div, cx: &mut Context<Self>) -> gpui::Div {
        // Pane / tab chords: swallowed behind a dialog (they act on what it
        // covers), and shell keys inside the terminal — handed back there (an
        // unhandled action lets the keystroke through to its key handler).
        macro_rules! unless_terminal {
            ($action:ty, |$this:ident, $window:ident, $cx:ident| $body:expr) => {
                cx.listener(|$this, _: &$action, $window, $cx| {
                    if $this.modal_open() {
                        return;
                    }
                    if $this.terminal_focused($window, $cx) {
                        $cx.propagate();
                        return;
                    }
                    $body
                })
            };
        }
        macro_rules! grid {
            ($action:ty, $motion:expr) => {
                cx.listener(|this, _: &$action, _, cx| this.grid_motion($motion, cx))
            };
        }
        macro_rules! tree {
            ($action:ty, $motion:expr) => {
                cx.listener(|this, _: &$action, _, cx| this.tree_move($motion, cx))
            };
        }
        root.on_action(unless_terminal!(FocusTree, |this, window, cx| this.focus_tree(window, cx)))
            .on_action(unless_terminal!(FocusEditor, |this, window, cx| this
                .focus_editor(window, cx)))
            .on_action(unless_terminal!(FocusGrid, |this, window, cx| this.focus_grid(window, cx)))
            .on_action(unless_terminal!(CloseTab, |this, window, cx| this
                .close_active_tab(window, cx)))
            .on_action(unless_terminal!(ApplyEdits, |this, window, cx| this.apply_active(cx)))
            .on_action(cx.listener(|this, _: &ToggleShortcuts, _, cx| this.toggle_shortcuts(cx)))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                if !this.modal_open() {
                    this.cycle_tab(true, window, cx)
                }
            }))
            .on_action(cx.listener(|this, _: &PrevTab, window, cx| {
                if !this.modal_open() {
                    this.cycle_tab(false, window, cx)
                }
            }))
            .on_action(
                cx.listener(|this, _: &FocusNextField, window, cx| {
                    this.step_focus(true, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &FocusPrevField, window, cx| {
                this.step_focus(false, window, cx)
            }))
            .on_action(cx.listener(|this, _: &AcceptCompletion, window, cx| {
                this.accept_completion(window, cx)
            }))
            .on_action(grid!(GridUp, Motion::Up))
            .on_action(grid!(GridDown, Motion::Down))
            .on_action(grid!(GridLeft, Motion::Left))
            .on_action(grid!(GridRight, Motion::Right))
            .on_action(grid!(GridRowStart, Motion::RowStart))
            .on_action(grid!(GridRowEnd, Motion::RowEnd))
            .on_action(grid!(GridFirst, Motion::First))
            .on_action(grid!(GridLast, Motion::Last))
            .on_action(grid!(GridTop, Motion::Top))
            .on_action(grid!(GridBottom, Motion::Bottom))
            .on_action(
                cx.listener(|this, _: &GridPageUp, _, cx| this.grid_page_motion(false, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &GridPageDown, _, cx| this.grid_page_motion(true, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &GridHalfPageUp, _, cx| {
                    this.grid_page_motion(false, true, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &GridHalfPageDown, _, cx| {
                this.grid_page_motion(true, true, cx)
            }))
            .on_action(cx.listener(|this, _: &GridNextCell, _, cx| this.grid_tab(true, cx)))
            .on_action(cx.listener(|this, _: &GridPrevCell, _, cx| this.grid_tab(false, cx)))
            .on_action(cx.listener(|this, _: &GridEdit, window, cx| this.grid_edit(window, cx)))
            .on_action(cx.listener(|this, _: &GridCopyCell, _, cx| this.grid_copy_cell(cx)))
            .on_action(tree!(TreeUp, TreeMove::Up))
            .on_action(tree!(TreeDown, TreeMove::Down))
            .on_action(tree!(TreeFirst, TreeMove::First))
            .on_action(tree!(TreeLast, TreeMove::Last))
            .on_action(tree!(TreePageUp, TreeMove::PageUp))
            .on_action(tree!(TreePageDown, TreeMove::PageDown))
            .on_action(cx.listener(|this, _: &TreeLeft, _, cx| this.tree_expand(false, cx)))
            .on_action(cx.listener(|this, _: &TreeRight, _, cx| this.tree_expand(true, cx)))
            .on_action(cx.listener(|this, _: &TreeOpenSelected, window, cx| {
                this.open_selected_tree_node(window, cx)
            }))
            .on_action(cx.listener(|this, _: &TreeEscape, window, cx| this.tree_escape(window, cx)))
            .on_action(
                cx.listener(|this, _: &TreeMenu, window, cx| this.open_tree_menu(window, cx)),
            )
            .on_action(cx.listener(|this, _: &TreeFocusFilter, window, cx| {
                this.focus_tree_filter(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &FilterToTree, window, cx| this.focus_tree(window, cx)),
            )
    }
}

// ---- helpers -------------------------------------------------------------

fn section_header(label: impl Into<SharedString>, c: Colors) -> impl IntoElement {
    h_flex()
        .px_3()
        .py_1()
        .bg(c.header)
        .border_b_1()
        .border_color(c.border)
        .text_color(c.fg_dim)
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .child(label.into())
}

/// The anchor puck drawn at the wheel-press point while autoscroll runs, so the
/// origin the scroll speed is measured from is visible. Purely decorative — it
/// carries no listeners, so it never takes a hitbox and can't eat the click that
/// exits the mode. Positioned in window coordinates (the root div is the window).
fn autoscroll_marker(anchor: gpui::Point<gpui::Pixels>, c: Colors) -> impl IntoElement {
    const R: f32 = 11.;
    div()
        .absolute()
        .left(anchor.x - px(R))
        .top(anchor.y - px(R))
        .w(px(R * 2.))
        .h(px(R * 2.))
        .rounded_full()
        .border_1()
        .border_color(c.border)
        .bg(c.header)
        .flex()
        .items_center()
        .justify_center()
        .child(div().w(px(4.)).h(px(4.)).rounded_full().bg(c.fg_dim))
}

/// Centered placeholder shown when no tab is open.
fn welcome_pane(c: Colors) -> impl IntoElement {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_2()
        .bg(c.center)
        .child(div().text_sm().text_color(c.fg_dim).child("No tab open"))
        .child(
            div()
                .text_xs()
                .text_color(c.fg_dim)
                .child("Open a table from the sidebar, or press + for a new query"),
        )
}

/// A small tree icon (chevron / database / table / view) tinted to `color`.
fn tree_icon(path: &'static str, color: Hsla) -> impl IntoElement {
    Icon::empty().path(path).text_color(color).small()
}

fn chevron_icon(expanded: bool) -> &'static str {
    if expanded {
        "icons/chevron-down.svg"
    } else {
        "icons/chevron-right.svg"
    }
}

/// One virtualized row of the schema tree, Zed project-panel style: an indent
/// guide per ancestor depth, chevron for folders, kind icon, dim meta text.
/// Relation rows open in a tab from a name mouse-down (its own target that
/// stops propagation, so it doesn't also fire the widget's select+toggle);
/// right-click opens a per-kind context menu.
fn schema_tree_row(
    ix: usize,
    entry: &TreeEntry,
    selected: bool,
    weak: &WeakEntity<Workspace>,
    _window: &mut Window,
    cx: &mut App,
) -> ListItem {
    let c = palette(cx);
    let item = entry.item();
    let (meta, tree_state, sel_bounds) = match weak.upgrade() {
        Some(ws) => {
            let ws = ws.read(cx);
            (
                ws.tree.meta.get(&item.id).cloned(),
                Some(ws.tree.state.clone()),
                Some(ws.tree.sel_bounds.clone()),
            )
        }
        None => (None, None, None),
    };

    // Continuous vertical guides: one 12px column per ancestor depth (rows are
    // uniform-height so the 1px rules line up across rows).
    let mut guides = h_flex().h_full().flex_shrink_0();
    for _ in 0..entry.depth() {
        guides = guides.child(
            div()
                .w(px(12.))
                .h_full()
                .flex_shrink_0()
                .child(div().w(px(1.)).h_full().ml(px(5.)).bg(c.border)),
        );
    }

    let mut row = h_flex()
        .h_full()
        .flex_1()
        .min_w(px(0.))
        .items_center()
        .gap_1p5()
        .overflow_hidden();
    row = row.child(if entry.is_folder() {
        tree_icon(chevron_icon(entry.is_expanded()), c.fg_dim).into_any_element()
    } else {
        div().w(px(14.)).flex_shrink_0().into_any_element()
    });

    row = match &meta {
        Some(NodeMeta::Db) => row
            .child(tree_icon("icons/database.svg", c.accent))
            .child(div().truncate().child(item.label.clone())),
        Some(NodeMeta::Schema { .. }) => row
            .child(tree_icon("icons/table.svg", c.fg_dim))
            .child(div().truncate().child(item.label.clone())),
        Some(NodeMeta::Rel { schema, name, kind }) => {
            let open = {
                let weak = weak.clone();
                let ts = tree_state.clone();
                let (s, t) = (schema.clone(), name.clone());
                move |ev: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                    // Keep the widget's row handler (select+toggle) from also
                    // firing; select manually, then open the tab.
                    cx.stop_propagation();
                    if let Some(ts) = &ts {
                        ts.update(cx, |ts, cx| ts.set_selected_index(Some(ix), cx));
                    }
                    let double = ev.click_count >= 2;
                    weak.update(cx, |this, cx| {
                        this.click_tree_relation(s.clone(), t.clone(), double, window, cx)
                    })
                    .ok();
                }
            };
            row.child(
                h_flex()
                    .id(("rel-open", ix))
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .items_center()
                    .gap_1p5()
                    .cursor_pointer()
                    .child(tree_icon(rel_icon(*kind), c.fg_dim))
                    .child(div().truncate().child(name.clone()))
                    .on_mouse_down(MouseButton::Left, open),
            )
        }
        Some(NodeMeta::Group) => row.child(
            div()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(c.fg_dim)
                .child(item.label.clone()),
        ),
        // Name must not be squeezed out by a long meta ("timestamp without
        // time zone NOT NULL"): name keeps its content width, meta shrinks
        // with an ellipsis.
        Some(NodeMeta::Leaf { name, meta }) => row
            .child(div().text_color(c.fg).flex_shrink_0().child(name.clone()))
            .when(!meta.is_empty(), |d| {
                d.child(
                    div()
                        .text_xs()
                        .text_color(c.fg_dim)
                        .min_w(px(0.))
                        .truncate()
                        .child(meta.clone()),
                )
            }),
        // Placeholder ("loading…") or unknown: the label, dimmed by the
        // widget's disabled styling.
        _ => row.child(item.label.clone()),
    };

    let content = h_flex()
        .h(px(24.))
        .items_center()
        .child(guides)
        .child(row)
        // Remember where the selected row is painted: the keyboard-opened
        // context menu anchors to it.
        .children(sel_bounds.filter(|_| selected).map(|cell| {
            canvas(move |bounds, _, _| cell.set(Some(bounds)), |_, _, _, _| {})
                .absolute()
                .size_full()
        }));

    // Per-kind right-click menu (the same items Shift+F10 shows).
    let content: AnyElement = match meta.filter(has_node_menu) {
        Some(meta) => {
            let weak = weak.clone();
            div()
                .w_full()
                .child(content)
                .context_menu(move |menu, _, _| node_menu(menu, &meta, &weak))
                .into_any_element()
        }
        None => content.into_any_element(),
    };

    ListItem::new(ix)
        .h(px(24.))
        .py_0()
        .px_1()
        .text_sm()
        .rounded_md()
        .child(content)
}

/// Whether a tree node has a context menu at all.
pub(super) fn has_node_menu(meta: &NodeMeta) -> bool {
    matches!(
        meta,
        NodeMeta::Rel { .. } | NodeMeta::Schema { .. } | NodeMeta::Leaf { .. }
    )
}

/// The context-menu items for a tree node — shared by the right-click menu and
/// the keyboard-opened one.
pub(super) fn node_menu(
    menu: PopupMenu,
    meta: &NodeMeta,
    weak: &WeakEntity<Workspace>,
) -> PopupMenu {
    let copy_name = |text: String| {
        PopupMenuItem::new("Copy name").on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
        })
    };
    match meta {
        NodeMeta::Rel { schema, name, .. } => {
            let (s, t, weak) = (schema.clone(), name.clone(), weak.clone());
            menu.item(PopupMenuItem::new("Open").on_click(move |_, window, cx| {
                weak.update(cx, |this, cx| {
                    this.open_table_tab(s.clone(), t.clone(), window, cx)
                })
                .ok();
            }))
            .item(copy_name(format!("{schema}.{name}")))
        }
        NodeMeta::Schema { name } => {
            let (n, weak) = (name.clone(), weak.clone());
            menu.item(PopupMenuItem::new("Refresh").on_click(move |_, _, cx| {
                weak.update(cx, |this, cx| this.load_relations(n.clone(), cx))
                    .ok();
            }))
            .item(copy_name(name.clone()))
        }
        NodeMeta::Leaf { name, .. } => menu.item(copy_name(name.clone())),
        _ => menu,
    }
}
