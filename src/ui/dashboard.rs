//! Workloads overview: status bars per kind, recent warnings and restarts,
//! and containers close to their limits.

use std::collections::BTreeMap;

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use k8s_openapi::api::{
    apps::v1 as apps, autoscaling::v2 as autoscaling, batch::v1 as batch, core::v1 as core, policy::v1 as policy,
};
use kube::Client;
use serde_json::Value;

use crate::details::OpenObject;
use crate::k8s::{self, ContainerUsage};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Tone {
    Good,
    Neutral,
    Warn,
    Bad,
}

struct KindSummary {
    title: &'static str,
    /// (label, count, tone), in display order.
    counts: Vec<(String, usize, Tone)>,
    /// Still waiting for at least one cluster.
    loading: bool,
    /// Clusters where it could not be listed: "forbidden", "failed: …".
    errors: Vec<String>,
}

/// Parts of the page that come from one list, and say so when it fails.
#[derive(Clone, Copy, PartialEq)]
enum Section {
    Warnings,
    /// Restarts and containers near their limits, from the pods.
    Pods,
}

/// Card order on the page.
const SLOTS: [(&str, &str); 11] = [
    ("Pods", "pods"),
    ("Deployments", "deployments"),
    ("Helm Releases", "secrets"),
    ("ReplicaSets", "replicasets"),
    ("StatefulSets", "statefulsets"),
    ("DaemonSets", "daemonsets"),
    ("CronJobs", "cronjobs"),
    ("Jobs", "jobs"),
    ("HPAs", "horizontalpodautoscalers"),
    ("Disruption Budgets", "poddisruptionbudgets"),
    ("PVCs", "persistentvolumeclaims"),
];

/// (label, count, tone) per state of a kind.
type Counts = Vec<(String, usize, Tone)>;

/// One finished part of a cluster's overview, sent as soon as it is ready.
enum Piece {
    /// A kind's counts, or why it could not be listed.
    Kind(usize, Result<Vec<(String, usize, Tone)>, String>),
    Warnings(Vec<Warning>),
    Restarts(Vec<Restart>),
    Hot(Vec<Hot>, bool),
    Failed(Section, String),
    Done(std::time::Duration),
}

/// What has arrived so far for one cluster.
#[derive(Default)]
struct ClusterState {
    context: String,
    kinds: Vec<Option<Result<Counts, String>>>,
    warnings: Option<Vec<Warning>>,
    restarts: Option<Vec<Restart>>,
    hot: Option<(Vec<Hot>, bool)>,
    /// Why the warnings, or the pods behind restarts and limits, could not be listed.
    warnings_error: Option<String>,
    pods_error: Option<String>,
}

struct Warning {
    reason: String,
    count: i64,
    last: jiff::Timestamp,
}

#[derive(Clone)]
struct Restart {
    context: String,
    namespace: String,
    pod: String,
    container: String,
    reason: String,
    count: i64,
    when: jiff::Timestamp,
}

#[derive(Clone)]
struct Hot {
    context: String,
    namespace: String,
    pod: String,
    container: String,
    cpu: (f64, Option<f64>),
    mem: (f64, Option<f64>),
}

#[derive(Default)]
struct Data {
    kinds: Vec<KindSummary>,
    warnings: Vec<Warning>,
    restarts: Vec<Restart>,
    hot: Vec<Hot>,
    /// Too many pods to read full specs; pick a namespace to see limits.
    hot_skipped: bool,
    loading_details: bool,
    /// Clusters whose warnings, or pods, could not be listed (and why).
    warnings_errors: Vec<String>,
    pods_errors: Vec<String>,
}

pub struct Dashboard {
    clusters: Vec<ClusterState>,
    multi: bool,
    _tasks: Vec<Task<()>>,
}

impl EventEmitter<OpenObject> for Dashboard {}

impl Dashboard {
    pub fn new(clusters: Vec<(String, Client)>, namespace: Option<String>, cx: &mut Context<Self>) -> Self {
        let multi = clusters.len() > 1;
        let mut tasks = Vec::new();
        for (ix, (ctx, client)) in clusters.iter().cloned().enumerate() {
            let namespace = namespace.clone();
            tasks.push(cx.spawn(async move |this, cx| {
                loop {
                    let exec = cx.background_executor().clone();
                    if !crate::app::wait_visible(&this, cx, &exec).await {
                        break;
                    }
                    let (tx, mut rx) = futures::channel::mpsc::unbounded();
                    let load = load(client.clone(), namespace.clone(), ctx.clone(), tx);
                    let pieces = async {
                        let mut took = std::time::Duration::from_secs(1);
                        while let Some(piece) = futures::StreamExt::next(&mut rx).await {
                            if let Piece::Done(d) = &piece {
                                took = *d;
                            }
                            if this.update(cx, |this, cx| this.apply(ix, piece, cx)).is_err() {
                                break;
                            }
                        }
                        took
                    };
                    let (_, took) = futures::join!(load, pieces);
                    // Big clusters take long to list; don't hammer their API servers.
                    let wait = (took * 2).max(crate::settings::secs(crate::settings::get().overview_refresh));
                    cx.background_executor().timer(wait).await;
                }
            }));
        }
        let states = clusters
            .iter()
            .map(|(ctx, _)| ClusterState { context: ctx.clone(), kinds: vec![None; SLOTS.len()], ..Default::default() })
            .collect();
        Self { clusters: states, multi, _tasks: tasks }
    }

    fn apply(&mut self, ix: usize, piece: Piece, cx: &mut Context<Self>) {
        let Some(state) = self.clusters.get_mut(ix) else { return };
        match piece {
            Piece::Kind(slot, counts) => state.kinds[slot] = Some(counts),
            // Data arriving clears an earlier failure; a failure follows its (empty) data.
            Piece::Warnings(w) => (state.warnings, state.warnings_error) = (Some(w), None),
            Piece::Restarts(r) => (state.restarts, state.pods_error) = (Some(r), None),
            Piece::Hot(h, skipped) => state.hot = Some((h, skipped)),
            Piece::Failed(Section::Warnings, e) => state.warnings_error = Some(e),
            Piece::Failed(Section::Pods, e) => state.pods_error = Some(e),
            Piece::Done(_) => {}
        }
        cx.notify();
    }

    /// Everything that has arrived, added up across clusters.
    fn merged(&mut self) -> Data {
        let mut kinds: Vec<KindSummary> = SLOTS
            .iter()
            .map(|(title, _)| KindSummary { title, counts: Vec::new(), loading: false, errors: Vec::new() })
            .collect();
        let mut out = Data::default();
        let multi = self.multi;
        // Which cluster an error is about, when there are several.
        let about = |ctx: &str, e: &str| if multi { format!("{ctx}: {e}") } else { e.to_string() };
        for state in &mut self.clusters {
            for (slot, k) in kinds.iter_mut().enumerate() {
                match &state.kinds[slot] {
                    Some(Ok(counts)) => {
                        for (label, n, tone) in counts {
                            match k.counts.iter_mut().find(|c| c.0 == *label) {
                                Some(c) => c.1 += n,
                                None => k.counts.push((label.clone(), *n, *tone)),
                            }
                        }
                    }
                    Some(Err(e)) => k.errors.push(about(&state.context, e)),
                    None => k.loading = true,
                }
            }
            out.warnings_errors.extend(state.warnings_error.iter().map(|e| about(&state.context, e)));
            out.pods_errors.extend(state.pods_error.iter().map(|e| about(&state.context, e)));
            for w in state.warnings.iter().flatten() {
                match out.warnings.iter_mut().find(|x| x.reason == w.reason) {
                    Some(x) => {
                        x.count += w.count;
                        x.last = x.last.max(w.last);
                    }
                    None => out.warnings.push(Warning { reason: w.reason.clone(), count: w.count, last: w.last }),
                }
            }
            for r in state.restarts.iter().flatten() {
                out.restarts.push(r.clone());
            }
            if let Some((hot, skipped)) = &state.hot {
                out.hot.extend(hot.iter().cloned());
                out.hot_skipped |= skipped;
            }
            out.loading_details |= state.warnings.is_none() || state.restarts.is_none() || state.hot.is_none();
        }
        for k in &mut kinds {
            k.counts.sort_by(|a, b| (a.2, &a.0).cmp(&(b.2, &b.0)));
        }
        out.kinds = kinds;
        out.warnings.sort_by_key(|w| std::cmp::Reverse(w.count));
        out.restarts.sort_by_key(|r| std::cmp::Reverse(r.when));
        out
    }
}

/// Objects of a kind, or why they could not be listed ("forbidden", "failed: …").
async fn list(client: &Client, ar: kube::discovery::ApiResource, ns: &Option<String>) -> Result<Vec<Value>, String> {
    k8s::list_values(client.clone(), ar, ns.clone(), None).await.map_err(|e| k8s::lists::load_error(&e))
}

fn n(v: &Value, p: &str) -> i64 {
    v.pointer(p).and_then(Value::as_i64).unwrap_or(0)
}

fn st<'a>(v: &'a Value, p: &str) -> &'a str {
    v.pointer(p).and_then(Value::as_str).unwrap_or_default()
}

fn ts(v: &Value, p: &str) -> Option<jiff::Timestamp> {
    v.pointer(p).and_then(Value::as_str).and_then(|t| t.parse().ok())
}

fn tally<L: Into<String>>(items: impl Iterator<Item = (L, Tone)>) -> Vec<(String, usize, Tone)> {
    let mut map: BTreeMap<(Tone, String), usize> = BTreeMap::new();
    for (label, tone) in items {
        *map.entry((tone, label.into())).or_default() += 1;
    }
    map.into_iter().map(|((tone, label), n)| (label, n, tone)).collect()
}

/// Pods beyond this count are summarized from kubectl's table only (no full specs).
const FULL_POD_LIMIT: usize = 4000;

/// "3 (12m ago)" -> how long ago, from kubectl's Restarts column.
fn restart_ago(cell: &str) -> Option<jiff::SignedDuration> {
    let inner = cell.split_once('(')?.1.strip_suffix(" ago)")?;
    let mut secs = 0i64;
    let mut num = String::new();
    for ch in inner.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
            continue;
        }
        let n: i64 = num.parse().ok()?;
        num.clear();
        secs += n * match ch {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            'y' => 86400 * 365,
            _ => return None,
        };
    }
    Some(jiff::SignedDuration::from_secs(secs))
}

/// kubectl's table for one kind: light even on very large clusters.
struct Tbl {
    defs: Vec<k8s::ColumnDef>,
    rows: Vec<std::sync::Arc<k8s::Row>>,
}

impl Tbl {
    fn cell<'a>(&self, row: &'a k8s::Row, name: &str) -> &'a str {
        self.defs
            .iter()
            .position(|d| d.name == name)
            .and_then(|i| row.cells.get(i))
            .map(String::as_str)
            .unwrap_or_default()
    }

    /// "3/5" -> (3, 5)
    fn ratio(&self, row: &k8s::Row, name: &str) -> (i64, i64) {
        let c = self.cell(row, name);
        let (a, b) = c.split_once('/').unwrap_or((c, c));
        (a.trim().parse().unwrap_or(0), b.trim().parse().unwrap_or(0))
    }
}

/// kubectl's table for a kind, or why it could not be listed ("forbidden", "failed: …").
/// A kind's rows for its counts; `with_namespaces` when each row needs its namespace too.
async fn table(
    client: Client,
    ar: kube::discovery::ApiResource,
    ns: Option<String>,
    with_namespaces: bool,
) -> Result<Tbl, String> {
    #[cfg(feature = "dev")]
    let (started, plural) = (std::time::Instant::now(), ar.plural.clone());
    let kind = k8s::Kind { ar, namespaced: true, section: "", label: String::new() };
    // Counts and restarts need only the cells: big lists come without their objects.
    let result = k8s::list_light(client, kind, ns, with_namespaces).await;
    #[cfg(feature = "dev")]
    if std::env::var("RECOUVRANCE_DEBUG").is_ok() {
        eprintln!("table {plural}: {:?} rows={:?}", started.elapsed(), result.as_ref().map(|r| r.1.len()));
    }
    result.map(|(defs, rows)| Tbl { defs, rows }).map_err(|e| k8s::lists::load_error(&e))
}

fn ready_state(ready: i64, desired: i64) -> (&'static str, Tone) {
    if desired == 0 {
        ("Idle", Tone::Neutral)
    } else if ready >= desired {
        ("Running", Tone::Good)
    } else {
        ("Unavailable", Tone::Bad)
    }
}

fn pod_counts(pods: &Tbl) -> Vec<(String, usize, Tone)> {
    // kubectl's Status column says *why* a pod is unhealthy (CrashLoopBackOff, …).
    // Colors as in the pod list; a status we do not know is a failure reason here.
    use crate::status::Tone as T;
    tally(pods.rows.iter().map(|r| {
        let status = pods.cell(r, "Status");
        let tone = crate::status::tone(status);
        match (status, tone) {
            ("", _) => ("Unknown".to_string(), Tone::Warn),
            ("Completed" | "Succeeded", _) => ("Completed".to_string(), Tone::Neutral),
            (s, Some(T::Busy)) if s.starts_with("Init:") => ("Initializing".to_string(), Tone::Warn),
            (s, Some(T::Good)) => (s.to_string(), Tone::Good),
            (s, Some(T::Done)) => (s.to_string(), Tone::Neutral),
            (s, Some(T::Busy)) => (s.to_string(), Tone::Warn),
            (s, Some(T::Bad) | None) => (s.to_string(), Tone::Bad),
        }
    }))
}

/// The parts of a pod `pod_details` reads: managed fields, volumes and the rest are skipped
/// while parsing, which matters at thousands of pods.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct PodDetail {
    metadata: PodMeta,
    spec: PodSpecLite,
    status: PodStatusLite,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct PodMeta {
    name: String,
    namespace: String,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct PodSpecLite {
    containers: Vec<Value>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct PodStatusLite {
    container_statuses: Vec<Value>,
}

/// Restarts in the last hour and containers near their limits, from pod specs.
async fn pod_details(client: &Client, ns: &Option<String>, context: &str) -> Result<(Vec<Restart>, Vec<Hot>), String> {
    let usage = async {
        match ns {
            Some(ns) => k8s::container_usage(client.clone(), ns.clone(), None).await.unwrap_or_default(),
            None => all_usage(client).await,
        }
    };
    let pods = async {
        k8s::lists::list_lean::<PodDetail>(client.clone(), k8s::ar_of::<core::Pod>(), ns.clone())
            .await
            .map_err(|e| k8s::lists::load_error(&e))
    };
    let (pods, usage): (Result<Vec<PodDetail>, String>, ContainerUsage) = futures::join!(pods, usage);
    let pods = pods?;
    let hour_ago = jiff::Timestamp::now() - jiff::SignedDuration::from_secs(3600);
    let (mut restarts, mut hot) = (Vec::new(), Vec::new());
    for p in &pods {
        let (namespace, pod) = (p.metadata.namespace.clone(), p.metadata.name.clone());
        for c in &p.status.container_statuses {
            if let Some(when) = ts(c, "/lastState/terminated/finishedAt").filter(|t| *t >= hour_ago) {
                restarts.push(Restart {
                    context: context.to_string(),
                    namespace: namespace.clone(),
                    pod: pod.clone(),
                    container: st(c, "/name").into(),
                    reason: st(c, "/lastState/terminated/reason").into(),
                    count: n(c, "/restartCount"),
                    when,
                });
            }
        }
        for c in &p.spec.containers {
            let name = st(c, "/name");
            let Some(&(cpu, mem)) = usage.get(&pod).and_then(|u| u.get(name)) else { continue };
            let cpu_limit = c.pointer("/resources/limits/cpu").and_then(Value::as_str).map(k8s::parse_cpu);
            let mem_limit = c.pointer("/resources/limits/memory").and_then(Value::as_str).map(k8s::parse_bytes);
            let near = |v: f64, l: Option<f64>| l.is_some_and(|l| l > 0.0 && v >= 0.95 * l);
            if near(cpu, cpu_limit) || near(mem, mem_limit) {
                hot.push(Hot {
                    context: context.to_string(),
                    namespace: namespace.clone(),
                    pod: pod.clone(),
                    container: name.into(),
                    cpu: (cpu, cpu_limit),
                    mem: (mem, mem_limit),
                });
            }
        }
    }
    restarts.sort_by_key(|r| std::cmp::Reverse(r.when));
    Ok((restarts, hot))
}

/// Restarts in the last hour from kubectl's table ("3 (12m ago)"), for very large clusters.
fn table_restarts(pods: &Tbl, context: &str) -> Vec<Restart> {
    let now = jiff::Timestamp::now();
    let mut out: Vec<Restart> = pods
        .rows
        .iter()
        .filter_map(|r| {
            let cell = pods.cell(r, "Restarts");
            let ago = restart_ago(cell).filter(|d| d.as_secs() <= 3600)?;
            Some(Restart {
                context: context.to_string(),
                namespace: r.namespace.clone().unwrap_or_default(),
                pod: r.name.clone(),
                container: String::new(),
                reason: pods.cell(r, "Status").to_string(),
                count: cell.split_whitespace().next().and_then(|n| n.parse().ok()).unwrap_or(0),
                when: now - ago,
            })
        })
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.when));
    out
}

/// Load one cluster's overview, sending each part as soon as it is ready:
/// small kinds show up in a second even when pods take a minute.
async fn load(client: Client, ns: Option<String>, context: String, tx: futures::channel::mpsc::UnboundedSender<Piece>) {
    let started = std::time::Instant::now();
    let send = |p: Piece| {
        let _ = tx.unbounded_send(p);
    };
    let c = || client.clone();
    let slot = |kind: &str| SLOTS.iter().position(|(_, k)| *k == kind).unwrap_or(0);

    let pods = async {
        let t = match table(c(), k8s::ar_of::<core::Pod>(), ns.clone(), true).await {
            Ok(t) => t,
            Err(e) => {
                send(Piece::Kind(slot("pods"), Err(e.clone())));
                send(Piece::Restarts(Vec::new()));
                send(Piece::Hot(Vec::new(), false));
                send(Piece::Failed(Section::Pods, e));
                return;
            }
        };
        send(Piece::Kind(slot("pods"), Ok(pod_counts(&t))));
        if t.rows.len() <= FULL_POD_LIMIT {
            let details = pod_details(&client, &ns, &context).await;
            let (restarts, hot) = details.clone().unwrap_or_default();
            send(Piece::Restarts(restarts));
            send(Piece::Hot(hot, false));
            if let Err(e) = details {
                send(Piece::Failed(Section::Pods, e));
            }
        } else {
            send(Piece::Restarts(table_restarts(&t, &context)));
            send(Piece::Hot(Vec::new(), true));
        }
    };
    let ready_ratio = |ar, kind: &'static str| {
        let client = c();
        let ns = ns.clone();
        async move {
            let t = table(client, ar, ns, false).await;
            send(Piece::Kind(
                slot(kind),
                t.map(|t| {
                    tally(t.rows.iter().map(|r| {
                        let (ready, desired) = t.ratio(r, "Ready");
                        ready_state(ready, desired)
                    }))
                }),
            ));
        }
    };
    let ready_columns = |ar, kind: &'static str| {
        let client = c();
        let ns = ns.clone();
        async move {
            let t = table(client, ar, ns, false).await;
            send(Piece::Kind(
                slot(kind),
                t.map(|t| {
                    tally(t.rows.iter().map(|r| {
                        let desired = t.cell(r, "Desired").parse().unwrap_or(0);
                        let ready = t.cell(r, "Ready").parse().unwrap_or(0);
                        ready_state(ready, desired)
                    }))
                }),
            ));
        }
    };
    let helm = async {
        let releases = k8s::list_labels(c(), k8s::ar_of::<core::Secret>(), ns.clone(), "owner=helm".into())
            .await
            .map_err(|e| k8s::lists::load_error(&e));
        send(Piece::Kind(slot("secrets"), releases.map(|r| helm_counts(&r))));
    };
    let cronjobs = async {
        let t = table(c(), k8s::ar_of::<batch::CronJob>(), ns.clone(), false).await;
        send(Piece::Kind(
            slot("cronjobs"),
            t.map(|t| {
                tally(t.rows.iter().map(|r| {
                    if t.cell(r, "Suspend") == "True" {
                        ("Suspended", Tone::Neutral)
                    } else {
                        ("Scheduled", Tone::Good)
                    }
                }))
            }),
        ));
    };
    let jobs = async {
        let t = table(c(), k8s::ar_of::<batch::Job>(), ns.clone(), false).await;
        send(Piece::Kind(
            slot("jobs"),
            t.map(|t| {
                tally(t.rows.iter().map(|r| {
                    let (done, want) = t.ratio(r, "Completions");
                    match t.cell(r, "Status") {
                        "Complete" => ("Complete", Tone::Neutral),
                        "Failed" => ("Failed", Tone::Bad),
                        "Running" => ("Running", Tone::Good),
                        "Suspended" => ("Suspended", Tone::Neutral),
                        // Older servers have no Status column.
                        _ if want > 0 && done >= want => ("Complete", Tone::Neutral),
                        _ => ("Running", Tone::Good),
                    }
                }))
            }),
        ));
    };
    let hpas = async {
        let hpas = list(&client, k8s::ar_of::<autoscaling::HorizontalPodAutoscaler>(), &ns).await;
        send(Piece::Kind(slot("horizontalpodautoscalers"), hpas.map(|hpas| hpa_counts(&hpas))));
    };
    let pdbs = async {
        let pdbs = list(&client, k8s::ar_of::<policy::PodDisruptionBudget>(), &ns).await;
        send(Piece::Kind(
            slot("poddisruptionbudgets"),
            pdbs.map(|pdbs| {
                tally(pdbs.iter().map(|p| {
                    if n(p, "/status/disruptionsAllowed") > 0 {
                        ("DisruptionAllowed", Tone::Good)
                    } else {
                        ("InsufficientPods", Tone::Bad)
                    }
                }))
            }),
        ));
    };
    let pvcs = async {
        let t = table(c(), k8s::ar_of::<core::PersistentVolumeClaim>(), ns.clone(), false).await;
        send(Piece::Kind(
            slot("persistentvolumeclaims"),
            t.map(|t| {
                tally(t.rows.iter().map(|r| match t.cell(r, "Status") {
                    "Bound" => ("Bound", Tone::Good),
                    "Pending" => ("Pending", Tone::Warn),
                    _ => ("Lost", Tone::Bad),
                }))
            }),
        ));
    };
    let warnings = async {
        let events =
            k8s::list_values_fields(c(), k8s::ar_of::<core::Event>(), ns.clone(), None, Some("type=Warning".into()))
                .await
                .map_err(|e| k8s::lists::load_error(&e));
        send(Piece::Warnings(events.as_deref().map(recent_warnings).unwrap_or_default()));
        if let Err(e) = events {
            send(Piece::Failed(Section::Warnings, e));
        }
    };
    futures::join!(
        pods,
        ready_ratio(k8s::ar_of::<apps::Deployment>(), "deployments"),
        helm,
        ready_columns(k8s::ar_of::<apps::ReplicaSet>(), "replicasets"),
        ready_ratio(k8s::ar_of::<apps::StatefulSet>(), "statefulsets"),
        ready_columns(k8s::ar_of::<apps::DaemonSet>(), "daemonsets"),
        cronjobs,
        jobs,
        hpas,
        pdbs,
        pvcs,
        warnings,
    );
    send(Piece::Done(started.elapsed()));
}

/// Helm releases by the status of their latest revision.
fn helm_counts(releases: &[(Option<String>, BTreeMap<String, String>)]) -> Vec<(String, usize, Tone)> {
    // Helm keeps every revision; count only the latest per release.
    let mut latest: BTreeMap<(String, String), (i64, String)> = BTreeMap::new();
    for (namespace, labels) in releases {
        let get = |k: &str| labels.get(k).cloned().unwrap_or_default();
        let key = (namespace.clone().unwrap_or_default(), get("name"));
        let version: i64 = get("version").parse().unwrap_or(0);
        let status = get("status");
        let e = latest.entry(key).or_insert((version, status.clone()));
        if version >= e.0 {
            *e = (version, status);
        }
    }
    tally(latest.values().map(|(_, status)| match status.as_str() {
        "deployed" => ("Deployed", Tone::Good),
        "failed" => ("Failed", Tone::Bad),
        "superseded" | "uninstalled" => ("Superseded", Tone::Neutral),
        _ => ("Pending", Tone::Warn),
    }))
}

/// Warning events of the last hour, by reason, most frequent first.
fn recent_warnings(events: &[Value]) -> Vec<Warning> {
    let hour_ago = jiff::Timestamp::now() - jiff::SignedDuration::from_secs(3600);
    let mut by_reason: BTreeMap<String, Warning> = BTreeMap::new();
    for e in events {
        let Some(last) =
            ts(e, "/lastTimestamp").or_else(|| ts(e, "/eventTime")).or_else(|| ts(e, "/metadata/creationTimestamp"))
        else {
            continue;
        };
        if last < hour_ago {
            continue;
        }
        let w = by_reason.entry(st(e, "/reason").to_string()).or_insert(Warning {
            reason: st(e, "/reason").into(),
            count: 0,
            last,
        });
        w.count += n(e, "/count").max(1);
        w.last = w.last.max(last);
    }
    let mut warnings: Vec<Warning> = by_reason.into_values().collect();
    warnings.sort_by_key(|w| std::cmp::Reverse(w.count));
    warnings
}

/// HPAs by whether they are scaling freely, limited, or unknown.
fn hpa_counts(hpas: &[Value]) -> Vec<(String, usize, Tone)> {
    tally(hpas.iter().map(|h| {
        let cond = |t: &str| {
            h.pointer("/status/conditions")
                .and_then(Value::as_array)
                .and_then(|cs| cs.iter().find(|c| c["type"].as_str() == Some(t)))
                .and_then(|c| c["status"].as_str())
                .unwrap_or("Unknown")
                .to_string()
        };
        if cond("ScalingLimited") == "True" {
            ("ScalingLimited", Tone::Warn)
        } else if cond("ScalingActive") == "True" {
            ("AtTarget", Tone::Good)
        } else {
            ("Unknown", Tone::Warn)
        }
    }))
}

/// "forbidden" / "failed: …" next to a section's title, when its list could not be read.
fn section_error(errors: &[String], color: Hsla) -> Option<Div> {
    (!errors.is_empty()).then(|| div().text_xs().text_color(color).child(errors.join(", ")))
}

/// Usage for every namespace in one call.
async fn all_usage(client: &Client) -> ContainerUsage {
    let client = client.clone();
    k8s::run(async move {
        let list: Value =
            client.request(http::Request::get("/apis/metrics.k8s.io/v1beta1/pods").body(Vec::new())?).await?;
        let mut out = ContainerUsage::new();
        for item in list["items"].as_array().into_iter().flatten() {
            let pod = st(item, "/metadata/name").to_string();
            let entry = out.entry(pod).or_default();
            for c in item["containers"].as_array().into_iter().flatten() {
                let cpu = c.pointer("/usage/cpu").and_then(Value::as_str).map(k8s::parse_cpu).unwrap_or(0.0);
                let mem = c.pointer("/usage/memory").and_then(Value::as_str).map(k8s::parse_bytes).unwrap_or(0.0);
                entry.insert(st(c, "/name").to_string(), (cpu, mem));
            }
        }
        Ok(out)
    })
    .await
    .unwrap_or_default()
}

impl Dashboard {
    fn tone(&self, t: Tone, cx: &App) -> Hsla {
        let th = cx.theme();
        match t {
            Tone::Good => th.success,
            Tone::Neutral => th.muted_foreground,
            Tone::Warn => th.warning,
            Tone::Bad => th.danger,
        }
    }

    fn open_link(
        &self,
        text: String,
        kind: &str,
        name: String,
        ns: String,
        context: String,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let kind = kind.to_string();
        div()
            .id(SharedString::from(format!("dash-{kind}-{ns}-{name}")))
            .text_color(cx.theme().blue)
            .cursor_pointer()
            .hover(|s| s.underline())
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(OpenObject {
                    kind: kind.clone(),
                    name: name.clone(),
                    namespace: Some(ns.clone()),
                    context: Some(context.clone()),
                })
            }))
            .child(text)
    }
}

impl Render for Dashboard {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let data = self.merged();
        let data = &data;

        let bars = h_flex().flex_wrap().items_start().gap_4().children(data.kinds.iter().map(|k| {
            let total: usize = k.counts.iter().map(|c| c.1).sum();
            let segments = k.counts.iter().map(|(_, n, tone)| {
                div().h_full().bg(self.tone(*tone, cx)).w(relative(*n as f32 / total.max(1) as f32))
            });
            v_flex()
                .w(px(170.))
                .gap_1()
                .child(div().text_base().child(k.title))
                .child(
                    h_flex().h(px(12.)).w_full().rounded_sm().overflow_hidden().bg(theme.secondary).children(segments),
                )
                .child(
                    v_flex().text_xs().children(
                        k.counts.iter().map(|(label, n, tone)| {
                            div().text_color(self.tone(*tone, cx)).child(format!("{n} {label}"))
                        }),
                    ),
                )
                .when(!k.errors.is_empty(), |d| {
                    d.child(div().text_xs().text_color(theme.danger).child(k.errors.join(", ")))
                })
                .when(total == 0 && !k.loading && k.errors.is_empty(), |d| {
                    d.child(div().text_xs().text_color(muted).child("None"))
                })
                .when(k.loading, |d| d.child(div().text_xs().text_color(muted).child("Loading…")))
                .into_any_element()
        }));

        let warnings = v_flex()
            .w(px(380.))
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_baseline()
                    .child(div().text_base().child("Recent warnings"))
                    .child(
                        // Event counts are cumulative since first seen, so count reasons, not occurrences.
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(format!("{} reasons active in the last hour", data.warnings.len())),
                    )
                    .children(section_error(&data.warnings_errors, theme.danger)),
            )
            .child(
                v_flex()
                    .p_2()
                    .min_h(px(160.))
                    .rounded_md()
                    .border_1()
                    .border_dashed()
                    .border_color(theme.border)
                    .when(data.warnings.is_empty() && !data.loading_details && data.warnings_errors.is_empty(), |d| {
                        d.child(div().text_sm().text_color(muted).child("No warnings in the last hour."))
                    })
                    .when(data.warnings.is_empty() && data.loading_details, |d| {
                        d.child(div().text_sm().text_color(muted).child("Loading…"))
                    })
                    .children(data.warnings.iter().take(12).map(|w| {
                        h_flex()
                            .gap_1()
                            .text_sm()
                            .child(div().text_color(theme.warning).child(w.reason.clone()))
                            .child(div().text_xs().text_color(muted).child(format!("×{} total", w.count)))
                            .child(div().text_xs().text_color(muted).child(format!("{} ago", k8s::age(w.last))))
                    })),
            );

        let (danger, border) = (theme.danger, theme.border);
        let restarts_rows: Vec<AnyElement> = data
            .restarts
            .iter()
            .take(12)
            .map(|r| {
                h_flex()
                    .gap_2()
                    .text_sm()
                    .child(div().text_color(muted).child(if self.multi {
                        format!("{} · {} /", r.context, r.namespace)
                    } else {
                        format!("{} /", r.namespace)
                    }))
                    .child(self.open_link(
                        r.pod.clone(),
                        "pods",
                        r.pod.clone(),
                        r.namespace.clone(),
                        r.context.clone(),
                        cx,
                    ))
                    .child(div().text_color(danger).text_xs().child(r.reason.clone()))
                    .child(div().text_xs().child(if r.container.is_empty() {
                        format!("(×{})", r.count)
                    } else {
                        format!("(×{}) {}", r.count, r.container)
                    }))
                    .child(div().text_xs().text_color(muted).child(format!("{} ago", k8s::age(r.when))))
                    .into_any_element()
            })
            .collect();
        let restarts = v_flex()
            .flex_1()
            .min_w(px(380.))
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_baseline()
                    .child(div().text_base().child("Recent restarts"))
                    .children(section_error(&data.pods_errors, danger)),
            )
            .child(
                v_flex()
                    .p_2()
                    .min_h(px(160.))
                    .rounded_md()
                    .border_1()
                    .border_dashed()
                    .border_color(theme.border)
                    .when(restarts_rows.is_empty() && data.pods_errors.is_empty(), |d| {
                        d.items_center().justify_center().child(div().text_sm().text_color(muted).child(
                            if data.loading_details {
                                "Loading…"
                            } else {
                                "No containers have restarted in the last hour."
                            },
                        ))
                    })
                    .children(restarts_rows),
            );

        let hot_rows: Vec<AnyElement> = data
            .hot
            .iter()
            .map(|h| {
                let fmt = |(v, l): (f64, Option<f64>), f: fn(f64) -> String| match l {
                    Some(l) => format!("{} / {}", f(v), f(l)),
                    None => f(v),
                };
                h_flex()
                    .gap_3()
                    .py_1()
                    .text_sm()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        h_flex()
                            .w(px(420.))
                            .gap_1()
                            .child(div().text_color(muted).child(if self.multi {
                                format!("{} · {} /", h.context, h.namespace)
                            } else {
                                format!("{} /", h.namespace)
                            }))
                            .child(self.open_link(
                                h.pod.clone(),
                                "pods",
                                h.pod.clone(),
                                h.namespace.clone(),
                                h.context.clone(),
                                cx,
                            )),
                    )
                    .child(div().w(px(160.)).child(h.container.clone()))
                    .child(div().w(px(160.)).child(fmt(h.cpu, k8s::format_cpu)))
                    .child(div().w(px(160.)).child(fmt(h.mem, k8s::format_bytes)))
                    .into_any_element()
            })
            .collect();
        let theme = cx.theme();
        let hot = v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_baseline()
                    .child(div().text_base().child("Abnormal resource usage"))
                    .children(section_error(&data.pods_errors, theme.danger)),
            )
            .child(div().text_xs().text_color(muted).child("Containers using more than 95% of a CPU or memory limit"))
            .child(
                h_flex()
                    .gap_3()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().w(px(420.)).child("Pod"))
                    .child(div().w(px(160.)).child("Container"))
                    .child(div().w(px(160.)).child("CPU"))
                    .child(div().w(px(160.)).child("Memory")),
            )
            .when(data.hot_skipped, |d| {
                d.child(div().py_1().text_xs().text_color(theme.warning).child(format!(
                    "More than {FULL_POD_LIMIT} pods: limits not checked everywhere. Pick a namespace to see them."
                )))
            })
            .when(hot_rows.is_empty() && !data.hot_skipped && data.pods_errors.is_empty(), |d| {
                d.child(div().py_2().text_sm().text_color(muted).child(if data.loading_details {
                    "Loading…"
                } else {
                    "Nothing close to its limits."
                }))
            })
            .children(hot_rows);

        v_flex()
            .id("dashboard")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_8()
            .child(bars)
            .child(h_flex().items_start().gap_6().child(warnings).child(restarts))
            .child(hot)
            .into_any_element()
    }
}

/// Timing aid for very large clusters (`RECOUVRANCE_SMOKE=ctx RECOUVRANCE_DASH=1`).
#[cfg(feature = "dev")]
pub async fn debug_load(client: Client) -> bool {
    let (tx, _rx) = futures::channel::mpsc::unbounded();
    load(client, None, "debug".into(), tx).await;
    true
}

#[cfg(test)]
mod tests {
    use super::{Tbl, Tone, pod_counts, restart_ago};
    use crate::k8s;

    #[test]
    fn pod_counts_follow_status_tones() {
        let defs = vec![k8s::ColumnDef { name: "Status".into(), priority: 0 }];
        let row = |status: &str| {
            std::sync::Arc::new(k8s::Row {
                uid: status.into(),
                name: status.into(),
                namespace: None,
                created: None,
                cells: vec![status.to_string()],
                labels: "".into(),
                deleting: false,
                deleted_at: None,
            })
        };
        let statuses = ["Running", "Succeeded", "Init:0/1", "Init:Error", "Pending", "NotReady", "Unready", ""];
        let t = Tbl { defs, rows: statuses.iter().map(|s| row(s)).collect() };
        let mut counts = pod_counts(&t);
        counts.sort_by(|a, b| a.0.cmp(&b.0));
        let want = [
            ("Completed", Tone::Neutral),
            ("Init:Error", Tone::Bad),
            ("Initializing", Tone::Warn),
            ("NotReady", Tone::Bad),
            ("Pending", Tone::Warn),
            ("Running", Tone::Good),
            ("Unknown", Tone::Warn),
            ("Unready", Tone::Bad),
        ];
        let got: Vec<(&str, Tone)> = counts.iter().map(|(l, _, t)| (l.as_str(), *t)).collect();
        assert_eq!(got, want);
    }

    #[test]
    fn parses_restart_ages() {
        assert_eq!(restart_ago("3 (12m ago)").unwrap().as_secs(), 720);
        assert_eq!(restart_ago("1 (2d4h ago)").unwrap().as_secs(), 2 * 86400 + 4 * 3600);
        assert!(restart_ago("0").is_none());
    }
}
