//! The logs of the checked rows of a table: pods, or the pods of workloads and Services,
//! across namespaces and clusters, merged in time order in one view with the log controls
//! of an object's page.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::time::Duration;

use futures::StreamExt as _;
use gpui_kit::component::{ActiveTheme as _, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use super::logs::{LogsView, MAX_LOG_PODS, PlacedPod, PodPlace, follow_first};
use crate::k8s::{self, Kind, LogPod};

/// A checked row whose logs are read.
#[derive(Clone)]
pub struct LogTarget {
    pub context: String,
    pub client: Client,
    pub namespace: String,
    pub name: String,
    /// When it was made: past the limit of rows read, the newest pods go first.
    pub created: Option<jiff::Timestamp>,
}

/// Rows of one cluster read at once while finding their pods.
const READS_AT_ONCE: usize = 8;

/// How long reading one row may take. The client waits minutes for a stalled API server;
/// past this, the row is reported and its cluster's other rows go on.
const READ_TIMEOUT: Duration = Duration::from_secs(15);

/// What a row's pods gave: its pods, or what failed.
type RowPods = Result<Vec<LogPod>, String>;

/// Errors shown one by one; past this many, the rest are counted.
const ERRORS_SHOWN: usize = 5;

pub struct LogsPage {
    kind: Kind,
    /// The rows read (at most `MAX_LOG_PODS`), and the checked rows left out past it.
    targets: Vec<LogTarget>,
    rows_left_out: usize,
    logs: Option<Entity<LogsView>>,
    /// Each row's pods as last read (by index in `targets`); None until its cluster answers.
    /// After F5, a cluster's old pods stay until its rows are read again.
    found: Vec<Option<RowPods>>,
    /// The contexts whose rows are being read (when opened, and after F5).
    waiting: Vec<String>,
    /// The pods followed now, by context, namespace, name and containers: a cluster's
    /// answer that changes none of them does not restart the streams.
    followed: Vec<(String, String, String, Vec<String>)>,
    /// What failed, each cluster's own: one cluster's error never hides another's pods.
    errors: Vec<String>,
    find: crate::latest::Latest,
    _visible: Task<()>,
}

impl LogsPage {
    pub fn new(kind: Kind, targets: Vec<LogTarget>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (targets, rows_left_out) = rows_to_read(kind.is("", "Pod"), targets, |t| t.created);
        // Streams run while the page is shown, and stop while it is in the back/forward
        // history; a hidden window keeps them, as an object's page does.
        let visible = cx.spawn(async move |this, cx| {
            loop {
                let exec = cx.background_executor().clone();
                let shown = crate::app::wait_visible_or_away(&this, cx, &exec, |page: &mut Self, cx| page.pause(cx));
                if shown.await.is_none() || this.update(cx, Self::resume).is_err() {
                    break;
                }
                exec.timer(std::time::Duration::from_millis(500)).await;
            }
        });
        let mut this = Self {
            kind,
            targets,
            rows_left_out,
            logs: None,
            found: Vec::new(),
            waiting: Vec::new(),
            followed: Vec::new(),
            errors: Vec::new(),
            find: Default::default(),
            _visible: visible,
        };
        this.find_pods(window, cx);
        this
    }

    /// The page's title: what was checked.
    pub fn title(&self) -> String {
        let n = self.targets.len() + self.rows_left_out;
        format!("Logs of {}", crate::plural::count_as(n, &self.kind.ar.kind, &self.kind.label))
    }

    /// Find the rows' pods again (F5): a rollout replaces them.
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.find_pods(window, cx);
    }

    fn pause(&mut self, cx: &mut Context<Self>) {
        if let Some(l) = &self.logs {
            l.update(cx, |l, _| l.pause());
        }
    }

    fn resume(&mut self, cx: &mut Context<Self>) {
        if let Some(l) = &self.logs {
            l.update(cx, LogsView::resume);
        }
    }

    /// Read each row's pods, each cluster on its own, and show each cluster's as soon as its
    /// rows are read: a cluster that does not answer never holds back another's pods. The
    /// newest search replaces (and stops) a running one.
    fn find_pods(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (pods_rows, kind) = (self.kind.is("", "Pod"), self.kind.clone());
        self.found.resize(self.targets.len(), None);
        self.waiting = contexts_of(&self.targets, |t| t.context.as_str());
        // The first answer of a new search restarts the streams (F5 reconnects them).
        self.followed.clear();
        let rows: Vec<(usize, LogTarget)> = self.targets.iter().cloned().enumerate().collect();
        let exec = cx.background_executor().clone();
        let read = move |(_, t): &(usize, LogTarget)| {
            let (client, kind, ns, name) = (t.client.clone(), kind.clone(), t.namespace.clone(), t.name.clone());
            async move {
                let pods = if pods_rows {
                    k8s::log_pod(client, ns, name).await.map(|p| vec![p])
                } else {
                    k8s::log_pods_of(client, kind, ns, name).await
                };
                pods.map_err(|e| k8s::short_error(&e))
            }
        };
        let parts = find_by_context(rows, |(_, t)| t.context.clone(), read, move || exec.timer(READ_TIMEOUT));
        self.find.spawn_each_in(window, cx, parts, Self::context_read);
        cx.notify();
    }

    /// A cluster's rows are read: keep their pods and show what is known so far.
    fn context_read(&mut self, part: Vec<((usize, LogTarget), RowPods)>, window: &mut Window, cx: &mut Context<Self>) {
        for ((ix, t), pods) in part {
            self.waiting.retain(|c| *c != t.context);
            if let Some(slot) = self.found.get_mut(ix) {
                *slot = Some(pods);
            }
        }
        self.show_pods(window, cx);
    }

    fn show_pods(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let found: Vec<Found<PodPlace>> = self
            .targets
            .iter()
            .zip(&self.found)
            .filter_map(|(t, pods)| {
                let place = PodPlace { context: t.context.clone(), client: t.client.clone(), ns: t.namespace.clone() };
                Some(Found { place, row: format!("{}/{}", t.namespace, t.name), pods: pods.clone()? })
            })
            .collect();
        let Gathered { pods, left_out, errors } = gather(found, |p| (p.context.as_str(), p.ns.as_str()));
        self.errors = errors;
        let pods: Vec<PlacedPod> =
            pods.into_iter().map(|(place, p)| PlacedPod { place, name: p.name, containers: p.containers }).collect();
        let followed: Vec<(String, String, String, Vec<String>)> = pods
            .iter()
            .map(|p| (p.place.context.clone(), p.place.ns.clone(), p.name.clone(), p.containers.clone()))
            .collect();
        if let Some(logs) = self.logs.as_ref().filter(|_| followed == self.followed) {
            logs.update(cx, |l, cx| {
                l.left_out = left_out;
                cx.notify();
            });
            cx.notify();
            return;
        }
        self.followed = followed;
        if pods.is_empty() {
            self.logs = None;
        } else if let Some(logs) = &self.logs {
            logs.update(cx, |l, cx| l.set_places(pods, left_out, window, cx));
        } else if let Some(home) = pods.first().map(|p| p.place.clone()) {
            self.logs = Some(cx.new(|cx| LogsView::of_places(home, pods, left_out, window, cx)));
            if crate::app::is_current(cx.entity_id(), cx) {
                self.resume(cx);
            }
        }
        cx.notify();
    }

    /// Where the rows are: their contexts and how many namespaces.
    fn scope(&self) -> String {
        let contexts: BTreeSet<&str> = self.targets.iter().map(|t| t.context.as_str()).collect();
        let namespaces: HashSet<(&str, &str)> =
            self.targets.iter().map(|t| (t.context.as_str(), t.namespace.as_str())).collect();
        let on = match contexts.len() {
            0..=3 => contexts.into_iter().collect::<Vec<_>>().join(", "),
            n => format!("{n} contexts"),
        };
        format!("On {on} · {}", crate::plural::count(namespaces.len(), "namespace"))
    }
}

impl Render for LogsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, danger) = (theme.muted_foreground, theme.danger);
        let extra = self.errors.len().saturating_sub(ERRORS_SHOWN);
        let finding = !self.waiting.is_empty();
        let body = match (&self.logs, finding) {
            (Some(logs), _) => logs.clone().into_any_element(),
            (None, true) => div().text_sm().text_color(muted).child("Finding their pods…").into_any_element(),
            (None, false) => div().text_sm().text_color(muted).child("No pods found").into_any_element(),
        };
        v_flex()
            .size_full()
            .p_2()
            .gap_2()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child(self.title()))
                    .child(div().text_xs().text_color(muted).child(self.scope()))
                    .when(self.rows_left_out > 0, |d| {
                        d.child(div().text_xs().text_color(muted).child(format!(
                            "{} left out: the pods of {MAX_LOG_PODS} rows at most are read",
                            crate::plural::count(self.rows_left_out, "checked row")
                        )))
                    })
                    .when(finding && self.logs.is_some(), |d| {
                        let on = self.waiting.join(", ");
                        d.child(div().text_xs().text_color(muted).child(format!("Finding pods on {on}…")))
                    })
                    .children(
                        self.errors
                            .iter()
                            .take(ERRORS_SHOWN)
                            .map(|e| div().text_xs().text_color(danger).child(e.clone())),
                    )
                    .when(extra > 0, |d| {
                        d.child(div().text_xs().text_color(danger).child(format!("…and {extra} more errors")))
                    }),
            )
            .child(div().flex_1().min_h_0().child(body))
    }
}

/// The rows whose pods are read: all of them in their order, or past `MAX_LOG_PODS` the
/// first ones (for pods, the newest by `created`), and how many are left out. Each row is a
/// request or two: checking every row of a big list does not send thousands.
fn rows_to_read<T>(pods: bool, mut rows: Vec<T>, created: impl Fn(&T) -> Option<jiff::Timestamp>) -> (Vec<T>, usize) {
    let left_out = rows.len().saturating_sub(MAX_LOG_PODS);
    if left_out > 0 {
        if pods {
            rows.sort_by_key(|r| std::cmp::Reverse(created(r)));
        }
        rows.truncate(MAX_LOG_PODS);
    }
    (rows, left_out)
}

/// The contexts of `rows`, each once, in their order.
fn contexts_of<T>(rows: &[T], context: impl Fn(&T) -> &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for r in rows {
        let c = context(r);
        if !out.iter().any(|o| o == c) {
            out.push(c.to_string());
        }
    }
    out
}

/// The rows' pods, a cluster at a time: each context's rows are read on their own (at most
/// `READS_AT_ONCE` at once, in their order), and its results come as one part as soon as
/// they are all in. A cluster that does not answer holds back none of the others; a read
/// still running when `timeout` ends is reported as not answering.
fn find_by_context<T, F, W>(
    rows: Vec<T>,
    context: impl Fn(&T) -> String,
    read: impl Fn(&T) -> F + Clone,
    timeout: impl Fn() -> W + Clone,
) -> impl futures::Stream<Item = Vec<(T, RowPods)>>
where
    F: Future<Output = RowPods>,
    W: Future<Output = ()>,
{
    let mut by_context: Vec<(String, Vec<T>)> = Vec::new();
    for r in rows {
        let c = context(&r);
        match by_context.iter_mut().find(|(k, _)| *k == c) {
            Some((_, list)) => list.push(r),
            None => by_context.push((c, vec![r])),
        }
    }
    let contexts = by_context.len();
    futures::stream::iter(by_context)
        .map(move |(_, rows)| {
            let (read, timeout) = (read.clone(), timeout.clone());
            futures::stream::iter(rows)
                .map(move |r| {
                    let (pods, late) = (read(&r), timeout());
                    async move {
                        let pods = match futures::future::select(Box::pin(pods), Box::pin(late)).await {
                            futures::future::Either::Left((pods, _)) => pods,
                            futures::future::Either::Right(_) => {
                                Err(format!("no answer in {}s", READ_TIMEOUT.as_secs()))
                            }
                        };
                        (r, pods)
                    }
                })
                .buffered(READS_AT_ONCE)
                .collect::<Vec<_>>()
        })
        .buffer_unordered(contexts.max(1))
}

/// What reading one row gave: where it is, its name (`namespace/name`) and its pods.
struct Found<P> {
    place: P,
    row: String,
    pods: Result<Vec<LogPod>, String>,
}

/// The pods of the rows read, and what failed.
struct Gathered<P> {
    /// Each pod once, with its row's place, at most `MAX_LOG_PODS`.
    pods: Vec<(P, LogPod)>,
    /// Pods past `MAX_LOG_PODS`, not followed.
    left_out: usize,
    /// What failed, by context: rows with the same error on a context are counted together.
    errors: Vec<String>,
}

/// The pods the rows stand for, each once: two Services can select the same pods. Pods are
/// keyed by context, namespace and name (`at` gives a place's context and namespace): two
/// clusters can have pods of the same name, and both are read. Past `MAX_LOG_PODS`, the
/// running then the newest are followed. A row that failed is reported and never hides the
/// pods of the others.
fn gather<P: Clone>(found: Vec<Found<P>>, at: impl Fn(&P) -> (&str, &str)) -> Gathered<P> {
    let mut seen: HashSet<(String, String, String)> = HashSet::new();
    let mut pods = Vec::new();
    let mut failed: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for f in found {
        let (context, ns) = at(&f.place);
        match f.pods {
            Ok(list) => {
                for pod in list {
                    if seen.insert((context.to_string(), ns.to_string(), pod.name.clone())) {
                        pods.push((f.place.clone(), pod));
                    }
                }
            }
            Err(e) => failed.entry((context.to_string(), e)).or_default().push(f.row),
        }
    }
    let found = pods.len();
    let pods = follow_first(pods, |(_, p)| (p.running, p.created.clone()));
    let left_out = found - pods.len();
    let errors = failed
        .into_iter()
        .map(|((context, e), rows)| match rows.as_slice() {
            [row] => format!("{context}: {row}: {e}"),
            _ => format!("{context}: {}: {e}", crate::plural::count(rows.len(), "row")),
        })
        .collect();
    Gathered { pods, left_out, errors }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashSet};

    use futures::StreamExt as _;

    use super::{Found, Gathered, LogPod, MAX_LOG_PODS, RowPods, find_by_context, gather, rows_to_read};

    fn pod(name: &str, running: bool, created: &str) -> LogPod {
        LogPod {
            name: name.into(),
            namespace: "shop".into(),
            running,
            created: created.into(),
            containers: vec!["app".into()],
        }
    }

    type Place = (&'static str, &'static str);

    fn found(place: Place, name: &str, pods: Result<Vec<LogPod>, &str>) -> Found<Place> {
        Found { place, row: format!("{}/{name}", place.1), pods: pods.map_err(String::from) }
    }

    fn names(g: &Gathered<Place>) -> Vec<String> {
        g.pods.iter().map(|((c, n), p)| format!("{c}:{n}/{}", p.name)).collect()
    }

    /// Two Services selecting the same pods give each pod once; the same pod name on two
    /// clusters is two pods; a cluster that fails is said, and the other's pods still come.
    #[test]
    fn each_pod_once_by_context_and_one_failure_hides_nothing() {
        let t = "2026-11-20T10:00:00Z";
        let g = gather(
            vec![
                found(("eu", "shop"), "web", Ok(vec![pod("web-1", true, t), pod("web-2", true, t)])),
                found(("eu", "shop"), "web-canary", Ok(vec![pod("web-2", true, t)])),
                found(("us", "shop"), "web", Ok(vec![pod("web-1", true, t)])),
                found(("ap", "shop"), "web", Err("connection refused")),
                found(("ap", "shop"), "api", Err("connection refused")),
                found(("us", "shop"), "gone", Err("not found")),
            ],
            |p| *p,
        );
        assert_eq!(names(&g), ["eu:shop/web-1", "eu:shop/web-2", "us:shop/web-1"]);
        assert_eq!(g.left_out, 0);
        assert_eq!(g.errors, ["ap: 2 rows: connection refused", "us: shop/gone: not found"]);
    }

    /// Past the limit, the running pods go first, newest first, and the rest are counted.
    #[test]
    fn past_the_limit_the_running_and_newest_are_followed() {
        let old = "2026-01-01T00:00:00Z";
        let mut pods: Vec<LogPod> = (0..MAX_LOG_PODS).map(|i| pod(&format!("old-{i}"), true, old)).collect();
        pods.push(pod("pending", false, "2026-03-01T00:00:00Z"));
        pods.push(pod("new", true, "2026-02-01T00:00:00Z"));
        let g = gather(vec![found(("eu", "shop"), "web", Ok(pods))], |p| *p);
        assert_eq!(g.pods.len(), MAX_LOG_PODS);
        assert_eq!(g.left_out, 2);
        assert_eq!(g.pods[0].1.name, "new");
        assert!(g.pods.iter().all(|(_, p)| p.name != "pending"));
    }

    proptest::proptest! {
        /// Whatever the rows give, no pod (by context, namespace and name) is followed twice,
        /// at most `MAX_LOG_PODS` are, every pod found is followed or counted, and every
        /// row that failed is in an error.
        #[test]
        fn gathered_pods_are_unique_bounded_and_counted(
            rows in proptest::collection::vec(
                (0usize..3, 0usize..2, proptest::option::of(proptest::collection::vec((0usize..40, proptest::bool::ANY), 0..30))),
                0..12,
            )
        ) {
            const CONTEXTS: [&str; 3] = ["eu", "us", "ap"];
            const NAMESPACES: [&str; 2] = ["shop", "ingress"];
            let found: Vec<Found<Place>> = rows
                .iter()
                .enumerate()
                .map(|(i, (c, n, pods))| Found {
                    place: (CONTEXTS[*c], NAMESPACES[*n]),
                    row: format!("{}/row-{i}", NAMESPACES[*n]),
                    pods: pods
                        .as_ref()
                        .map(|list| list.iter().map(|(p, running)| pod(&format!("p-{p}"), *running, "2026-01-01T00:00:00Z")).collect())
                        .ok_or_else(|| "forbidden".to_string()),
                })
                .collect();
            let distinct: HashSet<(usize, usize, usize)> = rows
                .iter()
                .filter_map(|(c, n, pods)| Some((c, n, pods.as_ref()?)))
                .flat_map(|(c, n, pods)| pods.iter().map(move |(p, _)| (*c, *n, *p)))
                .collect();
            let failed_contexts: BTreeSet<&str> =
                rows.iter().filter(|r| r.2.is_none()).map(|(c, _, _)| CONTEXTS[*c]).collect();
            let g = gather(found, |p| *p);
            let keys: HashSet<String> = names(&g).into_iter().collect();
            proptest::prop_assert_eq!(keys.len(), g.pods.len());
            proptest::prop_assert!(g.pods.len() <= MAX_LOG_PODS);
            proptest::prop_assert_eq!(g.pods.len() + g.left_out, distinct.len());
            for c in failed_contexts {
                let prefix = format!("{c}: ");
                proptest::prop_assert!(g.errors.iter().any(|e| e.starts_with(&prefix)));
            }
        }
    }

    /// A cluster that never answers holds back none of another's pods: those come first,
    /// whole; its own rows end in an error once their reads time out.
    #[test]
    fn a_cluster_that_never_answers_holds_back_no_other() {
        let rows: Vec<(&str, &str)> =
            (0..20).map(|i| if i % 2 == 0 { ("us", "stuck") } else { ("eu", "web") }).collect();
        let read = |(context, name): &(&str, &str)| {
            let pods: Option<RowPods> =
                (*context == "eu").then(|| Ok(vec![pod(&format!("{name}-1"), true, "2026-11-20T10:00:00Z")]));
            async move {
                match pods {
                    Some(p) => p,
                    None => futures::future::pending().await,
                }
            }
        };
        // No timeout: the stuck cluster's rows never end, and the other's still come.
        let mut parts =
            std::pin::pin!(find_by_context(rows.clone(), |r| r.0.to_string(), read, futures::future::pending::<()>));
        let first = futures::executor::block_on(parts.next()).unwrap();
        assert_eq!(first.len(), 10);
        assert!(first.iter().all(|((c, _), pods)| *c == "eu" && pods.is_ok()));
        // With a timeout, the stuck cluster's rows end in an error.
        let all: Vec<_> =
            futures::executor::block_on(find_by_context(rows, |r| r.0.to_string(), read, || async {}).collect());
        let stuck: Vec<&RowPods> = all.iter().flatten().filter(|((c, _), _)| *c == "us").map(|(_, p)| p).collect();
        assert_eq!(stuck.len(), 10);
        assert!(stuck.iter().all(|p| matches!(p, Err(e) if e.starts_with("no answer"))));
        let eu = all.iter().flatten().filter(|((c, _), p)| *c == "eu" && p.is_ok()).count();
        assert_eq!(eu, 10);
    }

    /// A few rows are all read in their order; past the limit, the newest pods, or the first
    /// workloads.
    #[test]
    fn past_the_limit_the_first_rows_are_read() {
        let at = |s: i64| jiff::Timestamp::from_second(1_700_000_000 + s).ok();
        let few: Vec<(usize, Option<jiff::Timestamp>)> = (0..3).map(|i| (i, at(i as i64))).collect();
        let (rows, left_out) = rows_to_read(true, few, |r| r.1);
        assert_eq!((rows.iter().map(|r| r.0).collect::<Vec<_>>(), left_out), (vec![0, 1, 2], 0));
        let many: Vec<(usize, Option<jiff::Timestamp>)> = (0..MAX_LOG_PODS + 5).map(|i| (i, at(i as i64))).collect();
        let (pods, left_out) = rows_to_read(true, many.clone(), |r| r.1);
        assert_eq!((pods.len(), left_out), (MAX_LOG_PODS, 5));
        assert_eq!(pods[0].0, MAX_LOG_PODS + 4, "the newest pod first");
        let (workloads, _) = rows_to_read(false, many, |r| r.1);
        assert_eq!(workloads[0].0, 0, "workloads keep their order");
    }
}
