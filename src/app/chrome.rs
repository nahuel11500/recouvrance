//! Window chrome: context picker page, approvals, context strip, command palette, zoom.

#[allow(unused_imports)]
use super::*;

impl AppView {
    pub(super) fn zoom_by(&mut self, delta: Option<f32>, window: &mut Window, cx: &mut Context<Self>) {
        let z = delta.map(|d| zoom(cx) + d).unwrap_or(1.0);
        set_zoom(z, window, cx);
        // Column widths scale too; they are computed when the table refreshes.
        self.table.update(cx, |t, cx| t.refresh(cx));
    }

    pub(super) fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut items = Vec::new();
        for (label, page, icon) in [
            ("Cluster overview", "cluster", Lucide::Server),
            ("Workloads overview", "workloads", Lucide::LayoutDashboard),
            ("Helm releases", "helm", Lucide::Ship),
            ("Agents", "agents", Lucide::Bot),
            ("Contexts", "contexts", Lucide::Server),
            ("Settings", "settings", Lucide::Settings),
            ("Terminal (kubectl on this context)", "terminal", Lucide::SquareTerminal),
            ("Create / apply YAML", "apply", Lucide::Plus),
            ("Problems (what is broken)", "problems", Lucide::TriangleAlert),
            ("My access (RBAC in this namespace)", "access", Lucide::KeyRound),
            ("Install a Helm chart", "helm-install", Lucide::Download),
            ("Assistant (ask your clusters)", "assistant", Lucide::Sparkles),
            ("New window (Ctrl Shift N)", "new-window", Lucide::AppWindow),
            ("Zoom in (Ctrl +)", "zoom-in", Lucide::ZoomIn),
            ("Zoom out (Ctrl -)", "zoom-out", Lucide::ZoomOut),
            ("Reset zoom (Ctrl 0)", "zoom-reset", Lucide::Search),
            ("Toggle dark / light theme", "theme", Lucide::Moon),
        ] {
            items.push(PaletteItem { label: label.into(), detail: "page".into(), icon, target: Target::Page(page) });
        }
        for k in &self.kinds {
            items.push(PaletteItem {
                label: k.label.clone(),
                detail: format!("{} · {}", k.kubectl_name(), k.section),
                icon: section_icon(k.section),
                target: Target::Kind(k.clone()),
            });
        }
        for c in &self.all_contexts {
            items.push(PaletteItem {
                label: c.clone(),
                detail: "switch context".into(),
                icon: Lucide::Ship,
                target: Target::Context(c.clone()),
            });
        }
        // With several clusters, objects say which one they are on.
        let names: std::rc::Rc<Vec<String>> = std::rc::Rc::new(self.clusters.iter().map(|(c, _)| c.clone()).collect());
        for o in self.favorites.read(cx).objects() {
            if let Some(ctx) = self.clusters.iter().position(|(c, _)| *c == o.context) {
                items.push(PaletteItem {
                    label: o.name.clone(),
                    detail: format!(
                        "★ {} · {}{}",
                        o.kind,
                        o.namespace.clone().unwrap_or_default(),
                        on_cluster(&names, ctx)
                    ),
                    icon: Lucide::Star,
                    target: Target::Object {
                        ctx,
                        kind: o.kind.clone(),
                        name: o.name.clone(),
                        namespace: o.namespace.clone(),
                    },
                });
            }
        }
        // Objects of the current table.
        if let Some(kind) = &self.kind {
            let t = self.table.read(cx).delegate();
            for ix in 0..t.rows_count_visible().min(5000) {
                let (Some(row), Some(ctx)) = (t.row(ix), t.row_context(ix)) else { continue };
                items.push(PaletteItem {
                    label: row.name.clone(),
                    detail: format!(
                        "{} · {}{}",
                        kind.label,
                        row.namespace.clone().unwrap_or_default(),
                        on_cluster(&names, ctx)
                    ),
                    icon: section_icon(kind.section),
                    target: Target::Object {
                        ctx,
                        kind: kind.kubectl_name(),
                        name: row.name.clone(),
                        namespace: row.namespace.clone(),
                    },
                });
            }
        }
        let index_items = move |entries: &[IndexEntry]| -> Vec<PaletteItem> {
            entries
                .iter()
                .map(|e| PaletteItem {
                    label: e.name.clone(),
                    detail: format!(
                        "{}{}{}",
                        e.label,
                        e.namespace.as_ref().map(|n| format!(" · {n}")).unwrap_or_default(),
                        on_cluster(&names, e.ctx)
                    ),
                    icon: e.icon,
                    target: Target::Object {
                        ctx: e.ctx,
                        kind: e.kind.clone(),
                        name: e.name.clone(),
                        namespace: e.namespace.clone(),
                    },
                })
                .collect()
        };
        items.extend(index_items(&self.search_index));
        let palette = cx.new(|cx| Palette::new(items, window, cx));
        // (Re)build the cluster-wide name index in the background; results stream in.
        let stale = self.index_built.is_none_or(|t| t.elapsed().as_secs() > 300);
        if stale && !self.clusters.is_empty() {
            self.search_index.clear();
            self.index_built = Some(std::time::Instant::now());
            self._index_tasks.clear();
            let total = self.clusters.len() * SEARCH_KINDS.len();
            palette.update(cx, |p, cx| p.set_status(Some(format!("indexing… 0/{total}")), cx));
            let done = std::rc::Rc::new(std::cell::Cell::new(0usize));
            for (ctx, (_, client)) in self.clusters.clone().into_iter().enumerate() {
                for kind_name in SEARCH_KINDS {
                    let Some(kind) = k8s::find_kind(&self.kinds, kind_name).cloned() else { continue };
                    let (client, palette, done) = (client.clone(), palette.downgrade(), done.clone());
                    let index_items = index_items.clone();
                    self._index_tasks.push(cx.spawn(async move |this, cx| {
                        let names = k8s::list_names(client, kind.ar.clone(), None).await.unwrap_or_default();
                        let entries: Vec<IndexEntry> = names
                            .into_iter()
                            .map(|(namespace, name)| IndexEntry {
                                ctx,
                                kind: kind.kubectl_name(),
                                label: kind.label.clone(),
                                icon: section_icon(kind.section),
                                namespace,
                                name,
                            })
                            .collect();
                        let new_items = index_items(&entries);
                        done.set(done.get() + 1);
                        let status = (done.get() < total).then(|| format!("indexing… {}/{total}", done.get()));
                        let _ = palette.update(cx, |p, cx| {
                            p.add_items(new_items, cx);
                            p.set_status(status, cx);
                        });
                        let _ = this.update(cx, |this, _| this.search_index.extend(entries));
                    }));
                }
            }
        }
        self._subs.push(cx.subscribe_in(&palette, window, |this, _, ev: &PaletteEvent, window, cx| {
            this.palette = None;
            window.focus(&this.focus, cx);
            if let PaletteEvent::Pick(target) = ev {
                match target.clone() {
                    Target::Kind(k) => this.select_kind(k, cx),
                    Target::Page("workloads") => this.open_workloads(window, cx),
                    Target::Page("helm") => this.open_helm(window, cx),
                    Target::Page("cluster") => this.open_cluster(window, cx),
                    Target::Page("settings") => this.navigate(Route::Settings, cx),
                    Target::Page("terminal") => this.open_terminal(window, cx),
                    Target::Page("apply") => this.open_apply(window, cx),
                    Target::Page("new-window") => open_main_window(cx),
                    Target::Page("problems") => this.open_problems(window, cx),
                    Target::Page("access") => this.open_access(window, cx),
                    Target::Page("helm-install") => this.open_helm_install(window, cx),
                    Target::Page("assistant") => this.open_assistant(window, cx),
                    Target::Page("zoom-in") => this.zoom_by(Some(0.1), window, cx),
                    Target::Page("zoom-out") => this.zoom_by(Some(-0.1), window, cx),
                    Target::Page("zoom-reset") => this.zoom_by(None, window, cx),
                    Target::Page("theme") => {
                        let mode = if cx.theme().is_dark() { ThemeMode::Light } else { ThemeMode::Dark };
                        set_theme(mode, Some(window), cx);
                    }
                    Target::Page("agents") => this.navigate(Route::Agents, cx),
                    Target::Page(_) => this.navigate(Route::Contexts, cx),
                    Target::Context(c) => this.connect_many(vec![c], window, cx),
                    Target::Object { ctx, kind, name, namespace } => {
                        this.open_object_in(ctx, &kind, name, namespace, window, cx)
                    }
                }
            }
            cx.notify();
        }));
        self.palette = Some(palette);
        cx.notify();
    }

    /// A thin bar in the connected contexts' colors, so a colored (e.g. prod) cluster is obvious.
    pub(super) fn context_strip(&self) -> Option<AnyElement> {
        let colors: Vec<u32> = self.clusters.iter().filter_map(|c| crate::settings::context_color(&c.0)).collect();
        if colors.is_empty() {
            return None;
        }
        Some(
            h_flex()
                .h(px(3.))
                .w_full()
                .children(colors.into_iter().map(|c| div().flex_1().h_full().bg(rgb(c))))
                .into_any_element(),
        )
    }

    pub(super) fn render_approvals(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex().children(self.approvals.iter().map(|a| {
            let (deny, approve) = (a.id, a.id);
            let hub = self.hub.clone();
            let hub2 = self.hub.clone();
            h_flex()
                .gap_3()
                .px_3()
                .py_2()
                .bg(theme.warning.opacity(0.15))
                .border_b_1()
                .border_color(theme.warning)
                .child(Icon::new(Lucide::Bot).size_4())
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_sm()
                        .child(format!("{} wants to run {}: ", a.agent, a.tool))
                        // Patches and manifests are shown whole: what is approved is what runs.
                        .child(
                            div()
                                .id(("approval-summary", a.id as usize))
                                .max_h(px(240.))
                                .overflow_y_scroll()
                                .font_family(theme.mono_font_family.clone())
                                .text_xs()
                                .child(a.summary.clone()),
                        ),
                )
                // Only offered when a rule can cover exactly this request (not exec, for one).
                .when(a.as_rule().is_some(), |d| {
                    let (hub, approval) = (self.hub.clone(), a.clone());
                    let hour = Button::new(("hour", a.id as usize)).small().ghost().label("Allow for 1h").on_click(
                        move |_, window, cx| {
                            if let Some(rule) = approval.as_rule_for_an_hour()
                                && let Err(e) =
                                    hub.update_agent(&approval.agent_id, |agent| agent.policy.rules.push(rule))
                            {
                                window.push_notification(Notification::error(format!("Rule not saved: {e:#}")), cx);
                            }
                            hub.resolve_approval(approval.id, true);
                        },
                    );
                    let (hub, approval) = (self.hub.clone(), a.clone());
                    let always = Button::new(("always", a.id as usize))
                        .small()
                        .ghost()
                        .label("Always allow this")
                        .tooltip("Approve, and add a rule allowing exactly this request from now on")
                        .on_click(move |_, window, cx| {
                            if let Some(rule) = approval.as_rule()
                                && let Err(e) =
                                    hub.update_agent(&approval.agent_id, |agent| agent.policy.rules.push(rule))
                            {
                                window.push_notification(Notification::error(format!("Rule not saved: {e:#}")), cx);
                            }
                            hub.resolve_approval(approval.id, true);
                        });
                    d.child(hour).child(always)
                })
                .child(
                    Button::new(("deny", a.id as usize))
                        .small()
                        .label("Deny")
                        .on_click(move |_, _, _| hub.resolve_approval(deny, false)),
                )
                .child(
                    Button::new(("approve", a.id as usize))
                        .small()
                        .primary()
                        .label("Approve")
                        .on_click(move |_, _, _| hub2.resolve_approval(approve, true)),
                )
        }))
    }

    pub(super) fn toggle_context(&mut self, context: String, cx: &mut Context<Self>) {
        if !self.picked.remove(&context) {
            self.picked.insert(context);
        }
        cx.notify();
    }

    pub(super) fn connect_picked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Keep the kubeconfig order so the primary context is predictable.
        let contexts: Vec<String> = self.all_contexts.iter().filter(|c| self.picked.contains(*c)).cloned().collect();
        self.connect_many(contexts, window, cx);
    }

    pub(super) fn render_contexts(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        v_flex().size_full().items_center().justify_center().child(
            v_flex()
                .w(px(900.))
                .gap_4()
                .child(
                    h_flex()
                        .gap_2()
                        .child(crate::ui::logo(px(40.)))
                        .child(div().text_2xl().font_weight(FontWeight::BOLD).child("Recouvrance")),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child("Select one or more contexts to get started"),
                )
                .child(h_flex().flex_wrap().gap_2().children(self.all_contexts.iter().enumerate().map(|(i, ctx)| {
                    let name = ctx.clone();
                    let risky = crate::settings::is_protected(ctx);
                    let accent: Hsla = crate::settings::context_color(ctx).map(|c| rgb(c).into()).unwrap_or(theme.blue);
                    let picked = self.picked.contains(ctx);
                    h_flex()
                        .id(("ctx-card", i))
                        .w(px(290.))
                        .gap_3()
                        .p_3()
                        .rounded_md()
                        .border_1()
                        .border_color(if picked { theme.blue } else { theme.border })
                        .when(picked, |d| d.bg(theme.blue.opacity(0.08)))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.list_hover))
                        .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                            // Double click connects to just this one.
                            if ev.click_count() >= 2 {
                                this.picked = [name.clone()].into();
                                this.connect_picked(window, cx);
                            } else {
                                this.toggle_context(name.clone(), cx);
                            }
                        }))
                        .child(Icon::new(Lucide::Ship).size_6().text_color(accent))
                        .child(
                            v_flex()
                                .min_w_0()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(FontWeight::MEDIUM)
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .child(ctx.clone()),
                                )
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(if risky { theme.warning } else { theme.muted_foreground })
                                        .child(if risky { "protected" } else { "context" }),
                                ),
                        )
                })))
                .child(
                    h_flex()
                        .justify_center()
                        .gap_2()
                        .pt_4()
                        .child(
                            Button::new("add-kubeconfig")
                                .ghost()
                                .icon(Lucide::Plus)
                                .label("Add a kubeconfig")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.settings_view.update(cx, |s, cx| s.show_kubeconfig(cx));
                                    this.navigate(Route::Settings, cx);
                                })),
                        )
                        .child(
                            Button::new("connect")
                                .primary()
                                .label(match self.picked.len() {
                                    0 => "Connect".to_string(),
                                    1 => "Connect →".to_string(),
                                    n => format!("Connect to {n} contexts →"),
                                })
                                .disabled(self.picked.is_empty())
                                .on_click(cx.listener(|this, _, window, cx| this.connect_picked(window, cx))),
                        ),
                ),
        )
    }
}

/// " · <context>" for an object on cluster `ctx`, when several are connected.
fn on_cluster(names: &[String], ctx: usize) -> String {
    match names.get(ctx) {
        Some(name) if names.len() > 1 => format!(" · {name}"),
        _ => String::new(),
    }
}
