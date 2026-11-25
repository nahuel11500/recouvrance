//! The window layout and its actions.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::table::DataTable;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Selectable as _, Sizable as _, Theme, ThemeMode, TitleBar, WindowExt as _,
    h_flex, h_resizable, resizable_panel, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{
    AppView, CloseTab, CloseWindow, ConnectArgoCd, EscapeBack, FocusFilter, GoAssistant, GoBack, GoCluster, GoForward,
    GoHelm, GoProblems, GoSettings, GoWorkloads, MinimizeWindow, NewTab, NextTab, OpenAccess, OpenAgentActivity,
    OpenAgents, OpenApiRequests, OpenApply, OpenArgo, OpenContexts, OpenHelmInstall, OpenPalette, OpenSelected,
    OpenTerminal, OpenTimeline, OpenUpgradeCheck, PrevTab, RefreshPage, Route, RowCopyName, RowDelete, RowEvents,
    RowFavorite, RowLogs, RowOpen, RowOpenTab, RowRestart, RowRun, RowScale, RowShell, RowYaml, ShowAbout,
    ShowShortcuts, ToggleAssistant, ToggleFavorite, ToggleSidebar, ToggleTheme, ToggleWriteMode, ZoomIn, ZoomOut,
    ZoomReset, ZoomWindow, set_theme, with_keys,
};
use crate::favorites::Favorite;
use crate::ui::a11y::Control as _;

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(feature = "dev")]
        let _t = crate::dev::timing::span("app render");
        self.keep_focus(window, cx);
        self.sync_kind_filter(window, cx);
        let page = self.render_content(cx);
        // The assistant beside the page, told what is on screen now.
        let page = match self.docked_assistant() {
            Some(assistant) => {
                let scope = self.assistant_scope(cx);
                assistant.update(cx, |a, cx| a.set_scope(scope, cx));
                h_resizable("assistant-dock")
                    .child(resizable_panel().child(page))
                    .child(resizable_panel().size(px(420.)).size_range(px(300.)..px(900.)).child(assistant))
                    .into_any_element()
            }
            None => page,
        };
        let content = v_flex()
            .size_full()
            .children(self.render_tabs(cx))
            .child(div().flex_1().min_h_0().child(page))
            .into_any_element();
        let body = if self.sidebar_hidden {
            div().size_full().child(content).into_any_element()
        } else {
            // The sidebar keeps its width when the window is resized; the page takes the rest.
            let (width, range) =
                (crate::settings::with(crate::settings::sidebar_width_in), crate::settings::SIDEBAR_WIDTH_RANGE);
            h_resizable("body")
                // The width dragged to is kept for the next launch.
                .on_resize(|state, _, cx| {
                    if let Some(w) = state.read(cx).sizes().first().map(|w| f32::from(*w))
                        && w != crate::settings::with(|s| s.sidebar_width)
                    {
                        crate::settings::update(|s| s.sidebar_width = w);
                    }
                })
                .child(
                    resizable_panel()
                        .size(px(width))
                        .size_range(px(*range.start())..px(*range.end()))
                        .fixed()
                        .child(self.render_sidebar(cx)),
                )
                .child(resizable_panel().child(content))
                .into_any_element()
        };
        let app = v_flex().id("app").relative().size_full().track_focus(&self.focus).key_context("App");
        self.with_actions(app, cx)
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .children(self.context_strip())
            .child(TitleBar::new().child(self.render_toolbar(cx)))
            .child(self.approvals.clone())
            .child(div().flex_1().min_h_0().child(body))
            .children(self.palette.clone())
    }
}

impl AppView {
    /// Settings, on its About section.
    pub(super) fn show_about(&mut self, cx: &mut Context<Self>) {
        self.settings_view.update(cx, crate::ui::settings_view::SettingsView::show_about);
        self.navigate(Route::Settings, cx);
    }

    /// Keep keyboard focus somewhere useful: when the focused element went away
    /// (e.g. the table after opening a row), the list gets it back, or the app
    /// itself so Esc, Ctrl+K and the rest keep working.
    /// A handle can stay focused after its element stopped being drawn, so
    /// check that focus is really inside this view (or a popup over it).
    /// Dialogs and sheets draw outside this view: leave focus alone while one is
    /// open, or their inputs lose it on every frame and can't be typed in.
    fn keep_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        let focused = window.focused(cx);
        let lost = focused.is_none()
            || (!self.focus.contains_focused(window, cx) && self.palette.is_none() && !self.ctx_open && !self.ns_open);
        let overlay = window.has_active_dialog(cx) || window.has_active_sheet(cx);
        if !overlay && (lost || focused.as_ref() == Some(&self.focus)) {
            if matches!(self.route(), Route::Resources(_)) && !self.clusters.is_empty() {
                let table_focus = self.table.read(cx).focus_handle(cx);
                window.focus(&table_focus, cx);
            } else if lost {
                window.focus(&self.focus, cx);
            }
        }
    }

    /// The top bar: which clusters and namespaces, then app-wide actions.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let (view, view2) = (cx.entity().downgrade(), cx.entity().downgrade());
        let refreshable = matches!(
            self.route(),
            Route::Cluster(_)
                | Route::Workloads(_)
                | Route::Problems(_)
                | Route::Timeline(_)
                | Route::Helm(_)
                | Route::Argo(_)
        );
        h_flex()
            .flex_1()
            .min_w_0()
            .gap_2()
            .pr_2()
            .child(
                Button::new("toggle-sidebar")
                    .ghost()
                    .small()
                    .icon(if self.sidebar_hidden { Lucide::PanelLeftOpen } else { Lucide::PanelLeftClose })
                    .accessibility_label(if self.sidebar_hidden { "Show the sidebar" } else { "Hide the sidebar" })
                    .tooltip(with_keys("Show/hide the sidebar ({secondary-b})"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.sidebar_hidden = !this.sidebar_hidden;
                        cx.notify();
                    })),
            )
            .child(
                Popover::new("ctx-popover")
                    .open(self.ctx_open)
                    .on_open_change(move |open, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.ctx_open = *open;
                            cx.notify();
                        });
                    })
                    .trigger(self.ctx_trigger(&theme))
                    .child(self.ctx_picker.clone()),
            )
            .when(!self.clusters.is_empty(), |d| {
                d.child(
                    Popover::new("ns-popover")
                        .open(self.ns_open)
                        .on_open_change(move |open, _, cx| {
                            let _ = view2.update(cx, |this, cx| {
                                this.ns_open = *open;
                                cx.notify();
                            });
                        })
                        .trigger(self.ns_trigger(&theme))
                        .track_focus(&self.ns_picker.read(cx).focus_handle(cx))
                        .child(self.ns_picker.clone()),
                )
            })
            .child(div().flex_1())
            .when(refreshable, |d| {
                d.child(
                    Button::new("refresh-page")
                        .ghost()
                        .small()
                        .icon(Lucide::RefreshCw)
                        .accessibility_label("Refresh")
                        .tooltip(with_keys("Refresh ({f5|secondary-r})"))
                        .on_click(cx.listener(|this, _, window, cx| this.refresh_page(window, cx))),
                )
            })
            .child(
                search_field(&theme, with_keys("{secondary-k}"))
                    .on_click(cx.listener(|this, _, window, cx| this.open_palette(window, cx))),
            )
            .child(write_mode_button(cx))
            .child(
                Button::new("toggle-assistant")
                    .ghost()
                    .small()
                    .icon(Lucide::Sparkles)
                    .accessibility_label("Ask the assistant about what is on screen")
                    .selected(self.docked_assistant().is_some())
                    .tooltip(with_keys("Ask the assistant about what is on screen ({secondary-j})"))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_assistant(window, cx))),
            )
            .child(
                Button::new("create")
                    .ghost()
                    .small()
                    .icon(Lucide::Plus)
                    .accessibility_label("Create / apply YAML")
                    .tooltip("Create / apply YAML")
                    .on_click(cx.listener(|this, _, window, cx| this.open_apply(window, cx))),
            )
            .child(
                Button::new("local-terminal")
                    .ghost()
                    .small()
                    .icon(Lucide::SquareTerminal)
                    .accessibility_label("Terminal")
                    .tooltip("Terminal (kubectl set to this context)")
                    .on_click(cx.listener(|this, _, window, cx| this.open_terminal(window, cx))),
            )
            .child(
                Button::new("settings")
                    .ghost()
                    .small()
                    .icon(Lucide::Settings)
                    .accessibility_label("Settings")
                    .tooltip("Settings")
                    .on_click(cx.listener(|this, _, _, cx| this.navigate(Route::Settings, cx))),
            )
            .child(
                Button::new("theme")
                    .ghost()
                    .small()
                    .icon(if cx.theme().is_dark() { IconName::Sun } else { IconName::Moon })
                    .accessibility_label(theme_toggle_name(cx.theme().is_dark()))
                    .tooltip(theme_toggle_name(cx.theme().is_dark()))
                    .on_click(|_, window, cx| toggle_theme(window, cx)),
            )
            .child(
                Button::new("shortcuts")
                    .ghost()
                    .small()
                    .icon(Lucide::Keyboard)
                    .accessibility_label("Keyboard shortcuts")
                    .tooltip(with_keys("Keyboard shortcuts ({?})"))
                    .on_click(|_, window, cx| super::shortcuts::open_panel(window, cx)),
            )
            .child(
                Button::new("version")
                    .ghost()
                    .xsmall()
                    .label(concat!("v", env!("CARGO_PKG_VERSION")))
                    .tooltip("About Recouvrance")
                    .on_click(cx.listener(|this, _, _, cx| this.show_about(cx))),
            )
    }

    /// The cluster picker's button: a dot per cluster, a lock when one is protected.
    fn ctx_trigger(&self, theme: &Theme) -> Button {
        // While none of the set is connected, the set it is connecting to (or failed to).
        let names: Vec<String> = if self.clusters.is_empty() {
            self.conn.iter().map(|(c, _)| c.clone()).collect()
        } else {
            self.clusters.iter().map(|c| c.0.clone()).collect()
        };
        let dots: Vec<Hsla> =
            names.iter().map(|n| crate::settings::context_color(n).map_or(theme.blue, |c| rgb(c).into())).collect();
        let protected = names.iter().any(|n| crate::settings::is_protected(n));
        let label = match names.len() {
            0 => "Choose a cluster".to_string(),
            1 => crate::k8s::short_names::short_name(&names[0]),
            n => format!("{n} clusters"),
        };
        // The full names, which the label shortens or counts.
        Button::new("ctx-trigger").outline().when(!names.is_empty(), |b| b.tooltip(names.join("\n"))).child(
            h_flex()
                .gap_2()
                .child(
                    h_flex()
                        .gap_0p5()
                        .children(dots.into_iter().take(4).map(|c| div().size(px(9.)).rounded_full().bg(c))),
                )
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .max_w(px(260.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(label),
                )
                .when(protected, |d| d.child(Icon::new(Lucide::Lock).size_3p5().text_color(theme.warning)))
                .child(Icon::new(Lucide::ChevronDown).size_3p5().text_color(theme.muted_foreground)),
        )
    }

    /// The namespace picker's button.
    fn ns_trigger(&self, theme: &Theme) -> Button {
        let selected: Vec<String> = self.namespaces.clone();
        // Names in full while they fit; the rest as a count.
        let label = match selected.len() {
            0 => "All namespaces".to_string(),
            1 | 2 => selected.join(", "),
            n => format!("{}, {} +{}", selected[0], selected[1], n - 2),
        };
        Button::new("ns-trigger").outline().tooltip(selected.join("\n")).child(
            h_flex()
                .gap_2()
                .min_w(px(180.))
                .child(Icon::new(Lucide::Layers).size_4().text_color(theme.muted_foreground))
                .child(
                    div()
                        .flex_1()
                        .text_sm()
                        .max_w(px(360.))
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(label),
                )
                .child(Icon::new(Lucide::ChevronDown).size_3p5().text_color(theme.muted_foreground)),
        )
    }

    /// The resource list: its bar (kind, count, filter, actions), status and table.
    fn render_list(&self, cx: &mut Context<Self>) -> AnyElement {
        let (total, shown) = {
            let t = self.table.read(cx).delegate();
            (t.total(), t.rows_count_visible())
        };
        let kind_label = self.kind.as_ref().map_or_default(|k| k.label.clone());
        let view_bar = h_flex()
            .gap_3()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(kind_label))
            .child(
                h_flex()
                    .gap_1p5()
                    .px_2()
                    .py_0p5()
                    .rounded_full()
                    .bg(cx.theme().secondary)
                    .text_xs()
                    .child(div().size(px(7.)).rounded_full().bg(
                        if self.watch.is_some() && self.status_line().is_none() {
                            cx.theme().success
                        } else {
                            cx.theme().muted_foreground
                        },
                    ))
                    .child(if shown == total { format!("{total}") } else { format!("{shown} of {total}") })
                    .when(self.list.is_loading() && total > 0, |d| {
                        d.child(div().text_color(cx.theme().muted_foreground).child("loading…"))
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .max_w(px(720.))
                    .child(Input::new(&self.filter).prefix(Icon::new(Lucide::ListFilter).size_4()).cleanable(true)),
            )
            .child(self.render_filter_menu(cx))
            .child(div().flex_1())
            .child(
                Button::new("refresh-list")
                    .ghost()
                    .small()
                    .icon(Lucide::RefreshCw)
                    .accessibility_label("Reload the list")
                    .tooltip("Reload the list")
                    // Lists again, as F5 does: `restart_watch` would bring the kept list back.
                    .on_click(cx.listener(|this, _, _, cx| this.reload_watch(cx))),
            )
            .child(
                Button::new("export-csv")
                    .ghost()
                    .small()
                    .icon(Lucide::FileSpreadsheet)
                    .accessibility_label("Export the rows shown to CSV")
                    .tooltip("Export the rows shown to CSV (~/Downloads)")
                    .on_click(cx.listener(|this, _, window, cx| this.export_csv(window, cx))),
            )
            .child(
                Button::new("save-view")
                    .ghost()
                    .small()
                    .icon(Lucide::Bookmark)
                    .label("Save view")
                    .on_click(cx.listener(|this, _, window, cx| this.save_view_dialog(window, cx))),
            )
            .child(self.render_columns_menu(cx));
        // None of the set connected yet: each context's state, not an empty table.
        if self.nothing_connected() {
            return v_flex().size_full().child(view_bar).child(self.render_connect(cx)).into_any_element();
        }
        let banner = self.render_conn_banner(cx);
        let (status, status_hover) = self.status_lines();
        v_flex()
            .size_full()
            .child(view_bar)
            .children(banner)
            .when_some(status, |d, s| {
                // A context named by its short name: the full names on hover.
                let line = div().id("status-line").px_3().py_1().text_sm().text_color(cx.theme().muted_foreground);
                let line = line.when_some(status_hover, |l, full| {
                    l.tooltip(move |w, cx| component::tooltip::Tooltip::new(full.clone()).build(w, cx))
                });
                d.child(line.child(s))
            })
            .children(self.render_selection_bar(cx))
            // Row presses are recorded by the table itself (see `ResourceTable::render_tr`).
            .child(div().flex_1().min_h_0().child(DataTable::new(&self.table).small().stripe(false).bordered(false)))
            .into_any_element()
    }

    /// A page made of what the contexts of the set hold (Problems, the overviews, Timeline,
    /// Helm), as the list is shown: each context that isn't connected said over it, so its
    /// silence doesn't read as nothing there; the connect panel in its place while none is.
    fn over_set(&self, page: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        if self.nothing_connected() {
            return self.render_connect(cx);
        }
        match self.render_conn_banner(cx) {
            None => page,
            Some(banner) => {
                v_flex().size_full().child(banner).child(div().flex_1().min_h_0().child(page)).into_any_element()
            }
        }
    }

    fn render_content(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.route() {
            Route::Resources(_) => self.render_list(cx),
            Route::Detail(d) => d.clone().into_any_element(),
            Route::Workloads(d) => self.over_set(d.clone().into_any_element(), cx),
            Route::Cluster(d) => self.over_set(d.clone().into_any_element(), cx),
            Route::Problems(d) => self.over_set(d.clone().into_any_element(), cx),
            Route::Timeline(d) => self.over_set(d.clone().into_any_element(), cx),
            Route::Access(d) => d.clone().into_any_element(),
            Route::HelmInstall(d) => d.clone().into_any_element(),
            Route::Assistant(d) => d.clone().into_any_element(),
            Route::Helm(d) => self.over_set(d.clone().into_any_element(), cx),
            Route::HelmRelease(d) => d.clone().into_any_element(),
            Route::Argo(d) => d.clone().into_any_element(),
            Route::Contexts => self.render_contexts(cx).into_any_element(),
            Route::Settings => self.settings_view.clone().into_any_element(),
            Route::Apply(a) => a.clone().into_any_element(),
            Route::Logs(l) => l.clone().into_any_element(),
            Route::Terminal(t) => div().size_full().p_2().child(t.clone()).into_any_element(),
            Route::Agents => self.agents.clone().into_any_element(),
        }
    }

    /// The window's keyboard actions.
    fn with_actions(&self, app: Stateful<Div>, cx: &mut Context<Self>) -> Stateful<Div> {
        // New window, Quit and the other app-wide actions are handled without a window (`menus`).
        app.on_action(cx.listener(|this, _: &OpenPalette, window, cx| this.open_palette(window, cx)))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| this.new_tab(None, window, cx)))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| this.close_tab_or_window(window, cx)))
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            .on_action(|_: &MinimizeWindow, window, _| window.minimize_window())
            .on_action(|_: &ZoomWindow, window, _| window.zoom_window())
            .on_action(cx.listener(|this, _: &ShowAbout, _, cx| this.show_about(cx)))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                let n = this.tabs.len();
                this.switch_tab((this.active_tab + 1) % n, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevTab, window, cx| {
                let n = this.tabs.len();
                this.switch_tab((this.active_tab + n - 1) % n, window, cx);
            }))
            .on_action(cx.listener(|this, _: &RowOpenTab, window, cx| {
                // Open the right-clicked row in a new tab; this tab stays on the list.
                this.open_row(None, window, cx);
                if matches!(this.route(), Route::Detail(_)) {
                    let detail = this.route().clone();
                    this.go(-1, cx);
                    this.history.truncate(this.pos + 1);
                    this.new_tab(Some(detail), window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, window, cx| this.zoom_by(Some(0.1), window, cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, window, cx| this.zoom_by(Some(-0.1), window, cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, window, cx| this.zoom_by(None, window, cx)))
            .on_action(cx.listener(|this, _: &GoBack, _, cx| this.go(-1, cx)))
            .on_action(cx.listener(|this, _: &RefreshPage, window, cx| this.refresh_page(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar_hidden = !this.sidebar_hidden;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &OpenSelected, window, cx| {
                if let Some(ix) = this.table.read(cx).selected_row() {
                    this.open_detail(ix, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &EscapeBack, _, cx| {
                if matches!(this.route(), Route::Detail(_) | Route::HelmRelease(_) | Route::Logs(_)) {
                    this.go(-1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &GoForward, _, cx| this.go(1, cx)))
            .on_action(|_: &ShowShortcuts, window, cx| super::shortcuts::open_panel(window, cx))
            // Pages and commands the palette lists without a key (see `commands::COMMANDS`).
            .on_action(cx.listener(|this, _: &OpenArgo, window, cx| this.open_argo(window, cx)))
            .on_action(|_: &ConnectArgoCd, window, cx| crate::ui::argo_connect::open(None, window, cx))
            .on_action(cx.listener(|this, _: &OpenTimeline, window, cx| this.open_timeline(window, cx)))
            .on_action(cx.listener(|this, _: &OpenAccess, window, cx| this.open_access(window, cx)))
            .on_action(cx.listener(|this, _: &OpenUpgradeCheck, window, cx| this.open_upgrade_check(window, cx)))
            .on_action(cx.listener(|this, _: &OpenAgents, _, cx| this.navigate(Route::Agents, cx)))
            .on_action(cx.listener(|this, _: &OpenAgentActivity, _, cx| {
                this.agents.update(cx, crate::agents::AgentsView::show_activity);
                this.navigate(Route::Agents, cx);
            }))
            .on_action(cx.listener(|this, _: &OpenContexts, _, cx| this.navigate(Route::Contexts, cx)))
            .on_action(cx.listener(|this, _: &OpenTerminal, window, cx| this.open_terminal(window, cx)))
            .on_action(cx.listener(|this, _: &OpenApply, window, cx| this.open_apply(window, cx)))
            .on_action(cx.listener(|this, _: &OpenHelmInstall, window, cx| this.open_helm_install(window, cx)))
            .on_action(cx.listener(|this, _: &OpenApiRequests, window, cx| {
                this.open_settings_section(crate::ui::settings_view::Section::Requests, window, cx);
            }))
            .on_action(|_: &ToggleWriteMode, window, cx| toggle_write_mode(window, cx))
            .on_action(|_: &ToggleTheme, window, cx| toggle_theme(window, cx))
            .on_action(cx.listener(|this, _: &GoProblems, window, cx| this.open_problems(window, cx)))
            .on_action(cx.listener(|this, _: &GoCluster, window, cx| this.open_cluster(window, cx)))
            .on_action(cx.listener(|this, _: &GoWorkloads, window, cx| this.open_workloads(window, cx)))
            .on_action(cx.listener(|this, _: &GoHelm, window, cx| this.open_helm(window, cx)))
            .on_action(cx.listener(|this, _: &GoAssistant, window, cx| this.open_assistant(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleAssistant, window, cx| this.toggle_assistant(window, cx)))
            .on_action(cx.listener(|this, _: &GoSettings, _, cx| this.navigate(Route::Settings, cx)))
            .on_action(cx.listener(|this, _: &FocusFilter, window, cx| {
                this.filter.update(cx, |s, cx| s.focus(window, cx));
            }))
            .on_action(cx.listener(|this, _: &RowOpen, window, cx| this.open_row(None, window, cx)))
            .on_action(cx.listener(|this, _: &RowLogs, window, cx| this.open_row(Some("logs"), window, cx)))
            .on_action(cx.listener(|this, _: &RowShell, window, cx| this.open_row(Some("shell"), window, cx)))
            .on_action(cx.listener(|this, _: &RowYaml, window, cx| this.open_row(Some("yaml"), window, cx)))
            .on_action(cx.listener(|this, _: &RowEvents, window, cx| this.open_row(Some("events"), window, cx)))
            .on_action(cx.listener(|this, _: &RowRun, window, cx| this.row_quick_action("run", window, cx)))
            .on_action(cx.listener(|this, _: &RowScale, window, cx| this.row_quick_action("scale", window, cx)))
            .on_action(cx.listener(|this, _: &RowRestart, window, cx| this.row_quick_action("restart", window, cx)))
            .on_action(cx.listener(|this, _: &RowDelete, window, cx| this.row_quick_action("delete", window, cx)))
            .on_action(cx.listener(|this, _: &RowCopyName, _, cx| {
                if let Some((_, row)) = this.row_object(cx) {
                    cx.write_to_clipboard(ClipboardItem::new_string(row.name.clone()));
                }
            }))
            .on_action(cx.listener(|this, _: &RowFavorite, _, cx| {
                let (Some((ctx, row)), Some(kind)) = (this.row_object(cx), this.kind.clone()) else { return };
                let Some(context) = this.clusters.get(ctx).map(|c| c.0.clone()) else { return };
                // An Argo CD Application's row stars the Application, as its page does.
                let fav = Favorite::for_object(&context, &kind, row.namespace.clone(), row.name.clone());
                this.favorites.update(cx, |f, cx| {
                    if !f.has(&fav) {
                        f.toggle(fav, cx);
                    }
                });
            }))
            .on_action(cx.listener(|this, _: &ToggleFavorite, window, cx| this.toggle_shown_favorite(window, cx)))
    }
}

/// The narrowest the search field gets: its icon and its shortcut hint whole ("Ctrl+K" is the
/// longest), with the placeholder cut to nothing.
const SEARCH_MIN: f32 = 120.;

/// The top bar's button that opens the palette. When the bar runs out of room it shrinks before
/// the buttons beside it: its placeholder is cut first, down to `SEARCH_MIN`, where the hint still
/// fits. The hint never shrinks, and the field's content can always shrink to fit its width, so
/// the hint can't run past the field's edge under the write-mode badge. It did when the field
/// kept a fixed 240px its content was wider than.
fn search_field(theme: &Theme, hint: String) -> Stateful<Div> {
    h_flex()
        .id("palette-trigger")
        .debug_selector(|| "search-field".into())
        .gap_2()
        .h(px(28.))
        .w(px(240.))
        .min_w(px(SEARCH_MIN))
        .px_3()
        .rounded_md()
        .bg(theme.secondary)
        .cursor_pointer()
        .hover(|s| s.bg(theme.list_hover))
        .control(Role::Button, "Jump to anything", theme.ring)
        .aria_keyshortcuts(hint.clone())
        .text_sm()
        .text_color(theme.muted_foreground)
        .child(Icon::new(Lucide::Search).size_4().flex_shrink_0())
        .child(
            div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child("Jump to anything…"),
        )
        .child(
            div()
                .debug_selector(|| "search-hint".into())
                .flex_shrink_0()
                .whitespace_nowrap()
                .text_xs()
                .px_1()
                .rounded_sm()
                .border_1()
                .border_color(theme.border)
                .child(hint),
        )
}

/// Read-only or writes on; turning writes on asks first.
fn write_mode_button(cx: &App) -> Button {
    let writes = crate::details::writes_enabled(cx);
    Button::new("write-mode")
        .xsmall()
        .when(!writes, |b| b.ghost().label("read-only"))
        .when(writes, |b| b.warning().label("writes on"))
        .tooltip(if writes {
            "Write actions are on — click to go back to read-only"
        } else {
            "Click to enable write actions"
        })
        .on_click(|_, window, cx| toggle_write_mode(window, cx))
}

/// Back to read-only at once, or writes on once confirmed: the badge and the palette do the same.
fn toggle_write_mode(window: &mut Window, cx: &mut App) {
    if crate::details::writes_enabled(cx) {
        crate::details::set_writes(false, cx);
        cx.refresh_windows();
        return;
    }
    window.open_alert_dialog(cx, |alert, _, _| {
        alert
            .title("Enable write actions?")
            .description(
                "Scale, restart, delete, YAML edits, shells and plugins become available, using your own \
                 kubeconfig credentials. Every action asks for confirmation. Agents are not affected: their \
                 access stays defined by their rules.",
            )
            .show_cancel(true)
            .ok_text("Enable writes")
            .on_ok(|_, _, cx| {
                crate::details::set_writes(true, cx);
                cx.refresh_windows();
                true
            })
    });
}

/// What the theme button does, from the theme on screen.
fn theme_toggle_name(dark: bool) -> &'static str {
    if dark { "Switch to the light theme" } else { "Switch to the dark theme" }
}

fn toggle_theme(window: &mut Window, cx: &mut App) {
    let mode = if cx.theme().is_dark() { ThemeMode::Light } else { ThemeMode::Dark };
    set_theme(mode, Some(window), cx);
}

#[cfg(all(test, feature = "dev"))]
mod layout_tests {
    use gpui_kit::component::{ActiveTheme as _, h_flex};
    // Not `gpui_kit::*`: its `test` macro would shadow the `#[test]` that `#[gpui_kit::test]` expands to.
    use gpui_kit::{
        Bounds, Context, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, Styled as _,
        TestAppContext, Window, div, px,
    };

    use super::{SEARCH_MIN, search_field, write_mode_button};

    /// The search field and the write-mode badge as the top bar lays them out, `width` wide.
    struct Bar {
        width: f32,
        hint: &'static str,
    }

    impl Render for Bar {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = cx.theme().clone();
            h_flex()
                .w(px(self.width))
                .gap_2()
                .child(search_field(&theme, self.hint.to_string()))
                .child(div().debug_selector(|| "write-badge".into()).child(write_mode_button(cx)))
        }
    }

    /// Where the field, its hint and the badge are drawn in a bar `width` wide.
    fn drawn(cx: &mut TestAppContext, width: f32, hint: &'static str) -> [Bounds<Pixels>; 3] {
        let (_, cx) = cx.add_window_view(|_, _| Bar { width, hint });
        cx.update(|window, cx| drop(window.draw(cx)));
        ["search-field", "search-hint", "write-badge"]
            .map(|name| cx.debug_bounds(name).unwrap_or_else(|| panic!("{name} is drawn")))
    }

    #[gpui_kit::test]
    fn the_badge_never_covers_the_search_hint(cx: &mut TestAppContext) {
        cx.update(|cx| {
            gpui_kit::init(cx);
            cx.set_global(crate::details::WriteMode(true));
        });
        // Linux's hint and macOS's (#899), from a bar with no room to spare to one with plenty.
        for hint in ["Ctrl+K", "⌘K"] {
            for width in [150., 200., 240., 280., 320., 360., 600.] {
                let [field, hint_box, badge] = drawn(cx, width, hint);
                let at = format!("{hint} in {width}px");
                assert!(
                    hint_box.left() >= field.left() && hint_box.right() <= field.right(),
                    "{at}: the hint {hint_box:?} runs out of the field {field:?}"
                );
                assert!(badge.left() >= field.right(), "{at}: the badge {badge:?} starts inside the field {field:?}");
                assert!(!hint_box.intersects(&badge), "{at}: the badge {badge:?} covers the hint {hint_box:?}");
                assert!(f32::from(field.size.width) >= SEARCH_MIN - 0.5, "{at}: the field is {field:?}");
            }
        }
    }
}
