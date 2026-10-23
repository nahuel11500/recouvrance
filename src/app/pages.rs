//! Opening pages: objects, workloads, cluster, problems, Helm, assistant, terminal, apply, plugins.

#[allow(unused_imports)]
use super::*;

impl AppView {
    /// The connected contexts and the namespace a scoped page shows now.
    fn page_scope(&self) -> (Vec<String>, Option<String>) {
        (self.clusters.iter().map(|(c, _)| c.clone()).collect(), self.namespace.clone())
    }

    /// Note what a Workloads or Problems page was built for.
    pub(super) fn remember_scope(&mut self, page: EntityId) {
        let scope = self.page_scope();
        self.page_scopes.insert(page, scope);
    }

    /// Whether a page was built for the contexts and namespace shown now.
    fn scope_matches(&self, page: EntityId) -> bool {
        self.page_scopes.get(&page) == Some(&self.page_scope())
    }

    pub(super) fn select_kind(&mut self, kind: Kind, cx: &mut Context<Self>) {
        self.navigate(Route::Resources(Some(kind)), cx);
    }

    /// Open an object by kubectl-style kind name, e.g. from a link in details.
    pub(super) fn open_object_in(
        &mut self,
        ctx: usize,
        kind: &str,
        name: String,
        namespace: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(kind) = k8s::find_kind(&self.kinds, kind).cloned() else {
            return;
        };
        // No name: open the list of that kind (e.g. a CRD's instances).
        if name.is_empty() {
            // The table too: the same kind's list keeps its watch, and its filter with it.
            self.apply_filter_text(String::new(), window, cx);
            self.select_kind(kind, cx);
            return;
        }
        let namespace = if kind.namespaced { namespace } else { None };
        let obj = ObjectRef { kind, name, namespace, uid: String::new() };
        self.show_detail_in(ctx, obj, window, cx);
    }

    pub(super) fn show_detail_in(&mut self, ctx: usize, obj: ObjectRef, window: &mut Window, cx: &mut Context<Self>) {
        let Some((context, client)) = self.clusters.get(ctx).cloned() else { return };
        let risky = crate::settings::is_protected(&context);
        let forwards = self.forwards.clone();
        let favorites = self.favorites.clone();
        let clusters = self.clusters.clone();
        let context_name = context.clone();
        let panel = cx.new(|cx| {
            let mut p = DetailPanel::new(client, context, risky, forwards, favorites, obj, window, cx);
            p.clusters = clusters;
            p
        });
        cx.subscribe_in(&panel, window, |this, _, ev: &crate::plugins::RunPlugin, window, cx| {
            this.run_plugin(ev, window, cx)
        })
        .detach();
        cx.subscribe_in(&panel, window, move |this, _, ev: &crate::details::Investigate, window, cx| {
            let q = crate::assistant::investigation(
                &context_name,
                &ev.kind,
                ev.namespace.as_deref(),
                &ev.name,
                &ev.findings,
            );
            this.open_assistant(window, cx);
            if let Some(page) = this.assistant_page() {
                page.update(cx, |a, cx| a.start_over(q, cx));
            }
        })
        .detach();
        // Released with the page.
        cx.subscribe_in(&panel, window, move |this, _, ev: &OpenObject, window, cx| {
            let target =
                ev.context.as_ref().and_then(|c| this.clusters.iter().position(|(n, _)| n == c)).unwrap_or(ctx);
            this.open_object_in(target, &ev.kind, ev.name.clone(), ev.namespace.clone(), window, cx);
        })
        .detach();
        self.navigate(Route::Detail(panel), cx);
    }

    pub(super) fn open_workloads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Not connected: open the cluster switcher instead of doing nothing.
        if self.clusters.is_empty() {
            self.ctx_open = true;
            cx.notify();
            return;
        }
        // One instance per page: reuse it from history instead of piling up pollers,
        // unless it was built for other contexts or another namespace.
        if let Some(existing) = self.history.iter().find_map(|r| match r {
            Route::Workloads(e) if self.scope_matches(e.entity_id()) => Some(e.clone()),
            _ => None,
        }) {
            self.navigate(Route::Workloads(existing), cx);
            return;
        }
        let dash = cx.new(|cx| Dashboard::new(self.clusters.clone(), self.namespace.clone(), cx));
        self.remember_scope(dash.entity_id());
        // Released with the page.
        cx.subscribe_in(&dash, window, |this, _, ev: &OpenObject, window, cx| {
            let ctx = ev.context.as_ref().and_then(|c| this.clusters.iter().position(|(n, _)| n == c)).unwrap_or(0);
            this.open_object_in(ctx, &ev.kind, ev.name.clone(), ev.namespace.clone(), window, cx);
        })
        .detach();
        self.navigate(Route::Workloads(dash), cx);
    }

    pub(super) fn open_cluster(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Not connected: open the cluster switcher instead of doing nothing.
        if self.clusters.is_empty() {
            self.ctx_open = true;
            cx.notify();
            return;
        }
        // One instance per page: reuse it from history instead of piling up pollers.
        if let Some(existing) = self.history.iter().find_map(|r| match r {
            Route::Cluster(e) => Some(e.clone()),
            _ => None,
        }) {
            self.navigate(Route::Cluster(existing), cx);
            return;
        }
        let page = cx.new(|cx| ClusterOverview::new(self.clusters.clone(), cx));
        // Released with the page.
        cx.subscribe_in(&page, window, |this, _, ev: &OpenObject, window, cx| {
            let ctx = ev.context.as_ref().and_then(|c| this.clusters.iter().position(|(n, _)| n == c)).unwrap_or(0);
            this.open_object_in(ctx, &ev.kind, ev.name.clone(), ev.namespace.clone(), window, cx);
        })
        .detach();
        self.navigate(Route::Cluster(page), cx);
    }

    /// A plugin runs in a terminal page, or in the background with a notification.
    pub(super) fn run_plugin(&mut self, ev: &crate::plugins::RunPlugin, window: &mut Window, cx: &mut Context<Self>) {
        let Some(client) = self.clusters.iter().find(|c| c.0 == ev.context).map(|c| c.1.clone()) else { return };
        // Asked and confirmed with write mode on, but it may have been turned off since.
        let writes = !ev.plugin.read_only;
        if writes && !crate::details::writes_still_on(window, cx) {
            return;
        }
        match ev.plugin.mode {
            crate::plugins::Mode::Terminal => {
                let (ctx, ns, cmd, env) =
                    (ev.context.clone(), ev.namespace.clone(), ev.command.clone(), ev.env.clone());
                let t = cx.new(|cx| {
                    let t = crate::ui::terminal::TerminalView::local_command(client, ctx, ns, cmd, env, window, cx);
                    // Running a writing plugin again goes through its button: write mode,
                    // protection and its confirm, not the terminal's Reconnect.
                    if writes { t.run_once() } else { t }
                });
                self.navigate(Route::Terminal(t), cx);
            }
            crate::plugins::Mode::Background => {
                let (ctx, ns, cmd, env, name) = (
                    ev.context.clone(),
                    ev.namespace.clone(),
                    ev.command.clone(),
                    ev.env.clone(),
                    ev.plugin.name.clone(),
                );
                window.push_notification(Notification::info(format!("Running {name}…")), cx);
                cx.spawn_in(window, async move |_, cx| {
                    let r = k8s::run_command(ctx, ns, cmd, env).await;
                    let _ = cx.update(|window, cx| match r {
                        Ok(out) => window.push_notification(
                            Notification::success(format!("{name}: {}", out.lines().last().unwrap_or("done"))),
                            cx,
                        ),
                        Err(e) => window.push_notification(Notification::error(format!("{name} failed: {e:#}")), cx),
                    });
                })
                .detach();
            }
        }
    }

    fn assistant_page(&self) -> Option<Entity<crate::ui::assistant_view::AssistantView>> {
        self.history.iter().find_map(|r| match r {
            Route::Assistant(e) => Some(e.clone()),
            _ => None,
        })
    }

    /// One assistant chat per window, kept across navigation.
    pub(super) fn open_assistant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = match self.assistant_page() {
            Some(p) => p,
            None => {
                let scope = if self.clusters.is_empty() {
                    "no cluster open in Recouvrance (every kubeconfig context is available: start with list_contexts)"
                        .to_string()
                } else {
                    format!(
                        "contexts {}{}",
                        self.clusters.iter().map(|c| c.0.clone()).collect::<Vec<_>>().join(", "),
                        self.namespace.as_ref().map(|n| format!(", namespace {n}")).unwrap_or_default()
                    )
                };
                let hub = self.hub.clone();
                cx.new(|cx| crate::ui::assistant_view::AssistantView::new(hub, scope, window, cx))
            }
        };
        self.navigate(Route::Assistant(page), cx);
    }

    pub(super) fn open_helm_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((context, _)) = self.clusters.first().cloned() else { return };
        let ns = self.namespace.clone();
        let page = cx.new(|cx| crate::ui::helm_install::HelmInstall::new(context, ns, window, cx));
        self.navigate(Route::HelmInstall(page), cx);
    }

    pub(super) fn open_access(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((context, client)) = self.clusters.first().cloned() else { return };
        let (kinds, ns) = (self.kinds.clone(), self.namespace.clone());
        let page = cx.new(|cx| crate::ui::access_view::AccessView::new(context, client, kinds, ns, window, cx));
        self.navigate(Route::Access(page), cx);
    }

    pub(super) fn open_problems(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Not connected: open the cluster switcher instead of doing nothing.
        if self.clusters.is_empty() {
            self.ctx_open = true;
            cx.notify();
            return;
        }
        // One instance per page: reuse it from history instead of piling up pollers,
        // unless it was built for other contexts or another namespace.
        if let Some(existing) = self.history.iter().find_map(|r| match r {
            Route::Problems(e) if self.scope_matches(e.entity_id()) => Some(e.clone()),
            _ => None,
        }) {
            self.navigate(Route::Problems(existing), cx);
            return;
        }
        let (clusters, kinds, ns) = (self.clusters.clone(), self.kinds.clone(), self.namespace.clone());
        let page = cx.new(|cx| crate::ui::problems_view::ProblemsView::new(clusters, kinds, ns, cx));
        self.remember_scope(page.entity_id());
        // Released with the page.
        cx.subscribe_in(&page, window, |this, _, ev: &OpenObject, window, cx| {
            let ctx = ev.context.as_ref().and_then(|c| this.clusters.iter().position(|(n, _)| n == c)).unwrap_or(0);
            this.open_object_in(ctx, &ev.kind, ev.name.clone(), ev.namespace.clone(), window, cx);
        })
        .detach();
        self.navigate(Route::Problems(page), cx);
    }

    pub(super) fn open_apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((context, client)) = self.clusters.first().cloned() else { return };
        let (kinds, ns) = (self.kinds.clone(), self.namespace.clone());
        let page = cx.new(|cx| crate::ui::apply_view::ApplyView::new(context, client, kinds, ns, window, cx));
        self.navigate(Route::Apply(page), cx);
    }

    /// A local shell with kubectl set to the current context and namespace.
    pub(super) fn open_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((context, client)) = self.clusters.first().cloned() else { return };
        let ns = self.namespace.clone();
        let term = match self.terminals.get(&context) {
            Some(t) => t.clone(),
            None => {
                let t = cx.new(|cx| crate::ui::terminal::TerminalView::local(client, context.clone(), ns, window, cx));
                self.terminals.insert(context, t.clone());
                t
            }
        };
        term.update(cx, |t, cx| t.focus(window, cx));
        self.navigate(Route::Terminal(term), cx);
    }

    pub(super) fn open_helm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Not connected: open the cluster switcher instead of doing nothing.
        if self.clusters.is_empty() {
            self.ctx_open = true;
            cx.notify();
            return;
        }
        let list = cx.new(|cx| HelmList::new(self.clusters.clone(), self.namespace.clone(), window, cx));
        self.subscribe_helm(&list, window, cx);
        self.navigate(Route::Helm(list), cx);
    }

    /// Open a release or the install page from the release list (also after F5).
    pub(super) fn subscribe_helm(&mut self, list: &Entity<HelmList>, window: &mut Window, cx: &mut Context<Self>) {
        // Released with the page.
        cx.subscribe_in(list, window, |this, _, ev: &OpenRelease, window, cx| {
            let release = ev.0.clone();
            let Some((context, client)) = this.clusters.iter().find(|(c, _)| *c == release.context).cloned() else {
                return;
            };
            let page = cx.new(|cx| HelmRelease::new(client, context, release, window, cx));
            this.navigate(Route::HelmRelease(page), cx);
        })
        .detach();
        cx.subscribe_in(list, window, |this, _, _: &crate::ui::helm::OpenInstall, window, cx| {
            this.open_helm_install(window, cx)
        })
        .detach();
    }
}
