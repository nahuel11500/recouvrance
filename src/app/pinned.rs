//! Favorites from the window's side: opening one where it points, saying when its target is
//! gone, and starring what is shown.
//!
//! Opening never replaces the contexts connected: a context favorite connects that context
//! (as picking it does); an object or Application on a context that isn't connected opens its
//! page with a client of its own, as the Argo CD page opens Applications; a namespace of a
//! context that isn't connected adds that context to those connected, then picks it.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::{AppView, Route};
use crate::favorites::{FavNamespace, Favorite};
use crate::k8s::{self, ObjectRef};

/// Where a favorite stands, as the sidebar says it under its name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Standing {
    /// Its context is connected (and a namespace is there).
    Here,
    /// Its context isn't connected: an object or Application opens on it without connecting
    /// it, a namespace adds it to the contexts connected.
    Elsewhere,
    /// An Application known only by its Argo CD API server: it opens on the Argo CD page.
    ArgoApi,
    /// Its context is no longer in the kubeconfig.
    ContextGone,
    /// A namespace its connected contexts don't list (it may be one they can't list).
    NamespaceGone,
}

impl Standing {
    /// What the sidebar adds to the favorite's line, if anything.
    pub(super) fn note(self) -> Option<&'static str> {
        match self {
            Self::Here => None,
            Self::Elsewhere => Some("not connected"),
            Self::ArgoApi => Some("through Argo CD's API"),
            Self::ContextGone => Some("context gone from the kubeconfig"),
            Self::NamespaceGone => Some("namespace not listed"),
        }
    }

    /// Whether its target is gone or may be: the sidebar dims it.
    pub(super) fn gone(self) -> bool {
        matches!(self, Self::ContextGone | Self::NamespaceGone)
    }
}

/// Where `favorite` stands, given the contexts of the kubeconfig (`all`), those connected and
/// the namespaces they list (`namespaces`, listed or added by hand).
pub(super) fn standing(favorite: &Favorite, all: &[String], connected: &[String], namespaces: &[String]) -> Standing {
    let Some(context) = favorite.context() else { return Standing::ArgoApi };
    if !all.iter().any(|c| c == context) {
        return Standing::ContextGone;
    }
    if !connected.iter().any(|c| c == context) {
        return Standing::Elsewhere;
    }
    match favorite {
        Favorite::Namespace(n) if !namespaces.contains(&n.namespace) => Standing::NamespaceGone,
        _ => Standing::Here,
    }
}

/// The contexts connected once `context` is added to `connected` (at the end, once).
pub(super) fn with_context(connected: &[String], context: &str) -> Vec<String> {
    let mut all = connected.to_vec();
    if !all.iter().any(|c| c == context) {
        all.push(context.to_string());
    }
    all
}

impl AppView {
    fn connected_names(&self) -> Vec<String> {
        self.clusters.iter().map(|(c, _)| c.clone()).collect()
    }

    /// Where each of `favorites` stands now (see `standing`), worked out with one read of the
    /// namespaces known.
    pub(super) fn favorite_standings(&self, favorites: &[Favorite], cx: &App) -> Vec<Standing> {
        let connected = self.connected_names();
        let known = self.ns_picker.read(cx).known();
        favorites.iter().map(|f| standing(f, &self.all_contexts, &connected, known)).collect()
    }

    /// Open a favorite where it points (see the module's comment). A target that is gone is
    /// said, never silently skipped. `connect` is false once a connect made for it is done:
    /// it isn't connected again.
    pub(super) fn open_favorite(
        &mut self,
        favorite: Favorite,
        connect: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let title = favorite.title();
        let warn = |text: String, window: &mut Window, cx: &mut Context<Self>| {
            window.push_notification(Notification::warning(text), cx);
        };
        let standing = self.favorite_standings(std::slice::from_ref(&favorite), cx)[0];
        let Some(context) = favorite.context().map(String::from) else {
            // Read through Argo CD's API only: the Argo CD page lists it, its favorites first.
            window.push_notification(
                Notification::info(format!("{title} is read through Argo CD's API: it is on the Argo CD page")),
                cx,
            );
            self.open_argo(window, cx);
            return;
        };
        if standing == Standing::ContextGone {
            return warn(format!("{title}: the context {context} is no longer in the kubeconfig"), window, cx);
        }
        let at = self.clusters.iter().position(|(c, _)| *c == context);
        match favorite {
            Favorite::Context(c) => {
                if self.connected_names() != [c.as_str()] {
                    self.picked = std::collections::BTreeSet::from([c.clone()]);
                    self.connect_many(vec![c], window, cx);
                }
            }
            Favorite::Namespace(n) if at.is_some() => {
                // Not listed may still be there (no right to list namespaces): picked anyway.
                if standing == Standing::NamespaceGone {
                    warn(format!("The namespace {title} is not listed on {context}"), window, cx);
                }
                self.pick_namespace(n.namespace, cx);
            }
            favorite @ Favorite::Namespace(_) => {
                if !connect {
                    return warn(format!("{title}: {context} is not connected"), window, cx);
                }
                // Added to the contexts connected, not in their place.
                let contexts = with_context(&self.connected_names(), &context);
                self.picked = contexts.iter().cloned().collect();
                self.connect_many(contexts, window, cx);
                // Set after `connect_many`, which drops one left by an earlier connect.
                self.after_connect = Some(favorite);
            }
            Favorite::Application(app) => {
                let kind = k8s::argo::application_kind();
                if let Some(ix) = at {
                    let obj = ObjectRef { kind, name: app.name, namespace: Some(app.namespace), uid: String::new() };
                    self.show_detail_in(ix, obj, window, cx);
                } else {
                    self.open_elsewhere(context, Ok(kind), app.name, Some(app.namespace), window, cx);
                }
            }
            Favorite::Object(o) => {
                let Some(ix) = at else {
                    // Its kind as that context serves it: looked up there once connected.
                    return self.open_elsewhere(context, Err(o.kind), o.name, o.namespace, window, cx);
                };
                if k8s::find_kind(&self.kinds, &o.kind).is_none() {
                    return warn(format!("{title}: {} is not served on {context}", o.kind), window, cx);
                }
                self.open_object_in(ix, &o.kind, o.name, o.namespace, window, cx);
            }
        }
    }

    /// Open an object on `context`, which isn't connected, with a client of its own: the
    /// contexts connected stay as they are. Its kind is `Ok` when known, else (`Err`, a kubectl
    /// name) looked up there first. Its page opens only while the page the click was on is
    /// still shown.
    fn open_elsewhere(
        &mut self,
        context: String,
        kind: Result<k8s::Kind, String>,
        name: String,
        namespace: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.push_notification(Notification::info(format!("Connecting to {context}…")), cx);
        let asker = self.route().page();
        let on = context.clone();
        let work = async move {
            let client = k8s::connect(on.clone())
                .await
                .map_err(|e| format!("Could not connect to {on}: {}", k8s::short_error(&e)))?;
            let kind = match kind {
                Ok(kind) => kind,
                Err(wanted) => {
                    let found = k8s::discover_all(client.clone())
                        .await
                        .map_err(|e| format!("Could not list the kinds of {on}: {}", k8s::short_error(&e)))?;
                    k8s::find_kind(&found.kinds, &wanted).cloned().ok_or_else(|| {
                        format!("{wanted} is not served on {on}{}", k8s::maybe_skipped(&found.skipped))
                    })?
                }
            };
            let namespace = if kind.namespaced { namespace } else { None };
            Ok::<_, String>((client, ObjectRef { kind, name, namespace, uid: String::new() }))
        };
        // A later click replaces this one, and a page the user went to meanwhile stays.
        self.context_open.spawn_in(window, cx, work, move |this, opened, window, cx| match opened {
            Ok((client, obj)) if this.route().page() == asker => this.show_detail_on(context, client, obj, window, cx),
            Ok(_) => {}
            Err(e) => window.push_notification(Notification::error(e), cx),
        });
    }

    /// Show only namespace `ns`, as a click in the picker does.
    fn pick_namespace(&mut self, ns: String, cx: &mut Context<Self>) {
        self.ns_picker.update(cx, |p, cx| {
            crate::settings::visit_namespaces(&p.context, std::slice::from_ref(&ns));
            p.selected = [ns.clone()].into();
            cx.notify();
        });
        self.set_selected_namespaces(vec![ns], cx);
    }

    /// Star or unstar what is shown: the object or Application of a detail page; on a list,
    /// its selected row, else the one namespace picked (on every context connected). Other
    /// pages have nothing to star this way (an Application has its star on the Argo CD page).
    pub(super) fn toggle_shown_favorite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (shown, on_list) = match self.route() {
            Route::Detail(panel) => (Some(panel.read(cx).favorite()), false),
            Route::Resources(_) => (self.selected_row_favorite(cx), true),
            _ => (None, false),
        };
        if let Some(favorite) = shown {
            let on = !self.favorites.read(cx).has(&favorite);
            let what = favorite.title();
            self.favorites.update(cx, |f, cx| f.toggle(favorite, cx));
            let text = if on { format!("{what} added to favorites") } else { format!("{what} removed from favorites") };
            window.push_notification(Notification::info(text), cx);
            return;
        }
        let Some(ns) = self.namespace.clone().filter(|_| on_list) else {
            let text = if on_list {
                "Nothing to star here: select a row or pick one namespace"
            } else {
                "Nothing to star here: open an object, or star a row with its star"
            };
            window.push_notification(Notification::info(text), cx);
            return;
        };
        let contexts = self.ns_picker.read(cx).contexts.clone();
        let favorites = self.favorites.read(cx);
        let on = !contexts
            .into_iter()
            .any(|context| favorites.has(&Favorite::Namespace(FavNamespace { context, namespace: ns.clone() })));
        self.ns_picker.update(cx, |p, cx| p.toggle_favorite(&ns, cx));
        let text = if on { format!("{ns} added to favorites") } else { format!("{ns} removed from favorites") };
        window.push_notification(Notification::info(text), cx);
    }

    /// The selected row of the list as a favorite.
    fn selected_row_favorite(&self, cx: &App) -> Option<Favorite> {
        let kind = self.kind.as_ref()?;
        let table = self.table.read(cx);
        let ix = table.selected_row()?;
        let d = table.delegate();
        let (row, ctx) = (d.row(ix)?, d.row_context(ix)?);
        let (context, _) = self.clusters.get(ctx)?;
        Some(Favorite::for_object(context, kind, row.namespace.clone(), row.name.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::{Standing, standing, with_context};
    use crate::favorites::{FavApp, FavNamespace, FavObject, Favorite};

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    /// A favorite on a context that isn't connected opens without replacing those connected;
    /// one whose context left the kubeconfig, or whose namespace isn't listed, says so; an
    /// Application known only by its API server is never taken for a kubeconfig context.
    #[test]
    fn a_favorite_says_where_it_stands() {
        let all = names(&["prod", "staging", "hub"]);
        let connected = names(&["prod"]);
        let namespaces = names(&["web"]);
        let ns = |c: &str, n: &str| Favorite::Namespace(FavNamespace { context: c.into(), namespace: n.into() });
        let at = |f: &Favorite| standing(f, &all, &connected, &namespaces);
        assert_eq!(at(&ns("prod", "web")), Standing::Here);
        assert_eq!(at(&ns("prod", "gone")), Standing::NamespaceGone);
        // Not connected: its namespaces aren't known yet, it isn't said gone.
        assert_eq!(at(&ns("staging", "anything")), Standing::Elsewhere);
        assert_eq!(at(&ns("old", "web")), Standing::ContextGone);
        assert_eq!(at(&Favorite::Context("staging".into())), Standing::Elsewhere);
        assert_eq!(at(&Favorite::Context("prod".into())), Standing::Here);
        assert_eq!(at(&Favorite::Context("old".into())), Standing::ContextGone);
        let app = |argocd: &str, hub: Option<&str>| {
            Favorite::Application(FavApp {
                argocd: argocd.into(),
                namespace: "argocd".into(),
                name: "shop".into(),
                context: hub.map(Into::into),
            })
        };
        assert_eq!(at(&app("context:hub", None)), Standing::Elsewhere);
        assert_eq!(at(&app("https://argocd.example.com", Some("hub"))), Standing::Elsewhere);
        assert_eq!(at(&app("context:old-hub", None)), Standing::ContextGone);
        assert_eq!(at(&app("https://argocd.example.com", None)), Standing::ArgoApi);
        let obj = |c: &str| {
            Favorite::Object(FavObject {
                context: c.into(),
                kind: "deployments.apps".into(),
                namespace: Some("web".into()),
                name: "api".into(),
            })
        };
        assert_eq!(at(&obj("prod")), Standing::Here);
        assert_eq!(at(&obj("staging")), Standing::Elsewhere);
        assert!(Standing::Here.note().is_none());
        assert!(Standing::ContextGone.note().is_some() && Standing::ContextGone.gone());
        assert!(!Standing::ArgoApi.gone());
    }

    /// With nothing connected (the app just started), every favorite still in the kubeconfig
    /// opens by connecting its context.
    #[test]
    fn with_nothing_connected_favorites_connect_their_context() {
        let all = names(&["prod"]);
        let f = Favorite::Namespace(FavNamespace { context: "prod".into(), namespace: "web".into() });
        assert_eq!(standing(&f, &all, &[], &[]), Standing::Elsewhere);
    }

    /// A namespace of another context adds that context to those connected, once, after them.
    #[test]
    fn a_namespace_elsewhere_adds_its_context() {
        assert_eq!(with_context(&names(&["prod", "dev"]), "staging"), names(&["prod", "dev", "staging"]));
        assert_eq!(with_context(&names(&["prod", "dev"]), "dev"), names(&["prod", "dev"]));
        assert_eq!(with_context(&[], "dev"), names(&["dev"]));
    }
}
