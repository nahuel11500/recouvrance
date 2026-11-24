//! Each context connects on its own. A cluster that is slow, can't be reached or waits on
//! a browser login holds back only its own rows: the first rows come once one cluster has
//! answered and the others had `LATE_AFTER` more, and a cluster that answers later joins
//! the list then. The Pods list, the one shown first after most connects, starts as soon as
//! each client is built, alongside discovery and the namespace list.

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use futures::stream::FuturesUnordered;
use gpui_kit::*;
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::discovery::ApiResource;

use super::contexts::{add_kinds, cluster_namespace, listed_namespaces};
use super::overviews::Overview;
use super::{AppView, dev_var};
use crate::k8s::{self, Kind, TableMsg};
use crate::settings::{InitialView, SavedPage, SavedTab};

/// How long the first rows wait for the other clusters once one has answered. Clusters that
/// answer within it share the first list; one that answers later joins it, at the cost of
/// listing the others again (their list's clusters changed).
pub(super) const LATE_AFTER: Duration = Duration::from_secs(2);

/// A table watch started before its list is on screen: its guard and what it sends.
pub(super) type StartedList = (k8s::TaskGuard, UnboundedReceiver<TableMsg>);

/// A cluster's connect, discovery and namespaces.
pub(super) struct Reached {
    pub ctx: String,
    pub client: Client,
    pub kinds: Result<Vec<Kind>>,
    /// The namespaces listed, or the accessible ones set for the cluster; what failed else.
    pub namespaces: Result<Vec<String>, String>,
    /// The Pods list started with the connect (`EarlyList`).
    pub early: Option<StartedList>,
}

/// Whether a cluster answered: its discovery did. One that didn't can't show rows.
pub(super) fn answered(reached: &Result<Reached, String>) -> bool {
    reached.as_ref().is_ok_and(|r| r.kinds.is_ok())
}

/// Connect to `ctx`, then read its discovery and its namespaces at once (neither needs the
/// other). With `early`, its Pods list starts as soon as the client is built.
pub(super) async fn reach(ctx: String, early: Option<EarlyList>) -> Result<Reached, String> {
    let client = k8s::connect(ctx.clone()).await.map_err(|e| format!("{ctx}: {}", k8s::error_chain(&e)))?;
    // Accessible namespaces set for this cluster replace the listing.
    let accessible = crate::settings::cluster(&ctx).namespaces;
    // A cluster with them watches one of its namespaces, which the connect decides: not
    // started here.
    let early = early.filter(|_| accessible.is_empty()).map(|e| e.start(&ctx, &client));
    let namespaces = {
        let (name, c) = (ctx.clone(), client.clone());
        async move {
            // Namespace listing may be forbidden; that is not fatal.
            if accessible.is_empty() { listed_namespaces(&name, k8s::namespaces(c).await) } else { Ok(accessible) }
        }
    };
    let (kinds, namespaces) = futures::join!(k8s::discover(client.clone()), namespaces);
    Ok(Reached { ctx, client, kinds, namespaces, early })
}

/// The connects done once one of them `answered` and the others had until `late` ends (or
/// all of them are done, whichever comes first); the others stay in `pending`. Those that
/// fail before one answers are among them, so nothing is shown until something can be.
pub(super) async fn first_wave<F, T, L>(
    pending: &mut FuturesUnordered<F>,
    answered: impl Fn(&T) -> bool,
    late: impl FnOnce() -> L,
) -> Vec<T>
where
    F: Future<Output = T>,
    L: Future<Output = ()>,
{
    let mut done = Vec::new();
    while let Some(r) = pending.next().await {
        let ok = answered(&r);
        done.push(r);
        if ok {
            break;
        }
    }
    if pending.is_empty() {
        return done;
    }
    let late = late();
    futures::pin_mut!(late);
    loop {
        match futures::future::select(pending.next(), late.as_mut()).await {
            futures::future::Either::Left((Some(r), _)) => done.push(r),
            futures::future::Either::Left((None, _)) | futures::future::Either::Right(_) => return done,
        }
    }
}

/// What the bar above the list says about the connect: each cluster that failed, then each
/// one still connecting.
pub(super) fn connect_status(errors: &[String], waiting: &[String]) -> Option<SharedString> {
    let parts: Vec<String> =
        errors.iter().cloned().chain(waiting.iter().map(|c| format!("{c}: still connecting…"))).collect();
    (!parts.is_empty()).then(|| parts.join("; ").into())
}

/// The Pods list in every namespace, started on each cluster as soon as its client is built:
/// its rows then don't wait for discovery and the namespace list. The list's watch takes it
/// over when it shows the same list (`EarlyLists::take`); dropped else.
#[derive(Clone)]
pub(super) struct EarlyList {
    kind: Kind,
    extras: k8s::Extras,
    pause: k8s::Pause,
    /// Listed page by page, as a list with no rows from before on screen is.
    pages: bool,
}

impl EarlyList {
    fn start(&self, ctx: &str, client: &Client) -> StartedList {
        crate::timeline::open(ctx);
        k8s::watch_table_paused(
            client.clone(),
            self.kind.clone(),
            None,
            self.extras.clone(),
            self.pause.clone(),
            self.pages,
            crate::timeline::tap(ctx, &self.kind, None),
        )
    }
}

/// The early lists handed to the list's watch, by context.
pub(super) struct EarlyLists {
    pub of: EarlyList,
    pub lists: HashMap<String, StartedList>,
}

impl EarlyLists {
    /// Whether they are the list about to start: the same kind and columns. How they list
    /// their first rows (`pages`) only changes how those show: a list judged otherwise
    /// takes them over all the same rather than list every cluster again. Their pause is
    /// then the list's, so they pause and go on with it.
    pub(super) fn matches(&self, kind: &Kind, extras: &[(String, String)]) -> bool {
        self.of.kind.key() == kind.key() && self.of.extras.as_slice() == extras
    }

    pub(super) fn pause(&self) -> k8s::Pause {
        self.of.pause.clone()
    }

    /// The list started on `ctx`, when the list watches every namespace there.
    pub(super) fn take(&mut self, ctx: &str, ns: Option<&str>) -> Option<StartedList> {
        if ns.is_some() { None } else { self.lists.remove(ctx) }
    }
}

/// Whether the list a connect shows first is the Pods of every namespace: the tab the
/// session reopens (`restoring`), else the kind and namespaces shown now and the first page
/// set in Settings. Only then may the connect start it early; another list would waste it.
pub(super) fn pods_first(
    restoring: Option<&SavedTab>,
    kind: Option<&str>,
    namespaces: &[String],
    initial: InitialView,
) -> bool {
    let pods = |k: Option<&str>| k.is_none_or(|k| k == "pods");
    match restoring {
        // A tab saved without a kind keeps the one shown now.
        Some(tab) => tab.page == SavedPage::List && pods(tab.kind.as_deref().or(kind)) && tab.namespaces.is_empty(),
        None => pods(kind) && namespaces.is_empty() && initial == InitialView::Pods,
    }
}

impl AppView {
    /// The Pods list to start with the connect to `contexts`, when it is the list shown first.
    pub(super) fn early_list(&self, contexts: &[String]) -> Option<EarlyList> {
        if ["RECOUVRANCE_KIND", "RECOUVRANCE_NS", "RECOUVRANCE_PAGE"].iter().any(|v| dev_var(v).is_some()) {
            return None;
        }
        // The connect reopens the set's session (see `session::Restore::finished`).
        let saved = if self.restore == super::session::Restore::Pending(self.session_key.clone()) {
            crate::settings::session(&self.session_key)
        } else {
            None
        };
        let tab = saved.as_ref().and_then(|s| s.tabs.get(s.active_tab.min(s.tabs.len().saturating_sub(1))));
        let current = self.kind.as_ref().map(Kind::kubectl_name);
        if !pods_first(tab, current.as_deref(), &self.namespaces, crate::settings::get().initial_view) {
            return None;
        }
        let kind = k8s::kind_of(ApiResource::erase::<Pod>(&()), true);
        let custom = self.column_prefs.custom(&kind.kubectl_name());
        // Shown again from before (see `switch_watch`): the list then comes whole, not by pages.
        let watched: Vec<Option<Option<String>>> =
            contexts.iter().map(|c| cluster_namespace(&[], &crate::settings::cluster(c).namespaces)).collect();
        let key = super::watch::list_key(contexts, &kind, &watched, &custom);
        let pages = self.views.get(&key).is_none_or(|v| v.rows.len() != contexts.len());
        Some(EarlyList { kind, extras: std::sync::Arc::new(custom), pause: k8s::Pause::default(), pages })
    }

    /// A cluster that answered after the first rows joins: its kinds, namespaces, timeline
    /// watches and its rows in the list. The others' watches are listed again (the list's
    /// clusters changed), the Problems badge counts it too, and the page on screen shows it.
    /// Its early list (`early_of`) is taken over when the list shown is still the Pods.
    pub(super) fn join_late(
        &mut self,
        reached: Reached,
        early_of: Option<EarlyList>,
        errors: &mut Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Reached { ctx, client, kinds, namespaces, early } = reached;
        let Ok(found) = kinds else { return };
        let ix = self.clusters.len();
        add_kinds(&mut self.kinds, &mut self.kind_clusters, ix, found);
        k8s::sort_for_sidebar(&mut self.kinds);
        let kinds = self.kinds.clone();
        self.settings_view.update(cx, |s, _| s.set_kinds(kinds));
        let listed = match namespaces {
            Ok(names) => names,
            Err(e) => {
                errors.push(e);
                Vec::new()
            }
        };
        // The namespaces offered grow; the ones picked stay.
        let mut all = self.ns_picker.read(cx).known().to_vec();
        all.extend(listed);
        all.sort();
        all.dedup();
        let joined = ctx.clone();
        self.ns_picker.update(cx, |p, cx| {
            p.contexts.push(joined);
            p.set_namespaces(all, cx);
        });
        k8s::pod_copy::clean_up(&ctx, client.clone(), crate::settings::cluster(&ctx).namespaces);
        self.clusters.push((ctx.clone(), client.clone()));
        // Its own timeline watches: the others' go on.
        crate::timeline::open(&ctx);
        let own: Vec<Kind> = self.served_kinds(ix).cloned().collect();
        let own_where: HashMap<String, Vec<usize>> = own.iter().map(|k| (k.key(), vec![0])).collect();
        self.recording.extend(crate::timeline::start_watches(&[(ctx.clone(), client)], &own, &own_where));
        // The session's tabs were reopened without what only this cluster has.
        self.joined_late = true;
        if let (Some(of), Some(list)) = (early_of, early) {
            self.early = Some(EarlyLists { of, lists: HashMap::from([(ctx, list)]) });
        }
        self.restart_watch(cx);
        // The lists left, the one just left among them, watch the clusters before this one
        // joined: no list shows them again. Dropped now rather than kept streaming until the
        // sweep, and the one just left shares its pause with the list started early.
        self.kept.clear();
        self.problems.start(self.clusters.clone(), self.cluster_kinds(), cx);
        self.update_assistant_scope(cx);
        // The page on screen: a Workloads, Problems, Helm or Timeline page lists it too, a
        // Cluster or Argo CD overview, built for the clusters before, is built again.
        self.rescope_page(cx);
        if let Some((which @ (Overview::Cluster | Overview::Argo), _)) = Overview::of(self.route()) {
            let route = self.build_overview(which, window, cx);
            self.history[self.pos] = route;
            self.apply_route(cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;

    use futures::stream::FuturesUnordered;

    use super::{connect_status, first_wave, pods_first};
    use crate::settings::{InitialView, SavedPage, SavedTab};

    type Connect = Pin<Box<dyn Future<Output = (usize, bool)>>>;

    fn ready(ix: usize, ok: bool) -> Connect {
        Box::pin(async move { (ix, ok) })
    }

    fn never(ix: usize) -> Connect {
        Box::pin(async move {
            futures::future::pending::<()>().await;
            (ix, true)
        })
    }

    fn wave(connects: Vec<Connect>, late_ends: bool) -> (Vec<usize>, usize) {
        let mut pending: FuturesUnordered<Connect> = connects.into_iter().collect();
        let done = futures::executor::block_on(first_wave(
            &mut pending,
            |(_, ok): &(usize, bool)| *ok,
            || async move {
                if !late_ends {
                    futures::future::pending::<()>().await;
                }
            },
        ));
        let mut ixs: Vec<usize> = done.into_iter().map(|(ix, _)| ix).collect();
        ixs.sort_unstable();
        (ixs, pending.len())
    }

    #[test]
    fn a_cluster_that_never_answers_does_not_hold_back_the_others() {
        // One answered, one failed, one hangs (a dead cluster, a login in the browser).
        let (done, left) = wave(vec![ready(0, true), ready(1, false), never(2)], true);
        assert_eq!((done, left), (vec![0, 1], 1));
    }

    #[test]
    fn clusters_that_all_answer_share_the_first_rows() {
        // The wait never ends: every cluster is in, nothing is left to join late.
        let (done, left) = wave(vec![ready(0, true), ready(1, true), ready(2, false)], false);
        assert_eq!((done, left), (vec![0, 1, 2], 0));
        // One cluster, the default.
        assert_eq!(wave(vec![ready(0, true)], false), (vec![0], 0));
    }

    #[test]
    fn every_cluster_failing_is_one_wave() {
        assert_eq!(wave(vec![ready(0, false), ready(1, false)], false), (vec![0, 1], 0));
    }

    #[test]
    fn the_bar_names_failures_then_clusters_still_connecting() {
        let errors = vec!["a: connection refused".to_string()];
        let waiting = vec!["b".to_string()];
        assert_eq!(connect_status(&errors, &waiting).as_deref(), Some("a: connection refused; b: still connecting…"));
        assert_eq!(connect_status(&[], &[]), None);
        assert_eq!(connect_status(&[], &waiting).as_deref(), Some("b: still connecting…"));
    }

    #[test]
    fn pods_start_early_only_when_they_are_shown_first() {
        let tab = |page, kind: Option<&str>, ns: &[&str]| SavedTab {
            page,
            kind: kind.map(String::from),
            namespaces: ns.iter().map(ToString::to_string).collect(),
            filter: String::new(),
        };
        let none: &[String] = &[];
        // A first launch, or the Pods shown last: yes.
        assert!(pods_first(None, None, none, InitialView::Pods));
        assert!(pods_first(None, Some("pods"), none, InitialView::Pods));
        // Another kind, namespaces picked, another first page: no.
        assert!(!pods_first(None, Some("deployments.apps"), none, InitialView::Pods));
        assert!(!pods_first(None, None, &["team-a".to_string()], InitialView::Pods));
        assert!(!pods_first(None, None, none, InitialView::Workloads));
        // A session reopened: what its tab shows decides, not what is shown now.
        assert!(pods_first(
            Some(&tab(SavedPage::List, Some("pods"), &[])),
            Some("deployments.apps"),
            none,
            InitialView::Workloads
        ));
        assert!(pods_first(Some(&tab(SavedPage::List, None, &[])), None, none, InitialView::Pods));
        assert!(!pods_first(Some(&tab(SavedPage::List, None, &[])), Some("secrets"), none, InitialView::Pods));
        assert!(!pods_first(Some(&tab(SavedPage::List, Some("pods"), &["team-a"])), None, none, InitialView::Pods));
        assert!(!pods_first(Some(&tab(SavedPage::Workloads, Some("pods"), &[])), None, none, InitialView::Pods));
    }
}
