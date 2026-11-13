//! Opening pages: objects, workloads, cluster, problems, Helm, Argo CD, assistant, terminal, apply, plugins.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::{AppView, Route};
use crate::details::{DetailPanel, OpenObject};
use crate::k8s;
use crate::k8s::{Kind, ObjectRef};
use crate::ui::helm::{HelmList, HelmRelease, OpenRelease};

impl AppView {
    /// The connected contexts and the namespaces a scoped page shows now.
    fn page_scope(&self) -> (Vec<String>, Vec<String>) {
        (self.clusters.iter().map(|(c, _)| c.clone()).collect(), self.namespaces.clone())
    }

    /// The Workloads, Problems, Helm or Timeline page on screen lists what the namespaces chosen
    /// now ask for, when it was built for others.
    pub(super) fn rescope_page(&mut self, cx: &mut Context<Self>) {
        let id = match self.route() {
            Route::Workloads(p) => p.entity_id(),
            Route::Problems(p) => p.entity_id(),
            Route::Helm(p) => p.entity_id(),
            Route::Timeline(p) => p.entity_id(),
            _ => return,
        };
        if self.scope_matches(id) {
            return;
        }
        let (scope, namespaces) = (self.namespace_scope(), self.namespaces.clone());
        match self.route().clone() {
            Route::Workloads(p) => p.update(cx, |p, cx| p.set_scope(scope, cx)),
            Route::Problems(p) => {
                let kinds = self.cluster_kinds();
                p.update(cx, |p, cx| p.set_scope(scope, kinds, namespaces, cx));
            }
            Route::Helm(p) => p.update(cx, |p, cx| p.set_scope(scope, cx)),
            Route::Timeline(p) => p.update(cx, |p, cx| p.set_scope(scope, namespaces, cx)),
            _ => return,
        }
        self.remember_scope(id);
    }

    /// Note what a Workloads, Problems, Helm or Timeline page was built for.
    pub(super) fn remember_scope(&mut self, page: EntityId) {
        let scope = self.page_scope();
        self.page_scopes.insert(page, scope);
    }

    /// Whether a page was built for the contexts and namespaces shown now.
    pub(super) fn scope_matches(&self, page: EntityId) -> bool {
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
            // All of them: unfiltered. The same kind's list keeps its watch, so its box is
            // cleared here; another kind's opens with no filter.
            if self.kind.as_ref().map(Kind::key) == Some(kind.key()) {
                self.put_filter(String::new(), window, cx);
            }
            self.kind_filters.forget(&kind);
            self.select_kind(kind, cx);
            return;
        }
        let namespace = if kind.namespaced { namespace } else { None };
        let obj = ObjectRef { kind, name, namespace, uid: String::new() };
        self.show_detail_in(ctx, obj, window, cx);
    }

    pub(super) fn show_detail_in(&mut self, ctx: usize, obj: ObjectRef, window: &mut Window, cx: &mut Context<Self>) {
        let Some((context, client)) = self.clusters.get(ctx).cloned() else { return };
        self.show_detail_with(context, client, obj, window, cx);
    }

    /// Open an object on a context, connected or not (an Argo CD Application on Argo CD's
    /// own cluster): a connected one's page uses its client like any other.
    fn show_detail_on(
        &mut self,
        context: String,
        client: kube::Client,
        obj: ObjectRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.clusters.iter().position(|(n, _)| *n == context) {
            Some(ctx) => self.show_detail_in(ctx, obj, window, cx),
            None => self.show_detail_with(context, client, obj, window, cx),
        }
    }

    fn show_detail_with(
        &mut self,
        context: String,
        client: kube::Client,
        obj: ObjectRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let risky = crate::settings::is_protected(&context);
        let forwards = self.forwards.clone();
        let favorites = self.favorites.clone();
        let clusters = self.clusters.clone();
        let context_name = context.clone();
        let (own_context, own_client) = (context.clone(), client.clone());
        let panel = cx.new(|cx| {
            let mut p = DetailPanel::new(client, context, risky, forwards, favorites, obj, window, cx);
            p.clusters = clusters;
            p
        });
        cx.subscribe_in(&panel, window, |this, _, ev: &crate::plugins::RunPlugin, window, cx| {
            this.run_plugin(ev, window, cx);
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
            // Beside the page when the assistant is there, else on its own page.
            if this.docked_assistant().is_none() {
                this.open_assistant(window, cx);
            }
            if let Some(page) = this.assistant_page() {
                page.update(cx, |a, cx| a.start_over(q, cx));
            }
        })
        .detach();
        // Released with the page.
        cx.subscribe_in(&panel, window, move |this, _, ev: &OpenObject, window, cx| {
            let on = ev.context.as_deref().unwrap_or(&own_context);
            if let Some(target) = this.clusters.iter().position(|(n, _)| n == on) {
                this.open_object_in(target, &ev.kind, ev.name.clone(), ev.namespace.clone(), window, cx);
                return;
            }
            // A page on a context that is not connected: its links open on it, with its client
            // and the kinds every cluster has.
            let Some(kind) = k8s::find_kind(&this.kinds, &ev.kind).cloned().filter(|_| on == own_context) else {
                return;
            };
            let namespace = if kind.namespaced { ev.namespace.clone() } else { None };
            let obj = ObjectRef { kind, name: ev.name.clone(), namespace, uid: String::new() };
            this.show_detail_with(own_context.clone(), own_client.clone(), obj, window, cx);
        })
        .detach();
        cx.subscribe_in(&panel, window, |this, _, ev: &crate::details::OpenOnContext, window, cx| {
            this.show_detail_on(ev.context.clone(), ev.client.clone(), ev.obj.clone(), window, cx);
        })
        .detach();
        cx.subscribe_in(&panel, window, |this, _, ev: &crate::details::OpenNamespace, window, cx| {
            this.open_namespace_on(ev.context.clone(), ev.namespace.clone(), window, cx);
        })
        .detach();
        self.navigate(Route::Detail(panel), cx);
    }

    /// Open a namespace on a context: on a connected one like any link, else after
    /// connecting to it (an Application's destination is often not connected).
    fn open_namespace_on(&mut self, context: String, namespace: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ctx) = self.clusters.iter().position(|(n, _)| *n == context) {
            self.namespace_open.cancel();
            self.open_object_in(ctx, "namespaces", namespace, None, window, cx);
            return;
        }
        let kind = k8s::find_kind(&self.kinds, "namespaces").cloned().unwrap_or_else(|| Kind {
            ar: k8s::ar_of::<k8s_openapi::api::core::v1::Namespace>(),
            namespaced: false,
            section: "Cluster",
            label: "Namespaces".into(),
        });
        let obj = ObjectRef { kind, name: namespace, namespace: None, uid: String::new() };
        window.push_notification(Notification::info(format!("Connecting to {context}…")), cx);
        // A later click replaces this one: its page does not open over the newer one.
        let connect = k8s::connect(context.clone());
        self.namespace_open.spawn_in(window, cx, connect, move |this, client, window, cx| match client {
            Ok(client) => this.show_detail_with(context, client, obj, window, cx),
            Err(e) => window.push_notification(
                Notification::error(format!("Could not connect to {context}: {}", k8s::short_error(&e))),
                cx,
            ),
        });
    }

    /// Open an Application (on Argo CD's cluster) or where it deploys, from the Argo CD page
    /// (also after F5).
    pub(super) fn subscribe_argo(
        &mut self,
        page: &Entity<crate::ui::argo::ArgoView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Released with the page.
        cx.subscribe_in(page, window, |this, _, ev: &crate::details::OpenOnContext, window, cx| {
            this.show_detail_on(ev.context.clone(), ev.client.clone(), ev.obj.clone(), window, cx);
        })
        .detach();
        cx.subscribe_in(page, window, |this, _, ev: &crate::details::OpenNamespace, window, cx| {
            this.open_namespace_on(ev.context.clone(), ev.namespace.clone(), window, cx);
        })
        .detach();
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
        self.assistant.clone()
    }

    /// The window's assistant chat, made on first use and kept across pages and tabs.
    fn assistant_chat(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<crate::ui::assistant_view::AssistantView> {
        let scope = self.assistant_scope(cx);
        if let Some(p) = &self.assistant {
            p.update(cx, |p, cx| p.set_scope(scope, cx));
            return p.clone();
        }
        let hub = self.hub.clone();
        let page = cx.new(|cx| crate::ui::assistant_view::AssistantView::new(hub, scope, window, cx));
        cx.subscribe_in(&page, window, |this, _, ev: &crate::ui::assistant_view::DockEvent, window, cx| match ev {
            crate::ui::assistant_view::DockEvent::Close => {
                this.assistant_docked = false;
                cx.notify();
            }
            crate::ui::assistant_view::DockEvent::Expand => this.open_assistant(window, cx),
        })
        .detach();
        self.assistant = Some(page.clone());
        page
    }

    /// One assistant chat per window, kept across navigation.
    pub(super) fn open_assistant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = self.assistant_chat(window, cx);
        // On its page, it is not beside it too.
        self.assistant_docked = false;
        page.update(cx, |p, cx| p.set_docked(false, cx));
        self.navigate(Route::Assistant(page), cx);
    }

    /// Show or hide the assistant beside the page (Ctrl+J). On the assistant's own page it
    /// opens beside the page before it.
    pub(super) fn toggle_assistant(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = self.assistant_chat(window, cx);
        if matches!(self.route(), Route::Assistant(_)) {
            self.go(-1, cx);
            self.assistant_docked = !matches!(self.route(), Route::Assistant(_));
        } else {
            self.assistant_docked = !self.assistant_docked;
        }
        page.update(cx, |p, cx| p.set_docked(self.assistant_docked, cx));
        if self.assistant_docked {
            page.update(cx, |p, cx| p.focus_input(window, cx));
        }
        cx.notify();
    }

    /// The assistant beside the page, when it is open and the page is not its own.
    pub(super) fn docked_assistant(&self) -> Option<Entity<crate::ui::assistant_view::AssistantView>> {
        let own_page = matches!(self.route(), Route::Assistant(_));
        self.assistant.clone().filter(|_| self.assistant_docked && !own_page)
    }

    /// What the assistant is told the user is looking at: the clusters and namespaces, and
    /// the page on screen (an object, a list and its filter, Problems...).
    pub(super) fn assistant_scope(&self, cx: &App) -> String {
        let (contexts, namespaces) = self.page_scope();
        let base = scope_text(&contexts, &namespaces);
        match self.on_screen(cx).text() {
            Some(page) => format!("{base}; on screen: {page}"),
            None => base,
        }
    }

    fn on_screen(&self, cx: &App) -> OnScreen {
        let context_of = |i: usize| self.clusters.get(i).map_or_default(|(c, _)| c.clone());
        match self.route() {
            Route::Detail(d) => {
                let (context, obj) = d.read(cx).subject();
                OnScreen::Object { kind: obj.kind.ar.kind, namespace: obj.namespace, name: obj.name, context }
            }
            Route::Resources(Some(kind)) => {
                let d = self.table.read(cx).delegate();
                let selected = d.selected.as_ref().and_then(|(ctx, uid)| {
                    let row = d.find_row(*ctx, uid)?;
                    Some((context_of(*ctx), row.namespace.clone(), row.name.clone()))
                });
                OnScreen::List {
                    kind: kind.ar.kind.clone(),
                    filter: self.filter.read(cx).value().trim().to_string(),
                    selected,
                }
            }
            Route::HelmRelease(h) => {
                let (context, namespace, name) = h.read(cx).subject();
                OnScreen::Object { kind: "Helm release".into(), namespace: Some(namespace), name, context }
            }
            Route::Problems(_) => OnScreen::Page("the Problems page (what is broken)"),
            Route::Timeline(_) => OnScreen::Page("the Timeline page (recent changes and events)"),
            Route::Workloads(_) => OnScreen::Page("the Workloads overview"),
            Route::Cluster(_) => OnScreen::Page("the Cluster overview"),
            Route::Helm(_) => OnScreen::Page("the list of Helm releases"),
            Route::Argo(_) => OnScreen::Page("the Argo CD Applications page"),
            _ => OnScreen::Nothing,
        }
    }

    /// Tell the assistant chat about a new namespace selection or page.
    pub(super) fn update_assistant_scope(&self, cx: &mut Context<Self>) {
        let Some(page) = self.assistant.clone() else { return };
        let scope = self.assistant_scope(cx);
        page.update(cx, |p, cx| p.set_scope(scope, cx));
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
        let term = if let Some(t) = self.terminals.get(&context) {
            t.clone()
        } else {
            let t = cx.new(|cx| crate::ui::terminal::TerminalView::local(client, context.clone(), ns, window, cx));
            self.terminals.insert(context, t.clone());
            t
        };
        term.update(cx, |t, cx| t.focus(window, cx));
        self.navigate(Route::Terminal(term), cx);
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
            this.open_helm_install(window, cx);
        })
        .detach();
    }
}

/// The page on screen, as the assistant is told about it.
#[derive(Debug, PartialEq)]
enum OnScreen {
    Object {
        kind: String,
        namespace: Option<String>,
        name: String,
        context: String,
    },
    /// A list: its kind, its filter, and the row selected (context, namespace, name).
    List {
        kind: String,
        filter: String,
        selected: Option<(String, Option<String>, String)>,
    },
    Page(&'static str),
    Nothing,
}

impl OnScreen {
    fn text(&self) -> Option<String> {
        let place = |ns: &Option<String>, name: &str| match ns {
            Some(ns) => format!("{ns}/{name}"),
            None => name.to_string(),
        };
        Some(match self {
            Self::Object { kind, namespace, name, context } => {
                format!("the page of {kind} {} on context {context}", place(namespace, name))
            }
            Self::List { kind, filter, selected } => {
                let mut s = format!("the list of {kind} objects");
                if !filter.is_empty() {
                    s.push_str(&format!(" filtered by `{filter}`"));
                }
                if let Some((context, ns, name)) = selected {
                    s.push_str(&format!(", row selected: {kind} {} on context {context}", place(ns, name)));
                }
                s
            }
            Self::Page(p) => (*p).to_string(),
            Self::Nothing => return None,
        })
    }
}

/// What the assistant is told about `contexts` and the namespace picked.
fn scope_text(contexts: &[String], namespaces: &[String]) -> String {
    if contexts.is_empty() {
        return "no cluster open in Recouvrance (every kubeconfig context is available: start with list_contexts)"
            .to_string();
    }
    let namespace = match namespaces {
        [] => String::new(),
        [one] => format!(", namespace {one}"),
        several => format!(", namespaces {}", several.join(", ")),
    };
    format!("contexts {}{namespace}", contexts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::{OnScreen, scope_text};

    #[test]
    fn the_assistant_is_told_what_is_on_screen() {
        let pod = OnScreen::Object {
            kind: "Pod".into(),
            namespace: Some("monitoring".into()),
            name: "prom-0".into(),
            context: "prod".into(),
        };
        assert_eq!(pod.text().unwrap(), "the page of Pod monitoring/prom-0 on context prod");
        let node = OnScreen::Object { kind: "Node".into(), namespace: None, name: "n1".into(), context: "prod".into() };
        assert_eq!(node.text().unwrap(), "the page of Node n1 on context prod");
        let list = OnScreen::List {
            kind: "Deployment".into(),
            filter: "web".into(),
            selected: Some(("prod".into(), Some("shop".into()), "web".into())),
        };
        assert_eq!(
            list.text().unwrap(),
            "the list of Deployment objects filtered by `web`, row selected: Deployment shop/web on context prod"
        );
        let plain = OnScreen::List { kind: "Pod".into(), filter: String::new(), selected: None };
        assert_eq!(plain.text().unwrap(), "the list of Pod objects");
        assert_eq!(OnScreen::Nothing.text(), None);
    }

    #[test]
    fn the_assistant_scope_names_the_namespace_picked_now() {
        let contexts = ["a".to_string(), "b".to_string()];
        let ns = |l: &[&str]| l.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(scope_text(&contexts, &ns(&["y"])), "contexts a, b, namespace y");
        assert_eq!(scope_text(&contexts, &ns(&["x", "y"])), "contexts a, b, namespaces x, y");
        assert_eq!(scope_text(&contexts, &[]), "contexts a, b");
        assert!(scope_text(&[], &ns(&["y"])).starts_with("no cluster open"));
    }
}
