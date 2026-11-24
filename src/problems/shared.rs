//! One Problems scan per cluster and scope, shared by the sidebar badge of every window, the
//! Problems page and the agents' `find_problems`. A scope keeps the latest findings of each
//! check, replaced as the checks of a new scan finish: the page opens on the last ones at once
//! and shows the new ones as they come. At most one scan of a scope runs at a time: whoever
//! asks meanwhile waits for it instead of sending the same requests again.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use futures::future::{BoxFuture, Shared};
use futures::{FutureExt as _, StreamExt as _};
use kube::Client;

use super::{Problem, Scan, Severity};
use crate::alerts::Alerts;
use crate::k8s::lane::{Lane, Raisable};
use crate::k8s::{self, Kind};

/// What a scan reads: a cluster, and the namespaces it reads there, sorted (none: the whole
/// cluster).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Scope {
    pub context: String,
    pub namespaces: Vec<String>,
}

impl Scope {
    pub fn new(context: &str, namespaces: impl IntoIterator<Item = String>) -> Self {
        let mut namespaces: Vec<String> = namespaces.into_iter().collect();
        namespaces.sort();
        namespaces.dedup();
        Self { context: context.to_string(), namespaces }
    }

    /// The scope the app scans a cluster in: the namespaces set as accessible in Settings,
    /// or the whole cluster.
    pub fn home(context: &str) -> Self {
        Self::new(context, crate::settings::cluster(context).namespaces)
    }

    /// Whether a scan of this scope finds everything a scan of `other` would: the same
    /// cluster, whole or in every namespace of `other`.
    pub fn covers(&self, other: &Self) -> bool {
        self.context == other.context
            && (self.namespaces.is_empty()
                || !other.namespaces.is_empty()
                    && other.namespaces.iter().all(|ns| self.namespaces.binary_search(ns).is_ok()))
    }
}

/// What one check found, and what it could not read ("Pods (forbidden)").
struct Part {
    /// Its place among the scan's checks, to name what was skipped in that order.
    order: usize,
    problems: Vec<Problem>,
    skipped: Vec<String>,
}

/// A scan run to its end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Done {
    pub started: Instant,
    pub ended: Instant,
}

impl Done {
    pub fn took(self) -> Duration {
        self.ended.saturating_duration_since(self.started)
    }

    /// When the next scan is due: `every` after this one ended.
    pub fn due(self) -> Instant {
        self.ended + every(self.took())
    }
}

/// The latest findings of a scope: each check's, as its last run left them.
#[derive(Clone, Default)]
pub struct Found {
    parts: BTreeMap<Arc<str>, Arc<Part>>,
    /// The cluster's alerts, as the last scan read them.
    alerts: Option<Arc<Alerts>>,
    /// The last scan run to its end.
    pub done: Option<Done>,
    /// When the scan running now started.
    pub running: Option<Instant>,
}

impl Found {
    /// Findings of one check, for tests.
    #[cfg(test)]
    pub fn of(problems: Vec<Problem>, skipped: Vec<String>, done: Option<Done>) -> Self {
        let part = Arc::new(Part { order: 0, problems, skipped });
        Self { parts: BTreeMap::from([(Arc::from("all"), part)]), alerts: None, done, running: None }
    }

    /// The findings of the checks in `namespaces` (all when empty).
    fn objects<'a>(&'a self, namespaces: &'a [String]) -> impl Iterator<Item = &'a Problem> + 'a {
        self.parts.values().flat_map(|p| &p.problems).filter(move |p| within(p, namespaces))
    }

    /// The firing alerts in `namespaces` (all, cluster-wide ones too, when empty).
    fn alert_problems(&self, namespaces: &[String]) -> Vec<Problem> {
        self.alerts.as_ref().map_or_default(|a| crate::alerts::problems(&a.list, namespaces))
    }

    /// What was found in `namespaces` (the whole scope when empty), alerts included, worst
    /// first.
    pub fn problems(&self, namespaces: &[String]) -> Vec<Problem> {
        let mut out: Vec<Problem> = self.objects(namespaces).cloned().collect();
        out.extend(self.alert_problems(namespaces));
        super::sort(&mut out);
        out
    }

    /// (critical, warnings) in `namespaces`, as `problems` lists them.
    pub fn counts(&self, namespaces: &[String]) -> (usize, usize) {
        let alerts = self.alert_problems(namespaces);
        let mut counts = (0, 0);
        for p in self.objects(namespaces).chain(&alerts) {
            match p.severity {
                Severity::Critical => counts.0 += 1,
                Severity::Warning => counts.1 += 1,
                Severity::Info => {}
            }
        }
        counts
    }

    /// What could not be read, as "Pods (forbidden), Alerts (failed: …)"; None when
    /// everything was.
    pub fn skipped(&self) -> Option<String> {
        self.skipped_where(|_| true)
    }

    /// `skipped` of the entries `keep` keeps ("Pods (forbidden)"): an agent is told only of
    /// what its rules let it see.
    pub fn skipped_where(&self, keep: impl Fn(&str) -> bool) -> Option<String> {
        let mut parts: Vec<&Part> = self.parts.values().map(AsRef::as_ref).collect();
        parts.sort_by_key(|p| p.order);
        let mut all: Vec<String> = parts.iter().flat_map(|p| p.skipped.iter().cloned()).collect();
        if let Some(alerts) = &self.alerts {
            all.extend(Scan { problems: Vec::new(), skipped: crate::alerts::skipped(alerts) }.skipped_parts());
        }
        all.retain(|s| keep(s));
        (!all.is_empty()).then(|| all.join(", "))
    }

    /// Whether every check read all it lists, the alerts aside (a narrower scan reads them
    /// the same way): only then do these findings stand for a narrower scope. A scan of the
    /// whole cluster refused its cluster-wide lists says nothing of a namespace that may be
    /// read, and a list that timed out cluster-wide may work in one namespace.
    fn read_all(&self) -> bool {
        self.parts.values().all(|p| p.skipped.is_empty())
    }
}

/// Whether a check's finding belongs in `namespaces` (all when empty), as a scan of those
/// namespaces would have it: objects in them, cluster-scoped ones (Nodes and
/// PersistentVolumes, read whole by every scan), and those Namespaces themselves.
fn within(p: &Problem, namespaces: &[String]) -> bool {
    let picked = |ns: &str| namespaces.is_empty() || namespaces.iter().any(|n| n == ns);
    match p.namespace.as_deref() {
        Some(ns) => picked(ns),
        None if p.kind == "namespaces" => picked(&p.name),
        None => true,
    }
}

/// The least time between two scans of a scope.
const LEAST_EVERY: Duration = Duration::from_secs(120);

/// How long findings stand before their scope is scanned again: 8 times what the scan took,
/// 2 minutes at least, so a big cluster is scanned less often.
pub fn every(took: Duration) -> Duration {
    took.saturating_mul(8).max(LEAST_EVERY)
}

/// Findings recent enough for whoever asks.
#[derive(Clone, Copy, Debug)]
pub enum Fresh {
    /// Not yet due for the next scan (`every`): the badge and the page.
    Paced,
    /// From a scan started at this time or later: Refresh, or an agent that wants findings
    /// at most so old.
    Since(Instant),
}

impl Fresh {
    fn met_by(self, done: Done, now: Instant) -> bool {
        match self {
            Self::Paced => now < done.due(),
            Self::Since(at) => done.started >= at,
        }
    }
}

/// What a scan needs, from whoever starts it.
pub struct Ask {
    pub client: Client,
    /// The kinds the cluster serves: the others would fail with a 404.
    pub kinds: Vec<Kind>,
    /// The app's own scans feed the timeline with the lists they read; an agent's don't.
    pub record: bool,
}

/// The end of a scan, to wait for.
pub type Waiter = Shared<BoxFuture<'static, ()>>;

/// A scan running: its end, and its lane, raised when someone in the foreground waits for it.
#[derive(Clone)]
struct Running {
    waiter: Waiter,
    lane: Raisable,
}

struct Entry {
    found: Arc<Found>,
    /// The scan running.
    scan: Option<Running>,
    /// When it was last asked for.
    asked: Instant,
}

/// A scope nobody asked for in this long is forgotten: an agent's one-namespace scan, a
/// cluster no window shows any more.
const FORGET_AFTER: Duration = Duration::from_secs(30 * 60);

static SCANS: LazyLock<Mutex<HashMap<Scope, Entry>>> = LazyLock::new(Default::default);

static CHANGES: LazyLock<tokio::sync::watch::Sender<u64>> = LazyLock::new(|| tokio::sync::watch::channel(0).0);

fn lock() -> MutexGuard<'static, HashMap<Scope, Entry>> {
    SCANS.lock().unwrap_or_else(PoisonError::into_inner)
}

fn changed() {
    CHANGES.send_modify(|n| *n = n.wrapping_add(1));
}

/// Wakes when the findings of any scope change: a check finished, a scan started or ended.
/// Read `latest` again then.
pub fn changes() -> tokio::sync::watch::Receiver<u64> {
    CHANGES.subscribe()
}

/// Whether the findings kept for `key` may stand for `scope`: its own, or those of a scope
/// that covers it and read everything it lists (`Found::read_all`).
fn serves(key: &Scope, found: &Found, scope: &Scope) -> bool {
    key == scope || (key.covers(scope) && found.read_all())
}

/// Whether `scope`'s findings may stand for the narrower scopes it covers: not scanned yet,
/// or every check read all it lists. The page scans a namespace on its own otherwise.
pub fn may_cover(scope: &Scope) -> bool {
    lock().get(scope).is_none_or(|e| e.found.read_all())
}

/// The latest findings for `scope`: its own, or those of a scope that covers it (the cluster
/// scanned whole) when they are more recent and read everything. None before any scan
/// started. Read them with `scope.namespaces`.
pub fn latest(scope: &Scope) -> Option<Arc<Found>> {
    let scans = lock();
    (scans.iter())
        .filter(|(key, e)| serves(key, &e.found, scope))
        .max_by_key(|(key, e)| (e.found.done.map(|d| d.ended), *key == scope))
        .map(|(_, e)| e.found.clone())
}

/// Make sure the findings for `scope` are `fresh`: nothing to do when the latest are (its own
/// or those of a covering scope that read everything), else the scan running is the one to
/// wait for, else a scan starts with what `ask` gives. Returns what to wait for, if anything.
/// A scan runs to its end even when nobody waits any more, and its findings are kept for the
/// next one to ask.
///
/// `lane` is the asker's: the badge's scans go in the background, so the page on screen gets
/// the API first. A foreground asker who joins a background scan raises it to the
/// foreground, so the page, Refresh or an agent never waits at the badge's pace.
pub fn ensure(scope: &Scope, fresh: Fresh, lane: Lane, ask: impl FnOnce() -> Ask) -> Option<Waiter> {
    let now = Instant::now();
    let mut scans = lock();
    scans.retain(|_, e| e.scan.is_some() || now.saturating_duration_since(e.asked) < FORGET_AFTER);
    let mut running: Option<Running> = None;
    for (key, entry) in scans.iter_mut().filter(|(key, e)| serves(key, &e.found, scope)) {
        entry.asked = now;
        if entry.found.done.is_some_and(|d| fresh.met_by(d, now)) {
            return None;
        }
        if let Some(scan) = &entry.scan
            && (running.is_none() || key == scope)
        {
            running = Some(scan.clone());
        }
    }
    if let Some(scan) = running {
        if lane == Lane::Foreground {
            scan.lane.raise();
        }
        return Some(scan.waiter);
    }
    let ask = ask();
    let (done, ended) = futures::channel::oneshot::channel::<()>();
    let waiter: Waiter = ended.map(|_| ()).boxed().shared();
    let lane = Raisable::new(lane);
    let entry = scans.entry(scope.clone()).or_insert_with(|| Entry { found: Arc::default(), scan: None, asked: now });
    entry.scan = Some(Running { waiter: waiter.clone(), lane: lane.clone() });
    Arc::make_mut(&mut entry.found).running = Some(now);
    drop(scans);
    changed();
    // Findings from `at` on (Refresh, an agent checking a fix): no Service keeps a verdict
    // judged before.
    let judged_since = match fresh {
        Fresh::Since(at) => Some(at),
        Fresh::Paced => None,
    };
    let finish = Finish { scope: scope.clone(), started: now, checks: None, ended: false, done: Some(done) };
    k8s::spawn_detached(lane.run(run(scope.clone(), ask, judged_since, finish)));
    Some(waiter)
}

/// Findings for `scope` that are `fresh`: the latest when they are, else those of the scan
/// running or of a new one. A scan that started too early for `Since` is waited for, then
/// another one runs.
pub async fn fresh(scope: &Scope, fresh: Fresh, lane: Lane, ask: impl Fn() -> Ask) -> Option<Arc<Found>> {
    while let Some(scan) = ensure(scope, fresh, lane, &ask) {
        scan.await;
    }
    latest(scope)
}

/// Change the findings of `scope`, and say so.
fn update(scope: &Scope, change: impl FnOnce(&mut Found)) {
    {
        let mut scans = lock();
        if let Some(entry) = scans.get_mut(scope) {
            change(Arc::make_mut(&mut entry.found));
        }
    }
    changed();
}

/// A scan: each check's findings replace its last ones as soon as it finishes.
async fn run(scope: Scope, ask: Ask, judged_since: Option<Instant>, mut finish: Finish) {
    let Ask { client, kinds, record } = ask;
    let context = record.then_some(scope.context.as_str());
    let mut jobs = super::jobs(&client, &kinds, &scope.namespaces, context, judged_since);
    // Argo CD Applications deploying here, read on their hubs (no request when none is known).
    jobs.push(super::argo_apps::job(&scope.context, &client, &kinds, &scope.namespaces));
    finish.checks = Some(jobs.iter().map(|(name, _)| Arc::from(name.as_str())).collect());
    let checks = async {
        let mut running: futures::stream::FuturesUnordered<_> = (jobs.into_iter().enumerate())
            .map(|(order, (name, job))| job.map(move |scan| (order, name, scan)))
            .collect();
        while let Some((order, name, scan)) = running.next().await {
            let skipped = scan.skipped_parts();
            let part = Arc::new(Part { order, problems: scan.problems, skipped });
            update(&scope, |found| {
                found.parts.insert(Arc::from(name), part);
            });
        }
    };
    let alerts = async {
        let alerts = crate::alerts::active(&scope.context, client.clone()).await;
        update(&scope, |found| found.alerts = Some(alerts));
    };
    futures::join!(checks, alerts);
    finish.ended = true;
    drop(finish);
}

/// The part that says a scan stopped half way: no check has this name.
const STOPPED: &str = "\u{0}stopped";

/// Ends a scan, also one stopped half way (a panic): its findings get their time, a check
/// the scan no longer has is dropped, and whoever waits is woken. A scan stopped half way
/// says so in what could not be read, so it is never taken for an all-clear.
struct Finish {
    scope: Scope,
    started: Instant,
    /// The scan's checks, once known.
    checks: Option<HashSet<Arc<str>>>,
    /// Every check ran to its end.
    ended: bool,
    done: Option<futures::channel::oneshot::Sender<()>>,
}

impl Drop for Finish {
    fn drop(&mut self) {
        {
            let mut scans = lock();
            if let Some(entry) = scans.get_mut(&self.scope) {
                let found = Arc::make_mut(&mut entry.found);
                if !self.ended {
                    // The checks it didn't get to keep their last findings, if any.
                    let skipped = vec!["the rest (the scan stopped)".to_string()];
                    let part = Part { order: usize::MAX, problems: Vec::new(), skipped };
                    found.parts.insert(Arc::from(STOPPED), Arc::new(part));
                } else if let Some(checks) = &self.checks {
                    found.parts.retain(|name, _| checks.contains(name));
                }
                found.done = Some(Done { started: self.started, ended: Instant::now() });
                found.running = None;
                entry.scan = None;
            }
        }
        changed();
        if let Some(done) = self.done.take() {
            let _ = done.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn scope(context: &str, namespaces: &[&str]) -> Scope {
        Scope::new(context, namespaces.iter().map(ToString::to_string))
    }

    fn any_scope() -> impl Strategy<Value = Scope> {
        (prop::sample::select(&["a", "b"][..]), prop::sample::subsequence(&["x", "y", "z"][..], 0..=3))
            .prop_map(|(context, namespaces)| scope(context, &namespaces))
    }

    fn problem(severity: Severity, kind: &str, namespace: Option<&str>, name: &str) -> Problem {
        Problem {
            severity,
            kind: kind.into(),
            label: kind.into(),
            namespace: namespace.map(String::from),
            name: name.into(),
            reason: "broken".into(),
            age: None,
            on_hub: None,
        }
    }

    fn any_problem() -> impl Strategy<Value = Problem> {
        let ns = prop::option::of(prop::sample::select(&["x", "y", "z"][..]));
        let kind = prop::sample::select(&["pods", "nodes", "namespaces", "persistentvolumes"][..]);
        let severity = prop::sample::select(&[Severity::Critical, Severity::Warning, Severity::Info][..]);
        (severity, kind, ns, prop::sample::select(&["x", "y", "z", "n1"][..]))
            .prop_map(|(severity, kind, ns, name)| problem(severity, kind, ns, name))
    }

    fn found(problems: Vec<Problem>) -> Found {
        let mut found = Found::default();
        let part = Part { order: 0, problems, skipped: Vec::new() };
        found.parts.insert(Arc::from("all"), Arc::new(part));
        found
    }

    proptest! {
        /// A scope covers itself; the whole cluster covers every scope of its cluster; no
        /// scope covers another cluster's, nor a wider one of its own.
        #[test]
        fn what_a_scope_covers(a in any_scope(), b in any_scope(), c in any_scope()) {
            prop_assert!(a.covers(&a));
            let whole = scope(&a.context, &[]);
            prop_assert!(whole.covers(&a));
            if a.context != b.context {
                prop_assert!(!a.covers(&b));
            }
            if a.covers(&b) && !b.namespaces.is_empty() {
                prop_assert!(b.namespaces.iter().all(|ns| a.namespaces.is_empty() || a.namespaces.contains(ns)));
            }
            if !a.namespaces.is_empty() {
                prop_assert!(!a.covers(&whole));
            }
            if a.covers(&b) && b.covers(&c) {
                prop_assert!(a.covers(&c));
            }
        }

        /// What the whole cluster's scan shows for some namespaces is what a scan of those
        /// namespaces finds: their objects, every cluster-scoped one, those Namespaces.
        #[test]
        fn a_covering_scan_shows_what_a_narrower_one_finds(
            problems in prop::collection::vec(any_problem(), 0..20),
            picked in prop::sample::subsequence(&["x", "y", "z"][..], 0..=3),
        ) {
            let picked: Vec<String> = picked.into_iter().map(String::from).collect();
            let found = found(problems.clone());
            let shown = found.problems(&picked);
            let expected: Vec<&Problem> = problems
                .iter()
                .filter(|p| match (&p.namespace, p.kind.as_str()) {
                    _ if picked.is_empty() => true,
                    (Some(ns), _) => picked.contains(ns),
                    (None, "namespaces") => picked.contains(&p.name),
                    (None, _) => true,
                })
                .collect();
            prop_assert_eq!(shown.len(), expected.len());
            for p in &shown {
                prop_assert!(expected.iter().any(|e| e.namespace == p.namespace && e.name == p.name && e.kind == p.kind));
            }
            // Worst first, and the counts are those of the rows.
            prop_assert!(shown.windows(2).all(|w| w[0].severity <= w[1].severity));
            let count = |s| shown.iter().filter(|p| p.severity == s).count();
            prop_assert_eq!(found.counts(&picked), (count(Severity::Critical), count(Severity::Warning)));
        }

        /// A scope is scanned again only once its findings are older than 8 times what the
        /// scan took, 2 minutes at least; `Since` takes only a scan started from then on.
        #[test]
        fn scans_are_paced_by_their_cost(took in 0u64..3_600, age in 0u64..36_000, asked in 0u64..7_200) {
            let started = Instant::now();
            let done = Done { started, ended: started + Duration::from_secs(took) };
            let now = done.ended + Duration::from_secs(age);
            let every = Duration::from_secs((took * 8).max(120));
            prop_assert_eq!(Fresh::Paced.met_by(done, now), Duration::from_secs(age) < every);
            let at = started + Duration::from_secs(asked);
            prop_assert_eq!(Fresh::Since(at).met_by(done, now), asked == 0);
            prop_assert!(Fresh::Since(started).met_by(done, now));
        }
    }

    #[test]
    fn what_could_not_be_read_comes_in_the_order_of_the_checks() {
        let mut found = Found::default();
        let part = |order, skipped: &str| {
            let skipped = if skipped.is_empty() { Vec::new() } else { vec![skipped.to_string()] };
            Arc::new(Part { order, problems: Vec::new(), skipped })
        };
        found.parts.insert(Arc::from("Pods"), part(1, "Pods (forbidden)"));
        found.parts.insert(Arc::from("Nodes"), part(0, ""));
        found.parts.insert(Arc::from("Jobs"), part(2, "Jobs (failed: timeout)"));
        assert_eq!(found.skipped().unwrap(), "Pods (forbidden), Jobs (failed: timeout)");
        assert_eq!(Found::default().skipped(), None);
    }

    /// A cluster with one node `NotReady`; records the paths it was asked for. With `.1`, a
    /// user who may not list Pods cluster-wide but may in namespace `shop`, where there are
    /// none.
    #[derive(Clone, Default)]
    struct Cluster(Arc<Mutex<Vec<String>>>, bool);

    impl Cluster {
        fn asked(&self, path: &str) -> usize {
            self.0.lock().unwrap().iter().filter(|p| *p == path).count()
        }
    }

    impl tower::Service<http::Request<kube::client::Body>> for Cluster {
        type Response = http::Response<http_body_util::Full<bytes::Bytes>>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<kube::client::Body>) -> Self::Future {
            let path = req.uri().path().to_string();
            self.0.lock().unwrap().push(path.clone());
            let (code, body) = if path == "/api/v1/nodes" {
                let row = serde_json::json!({"cells": ["n1", "NotReady"], "object": {"metadata": {"name": "n1", "uid": "n1"}}});
                let defs = [serde_json::json!({"name": "Name"}), serde_json::json!({"name": "Status"})];
                (200, serde_json::json!({"kind": "Table", "columnDefinitions": defs, "rows": [row]}))
            } else if self.1 && path == "/api/v1/pods" {
                let status = serde_json::json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                    "reason": "Forbidden", "message": "pods is forbidden", "code": 403});
                (403, status)
            } else if self.1 && path == "/api/v1/namespaces/shop/pods" {
                let defs = [serde_json::json!({"name": "Name"}), serde_json::json!({"name": "Status"})];
                (200, serde_json::json!({"kind": "Table", "columnDefinitions": defs, "rows": []}))
            } else {
                (404, serde_json::json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "code": 404}))
            };
            let response = http::Response::builder().status(code).body(body.to_string().into_bytes().into()).unwrap();
            std::future::ready(Ok(response))
        }
    }

    fn ask(cluster: &Cluster) -> impl Fn() -> Ask {
        let client = Client::new(cluster.clone(), "default");
        let mut node = Kind::fake("", "Node");
        node.namespaced = false;
        move || Ask { client: client.clone(), kinds: vec![node.clone()], record: false }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_scan_serves_everyone_who_asks_meanwhile() {
        let cluster = Cluster::default();
        let whole = scope("shared-test-one-scan", &[]);
        // The badge, the page and an agent ask at once: one scan.
        let (a, b, c) = futures::join!(
            fresh(&whole, Fresh::Paced, Lane::Background, ask(&cluster)),
            fresh(&whole, Fresh::Paced, Lane::Background, ask(&cluster)),
            fresh(&whole, Fresh::Since(Instant::now()), Lane::Background, ask(&cluster)),
        );
        assert_eq!(cluster.asked("/api/v1/nodes"), 1);
        for found in [a, b, c] {
            let found = found.unwrap();
            assert!(found.done.is_some() && found.running.is_none());
            let names: Vec<String> = found.problems(&[]).into_iter().map(|p| p.name).collect();
            assert_eq!(names, ["n1"]);
        }
        // Paced: the findings stand, nothing is asked again.
        assert!(ensure(&whole, Fresh::Paced, Lane::Background, ask(&cluster)).is_none());
        // One namespace of the cluster: shown from the whole cluster's scan.
        let one = scope("shared-test-one-scan", &["shop"]);
        assert!(ensure(&one, Fresh::Paced, Lane::Background, ask(&cluster)).is_none());
        let found = latest(&one).unwrap();
        assert_eq!(found.problems(&one.namespaces).len(), 1, "nodes belong to every namespace");
        assert_eq!(cluster.asked("/api/v1/nodes"), 1);
        // Refresh: a new scan.
        fresh(&whole, Fresh::Since(Instant::now()), Lane::Background, ask(&cluster)).await;
        assert_eq!(cluster.asked("/api/v1/nodes"), 2);
        // Another cluster's scope is its own.
        assert!(latest(&scope("shared-test-other", &[])).is_none());
    }

    /// A user who may not list Pods cluster-wide but may read `shop`: the whole cluster's
    /// scan says nothing of `shop`, which is scanned on its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_scan_that_could_not_read_stands_for_no_narrower_scope() {
        let cluster = Cluster(Arc::default(), true);
        let client = Client::new(cluster.clone(), "default");
        let pod = Kind::fake("", "Pod");
        let asks = || Ask { client: client.clone(), kinds: vec![pod.clone()], record: false };
        let whole = scope("shared-test-forbidden", &[]);
        let found = fresh(&whole, Fresh::Paced, Lane::Background, &asks).await.unwrap();
        assert!(!found.read_all() && found.skipped().unwrap().contains("Pod"), "{:?}", found.skipped());
        assert!(!may_cover(&whole));
        let shop = scope("shared-test-forbidden", &["shop"]);
        assert!(latest(&shop).is_none(), "not shown from the whole cluster's scan");
        let found = fresh(&shop, Fresh::Paced, Lane::Background, &asks).await.unwrap();
        assert!(found.read_all(), "{:?}", found.skipped());
        assert_eq!(cluster.asked("/api/v1/namespaces/shop/pods"), 1);
        // Its own findings stand, though the whole cluster's are more recent.
        fresh(&whole, Fresh::Since(Instant::now()), Lane::Background, &asks).await;
        assert!(ensure(&shop, Fresh::Paced, Lane::Background, asks).is_none());
        assert!(latest(&shop).unwrap().read_all());
        assert_eq!(cluster.asked("/api/v1/namespaces/shop/pods"), 1);
        // With full rights, the whole cluster's scan stands for `shop`.
        let open = Cluster::default();
        let whole = scope("shared-test-open", &[]);
        fresh(&whole, Fresh::Paced, Lane::Background, ask(&open)).await;
        assert!(may_cover(&whole));
        assert!(ensure(&scope("shared-test-open", &["shop"]), Fresh::Paced, Lane::Background, ask(&open)).is_none());
    }

    #[test]
    fn a_scan_stopped_half_way_is_no_all_clear() {
        let s = scope("shared-test-stopped", &[]);
        lock().insert(s.clone(), Entry { found: Arc::default(), scan: None, asked: Instant::now() });
        let finish = |ended| Finish {
            scope: s.clone(),
            started: Instant::now(),
            checks: Some(HashSet::new()),
            ended,
            done: None,
        };
        drop(finish(false));
        let found = latest(&s).unwrap();
        assert!(found.done.is_some() && !found.read_all());
        assert_eq!(found.skipped().unwrap(), "the rest (the scan stopped)");
        // The next scan run to its end drops the note.
        drop(finish(true));
        assert_eq!(latest(&s).unwrap().skipped(), None);
    }

    /// The badge's scan runs in the background; the page, Refresh or an agent who then waits
    /// for it raises it, so they don't wait at the badge's pace. Another background asker
    /// leaves it where it is.
    #[test]
    fn a_foreground_asker_raises_the_background_scan_it_joins() {
        let s = scope("shared-test-raise", &[]);
        let lane = Raisable::new(Lane::Background);
        let waiter: Waiter = futures::future::pending().boxed().shared();
        let scan = Some(Running { waiter, lane: lane.clone() });
        lock().insert(s.clone(), Entry { found: Arc::default(), scan, asked: Instant::now() });
        let no_new_scan = || -> Ask { unreachable!("a scan is running") };
        assert!(ensure(&s, Fresh::Paced, Lane::Background, no_new_scan).is_some());
        assert_eq!(lane.lane(), Lane::Background, "a second badge leaves it in the background");
        assert!(ensure(&s, Fresh::Since(Instant::now()), Lane::Foreground, no_new_scan).is_some());
        assert_eq!(lane.lane(), Lane::Foreground, "the page waits for it in the foreground");
        // A narrower scope waiting for it raises it too.
        let lane = Raisable::new(Lane::Background);
        lock().get_mut(&s).unwrap().scan.as_mut().unwrap().lane = lane.clone();
        let one = scope("shared-test-raise", &["shop"]);
        assert!(ensure(&one, Fresh::Paced, Lane::Foreground, no_new_scan).is_some());
        assert_eq!(lane.lane(), Lane::Foreground);
        lock().remove(&s);
    }
}
