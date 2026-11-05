//! The pages built from the connected clusters (and, for most, the namespaces picked):
//! Cluster and Workloads overviews, Problems, Timeline, Helm releases and Argo CD. One
//! opener and one builder for all of them, used by the sidebar, the palette and F5 alike.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::{AppView, Route};
use crate::details::OpenObject;
use crate::ui::argo::ArgoView;
use crate::ui::cluster::ClusterOverview;
use crate::ui::dashboard::Dashboard;
use crate::ui::helm::HelmList;
use crate::ui::problems_view::ProblemsView;
use crate::ui::timeline_view::TimelineView;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Overview {
    Cluster,
    Workloads,
    Problems,
    Timeline,
    Helm,
    Argo,
}

impl Overview {
    /// The overview a route shows, with its page.
    fn of(route: &Route) -> Option<(Self, EntityId)> {
        Some(match route {
            Route::Cluster(p) => (Self::Cluster, p.entity_id()),
            Route::Workloads(p) => (Self::Workloads, p.entity_id()),
            Route::Problems(p) => (Self::Problems, p.entity_id()),
            Route::Timeline(p) => (Self::Timeline, p.entity_id()),
            Route::Helm(p) => (Self::Helm, p.entity_id()),
            Route::Argo(p) => (Self::Argo, p.entity_id()),
            _ => return None,
        })
    }

    /// Whether the page lists the namespaces picked: it is built again for others.
    fn scoped(self) -> bool {
        matches!(self, Self::Workloads | Self::Problems | Self::Timeline | Self::Helm)
    }

    /// Whether opening it again shows the one in history: the others poll, and one of each
    /// is enough. Helm's list is cheap and always read anew.
    fn reused(self) -> bool {
        self != Self::Helm
    }
}

impl AppView {
    pub(super) fn open_cluster(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_overview(Overview::Cluster, window, cx);
    }

    pub(super) fn open_workloads(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_overview(Overview::Workloads, window, cx);
    }

    pub(super) fn open_problems(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_overview(Overview::Problems, window, cx);
    }

    pub(super) fn open_timeline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_overview(Overview::Timeline, window, cx);
    }

    pub(super) fn open_helm(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_overview(Overview::Helm, window, cx);
    }

    pub(super) fn open_argo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_overview(Overview::Argo, window, cx);
    }

    /// Show an overview: the one in history when it was built for what is picked now, else
    /// a new one. Not connected: the cluster switcher opens instead.
    fn open_overview(&mut self, which: Overview, window: &mut Window, cx: &mut Context<Self>) {
        if self.clusters.is_empty() {
            self.ctx_open = true;
            cx.notify();
            return;
        }
        let existing = which.reused().then(|| {
            self.history.iter().find(|r| {
                Overview::of(r).is_some_and(|(w, id)| w == which && (!which.scoped() || self.scope_matches(id)))
            })
        });
        let route = match existing.flatten() {
            Some(route) => route.clone(),
            None => self.build_overview(which, window, cx),
        };
        self.navigate(route, cx);
    }

    /// A new overview page for what is picked now, its links wired to the window.
    fn build_overview(&mut self, which: Overview, window: &mut Window, cx: &mut Context<Self>) -> Route {
        let (clusters, scope, namespaces) = (self.clusters.clone(), self.namespace_scope(), self.namespaces.clone());
        let route = match which {
            Overview::Cluster => {
                let page = cx.new(|cx| ClusterOverview::new(clusters, cx));
                self.follow_links(&page, window, cx);
                Route::Cluster(page)
            }
            Overview::Workloads => {
                let page = cx.new(|cx| Dashboard::new(scope, cx));
                self.follow_links(&page, window, cx);
                Route::Workloads(page)
            }
            Overview::Problems => {
                let kinds = self.cluster_kinds();
                let page = cx.new(|cx| ProblemsView::new(scope, kinds, namespaces, window, cx));
                self.follow_links(&page, window, cx);
                Route::Problems(page)
            }
            Overview::Timeline => {
                let page = cx.new(|cx| TimelineView::new(scope, namespaces, window, cx));
                self.follow_links(&page, window, cx);
                Route::Timeline(page)
            }
            Overview::Helm => {
                let page = cx.new(|cx| HelmList::new(scope, window, cx));
                self.subscribe_helm(&page, window, cx);
                Route::Helm(page)
            }
            Overview::Argo => {
                let page = cx.new(|cx| ArgoView::new(clusters, window, cx));
                self.subscribe_argo(&page, window, cx);
                Route::Argo(page)
            }
        };
        if which.scoped()
            && let Some((_, id)) = Overview::of(&route)
        {
            self.remember_scope(id);
        }
        route
    }

    /// Open the objects a page links to (released with the page). A link without a context
    /// opens on the primary cluster; one to a context not connected opens nothing rather
    /// than the same name on another cluster.
    fn follow_links<V: EventEmitter<OpenObject>>(
        &mut self,
        page: &Entity<V>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.subscribe_in(page, window, |this, _, ev: &OpenObject, window, cx| {
            let names = this.clusters.iter().map(|(n, _)| n.as_str());
            if let Some(ctx) = link_target(names, ev.context.as_deref()) {
                this.open_object_in(ctx, &ev.kind, ev.name.clone(), ev.namespace.clone(), window, cx);
            }
        })
        .detach();
    }

    /// Reload what is shown now (F5): the list's watch, Problems and the Timeline in place
    /// (their filters stay), the other overviews built anew in the same place in history.
    pub(super) fn refresh_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.route().clone() {
            Route::Resources(_) => {
                self.restart_watch(cx);
                return;
            }
            Route::Problems(p) => p.update(cx, ProblemsView::refresh),
            Route::Timeline(p) => p.update(cx, TimelineView::refresh),
            route => {
                let Some((which, _)) = Overview::of(&route) else { return };
                let fresh = self.build_overview(which, window, cx);
                if let Some(slot) = self.history.get_mut(self.pos) {
                    *slot = fresh;
                }
                self.apply_route(cx);
            }
        }
        window.push_notification(Notification::info("Refreshed"), cx);
    }
}

/// The connected cluster a link opens on, by index: its context's, the primary one for a
/// link without a context, none for a context not connected.
fn link_target<'a>(mut connected: impl Iterator<Item = &'a str>, context: Option<&str>) -> Option<usize> {
    match context {
        None => Some(0),
        Some(c) => connected.position(|n| n == c),
    }
}

#[cfg(test)]
mod tests {
    use super::link_target;

    #[test]
    fn links_open_on_their_own_cluster_or_nowhere() {
        let connected = ["prod", "staging"];
        assert_eq!(link_target(connected.into_iter(), Some("staging")), Some(1));
        assert_eq!(link_target(connected.into_iter(), None), Some(0));
        // Never the same name on another cluster.
        assert_eq!(link_target(connected.into_iter(), Some("dev")), None);
    }
}
