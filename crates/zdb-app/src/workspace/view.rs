//! All view code for `Workspace`: the `render_*` panel/modal builders, the
//! `impl Render`, and small free view helpers. Split out of `mod.rs`. This is a
//! child module, so it reaches `Workspace`'s private fields and logic methods
//! directly — no visibility bumps needed.

use super::*;
use gpui::{AnyElement, MouseButton, MouseDownEvent, WeakEntity};
use gpui_component::list::ListItem;
use gpui_component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_component::tree::{tree, TreeEntry};
use gpui_component::ActiveTheme;

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
                    .icon(IconName::Globe)
                    .tooltip("Connections")
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_connections(cx)),
                    ),
            )
            .child(
                Button::new("settings")
                    .icon(Icon::empty().path("icons/settings.svg"))
                    .tooltip("Settings")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_settings(cx))),
            );
        if connected {
            actions = actions.child(
                Button::new("refresh")
                    .icon(Icon::empty().path("icons/refresh-cw.svg"))
                    .tooltip("Refresh schemas")
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.connect_or_refresh(cx))),
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
                    div()
                        .flex_1()
                        .child(Input::new(&self.filter_input).appearance(false).small()),
                )
        });

        let body: AnyElement = if connected {
            let weak = cx.weak_entity();
            div()
                .flex_grow()
                .min_h(px(0.))
                .on_action(cx.listener(|this, _: &TreeOpenSelected, window, cx| {
                    this.open_selected_tree_node(window, cx)
                }))
                .child(tree(&self.tree_state, move |ix, entry, selected, window, cx| {
                    schema_tree_row(ix, entry, selected, &weak, window, cx)
                }))
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

    fn render_center(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let strip = self.render_tab_strip(c, cx);
        let body = match self.active {
            Some(idx) if idx < self.tabs.len() => {
                Self::render_tab_body(&self.tabs[idx], c, cx).into_any_element()
            }
            _ => welcome_pane(c).into_any_element(),
        };
        v_flex().size_full().bg(c.center).child(strip).child(body)
    }

    /// The Zed-style tab strip: one chip per open tab + `+` (new query) and a
    /// scratch button.
    fn render_tab_strip(&self, c: Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let mut strip = h_flex()
            .h(px(34.))
            .flex_shrink_0()
            .items_center()
            .bg(c.header)
            .border_b_1()
            .border_color(c.border)
            .overflow_hidden();
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
            strip = strip.child(
                h_flex()
                    .h_full()
                    .items_center()
                    .flex_shrink_0()
                    .border_r_1()
                    .border_color(c.border)
                    .when(is_active, |d| d.bg(c.center))
                    .child(label)
                    .child(close),
            );
        }
        strip
            .child(
                Button::new("tab-add")
                    .icon(IconName::Plus)
                    .tooltip("New query")
                    .on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| this.open_query_tab(window, cx)),
                    ),
            )
            .child(
                Button::new("tab-scratch")
                    .icon(IconName::File)
                    .tooltip("Scratch (Ctrl/Cmd+Shift+E)")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.focus_scratch_tab(window, cx)
                    })),
            )
    }

    /// The body of one tab: the action toolbar + (editor split | table grid).
    fn render_tab_body(tab: &Tab, c: Colors, cx: &mut Context<Self>) -> impl IntoElement {
        let tab_id = tab.id;
        let editable = tab.edit_target.is_some();
        let is_editor = !matches!(tab.kind, TabKind::Table { .. });
        let has_result = !tab.headers.is_empty();
        let has_sql = match &tab.kind {
            TabKind::Table { .. } => true,
            _ => !tab.editor.read(cx).value().trim().is_empty(),
        };

        // Run toggles to Stop while a query is in flight.
        let run_btn = if tab.running {
            Button::new("run")
                .icon(Icon::empty().path("icons/circle-x.svg").text_color(rgba(0xef4444ff)))
                .tooltip("Stop")
                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.cancel(cx)))
        } else {
            Button::new("run")
                .icon(Icon::empty().path("icons/play.svg").text_color(rgba(0x22c55eff)))
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
        // Red only when actionable; neutral (theme fg) when disabled.
        let del_enabled = editable && tab.current_row.is_some();
        let del_color: Hsla = if del_enabled {
            rgba(0xef4444ff).into()
        } else {
            c.fg
        };
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
                    .icon(IconName::Plus)
                    .tooltip("Add row")
                    .disabled(!editable)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.add_row(tab_id, cx))),
            )
            .child(
                Button::new("del-row")
                    .icon(Icon::empty().path("icons/minus.svg").text_color(del_color))
                    .tooltip(del_tip)
                    .disabled(!del_enabled)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.delete_current_row(tab_id, cx)
                    })),
            )
            .child(
                Button::new("reload")
                    .icon(Icon::empty().path("icons/refresh-cw.svg").text_color(c.fg))
                    .tooltip("Refresh data")
                    .disabled(tab.base_sql.is_none())
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.reload_data(tab_id, cx)
                    })),
            )
            // EXPLAIN the current statement (plan shown in the grid).
            .child(
                Button::new("explain")
                    .label("Explain")
                    .tooltip("EXPLAIN this query")
                    .disabled(!has_sql)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.explain_active(false, cx)
                    })),
            )
            // Export / copy the current result.
            .child(
                Button::new("export-csv")
                    .label("Export")
                    .tooltip("Export result to CSV (more formats in the palette)")
                    .disabled(!has_result)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.export_active(ExportFormat::Csv, cx)
                    })),
            )
            .child(
                Button::new("copy-tsv")
                    .label("Copy")
                    .tooltip("Copy result to clipboard (TSV)")
                    .disabled(!has_result)
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.copy_active_tsv(cx))),
            );
        if is_editor {
            toolbar = toolbar.child(
                Button::new("format-sql")
                    .label("Format")
                    .tooltip("Format SQL")
                    .disabled(!has_sql)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.format_active(window, cx)
                    })),
            );
        }
        if tab.new_row_idx.is_some() {
            toolbar = toolbar.child(
                Button::new("save-row")
                    .icon(IconName::Check)
                    .tooltip("Save row")
                    .primary()
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.save_new_row(tab_id, cx)
                    })),
            );
        }

        let mut results = v_flex().size_full();
        if let TabKind::Table { schema, table } = &tab.kind {
            results = results.child(
                h_flex()
                    .px_2()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .bg(c.header)
                    .border_b_1()
                    .border_color(c.border)
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(c.fg_dim)
                            .child(format!("{schema}.{table}  WHERE")),
                    )
                    .child(div().flex_grow().child(Input::new(&tab.where_input))),
            );
        }
        // The results area shows the grid, or — after EXPLAIN — the plan text in
        // that same place (query data vs. plan, same spot).
        results = if let Some(plan) = &tab.plan {
            results.child(Self::plan_view(tab_id, plan, c, cx))
        } else {
            results.child(
                div()
                    .size_full()
                    .child(Table::new(&tab.table).stripe(true).bordered(true)),
            )
        };

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

        v_flex().size_full().bg(c.center).child(toolbar).child(body)
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

    fn render_bottom(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);

        let mut log_list = v_flex().p_1().gap(px(1.));
        for e in self.log_entries.iter().rev() {
            let glyph = if e.ok { "✓" } else { "✗" };
            log_list = log_list.child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(if e.ok { c.fg } else { c.fg_null })
                    .truncate()
                    .child(format!("{glyph}  {}", oneline(&e.sql))),
            );
        }
        let log_panel = v_flex()
            .w(px(520.))
            .flex_none()
            .h_full()
            .overflow_hidden()
            .border_r_1()
            .border_color(c.border)
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
        let pending_tab = self
            .active_tab()
            .filter(|t| !t.pending.is_empty())
            .map(|t| (t.id, t.pending.len()));
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
                                .label("Apply")
                                .primary()
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.apply_pending(tab_id, cx)
                                })),
                        )
                        .child(Button::new("cancel-edit").label("Cancel").on_click(
                            cx.listener(move |this, _: &ClickEvent, _, cx| {
                                this.cancel_pending(tab_id, cx)
                            }),
                        )),
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
            .child(log_panel)
            .child(right)
    }

    fn render_conn_manager(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let show_form = self.conn_adding || self.settings.connections.is_empty();

        let body = if show_form {
            let field = |label: &'static str, input: &Entity<InputState>| {
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(96.))
                            .flex_none()
                            .text_xs()
                            .text_color(c.fg_dim)
                            .child(label),
                    )
                    .child(div().w(px(320.)).child(Input::new(input)))
            };
            let mut buttons = h_flex().gap_2().pt_1().child(
                Button::new("add-conn")
                    .label("Save & Connect")
                    .primary()
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.add_connection(cx))),
            );
            // Allow returning to the list if there are saved connections.
            buttons = if self.settings.connections.is_empty() {
                buttons.child(Button::new("close-form").label("Close").on_click(
                    cx.listener(|this, _: &ClickEvent, _, cx| this.toggle_connections(cx)),
                ))
            } else {
                buttons.child(Button::new("back-list").label("Back").on_click(cx.listener(
                    |this, _: &ClickEvent, _, cx| this.show_conn_list(cx),
                )))
            };
            v_flex()
                .child(section_header("ADD CONNECTION", c))
                .child(
                    v_flex()
                        .p_2()
                        .gap_1()
                        .child(field("Name", &self.f_name))
                        .child(field("Host", &self.f_host))
                        .child(field("Port", &self.f_port))
                        .child(field("User", &self.f_user))
                        .child(field("Database", &self.f_db))
                        .child(field("Password", &self.f_password))
                        .child(field("SSL mode", &self.f_ssl))
                        .child(buttons),
                )
                .into_any_element()
        } else {
            let mut list = v_flex().p_1().gap(px(2.));
            for (i, e) in self.settings.connections.iter().enumerate() {
                let active =
                    self.conn.is_some() && self.cfg.as_ref().is_some_and(|cur| cur.name == e.name);
                let subtitle = format!("{}@{}:{}/{}", e.user, e.host, e.port, e.dbname);
                list = list.child(
                    div()
                        .id(SharedString::from(format!("conn-{i}")))
                        .px_2()
                        .py(px(6.))
                        .gap_2()
                        .flex()
                        .items_center()
                        .rounded_md()
                        .cursor_pointer()
                        .when(active, |d| d.bg(c.active))
                        .hover(|s| s.bg(c.hover))
                        .child(
                            div()
                                .size(px(7.))
                                .rounded_full()
                                .when(active, |d| d.bg(rgba(0x22c55eff)))
                                .when(!active, |d| d.border_1().border_color(c.fg_dim)),
                        )
                        .child(tree_icon("icons/database.svg", c.fg_dim))
                        .child(
                            v_flex()
                                .gap(px(1.))
                                .child(div().text_sm().text_color(c.fg).child(e.name.clone()))
                                .child(div().text_xs().text_color(c.fg_dim).child(subtitle)),
                        )
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.connect_saved(i, cx)
                        })),
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
                        .border_t_1()
                        .border_color(c.border)
                        .child(
                            Button::new("add-new")
                                .label("Add")
                                .primary()
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.show_add_form(cx)
                                })),
                        )
                        .child(Button::new("close-conn").label("Close").on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| this.toggle_connections(cx),
                        ))),
                )
                .into_any_element()
        };

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .justify_center()
            .items_start()
            .bg(rgba(0x000000aa))
            .child(
                v_flex()
                    .mt(px(60.))
                    .w(px(520.))
                    .bg(c.panel)
                    .border_1()
                    .border_color(c.border)
                    .rounded_md()
                    .child(body),
            )
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        let theme = self.settings.theme;

        let theme_btn = |label: &'static str, val: zdb_config::Theme| {
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

        // Keybinding reference (display-only).
        let keys: &[(&str, &str)] = &[
            ("Run query", "Ctrl+Enter"),
            ("Command palette", "Ctrl+Shift+P"),
            ("Connections", "Ctrl+Shift+O"),
            ("Scratch editor", "Ctrl+Shift+E"),
            ("Terminal", "Ctrl+`"),
            ("Cancel query", "Esc"),
        ];
        let mut keys_list = v_flex().px_3().py_1().gap(px(2.));
        for (action, key) in keys.iter().copied() {
            keys_list = keys_list.child(
                h_flex()
                    .py(px(2.))
                    .justify_between()
                    .child(div().text_sm().text_color(c.fg).child(action))
                    .child(
                        div()
                            .px_1p5()
                            .rounded_md()
                            .bg(c.header)
                            .text_xs()
                            .text_color(c.fg_dim)
                            .child(key),
                    ),
            );
        }

        let path = zdb_config::Settings::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(unavailable)".into());

        let body = v_flex()
            .child(section_header("SETTINGS", c))
            .child(setting_row(
                "Theme",
                h_flex()
                    .gap_1()
                    .child(theme_btn("Light", zdb_config::Theme::Light))
                    .child(theme_btn("Dark", zdb_config::Theme::Dark))
                    .into_any_element(),
            ))
            .child(section_header("KEYBINDINGS", c))
            .child(keys_list)
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
                    Button::new("close-settings")
                        .label("Close")
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.toggle_settings(cx)
                        })),
                ),
            );

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .justify_center()
            .items_start()
            .bg(rgba(0x000000aa))
            .child(
                v_flex()
                    .mt(px(60.))
                    .w(px(520.))
                    .bg(c.panel)
                    .border_1()
                    .border_color(c.border)
                    .rounded_md()
                    .child(body),
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

        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .justify_center()
            .items_start()
            .bg(rgba(0x000000aa))
            .child(
                v_flex()
                    .mt(px(80.))
                    .w(px(540.))
                    .bg(c.panel)
                    .border_1()
                    .border_color(c.border)
                    .rounded_md()
                    .child(
                        div()
                            .p_2()
                            .border_b_1()
                            .border_color(c.border)
                            .child(Input::new(&self.palette_input)),
                    )
                    .child(list),
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

        // Left: app name + connection status dot + active connection.
        let left = h_flex()
            .flex_shrink_0()
            .h_full()
            .items_center()
            .gap_2()
            .pl_3()
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
            .child(self.window_control(
                "win-max",
                max_icon,
                c.fg,
                c.header,
                max_area,
                cx,
            ))
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
        let bar = h_flex()
            .w(window.viewport_size().width)
            .h(px(34.))
            .flex_shrink_0()
            .items_center()
            .bg(c.header)
            .border_b_1()
            .border_color(c.border)
            .child(drag)
            .child(controls);

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
        let btn = btn.on_click(cx.listener(move |_, _: &ClickEvent, window, _| match id {
            "win-min" => window.minimize_window(),
            "win-max" => window.zoom_window(),
            _ => window.remove_window(),
        }));

        btn
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let c = palette(cx);
        // A table requested from a windowless context (e.g. the selftest) opens
        // here, where the window is available.
        if let Some((schema, table)) = self.pending_open.take() {
            self.open_table_tab(schema, table, window, cx);
        }
        // Filter cleared from a window-less context (connection switch).
        if self.pending_clear_filter {
            self.pending_clear_filter = false;
            self.filter_input
                .update(cx, |i, cx| i.set_value("", window, cx));
        }
        let center = self.render_center(cx).into_any_element();
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
                        .child(div().size_full().child(t.view.clone())),
                );
            }
        }

        // Both panels need an explicit size: gpui-component's resizable seeds any
        // panel without one to PANEL_MIN_SIZE, which squeezed the bottom panel
        // (query-review / log) off-screen. Sizes are ratio-scaled to the window.
        let main = v_resizable("zdb-root")
            .child(
                resizable_panel()
                    .size(px(560.))
                    .size_range(px(200.)..px(1200.))
                    .child(top),
            )
            .child(
                resizable_panel()
                    .size(px(200.))
                    .size_range(px(80.)..px(460.))
                    .child(self.render_bottom(cx)),
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
            );

        let mut root = div()
            .relative()
            .size_full()
            // Window-wide default text color (the ambient `window.text_style()`
            // is otherwise transparent).
            .text_color(c.fg)
            .key_context("zdb")
            .on_action(cx.listener(|this, _: &RunQuery, _, cx| this.run_active_tab(cx)))
            .on_action(cx.listener(|this, _: &CancelQuery, _, cx| this.cancel(cx)))
            .on_action(cx.listener(|this, _: &ToggleScratch, window, cx| {
                this.focus_scratch_tab(window, cx)
            }))
            .on_action(cx.listener(|this, _: &TogglePalette, _, cx| this.toggle_palette(cx)))
            .on_action(cx.listener(|this, _: &ClosePalette, _, cx| {
                this.close_palette(cx);
                if this.conn_manager_open {
                    this.conn_manager_open = false;
                    cx.notify();
                }
                if this.settings_open {
                    this.settings_open = false;
                    cx.notify();
                }
            }))
            .on_action(cx.listener(|this, _: &ToggleTerminal, window, cx| {
                this.toggle_terminal(window, cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleConnections, _, cx| {
                this.toggle_connections(cx)
            }))
            .on_action(cx.listener(|this, _: &ToggleSettings, _, cx| this.toggle_settings(cx)))
            .child(content);

        if self.conn_manager_open {
            root = root.child(self.render_conn_manager(cx));
        }
        if self.settings_open {
            root = root.child(self.render_settings(cx));
        }
        if self.palette_open {
            root = root.child(self.render_palette(cx));
        }
        root
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
    _selected: bool,
    weak: &WeakEntity<Workspace>,
    _window: &mut Window,
    cx: &mut App,
) -> ListItem {
    let c = palette(cx);
    let item = entry.item();
    let (meta, tree_state) = match weak.upgrade() {
        Some(ws) => {
            let ws = ws.read(cx);
            (ws.node_meta.get(&item.id).cloned(), Some(ws.tree_state.clone()))
        }
        None => (None, None),
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
                move |_: &MouseDownEvent, window: &mut Window, cx: &mut App| {
                    // Keep the widget's row handler (select+toggle) from also
                    // firing; select manually, then open the tab.
                    cx.stop_propagation();
                    if let Some(ts) = &ts {
                        ts.update(cx, |ts, cx| ts.set_selected_index(Some(ix), cx));
                    }
                    weak.update(cx, |this, cx| {
                        this.open_table_tab(s.clone(), t.clone(), window, cx)
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

    let content = h_flex().h(px(24.)).items_center().child(guides).child(row);

    // Per-kind right-click menu.
    let content: AnyElement = match &meta {
        Some(NodeMeta::Rel { schema, name, .. }) => {
            let (s, t, weak) = (schema.clone(), name.clone(), weak.clone());
            div()
                .w_full()
                .child(content)
                .context_menu(move |menu, _, _| {
                    let (s1, t1, w1) = (s.clone(), t.clone(), weak.clone());
                    let (s2, t2) = (s.clone(), t.clone());
                    menu.item(PopupMenuItem::new("Open").on_click(move |_, window, cx| {
                        w1.update(cx, |this, cx| {
                            this.open_table_tab(s1.clone(), t1.clone(), window, cx)
                        })
                        .ok();
                    }))
                    .item(PopupMenuItem::new("Copy name").on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(format!("{s2}.{t2}")));
                    }))
                })
                .into_any_element()
        }
        Some(NodeMeta::Schema { name }) => {
            let (n, weak) = (name.clone(), weak.clone());
            div()
                .w_full()
                .child(content)
                .context_menu(move |menu, _, _| {
                    let (n1, w1) = (n.clone(), weak.clone());
                    let n2 = n.clone();
                    menu.item(PopupMenuItem::new("Refresh").on_click(move |_, _, cx| {
                        w1.update(cx, |this, cx| this.load_relations(n1.clone(), cx)).ok();
                    }))
                    .item(PopupMenuItem::new("Copy name").on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(n2.clone()));
                    }))
                })
                .into_any_element()
        }
        Some(NodeMeta::Leaf { name, .. }) => {
            let n = name.clone();
            div()
                .w_full()
                .child(content)
                .context_menu(move |menu, _, _| {
                    let n1 = n.clone();
                    menu.item(PopupMenuItem::new("Copy name").on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(n1.clone()));
                    }))
                })
                .into_any_element()
        }
        _ => content.into_any_element(),
    };

    ListItem::new(ix)
        .h(px(24.))
        .py_0()
        .px_1()
        .text_sm()
        .rounded_md()
        .child(content)
}
