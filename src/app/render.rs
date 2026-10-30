//! The window layout and its actions.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::popover::Popover;
use gpui_kit::component::table::DataTable;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, Theme, ThemeMode, TitleBar, WindowExt as _, h_flex, h_resizable,
    resizable_panel, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{
    AppView, CloseTab, EscapeBack, FocusFilter, GoAssistant, GoBack, GoCluster, GoForward, GoHelm, GoProblems,
    GoSettings, GoWorkloads, NewTab, NewWindow, NextTab, OpenPalette, OpenSelected, PrevTab, RefreshPage, Route,
    RowCopyName, RowDelete, RowEvents, RowFavorite, RowLogs, RowOpen, RowOpenTab, RowRestart, RowRun, RowScale,
    RowShell, RowYaml, ShowShortcuts, ToggleSidebar, ZoomIn, ZoomOut, ZoomReset, open_main_window, set_theme,
};
use crate::favorites::FavObject;

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        #[cfg(feature = "dev")]
        let _t = crate::dev::timing::span("app render");
        self.keep_focus(window, cx);
        let content = v_flex()
            .size_full()
            .children(self.render_tabs(cx))
            .child(div().flex_1().min_h_0().child(self.render_content(cx)))
            .into_any_element();
        let body = if self.sidebar_hidden {
            div().size_full().child(content).into_any_element()
        } else {
            // The sidebar keeps its width when the window is resized; the page takes the rest.
            h_resizable("body")
                .child(resizable_panel().size(px(230.)).fixed().child(self.render_sidebar(cx)))
                .child(resizable_panel().child(content))
                .into_any_element()
        };
        let app = v_flex().id("app").relative().size_full().track_focus(&self.focus).key_context("App");
        self.with_actions(app, cx)
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .children(self.context_strip())
            .child(TitleBar::new().child(self.render_toolbar(cx)))
            .child(self.render_approvals(cx))
            .child(div().flex_1().min_h_0().child(body))
            .children(self.palette.clone())
    }
}

impl AppView {
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
        let refreshable =
            matches!(self.route(), Route::Cluster(_) | Route::Workloads(_) | Route::Problems(_) | Route::Helm(_));
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
                    .tooltip("Show/hide the sidebar (Ctrl+B)")
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
                        .tooltip("Refresh (F5)")
                        .on_click(cx.listener(|this, _, window, cx| this.refresh_page(window, cx))),
                )
            })
            .child(
                h_flex()
                    .id("palette-trigger")
                    .gap_2()
                    .h(px(28.))
                    .w(px(240.))
                    .px_3()
                    .rounded_md()
                    .bg(theme.secondary)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.list_hover))
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(Lucide::Search).size_4())
                    .child(div().flex_1().child("Jump to anything…"))
                    .child(div().text_xs().px_1().rounded_sm().border_1().border_color(theme.border).child("Ctrl K"))
                    .on_click(cx.listener(|this, _, window, cx| this.open_palette(window, cx))),
            )
            .child(write_mode_button(cx))
            .child(
                Button::new("create")
                    .ghost()
                    .small()
                    .icon(Lucide::Plus)
                    .tooltip("Create / apply YAML")
                    .on_click(cx.listener(|this, _, window, cx| this.open_apply(window, cx))),
            )
            .child(
                Button::new("local-terminal")
                    .ghost()
                    .small()
                    .icon(Lucide::SquareTerminal)
                    .tooltip("Terminal (kubectl set to this context)")
                    .on_click(cx.listener(|this, _, window, cx| this.open_terminal(window, cx))),
            )
            .child(
                Button::new("settings")
                    .ghost()
                    .small()
                    .icon(Lucide::Settings)
                    .tooltip("Settings")
                    .on_click(cx.listener(|this, _, _, cx| this.navigate(Route::Settings, cx))),
            )
            .child(
                Button::new("theme")
                    .ghost()
                    .small()
                    .icon(if cx.theme().is_dark() { IconName::Sun } else { IconName::Moon })
                    .on_click(|_, window, cx| {
                        let mode = if cx.theme().is_dark() { ThemeMode::Light } else { ThemeMode::Dark };
                        set_theme(mode, Some(window), cx);
                    }),
            )
            .child(
                Button::new("version")
                    .ghost()
                    .xsmall()
                    .label(concat!("v", env!("CARGO_PKG_VERSION")))
                    .tooltip("About Recouvrance")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.settings_view.update(cx, crate::ui::settings_view::SettingsView::show_about);
                        this.navigate(Route::Settings, cx);
                    })),
            )
    }

    /// The cluster picker's button: a dot per cluster, a lock when one is protected.
    fn ctx_trigger(&self, theme: &Theme) -> Button {
        let names: Vec<String> = self.clusters.iter().map(|c| c.0.clone()).collect();
        let dots: Vec<Hsla> =
            names.iter().map(|n| crate::settings::context_color(n).map_or(theme.blue, |c| rgb(c).into())).collect();
        let protected = names.iter().any(|n| crate::settings::is_protected(n));
        let label = match names.len() {
            0 => "Choose a cluster".to_string(),
            1 => names[0].clone(),
            n => format!("{n} clusters"),
        };
        Button::new("ctx-trigger").outline().child(
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
    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (total, shown) = {
            let t = self.table.read(cx).delegate();
            (t.total(), t.rows_count_visible())
        };
        let kind_label = self.kind.as_ref().map(|k| k.label.clone()).unwrap_or_default();
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
                    .tooltip("Reload the list")
                    .on_click(cx.listener(|this, _, _, cx| this.restart_watch(cx))),
            )
            .child(
                Button::new("export-csv")
                    .ghost()
                    .small()
                    .icon(Lucide::FileSpreadsheet)
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
        v_flex()
            .size_full()
            .child(view_bar)
            .when_some(self.status_line(), |d, s| {
                d.child(div().px_3().py_1().text_sm().text_color(cx.theme().muted_foreground).child(s))
            })
            .children(self.render_selection_bar(cx))
            // Row presses are recorded by the table itself (see `ResourceTable::render_tr`).
            .child(div().flex_1().min_h_0().child(DataTable::new(&self.table).small().stripe(false).bordered(false)))
    }

    fn render_content(&self, cx: &mut Context<Self>) -> AnyElement {
        match self.route() {
            Route::Resources(_) => self.render_list(cx).into_any_element(),
            Route::Detail(d) => d.clone().into_any_element(),
            Route::Workloads(d) => d.clone().into_any_element(),
            Route::Cluster(d) => d.clone().into_any_element(),
            Route::Problems(d) => d.clone().into_any_element(),
            Route::Timeline(d) => d.clone().into_any_element(),
            Route::Access(d) => d.clone().into_any_element(),
            Route::HelmInstall(d) => d.clone().into_any_element(),
            Route::Assistant(d) => d.clone().into_any_element(),
            Route::Helm(d) => d.clone().into_any_element(),
            Route::HelmRelease(d) => d.clone().into_any_element(),
            Route::Contexts => self.render_contexts(cx).into_any_element(),
            Route::Settings => self.settings_view.clone().into_any_element(),
            Route::Apply(a) => a.clone().into_any_element(),
            Route::Terminal(t) => div().size_full().p_2().child(t.clone()).into_any_element(),
            Route::Agents => self.agents.clone().into_any_element(),
        }
    }

    /// The window's keyboard actions.
    fn with_actions(&self, app: Stateful<Div>, cx: &mut Context<Self>) -> Stateful<Div> {
        app.on_action(cx.listener(|this, _: &OpenPalette, window, cx| this.open_palette(window, cx)))
            .on_action(|_: &NewWindow, _, cx| open_main_window(cx))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| this.new_tab(None, window, cx)))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| this.close_tab(this.active_tab, window, cx)))
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
                if matches!(this.route(), Route::Detail(_) | Route::HelmRelease(_)) {
                    this.go(-1, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &GoForward, _, cx| this.go(1, cx)))
            .on_action(|_: &ShowShortcuts, window, cx| super::shortcuts::open_panel(window, cx))
            .on_action(cx.listener(|this, _: &GoProblems, window, cx| this.open_problems(window, cx)))
            .on_action(cx.listener(|this, _: &GoCluster, window, cx| this.open_cluster(window, cx)))
            .on_action(cx.listener(|this, _: &GoWorkloads, window, cx| this.open_workloads(window, cx)))
            .on_action(cx.listener(|this, _: &GoHelm, window, cx| this.open_helm(window, cx)))
            .on_action(cx.listener(|this, _: &GoAssistant, window, cx| this.open_assistant(window, cx)))
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
                let fav = FavObject {
                    context,
                    kind: kind.kubectl_name(),
                    namespace: row.namespace.clone(),
                    name: row.name.clone(),
                };
                this.favorites.update(cx, |f, cx| {
                    if !f.has_object(&fav) {
                        f.toggle_object(fav, cx);
                    }
                });
            }))
    }
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
        .on_click(|_, window, cx| {
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
        })
}
