//! Favorites from the window's side: opening one where it points (connecting its context
//! first when it isn't), saying when its target is gone, and starring what is shown.

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
    /// Its context isn't connected: opening it connects it first (an Application opens on
    /// Argo CD's cluster without connecting it).
    Elsewhere,
    /// Its context is no longer in the kubeconfig.
    ContextGone,
    /// A namespace its connected context doesn't have.
    NamespaceGone,
}

impl Standing {
    /// What the sidebar adds to the favorite's line, if anything.
    pub(super) fn note(self) -> Option<&'static str> {
        match self {
            Self::Here => None,
            Self::Elsewhere => Some("not connected"),
            Self::ContextGone => Some("context gone from the kubeconfig"),
            Self::NamespaceGone => Some("namespace not found"),
        }
    }
}

/// Where `favorite` stands, given the contexts of the kubeconfig (`all`), those connected and
/// the namespaces they have (`namespaces`, listed or added by hand).
pub(super) fn standing(favorite: &Favorite, all: &[String], connected: &[String], namespaces: &[String]) -> Standing {
    let context = favorite.context();
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

impl AppView {
    fn connected_names(&self) -> Vec<String> {
        self.clusters.iter().map(|(c, _)| c.clone()).collect()
    }

    /// Where `favorite` stands now (see `standing`).
    pub(super) fn favorite_standing(&self, favorite: &Favorite, cx: &App) -> Standing {
        let known = self.ns_picker.read(cx).known().to_vec();
        standing(favorite, &self.all_contexts, &self.connected_names(), &known)
    }

    /// Open a favorite where it points: a context connects alone, a namespace is picked, an
    /// object or Application opens its page. On a context that isn't connected, `connect`
    /// connects it first and opens it then (an Application opens on Argo CD's cluster without
    /// replacing the contexts connected). A target that is gone is said, never silently skipped.
    pub(super) fn open_favorite(
        &mut self,
        favorite: Favorite,
        connect: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let context = favorite.context().to_string();
        let title = favorite.title();
        let warn = |text: String, window: &mut Window, cx: &mut Context<Self>| {
            window.push_notification(Notification::warning(text), cx);
        };
        match self.favorite_standing(&favorite, cx) {
            Standing::ContextGone => {
                return warn(format!("{title}: the context {context} is no longer in the kubeconfig"), window, cx);
            }
            Standing::NamespaceGone => {
                return warn(format!("The namespace {title} is not on {context} any more"), window, cx);
            }
            Standing::Elsewhere | Standing::Here => {}
        }
        let at = self.clusters.iter().position(|(c, _)| *c == context);
        match favorite {
            Favorite::Context(c) => {
                if self.connected_names() != [c.as_str()] {
                    self.picked = std::collections::BTreeSet::from([c.clone()]);
                    self.connect_many(vec![c], window, cx);
                }
            }
            Favorite::Application(app) => {
                let obj = ObjectRef {
                    kind: k8s::argo::application_kind(),
                    name: app.name,
                    namespace: Some(app.namespace),
                    uid: String::new(),
                };
                if let Some(ix) = at {
                    self.show_detail_in(ix, obj, window, cx);
                    return;
                }
                // Argo CD's cluster is often not one of those connected: its page opens with a
                // client of its own, as from the Argo CD page.
                window.push_notification(Notification::info(format!("Connecting to {context}…")), cx);
                let on = context.clone();
                let work = async move {
                    k8s::connect(on.clone())
                        .await
                        .map_err(|e| format!("Could not connect to {on}: {}", k8s::short_error(&e)))
                };
                self.context_open.spawn_in(window, cx, work, move |this, client, window, cx| match client {
                    Ok(client) => this.show_detail_on(context, client, obj, window, cx),
                    Err(e) => window.push_notification(Notification::error(e), cx),
                });
            }
            favorite if at.is_none() => {
                if !connect {
                    return warn(format!("{title}: {context} is not connected"), window, cx);
                }
                self.picked = std::collections::BTreeSet::from([context.clone()]);
                self.connect_many(vec![context], window, cx);
                // Set after `connect_many`, which drops one left by an earlier connect.
                self.after_connect = Some(favorite);
            }
            Favorite::Namespace(n) => self.pick_namespace(n.namespace, cx),
            Favorite::Object(o) => {
                let Some(ix) = at else { return };
                if k8s::find_kind(&self.kinds, &o.kind).is_none() {
                    return warn(format!("{title}: {} is not served on {context}", o.kind), window, cx);
                }
                self.open_object_in(ix, &o.kind, o.name, o.namespace, window, cx);
            }
        }
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

    /// Star or unstar what is shown: the object or Application of a detail page, the selected
    /// row of a list, else the one namespace picked (on every context connected).
    pub(super) fn toggle_shown_favorite(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let shown = match self.route() {
            Route::Detail(panel) => Some(panel.read(cx).favorite()),
            Route::Resources(_) => self.selected_row_favorite(cx),
            _ => None,
        };
        if let Some(favorite) = shown {
            let on = !self.favorites.read(cx).has(&favorite);
            let what = favorite.title();
            self.favorites.update(cx, |f, cx| f.toggle(favorite, cx));
            let text = if on { format!("{what} added to favorites") } else { format!("{what} removed from favorites") };
            window.push_notification(Notification::info(text), cx);
            return;
        }
        let Some(ns) = self.namespace.clone() else {
            let text = "Nothing to star here: open an object, select a row or pick one namespace";
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
    use super::{Standing, standing};
    use crate::favorites::{FavApp, FavNamespace, FavObject, Favorite};

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    /// A favorite on a context that isn't connected is opened by connecting it; one whose
    /// context left the kubeconfig, or whose namespace isn't there, says so.
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
        let app = |hub: &str| {
            Favorite::Application(FavApp { argocd: hub.into(), namespace: "argocd".into(), name: "shop".into() })
        };
        assert_eq!(at(&app("hub")), Standing::Elsewhere);
        assert_eq!(at(&app("old-hub")), Standing::ContextGone);
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
        assert!(Standing::ContextGone.note().is_some());
    }

    /// With nothing connected (the app just started), every favorite still in the kubeconfig
    /// opens by connecting its context.
    #[test]
    fn with_nothing_connected_favorites_connect_their_context() {
        let all = names(&["prod"]);
        let f = Favorite::Namespace(FavNamespace { context: "prod".into(), namespace: "web".into() });
        assert_eq!(standing(&f, &all, &[], &[]), Standing::Elsewhere);
    }
}
