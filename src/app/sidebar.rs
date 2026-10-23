//! The sidebar: pages, resource types by section, favorites, port-forwards.

#[allow(unused_imports)]
use super::*;
use gpui_kit::component::button::ButtonCustomVariant;

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

    pub(super) fn render_forwards(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.sidebar_foreground.opacity(0.62), theme.danger);
        let items = &self.forwards.read(cx).items;
        let contexts: Vec<String> = self.clusters.iter().map(|c| c.0.clone()).collect();
        let restorable = self.forwards.read(cx).restorable(&contexts).len();
        let recent = self.forwards.read(cx).recent(&contexts);
        v_flex()
            .px_2()
            .when(restorable > 1, |d| {
                d.child(
                    div().px_2().pt_2().child(
                        Button::new("restore-forwards")
                            .xsmall()
                            .custom(on_hull(cx))
                            .icon(Lucide::ArrowRightLeft)
                            .label(format!(
                                "Restore {restorable} port-forward{}",
                                if restorable > 1 { "s" } else { "" }
                            ))
                            .on_click(cx.listener(|this, _, _, cx| this.restore_forwards(cx))),
                    ),
                )
            })
            .when(!items.is_empty() || !recent.is_empty(), |d| {
                d.child(
                    h_flex()
                        .gap_1p5()
                        .pt_3()
                        .pb_1()
                        .px_2()
                        .text_xs()
                        .text_color(muted)
                        .child(Icon::new(Lucide::ArrowRightLeft).size_3p5())
                        .child("Port forwards"),
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
            .children(recent.into_iter().take(8).enumerate().map(|(i, saved)| {
                let target = match &saved.service {
                    Some(svc) => format!("svc/{svc}"),
                    None => crate::forwards::pod_base(&saved.pod).to_string(),
                };
                let local = saved.local.map(|p| format!("localhost:{p} → ")).unwrap_or_default();
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
            }))
    }

    pub(super) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.kind_filter.read(cx).value().to_lowercase();
        let selected = self.kind.as_ref().filter(|_| self.route().is_resources()).map(|k| k.key());
        let theme = cx.theme();
        let (muted, active_bg, fg) =
            (theme.sidebar_foreground.opacity(0.62), theme.sidebar_accent, theme.sidebar_foreground);
        // The selected type is marked with a brass bar, like a fitting on the hull.
        let brass = theme.sidebar_primary;
        let matches = |k: &Kind| {
            query.is_empty()
                || k.label.to_lowercase().contains(&query)
                || k.ar.kind.to_lowercase().contains(&query)
                || k.ar.group.to_lowercase().contains(&query)
        };

        let section_header = |label: SharedString, icon: Lucide| {
            h_flex()
                .gap_1p5()
                .pt_4()
                .pb_1()
                .px_2()
                .text_xs()
                .text_color(muted)
                .child(Icon::new(icon).size_3p5())
                .child(label)
        };
        let favs = self.favorites.clone();
        let fav_kinds: Vec<String> = favs.read(cx).kinds().to_vec();
        let item = |id: (&'static str, usize), label: String, indent: bool, is_selected: bool, star_key: String| {
            let group = SharedString::from(format!("{}-{}", id.0, id.1));
            let starred = fav_kinds.contains(&star_key);
            let favs = favs.clone();
            h_flex()
                .id(id)
                .group(group.clone())
                .px_2()
                .when(indent, |d| d.pl_5())
                .py(px(3.))
                .rounded_md()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(fg)
                .cursor_pointer()
                .border_l_2()
                .border_color(if is_selected { brass } else { transparent_black() })
                .when(is_selected, |d| d.bg(active_bg))
                .hover(|s| s.bg(active_bg.opacity(0.6)))
                .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(label))
                .child(
                    div()
                        .id(SharedString::from(format!("star-{group}")))
                        .when(!starred, |d| d.opacity(0.).group_hover(group.clone(), |s| s.opacity(0.5)))
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            let key = star_key.clone();
                            favs.update(cx, |f, cx| f.toggle_kind(key, cx));
                        })
                        .child(if starred {
                            Icon::new(IconName::StarFill).size_3().text_color(yellow())
                        } else {
                            Icon::new(Lucide::Star).size_3()
                        }),
                )
        };

        let mut children: Vec<AnyElement> = Vec::new();
        let fav_objects: Vec<FavObject> = self.favorites.read(cx).objects().to_vec();
        let views: Vec<SavedView> = self.favorites.read(cx).views().to_vec();
        if query.is_empty() && !views.is_empty() {
            children.push(section_header("Saved views".into(), Lucide::Bookmark).into_any_element());
            for (i, v) in views.into_iter().enumerate() {
                let favs = self.favorites.clone();
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
                let (v2, name) = (v.clone(), v.name.clone());
                children.push(
                    h_flex()
                        .id(("view", i))
                        .group("view")
                        .px_2()
                        .py(px(3.))
                        .gap_1()
                        .rounded_md()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(active_bg.opacity(0.6)))
                        .on_click(cx.listener(move |this, _, window, cx| this.apply_view(&v2, window, cx)))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(v.name.clone()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .child(detail),
                                ),
                        )
                        .child(
                            div()
                                .id(("unview", i))
                                .opacity(0.)
                                .group_hover("view", |s| s.opacity(0.6))
                                .on_click(move |_, _, cx| {
                                    cx.stop_propagation();
                                    favs.update(cx, |f, cx| f.remove_view(&name, cx));
                                })
                                .child(Icon::new(Lucide::X).size_3()),
                        )
                        .into_any_element(),
                );
            }
        }
        if query.is_empty() && (!fav_kinds.is_empty() || !fav_objects.is_empty()) {
            children.push(section_header("Favorites".into(), Lucide::Star).into_any_element());
            for (i, key) in fav_kinds.iter().enumerate() {
                let Some(kind) = k8s::find_kind(&self.kinds, key).cloned() else { continue };
                let is_selected = selected.as_deref() == Some(kind.key().as_str());
                let label = kind.label.clone();
                children.push(
                    item(("fav-kind", i), label, false, is_selected, key.clone())
                        .on_click(cx.listener(move |this, _, _, cx| this.select_kind(kind.clone(), cx)))
                        .into_any_element(),
                );
            }
            for (i, o) in fav_objects.iter().enumerate() {
                let Some(ctx) = self.clusters.iter().position(|(c, _)| *c == o.context) else { continue };
                let o2 = o.clone();
                let favs = self.favorites.clone();
                children.push(
                    h_flex()
                        .id(("fav-obj", i))
                        .group("fav-obj")
                        .px_2()
                        .py(px(3.))
                        .gap_1()
                        .rounded_md()
                        .text_sm()
                        .cursor_pointer()
                        .hover(|s| s.bg(active_bg.opacity(0.6)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_object_in(ctx, &o2.kind, o2.name.clone(), o2.namespace.clone(), window, cx)
                        }))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(o.name.clone()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .child(format!(
                                            "{} · {}",
                                            o.kind.split('.').next().unwrap_or_default(),
                                            o.namespace.clone().unwrap_or_default()
                                        )),
                                ),
                        )
                        .child(
                            div()
                                .id(("unfav", i))
                                .opacity(0.)
                                .group_hover("fav-obj", |s| s.opacity(0.6))
                                .on_click({
                                    let o = o.clone();
                                    move |_, _, cx| {
                                        cx.stop_propagation();
                                        favs.update(cx, |f, cx| f.toggle_object(o.clone(), cx));
                                    }
                                })
                                .child(Icon::new(Lucide::X).size_3()),
                        )
                        .into_any_element(),
                );
            }
        }
        let mut section = "";
        let mut group = String::new();
        for (i, kind) in self.kinds.iter().enumerate() {
            if !matches(kind) {
                continue;
            }
            if kind.section != section {
                section = kind.section;
                group.clear();
                children.push(section_header(section.into(), section_icon(section)).into_any_element());
            }
            let is_selected = selected.as_deref() == Some(kind.key().as_str());
            let k = kind.clone();
            let select = cx.listener(move |this, _, _, cx| this.select_kind(k.clone(), cx));
            if !k8s::is_custom(kind) {
                children.push(
                    item(("kind", i), kind.label.clone(), false, is_selected, kind.kubectl_name())
                        .on_click(select)
                        .into_any_element(),
                );
                continue;
            }
            // Custom resources: one collapsible row per API group, after the built-ins
            // of their section (under "Custom" when the section has built-ins too).
            let open = !query.is_empty() || self.expanded_groups.contains(&kind.ar.group);
            if kind.ar.group != group && group.is_empty() && section != k8s::CUSTOM_SECTION {
                children.push(
                    div().px_2().pt_1().text_xs().text_color(muted.opacity(0.8)).child("Custom").into_any_element(),
                );
            }
            if kind.ar.group != group {
                group = kind.ar.group.clone();
                let g = group.clone();
                children.push(
                    h_flex()
                        .id(("group", i))
                        .gap_1()
                        .px_2()
                        .py(px(3.))
                        .rounded_md()
                        .text_sm()
                        .text_color(fg)
                        .cursor_pointer()
                        .hover(|s| s.bg(active_bg.opacity(0.6)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.expanded_groups.remove(&g) {
                                this.expanded_groups.insert(g.clone());
                            }
                            cx.notify();
                        }))
                        .child(
                            Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight })
                                .size_3()
                                .text_color(muted),
                        )
                        .child(div().overflow_hidden().text_ellipsis().whitespace_nowrap().child(group.clone()))
                        .into_any_element(),
                );
            }
            if open {
                children.push(
                    item(("kind", i), kind.label.clone(), true, is_selected, kind.kubectl_name())
                        .on_click(select)
                        .into_any_element(),
                );
            }
        }

        let context = match self.clusters.len() {
            0 if self.connecting => {
                format!("Connecting to {}…", self.context.clone().unwrap_or_default())
            }
            0 => "Not connected".into(),
            1 => self.clusters[0].0.clone(),
            n => format!("{n} contexts"),
        };
        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().sidebar_foreground)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(
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
                    ),
            )
            .child({
                let pending = self.approvals.len();
                let on = self.route().is_agents();
                h_flex()
                    .id("nav-agents")
                    .mx_2()
                    .mt_3()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .when(on, |d| d.bg(active_bg))
                    .hover(|s| s.bg(active_bg.opacity(0.6)))
                    .on_click(cx.listener(|this, _, _, cx| this.navigate(Route::Agents, cx)))
                    .child(Icon::new(Lucide::Bot).size_4())
                    .child(div().flex_1().child("Agents"))
                    .when(!self.hub.agents_enabled(), |d| {
                        d.child(div().text_xs().text_color(cx.theme().danger).child("off"))
                    })
                    .when(pending > 0, |d| {
                        d.child(
                            div()
                                .px_1p5()
                                .rounded_full()
                                .text_xs()
                                .bg(cx.theme().warning)
                                .text_color(white())
                                .child(pending.to_string()),
                        )
                    })
            })
            .child({
                let on = matches!(self.route(), Route::Assistant(_));
                h_flex()
                    .id("nav-assistant")
                    .mx_2()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .when(on, |d| d.bg(active_bg))
                    .hover(|s| s.bg(active_bg.opacity(0.6)))
                    .on_click(cx.listener(|this, _, window, cx| this.open_assistant(window, cx)))
                    .child(Icon::new(Lucide::Sparkles).size_4())
                    .child("Assistant")
            })
            .child({
                let on = matches!(self.route(), Route::Problems(_));
                h_flex()
                    .id("nav-problems")
                    .mx_2()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .when(on, |d| d.bg(active_bg))
                    .hover(|s| s.bg(active_bg.opacity(0.6)))
                    .on_click(cx.listener(|this, _, window, cx| this.open_problems(window, cx)))
                    .child(Icon::new(Lucide::TriangleAlert).size_4())
                    .child(div().flex_1().child("Problems"))
                    .when_some(self.problem_counts, |d, (crit, warn)| {
                        let (n, color) = if crit > 0 { (crit, cx.theme().danger) } else { (warn, cx.theme().warning) };
                        d.when(n > 0, |d| {
                            d.child(
                                div()
                                    .px_1p5()
                                    .rounded_full()
                                    .text_xs()
                                    .bg(color)
                                    .text_color(white())
                                    .child(n.to_string()),
                            )
                        })
                    })
                    .when_some(self.problem_gaps.clone(), |d, gaps| {
                        d.tooltip(move |w, cx| component::tooltip::Tooltip::new(gaps.clone()).build(w, cx))
                    })
            })
            .child({
                let on = matches!(self.route(), Route::Cluster(_));
                h_flex()
                    .id("nav-cluster")
                    .mx_2()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .when(on, |d| d.bg(active_bg))
                    .hover(|s| s.bg(active_bg.opacity(0.6)))
                    .on_click(cx.listener(|this, _, window, cx| this.open_cluster(window, cx)))
                    .child(Icon::new(Lucide::Server).size_4())
                    .child("Cluster overview")
            })
            .child({
                let on = matches!(self.route(), Route::Workloads(_));
                h_flex()
                    .id("nav-workloads")
                    .mx_2()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .when(on, |d| d.bg(active_bg))
                    .hover(|s| s.bg(active_bg.opacity(0.6)))
                    .on_click(cx.listener(|this, _, window, cx| this.open_workloads(window, cx)))
                    .child(Icon::new(Lucide::LayoutDashboard).size_4())
                    .child("Workloads overview")
            })
            .child({
                let on = matches!(self.route(), Route::Helm(_) | Route::HelmRelease(_));
                h_flex()
                    .id("nav-helm")
                    .mx_2()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .when(on, |d| d.bg(active_bg))
                    .hover(|s| s.bg(active_bg.opacity(0.6)))
                    .on_click(cx.listener(|this, _, window, cx| this.open_helm(window, cx)))
                    .child(Icon::new(Lucide::Ship).size_4())
                    .child("Helm releases")
            })
            .child(self.render_forwards(cx))
            .child(div().px_2().pt_3().child(Input::new(&self.kind_filter).small().cleanable(true)))
            .child(v_flex().id("kinds").flex_1().min_h_0().overflow_y_scroll().px_2().pb_4().children(children))
    }
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
