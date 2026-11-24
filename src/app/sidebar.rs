//! The sidebar: pages, resource types by section, favorites, port-forwards.

use std::collections::HashMap;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::ButtonCustomVariant;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{AppView, Route};
use crate::favorites::{FavObject, Favorites, SavedView};
use crate::k8s;
use crate::k8s::Kind;

/// Buttons on the navy sidebar: light text, a slightly lighter hull on hover.
fn on_hull(cx: &App) -> ButtonCustomVariant {
    let t = cx.theme();
    ButtonCustomVariant::new(cx)
        .color(t.sidebar)
        .foreground(t.sidebar_foreground)
        .hover(t.sidebar_accent)
        .active(t.sidebar_accent)
        .shadow(false)
}

impl AppView {
    /// Start saved port-forwards again, finding their pods anew.
    /// Start a remembered forward again (its pod is found again by service or name).
    pub(super) fn start_saved_forward(&mut self, s: crate::settings::SavedForward, cx: &mut Context<Self>) {
        let Some(client) = self.clusters.iter().find(|c| c.0 == s.context).map(|c| c.1.clone()) else { return };
        let forwards = self.forwards.clone();
        cx.spawn(async move |_, cx| {
            // The pod's workload is worked out again (owner references, not the pod name).
            match k8s::resolve_forward_pod(client.clone(), s.namespace.clone(), s.service.clone(), s.pod.clone(), None)
                .await
            {
                Ok(pod) => {
                    forwards.update(cx, |f, cx| f.start(client, crate::settings::SavedForward { pod, ..s }, cx));
                }
                Err(e) => eprintln!("start forward {}/{}: {e:#}", s.namespace, s.pod),
            }
        })
        .detach();
    }

    pub(super) fn restore_forwards(&mut self, cx: &mut Context<Self>) {
        let contexts: Vec<String> = self.clusters.iter().map(|c| c.0.clone()).collect();
        let saved = self.forwards.read(cx).restorable(&contexts);
        for s in saved {
            // Restoring several at once does not open browser tabs.
            self.start_saved_forward(crate::settings::SavedForward { open_browser: false, ..s }, cx);
        }
    }

    /// The port forwards, at the bottom: one row saying how many run and how many are
    /// recent, which a click opens into the list.
    pub(super) fn render_forwards(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        // The sidebar is navy in both themes: status on it takes the bright colors.
        let status = super::bright_status();
        let (muted, danger, success, border, hover) = (
            theme.sidebar_foreground.opacity(0.62),
            status.danger,
            status.success,
            theme.sidebar_border,
            theme.sidebar_accent.opacity(0.6),
        );
        let items = &self.forwards.read(cx).items;
        let contexts: Vec<String> = self.clusters.iter().map(|c| c.0.clone()).collect();
        let restorable = self.forwards.read(cx).restorable(&contexts).len();
        let recent = self.forwards.read(cx).recent(&contexts);
        let agents = self.hub.forwards();
        let (running, recent_n) = (items.len() + agents.len(), recent.len().min(8));
        // The restorable ones are among the recent ones.
        if running + recent_n == 0 {
            return div().into_any_element();
        }
        let summary = match (running, recent_n) {
            (0, r) => format!("{r} recent"),
            (n, 0) => format!("{n} running"),
            (n, r) => format!("{n} running · {r} recent"),
        };
        let open = self.forwards_open;
        let header = h_flex()
            .id("forwards-toggle")
            .gap_1p5()
            .mx_2()
            .my_1()
            .px_2()
            .py_1()
            .rounded_md()
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(|this, _, _, cx| {
                this.forwards_open = !this.forwards_open;
                cx.notify();
            }))
            .child(Icon::new(Lucide::ArrowRightLeft).size_4())
            .child(div().flex_1().child("Port forwards"))
            .child(div().text_xs().text_color(if running > 0 { success } else { muted }).child(summary))
            .child(
                Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronUp }).size_3().text_color(muted),
            );
        if !open {
            return v_flex().border_t_1().border_color(border).child(header).into_any_element();
        }
        let list = v_flex()
            .id("forwards-list")
            .max_h(px(280.))
            .overflow_y_scroll()
            .px_2()
            .pb_2()
            .when(restorable > 1, |d| {
                d.child(
                    div().px_2().pb_1().child(
                        Button::new("restore-forwards")
                            .xsmall()
                            .custom(on_hull(cx))
                            .icon(Lucide::ArrowRightLeft)
                            .label(format!("Restore {restorable} port-forwards"))
                            .on_click(cx.listener(|this, _, _, cx| this.restore_forwards(cx))),
                    ),
                )
            })
            .children(items.iter().map(|f| {
                let id = f.id;
                let forwards = self.forwards.clone();
                let addr = f.address().unwrap_or_else(|| "starting…".into());
                let url = f.url();
                h_flex()
                    .gap_1()
                    .px_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div().text_sm().font_weight(FontWeight::MEDIUM).child(format!("{addr} → {}", f.remote)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if f.error.is_some() { danger } else { muted })
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(f.error.clone().unwrap_or_else(|| f.label.clone())),
                            ),
                    )
                    .when_some(url, |d, url| {
                        d.child(
                            Button::new(("open-pf", id))
                                .xsmall()
                                .custom(on_hull(cx))
                                .icon(Lucide::ExternalLink)
                                .tooltip(format!("Open {url}"))
                                .on_click(move |_, _, _| crate::forwards::open_url(&url)),
                        )
                    })
                    .child(
                        Button::new(("stop-pf", id))
                            .xsmall()
                            .custom(on_hull(cx))
                            .icon(Lucide::X)
                            .on_click(move |_, _, cx| forwards.update(cx, |f, cx| f.stop(id, cx))),
                    )
            }))
            // Agents' forwards: who opened them, until when; stopped like the others.
            .children(agents.into_iter().map(|f| {
                let (id, hub) = (f.id, self.hub.clone());
                let until = f.until.to_zoned_datetime(jiff::tz::TimeZone::system()).strftime("%H:%M");
                let detail = f
                    .error
                    .clone()
                    .unwrap_or_else(|| format!("{}/{} · agent {} · until {until}", f.namespace, f.target, f.agent));
                h_flex()
                    .gap_1()
                    .px_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(format!("127.0.0.1:{} → {}", f.local, f.remote)),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(if f.error.is_some() { danger } else { muted })
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(detail),
                            ),
                    )
                    .child(
                        Button::new(("stop-agent-pf", id as usize))
                            .xsmall()
                            .custom(on_hull(cx))
                            .icon(Lucide::X)
                            .tooltip("Stop the agent's port-forward")
                            .on_click(move |_, _, _| {
                                hub.stop_forwards(|x| x.id == id);
                            }),
                    )
            }))
            .children(recent.into_iter().take(8).enumerate().map(|(i, saved)| {
                let target = match &saved.service {
                    Some(svc) => format!("svc/{svc}"),
                    None => crate::forwards::pod_base(&saved.pod).to_string(),
                };
                let local = saved.local.map_or_default(|p| format!("localhost:{p} → "));
                let (start, forget) = (saved.clone(), saved.clone());
                h_flex()
                    .gap_1()
                    .px_2()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().text_color(muted).child(format!("{local}{}", saved.remote)))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(muted)
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(format!("{}/{target}", saved.namespace)),
                            ),
                    )
                    .child(
                        Button::new(("start-pf", i))
                            .xsmall()
                            .custom(on_hull(cx))
                            .icon(Lucide::Play)
                            .tooltip("Forward again")
                            .on_click(cx.listener(move |this, _, _, cx| this.start_saved_forward(start.clone(), cx))),
                    )
                    .child(
                        Button::new(("forget-pf", i))
                            .xsmall()
                            .custom(on_hull(cx))
                            .icon(Lucide::X)
                            .tooltip("Forget it")
                            .on_click(cx.listener(move |_, _, _, cx| {
                                crate::forwards::forget(&forget);
                                cx.notify();
                            })),
                    )
            }));
        v_flex().border_t_1().border_color(border).child(header).child(list).into_any_element()
    }

    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = Colors::of(cx);
        let query = self.kind_filter.read(cx).value().to_lowercase();
        let selected = self.kind.as_ref().filter(|_| self.route().is_resources()).map(Kind::key);
        // Pages, saved views, favorites and types scroll together under the filter, which
        // finds pages as well as types.
        let mut children: Vec<AnyElement> =
            self.page_links(&query, colors, cx).into_iter().map(IntoElement::into_any_element).collect();
        if query.is_empty() {
            children.extend(self.saved_views(colors, cx));
            children.extend(self.favorite_rows(selected.as_deref(), colors, cx));
        }
        let kinds = self.kind_rows(&query, selected.as_deref(), colors, cx);
        if kinds.is_empty() && !query.is_empty() {
            children.push(
                div()
                    .px_2()
                    .pt_3()
                    .text_sm()
                    .text_color(colors.muted)
                    .child(format!("No resource type matches “{query}”"))
                    .into_any_element(),
            );
        }
        children.extend(kinds);
        let theme = cx.theme();
        // The box sits on the navy hull: a darker field and the sidebar's own light text
        // (the input's light field left that inherited light text unreadable).
        let filter = Input::new(&self.kind_filter)
            .small()
            .cleanable(true)
            .prefix(Icon::new(Lucide::Search).size_3p5().text_color(colors.muted))
            .bg(theme.sidebar_accent)
            .border_color(theme.sidebar_border)
            .text_color(theme.sidebar_foreground);
        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().sidebar_foreground)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(self.sidebar_header(cx))
            .child(div().px_2().pt_3().pb_1().child(filter))
            .child(v_flex().id("kinds").flex_1().min_h_0().overflow_y_scroll().px_2().pb_4().children(children))
            .child(self.render_forwards(cx))
    }

    /// Logo, the connected contexts (a click manages them), back and forward.
    fn sidebar_header(&self, cx: &mut Context<Self>) -> Div {
        let context = match self.clusters.len() {
            0 if self.connecting => {
                format!("Connecting to {}…", self.context.clone().unwrap_or_default())
            }
            0 => "Not connected".into(),
            1 => self.clusters[0].0.clone(),
            n => format!("{n} contexts"),
        };
        let names: Vec<&str> = self.clusters.iter().map(|c| c.0.as_str()).collect();
        let tip = SharedString::from(if names.is_empty() {
            "Manage contexts".to_string()
        } else {
            format!("{} — click to manage contexts", names.join(" · "))
        });
        h_flex()
            .gap_2()
            .px_3()
            .pt_3()
            .child(crate::ui::logo(px(22.)))
            .child(
                div()
                    .id("sidebar-context")
                    .flex_1()
                    .min_w_0()
                    .font_weight(FontWeight::SEMIBOLD)
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, _, cx| this.navigate(Route::Contexts, cx)))
                    .tooltip(move |w, cx| component::tooltip::Tooltip::new(tip.clone()).build(w, cx))
                    .child(context),
            )
            .child(
                Button::new("back")
                    .xsmall()
                    .custom(on_hull(cx))
                    .icon(Lucide::ArrowLeft)
                    .disabled(self.pos == 0)
                    .on_click(cx.listener(|this, _, _, cx| this.go(-1, cx))),
            )
            .child(
                Button::new("forward")
                    .xsmall()
                    .custom(on_hull(cx))
                    .icon(Lucide::ArrowRight)
                    .disabled(self.pos + 1 >= self.history.len())
                    .on_click(cx.listener(|this, _, _, cx| this.go(1, cx))),
            )
    }

    /// The pages: agents (with pending approvals), assistant, problems (with their
    /// count), the timeline, overviews, Helm and Argo CD; those whose name has `query`.
    fn page_links(&self, query: &str, colors: Colors, cx: &mut Context<Self>) -> [Stateful<Div>; 8] {
        let route = self.route();
        // The sidebar is navy in both themes: status on it takes the bright colors, and a
        // badge's count the sidebar's own color (white reads at 2.2:1 on the bright amber).
        let (status, hull) = (super::bright_status(), cx.theme().sidebar);
        let (danger, warning) = (status.danger, status.warning);
        let badge = |n: usize, color: Hsla| {
            div().px_1p5().rounded_full().text_xs().bg(color).text_color(hull).child(n.to_string())
        };
        let pending = self.approvals.len();
        let nav_row = |id, icon, label, on| nav_row(id, icon, label, on, query, colors);
        [
            nav_row("nav-agents", Lucide::Bot, "Agents", route.is_agents())
                .on_click(cx.listener(|this, _, _, cx| this.navigate(Route::Agents, cx)))
                .when(!self.hub.agents_enabled(), |d| d.child(div().text_xs().text_color(danger).child("off")))
                .when(pending > 0, |d| d.child(badge(pending, warning))),
            nav_row("nav-assistant", Lucide::Sparkles, "Assistant", matches!(route, Route::Assistant(_)))
                .on_click(cx.listener(|this, _, window, cx| this.open_assistant(window, cx))),
            nav_row("nav-problems", Lucide::TriangleAlert, "Problems", matches!(route, Route::Problems(_)))
                .on_click(cx.listener(|this, _, window, cx| this.open_problems(window, cx)))
                .when_some(self.problems.shown(), |d, (n, critical)| {
                    d.child(badge(n, if critical { danger } else { warning }))
                })
                .when_some(self.problems.tooltip(), |d, tip| {
                    d.tooltip(move |w, cx| component::tooltip::Tooltip::new(tip.text()).build(w, cx))
                }),
            nav_row("nav-timeline", Lucide::ClockArrowDown, "Timeline", matches!(route, Route::Timeline(_)))
                .on_click(cx.listener(|this, _, window, cx| this.open_timeline(window, cx))),
            nav_row("nav-cluster", Lucide::Server, "Cluster overview", matches!(route, Route::Cluster(_)))
                .on_click(cx.listener(|this, _, window, cx| this.open_cluster(window, cx))),
            nav_row(
                "nav-workloads",
                Lucide::LayoutDashboard,
                "Workloads overview",
                matches!(route, Route::Workloads(_)),
            )
            .on_click(cx.listener(|this, _, window, cx| this.open_workloads(window, cx))),
            nav_row("nav-helm", Lucide::Ship, "Helm releases", matches!(route, Route::Helm(_) | Route::HelmRelease(_)))
                .on_click(cx.listener(|this, _, window, cx| this.open_helm(window, cx))),
            nav_row("nav-argo", Lucide::GitFork, "Argo CD", matches!(route, Route::Argo(_)))
                .on_click(cx.listener(|this, _, window, cx| this.open_argo(window, cx))),
        ]
    }

    /// Saved views: a click applies one, the cross removes it.
    fn saved_views(&self, colors: Colors, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let views: Vec<SavedView> = self.favorites.read(cx).views().to_vec();
        if views.is_empty() {
            return Vec::new();
        }
        let mut rows = vec![section_header("Saved views".into(), Lucide::Bookmark, colors).into_any_element()];
        for (i, v) in views.into_iter().enumerate() {
            let detail = format!(
                "{} · {}{}",
                v.kind.split('.').next().unwrap_or_default(),
                match v.namespaces.len() {
                    0 => "all namespaces".to_string(),
                    1 => v.namespaces[0].clone(),
                    n => format!("{n} namespaces"),
                },
                if v.filter.is_empty() { String::new() } else { format!(" · {}", v.filter) }
            );
            let (favs, name) = (self.favorites.clone(), v.name.clone());
            let remove = move |cx: &mut App| favs.update(cx, |f, cx| f.remove_view(&name, cx));
            let title = v.name.clone();
            rows.push(
                two_line_row(("view", i), ("unview", i), title, detail, colors, remove)
                    .on_click(cx.listener(move |this, _, window, cx| this.apply_view(&v, window, cx)))
                    .into_any_element(),
            );
        }
        rows
    }

    /// Starred kinds, then starred objects on the connected clusters.
    fn favorite_rows(&self, selected: Option<&str>, colors: Colors, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let fav_kinds: Vec<String> = self.favorites.read(cx).kinds().to_vec();
        let fav_objects: Vec<FavObject> = self.favorites.read(cx).objects().to_vec();
        if fav_kinds.is_empty() && fav_objects.is_empty() {
            return Vec::new();
        }
        let mut rows = vec![section_header("Favorites".into(), Lucide::Star, colors).into_any_element()];
        for (i, key) in fav_kinds.iter().enumerate() {
            let Some(kind) = k8s::find_kind(&self.kinds, key).cloned() else { continue };
            let is_selected = selected == Some(kind.key().as_str());
            rows.push(
                kind_row(
                    ("fav-kind", i),
                    kind.label.clone(),
                    false,
                    is_selected,
                    key.clone(),
                    true,
                    &self.favorites,
                    colors,
                )
                .on_click(cx.listener(move |this, _, _, cx| this.select_kind(kind.clone(), cx)))
                .into_any_element(),
            );
        }
        for (i, o) in fav_objects.into_iter().enumerate() {
            let Some(ctx) = self.clusters.iter().position(|(c, _)| *c == o.context) else { continue };
            let detail = format!(
                "{} · {}",
                o.kind.split('.').next().unwrap_or_default(),
                o.namespace.clone().unwrap_or_default()
            );
            let (favs, fav) = (self.favorites.clone(), o.clone());
            let remove = move |cx: &mut App| favs.update(cx, |f, cx| f.toggle_object(fav.clone(), cx));
            rows.push(
                two_line_row(("fav-obj", i), ("unfav", i), o.name.clone(), detail, colors, remove)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_object_in(ctx, &o.kind, o.name.clone(), o.namespace.clone(), window, cx);
                    }))
                    .into_any_element(),
            );
        }
        rows
    }

    /// The resource types matching `query`, by section; custom resources grouped by API
    /// group (folded unless searched or opened).
    fn kind_rows(
        &self,
        query: &str,
        selected: Option<&str>,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let fav_kinds = self.favorites.read(cx).kinds().to_vec();
        // The noise discovery keeps for lookups is never listed, even when searched for.
        let matches = |k: &Kind| {
            k8s::in_sidebar(k)
                && (query.is_empty()
                    || k.label.to_lowercase().contains(query)
                    || k.ar.kind.to_lowercase().contains(query)
                    || k.ar.group.to_lowercase().contains(query))
        };
        // How many types each section shows, and the sections folded (all open while searching).
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for k in self.kinds.iter().filter(|k| matches(k)) {
            *counts.entry(k.section).or_default() += 1;
        }
        let folded: Vec<String> =
            if query.is_empty() { crate::settings::with(|s| s.sidebar_folded.clone()) } else { Vec::new() };
        let mut rows = Vec::new();
        let mut section = "";
        let mut group = String::new();
        for (i, kind) in self.kinds.iter().enumerate() {
            if !matches(kind) {
                continue;
            }
            let shut = folded.iter().any(|f| f == kind.section);
            if kind.section != section {
                section = kind.section;
                group.clear();
                let count = counts.get(section).copied().unwrap_or_default();
                rows.push(self.section_row(i, section, count, shut, query.is_empty(), colors, cx).into_any_element());
            }
            if shut {
                continue;
            }
            let is_selected = selected == Some(kind.key().as_str());
            let k = kind.clone();
            let select = cx.listener(move |this, _, _, cx| this.select_kind(k.clone(), cx));
            let star_key = kind.kubectl_name();
            let starred = fav_kinds.contains(&star_key);
            let row = |indent| {
                kind_row(
                    ("kind", i),
                    kind.label.clone(),
                    indent,
                    is_selected,
                    star_key,
                    starred,
                    &self.favorites,
                    colors,
                )
                .on_click(select)
                .into_any_element()
            };
            if !k8s::is_custom(kind) {
                rows.push(row(false));
                continue;
            }
            // Custom resources: one collapsible row per API group, after the built-ins
            // of their section (under "Custom" when the section has built-ins too).
            let open = !query.is_empty() || self.expanded_groups.contains(&kind.ar.group);
            if kind.ar.group != group && group.is_empty() && section != k8s::CUSTOM_SECTION {
                rows.push(
                    div()
                        .px_2()
                        .pt_1()
                        .text_xs()
                        .text_color(colors.muted.opacity(0.8))
                        .child("Custom")
                        .into_any_element(),
                );
            }
            if kind.ar.group != group {
                group.clone_from(&kind.ar.group);
                let g = group.clone();
                rows.push(
                    tip_if_cut(h_flex().id(("group", i)), &group, colors.fits)
                        .gap_1()
                        .px_2()
                        .py(px(3.))
                        .rounded_md()
                        .text_sm()
                        .text_color(colors.fg)
                        .cursor_pointer()
                        .hover(|s| s.bg(colors.active_bg.opacity(0.6)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.expanded_groups.remove(&g) {
                                this.expanded_groups.insert(g.clone());
                            }
                            cx.notify();
                        }))
                        .child(
                            Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight })
                                .size_3()
                                .text_color(colors.muted),
                        )
                        .child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().child(group.clone()))
                        .into_any_element(),
                );
            }
            if open {
                rows.push(row(true));
            }
        }
        rows
    }
}

impl AppView {
    /// A section's header: its name and how many types it holds; a click folds or opens it
    /// (kept in settings.json), except while searching.
    #[allow(clippy::too_many_arguments)]
    fn section_row(
        &self,
        i: usize,
        section: &'static str,
        count: usize,
        shut: bool,
        foldable: bool,
        colors: Colors,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        section_header(section.into(), section_icon(section), colors)
            .id(("section", i))
            .rounded_md()
            .child(div().flex_1())
            .child(div().font_weight(FontWeight::NORMAL).text_color(colors.muted).child(count.to_string()))
            .when(foldable, |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(colors.active_bg.opacity(0.6)))
                    .child(
                        Icon::new(if shut { IconName::ChevronRight } else { IconName::ChevronDown })
                            .size_3()
                            .text_color(colors.muted),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| {
                        crate::settings::update(|s| {
                            if s.sidebar_folded.iter().any(|f| f == section) {
                                s.sidebar_folded.retain(|f| f != section);
                            } else {
                                s.sidebar_folded.push(section.to_string());
                            }
                        });
                        cx.notify();
                    }))
            })
    }
}

/// The sidebar's colors, and how many characters of a row's label fit.
#[derive(Clone, Copy)]
struct Colors {
    /// Characters a row's label shows before it is cut (a little under, to be safe).
    fits: usize,
    fg: Hsla,
    muted: Hsla,
    active_bg: Hsla,
    /// The selected type is marked with a brass bar, like a fitting on the hull.
    brass: Hsla,
}

impl Colors {
    fn of(cx: &App) -> Self {
        let theme = cx.theme();
        // About 7.5 px a character at the sidebar's text size; the row's padding and star
        // take the rest.
        let width = crate::settings::with(crate::settings::sidebar_width_in);
        let fits = ((width - 56.) / (7.5 * crate::app::zoom(cx))).max(0.) as usize;
        Self {
            fits,
            fg: theme.sidebar_foreground,
            muted: theme.sidebar_foreground.opacity(0.62),
            active_bg: theme.sidebar_accent,
            brass: theme.sidebar_primary,
        }
    }
}

/// The whole text in a tooltip, when `text` is longer than the `fits` characters shown.
fn tip_if_cut(row: Stateful<Div>, text: &str, fits: usize) -> Stateful<Div> {
    if text.chars().count() <= fits {
        return row;
    }
    let text = SharedString::from(text.to_string());
    row.tooltip(move |w, cx| component::tooltip::Tooltip::new(text.clone()).build(w, cx))
}

/// A section's name in small capitals, brighter than the details under it.
fn section_header(label: SharedString, icon: Lucide, colors: Colors) -> Div {
    h_flex()
        .gap_1p5()
        .mt_3()
        .py_1()
        .px_2()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(colors.fg.opacity(0.8))
        .child(Icon::new(icon).size_3p5())
        .child(label.to_uppercase())
}

/// A page link at the top of the sidebar, hidden when its name doesn't have `query`.
fn nav_row(
    id: &'static str,
    icon: Lucide,
    label: &'static str,
    on: bool,
    query: &str,
    colors: Colors,
) -> Stateful<Div> {
    h_flex()
        .id(id)
        .when(!label.to_lowercase().contains(query), Styled::hidden)
        .gap_2()
        .px_2()
        .py_1()
        .rounded_md()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .when(on, |d| d.bg(colors.active_bg))
        .hover(|s| s.bg(colors.active_bg.opacity(0.6)))
        .child(Icon::new(icon).size_4())
        .child(div().flex_1().child(label))
}

/// A resource type's row, with its star (shown on hover unless starred).
#[allow(clippy::too_many_arguments)]
fn kind_row(
    id: (&'static str, usize),
    label: String,
    indent: bool,
    is_selected: bool,
    star_key: String,
    starred: bool,
    favs: &Entity<Favorites>,
    colors: Colors,
) -> Stateful<Div> {
    let group = SharedString::from(format!("{}-{}", id.0, id.1));
    let favs = favs.clone();
    let row = tip_if_cut(h_flex().id(id), &label, colors.fits.saturating_sub(if indent { 3 } else { 0 }));
    row.group(group.clone())
        .px_2()
        .when(indent, Styled::pl_5)
        .py(px(3.))
        .rounded_md()
        .text_sm()
        .font_weight(FontWeight::MEDIUM)
        .text_color(colors.fg)
        .cursor_pointer()
        .border_l_2()
        .border_color(if is_selected { colors.brass } else { transparent_black() })
        .when(is_selected, |d| d.bg(colors.active_bg))
        .hover(|s| s.bg(colors.active_bg.opacity(0.6)))
        .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(label))
        .child(
            div()
                .id(SharedString::from(format!("star-{group}")))
                .when(!starred, |d| d.opacity(0.).group_hover(group.clone(), |s| s.opacity(0.5)))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    favs.update(cx, |f, cx| f.toggle_kind(star_key.clone(), cx));
                })
                .child(if starred {
                    Icon::new(IconName::StarFill).size_3().text_color(yellow())
                } else {
                    Icon::new(Lucide::Star).size_3()
                }),
        )
}

/// A saved view or a favorite object: its name over a detail line, and a cross (on
/// hover) that removes it.
fn two_line_row(
    id: (&'static str, usize),
    remove_id: (&'static str, usize),
    title: String,
    detail: String,
    colors: Colors,
    remove: impl Fn(&mut App) + 'static,
) -> Stateful<Div> {
    let line = || div().overflow_hidden().text_ellipsis().whitespace_nowrap();
    tip_if_cut(h_flex().id(id), &title, colors.fits)
        .group(id.0)
        .px_2()
        .py(px(3.))
        .gap_1()
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .hover(|s| s.bg(colors.active_bg.opacity(0.6)))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(line().font_weight(FontWeight::MEDIUM).child(title))
                .child(line().text_xs().text_color(colors.muted).child(detail)),
        )
        .child(
            div()
                .id(remove_id)
                .opacity(0.)
                .group_hover(id.0, |s| s.opacity(0.6))
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    remove(cx);
                })
                .child(Icon::new(Lucide::X).size_3()),
        )
}

pub(super) fn section_icon(section: &str) -> Lucide {
    match section {
        "Cluster" => Lucide::Server,
        "Workloads" => Lucide::Boxes,
        "Networking" => Lucide::Network,
        "Configuration" => Lucide::SlidersHorizontal,
        "Storage" => Lucide::Database,
        "Access Control" => Lucide::Shield,
        "Administration" => Lucide::Settings,
        "Data" => Lucide::Cylinder,
        "Observability" => Lucide::Activity,
        "Delivery" => Lucide::GitBranch,
        "Cloud" => Lucide::Cloud,
        _ => Lucide::Puzzle,
    }
}
