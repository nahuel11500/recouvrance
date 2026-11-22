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

use crate::details::OpenObject;
use crate::k8s;
use crate::status::Tone;

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
    /// Containers near their limits: metrics-server's usage.
    Usage,
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
    /// What its errors are about: the cluster, or the namespace in it.
    context: String,
    kinds: Vec<Option<Result<Counts, String>>>,
    warnings: Option<Vec<Warning>>,
    restarts: Option<Vec<Restart>>,
    hot: Option<(Vec<Hot>, bool)>,
    /// Why the warnings, or the pods behind restarts and limits, could not be listed.
    warnings_error: Option<String>,
    pods_error: Option<String>,
    /// Why the usage set against limits could not be read.
    usage_error: Option<String>,
}

/// Warning events of one reason: how many times, and when last.
type Warning = k8s::WarningReason;

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
    /// Clusters whose usage could not be read: their limits were not checked.
    usage_errors: Vec<String>,
}

pub struct Dashboard {
    /// One per list: a cluster, or one of the namespaces chosen there.
    clusters: Vec<ClusterState>,
    /// Several clusters: rows say which one.
    multi: bool,
    /// Several lists: errors say which one.
    labelled: bool,
    _tasks: Vec<Task<()>>,
}

impl EventEmitter<OpenObject> for Dashboard {}

impl Dashboard {
    pub fn new(scope: crate::ui::Scope, cx: &mut Context<Self>) -> Self {
        let mut dash = Self { clusters: Vec::new(), multi: false, labelled: false, _tasks: Vec::new() };
        dash.set_scope(scope, cx);
        dash
    }

    /// Show other clusters or namespaces: each namespace chosen is listed by itself, and
    /// what they hold added up per cluster.
    pub fn set_scope(&mut self, scope: crate::ui::Scope, cx: &mut Context<Self>) {
        self.multi = scope.len() > 1;
        let lists = crate::ui::scope_lists(scope);
        self.labelled = lists.len() > 1;
        // Replacing the tasks stops the old lists: their results never land over these.
        self._tasks = Vec::new();
        for (ix, (_, ctx, client, namespace)) in lists.iter().cloned().enumerate() {
            self._tasks.push(cx.spawn(async move |this, cx| {
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
                    let ((), took) = futures::join!(load, pieces);
                    // Big clusters take long to list; don't hammer their API servers.
                    let wait = (took * 2).max(crate::settings::secs(crate::settings::get().overview_refresh));
                    cx.background_executor().timer(wait).await;
                }
            }));
        }
        self.clusters = lists
            .into_iter()
            .map(|(label, ..)| ClusterState { context: label, kinds: vec![None; SLOTS.len()], ..Default::default() })
            .collect();
        cx.notify();
    }

    fn apply(&mut self, ix: usize, piece: Piece, cx: &mut Context<Self>) {
        let Some(state) = self.clusters.get_mut(ix) else { return };
        match piece {
            Piece::Kind(slot, counts) => state.kinds[slot] = Some(counts),
            // Data arriving clears an earlier failure; a failure follows its (empty) data.
            Piece::Warnings(w) => (state.warnings, state.warnings_error) = (Some(w), None),
            Piece::Restarts(r) => (state.restarts, state.pods_error) = (Some(r), None),
            Piece::Hot(h, skipped) => (state.hot, state.usage_error) = (Some((h, skipped)), None),
            Piece::Failed(Section::Warnings, e) => state.warnings_error = Some(e),
            Piece::Failed(Section::Pods, e) => state.pods_error = Some(e),
            Piece::Failed(Section::Usage, e) => state.usage_error = Some(e),
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
        let labelled = self.labelled;
        // Which cluster or namespace an error is about, when there are several.
        let about = |ctx: &str, e: &str| if labelled { format!("{ctx}: {e}") } else { e.to_string() };
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
            out.usage_errors.extend(state.usage_error.iter().map(|e| about(&state.context, e)));
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

/// Objects of a kind with only the fields `T` keeps, from the API server's cache, or why they
/// could not be listed ("forbidden", "failed: …").
async fn read_lean<T>(client: &Client, ar: kube::discovery::ApiResource, ns: Option<&str>) -> Result<Vec<T>, String>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    k8s::lists::list_lean_cached(client.clone(), ar, ns.map(String::from)).await.map_err(|e| k8s::lists::load_error(&e))
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
    // The same reading of a kubectl duration as the table's Age column.
    k8s::age_seconds(inner).map(jiff::SignedDuration::from_secs)
}

/// kubectl's table for one kind: light even on very large clusters.
struct Tbl {
    defs: Vec<k8s::ColumnDef>,
    rows: Vec<std::sync::Arc<k8s::Row>>,
}

impl Tbl {
    fn cell<'a>(&self, row: &'a k8s::Row, name: &str) -> &'a str {
        self.defs.iter().position(|d| d.name == name).and_then(|i| row.cells.get(i)).map_or_default(String::as_str)
    }

    /// "3/5" -> (3, 5)
    fn ratio(&self, row: &k8s::Row, name: &str) -> (i64, i64) {
        let c = self.cell(row, name);
        let (a, b) = c.split_once('/').unwrap_or((c, c));
        (a.trim().parse().unwrap_or(0), b.trim().parse().unwrap_or(0))
    }
}

/// kubectl's table for a kind, or why it could not be listed ("forbidden", "failed: …").
/// A kind's rows for its counts, from their cells alone; `named_over`: rows need their
/// namespace when there are more than that many.
async fn table(
    client: Client,
    ar: kube::discovery::ApiResource,
    ns: Option<String>,
    named_over: Option<usize>,
) -> Result<Tbl, String> {
    #[cfg(feature = "dev")]
    let (started, plural) = (std::time::Instant::now(), ar.plural.clone());
    let kind = k8s::Kind { ar, namespaced: true, section: "", label: String::new() };
    // Counts and restarts read the cells only: no object's metadata, however small the list.
    let result = k8s::list_cells(client, kind, ns, named_over).await;
    #[cfg(feature = "dev")]
    if std::env::var("RECOUVRANCE_DEBUG").is_ok() {
        eprintln!("table {plural}: {:?} rows={:?}", started.elapsed(), result.as_ref().map(|r| r.1.len()));
    }
    result.map(|(defs, rows)| Tbl { defs, rows }).map_err(|e| k8s::lists::load_error(&e))
}

fn ready_state(ready: i64, desired: i64) -> (&'static str, Tone) {
    if desired == 0 {
        ("Idle", Tone::Done)
    } else if ready >= desired {
        ("Running", Tone::Good)
    } else {
        ("Unavailable", Tone::Bad)
    }
}

fn pod_counts(pods: &Tbl) -> Vec<(String, usize, Tone)> {
    // kubectl's Status column says *why* a pod is unhealthy (CrashLoopBackOff, …).
    // Colors as in the pod list; a status we do not know is a failure reason here.
    tally(pods.rows.iter().map(|r| {
        let status = pods.cell(r, "Status");
        let tone = crate::status::tone(status);
        match (status, tone) {
            ("", _) => ("Unknown".to_string(), Tone::Busy),
            ("Completed" | "Succeeded", _) => ("Completed".to_string(), Tone::Done),
            (s, Some(Tone::Busy)) if s.starts_with("Init:") => ("Initializing".to_string(), Tone::Busy),
            (s, tone) => (s.to_string(), tone.unwrap_or(Tone::Bad)),
        }
    }))
}

/// The parts of a pod `pod_details` reads: managed fields, env, volumes, probes and the rest
/// are skipped while parsing, never held, which matters at thousands of pods.
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
    containers: Vec<ContainerLite>,
}

/// A container's name and limits.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct ContainerLite {
    name: String,
    resources: k8s::podspec::LeanResources,
}

#[derive(serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct PodStatusLite {
    container_statuses: Vec<ContainerStatusLite>,
}

/// A container's restarts, and how it last ended.
#[derive(serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ContainerStatusLite {
    name: String,
    restart_count: i64,
    last_state: LastState,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct LastState {
    terminated: Option<Terminated>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Terminated {
    finished_at: Option<String>,
    reason: Option<String>,
}

/// What `pod_details` found: restarts, containers near their limits, and why usage could
/// not be read (limits then unchecked, not fine).
type Details = (Vec<Restart>, Vec<Hot>, Option<String>);

/// Restarts in the last hour and containers near their limits, from pod specs.
async fn pod_details(client: &Client, ns: Option<&str>, context: &str) -> Result<Details, String> {
    let usage = async {
        if !crate::settings::metrics_on(context) {
            return Ok(k8s::PodContainerUsage::new());
        }
        k8s::pod_usage(client.clone(), ns.map(String::from), None).await.map(|(u, _)| u)
    };
    // From the API server's cache: a moment old at worst, which the next refresh catches up.
    let pods = read_lean::<PodDetail>(client, k8s::ar_of::<core::Pod>(), ns);
    let (pods, usage) = futures::join!(pods, usage);
    let pods = pods?;
    let (usage, usage_error) = match usage {
        Ok(u) => (u, None),
        Err(e) => (k8s::PodContainerUsage::new(), Some(unchecked_why(&e))),
    };
    let hour_ago = jiff::Timestamp::now() - jiff::SignedDuration::from_secs(3600);
    let (restarts, hot) = restarts_and_hot(&pods, &usage, context, hour_ago);
    Ok((restarts, hot, usage_error))
}

/// The containers of `pods` that ended at `since` or later, newest first, and those using more
/// than 95% of a CPU or memory limit by `usage`.
fn restarts_and_hot(
    pods: &[PodDetail],
    usage: &k8s::PodContainerUsage,
    context: &str,
    since: jiff::Timestamp,
) -> (Vec<Restart>, Vec<Hot>) {
    let (mut restarts, mut hot) = (Vec::new(), Vec::new());
    for p in pods {
        let key = (p.metadata.namespace.clone(), p.metadata.name.clone());
        let usage = usage.get(&key);
        let (namespace, pod) = key;
        for c in &p.status.container_statuses {
            let Some(end) = &c.last_state.terminated else { continue };
            let ended = end.finished_at.as_deref().and_then(|t| t.parse::<jiff::Timestamp>().ok());
            let Some(when) = ended.filter(|t| *t >= since) else { continue };
            restarts.push(Restart {
                context: context.to_string(),
                namespace: namespace.clone(),
                pod: pod.clone(),
                container: c.name.clone(),
                reason: end.reason.clone().unwrap_or_default(),
                count: c.restart_count,
                when,
            });
        }
        for c in &p.spec.containers {
            let Some(&(cpu, mem)) = usage.and_then(|u| u.get(&c.name)) else { continue };
            let cpu_limit = c.resources.limits.get("cpu").map(String::as_str).map(k8s::parse_cpu);
            let mem_limit = c.resources.limits.get("memory").map(String::as_str).map(k8s::parse_bytes);
            let near = |v: f64, l: Option<f64>| l.is_some_and(|l| l > 0.0 && v >= 0.95 * l);
            if near(cpu, cpu_limit) || near(mem, mem_limit) {
                hot.push(Hot {
                    context: context.to_string(),
                    namespace: namespace.clone(),
                    pod: pod.clone(),
                    container: c.name.clone(),
                    cpu: (cpu, cpu_limit),
                    mem: (mem, mem_limit),
                });
            }
        }
    }
    restarts.sort_by_key(|r| std::cmp::Reverse(r.when));
    (restarts, hot)
}

/// Why limits were not checked: metrics-server missing, unavailable, refused or failing.
fn unchecked_why(e: &anyhow::Error) -> String {
    match k8s::api_status(e) {
        Some(404 | 503) => format!("limits not checked: {}", k8s::metrics_error(e)),
        _ => format!("limits not checked: usage {}", k8s::lists::load_error(e)),
    }
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
        // Beyond `FULL_POD_LIMIT` pods, restarts come from the rows: they need their namespace.
        let t = match table(c(), k8s::ar_of::<core::Pod>(), ns.clone(), Some(FULL_POD_LIMIT)).await {
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
            match pod_details(&client, ns.as_deref(), &context).await {
                Ok((restarts, hot, usage_error)) => {
                    send(Piece::Restarts(restarts));
                    send(Piece::Hot(hot, false));
                    if let Some(e) = usage_error {
                        send(Piece::Failed(Section::Usage, e));
                    }
                }
                Err(e) => {
                    send(Piece::Restarts(Vec::new()));
                    send(Piece::Hot(Vec::new(), false));
                    send(Piece::Failed(Section::Pods, e));
                }
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
            let t = table(client, ar, ns, None).await;
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
            let t = table(client, ar, ns, None).await;
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
        // The current revisions only, as the Helm page lists them: a release's newest revision is
        // never superseded, and the older ones are most of Helm's Secrets.
        let releases = k8s::list_labels(c(), k8s::ar_of::<core::Secret>(), ns.clone(), crate::helm::CURRENT.into())
            .await
            .map_err(|e| k8s::lists::load_error(&e));
        send(Piece::Kind(slot("secrets"), releases.map(|r| helm_counts(&r))));
    };
    let cronjobs = async {
        let t = table(c(), k8s::ar_of::<batch::CronJob>(), ns.clone(), None).await;
        send(Piece::Kind(
            slot("cronjobs"),
            t.map(|t| {
                tally(t.rows.iter().map(|r| {
                    if t.cell(r, "Suspend") == "True" { ("Suspended", Tone::Done) } else { ("Scheduled", Tone::Good) }
                }))
            }),
        ));
    };
    let jobs = async {
        let t = table(c(), k8s::ar_of::<batch::Job>(), ns.clone(), None).await;
        send(Piece::Kind(
            slot("jobs"),
            t.map(|t| {
                tally(t.rows.iter().map(|r| {
                    let (done, want) = t.ratio(r, "Completions");
                    match t.cell(r, "Status") {
                        "Complete" => ("Complete", Tone::Done),
                        "Failed" => ("Failed", Tone::Bad),
                        "Running" => ("Running", Tone::Good),
                        "Suspended" => ("Suspended", Tone::Done),
                        // Older servers have no Status column.
                        _ if want > 0 && done >= want => ("Complete", Tone::Done),
                        _ => ("Running", Tone::Good),
                    }
                }))
            }),
        ));
    };
    let hpas = async {
        let hpas =
            read_lean::<HpaLite>(&client, k8s::ar_of::<autoscaling::HorizontalPodAutoscaler>(), ns.as_deref()).await;
        send(Piece::Kind(slot("horizontalpodautoscalers"), hpas.map(|hpas| hpa_counts(&hpas))));
    };
    let pdbs = async {
        let pdbs = read_lean::<PdbLite>(&client, k8s::ar_of::<policy::PodDisruptionBudget>(), ns.as_deref()).await;
        send(Piece::Kind(slot("poddisruptionbudgets"), pdbs.map(|pdbs| pdb_counts(&pdbs))));
    };
    let pvcs = async {
        let t = table(c(), k8s::ar_of::<core::PersistentVolumeClaim>(), ns.clone(), None).await;
        send(Piece::Kind(
            slot("persistentvolumeclaims"),
            t.map(|t| {
                tally(t.rows.iter().map(|r| match t.cell(r, "Status") {
                    "Bound" => ("Bound", Tone::Good),
                    "Pending" => ("Pending", Tone::Busy),
                    _ => ("Lost", Tone::Bad),
                }))
            }),
        ));
    };
    let warnings = async {
        let hour_ago = jiff::Timestamp::now() - jiff::SignedDuration::from_secs(3600);
        match k8s::warning_reasons(c(), ns.clone(), hour_ago).await {
            Ok(reasons) => send(Piece::Warnings(reasons)),
            Err(e) => {
                send(Piece::Warnings(Vec::new()));
                send(Piece::Failed(Section::Warnings, k8s::lists::load_error(&e)));
            }
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
        "superseded" | "uninstalled" => ("Superseded", Tone::Done),
        _ => ("Pending", Tone::Busy),
    }))
}

/// An HPA's conditions: all the overview reads of it.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct HpaLite {
    status: HpaStatusLite,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct HpaStatusLite {
    conditions: Vec<ConditionLite>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct ConditionLite {
    #[serde(rename = "type")]
    type_: String,
    status: String,
}

/// HPAs by whether they are scaling freely, limited, or unknown.
fn hpa_counts(hpas: &[HpaLite]) -> Vec<(String, usize, Tone)> {
    tally(hpas.iter().map(|h| {
        let cond = |t: &str| h.status.conditions.iter().find(|c| c.type_ == t).map_or("Unknown", |c| c.status.as_str());
        if cond("ScalingLimited") == "True" {
            ("ScalingLimited", Tone::Busy)
        } else if cond("ScalingActive") == "True" {
            ("AtTarget", Tone::Good)
        } else {
            ("Unknown", Tone::Busy)
        }
    }))
}

/// A budget's status numbers: all the overview reads of it.
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct PdbLite {
    status: PdbStatusLite,
}

#[derive(serde::Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct PdbStatusLite {
    disruptions_allowed: Option<i64>,
    expected_pods: Option<i64>,
}

/// PodDisruptionBudgets by whether they block evictions now. One that covers no pods blocks nothing.
fn pdb_counts(pdbs: &[PdbLite]) -> Vec<(String, usize, Tone)> {
    tally(pdbs.iter().map(|p| {
        let (allowed, expected) = (p.status.disruptions_allowed.unwrap_or(0), p.status.expected_pods.unwrap_or(0));
        if crate::overview::blocks_evictions(allowed, expected) {
            ("InsufficientPods", Tone::Bad)
        } else if expected == 0 {
            ("NoPods", Tone::Done)
        } else {
            ("DisruptionAllowed", Tone::Good)
        }
    }))
}

/// "forbidden" / "failed: …" next to a section's title, when its list could not be read.
fn section_error(errors: &[String], color: Hsla) -> Option<Div> {
    (!errors.is_empty()).then(|| div().text_xs().text_color(color).child(errors.join(", ")))
}

impl Dashboard {
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
            .hover(Styled::underline)
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(OpenObject {
                    kind: kind.clone(),
                    name: name.clone(),
                    namespace: Some(ns.clone()),
                    context: Some(context.clone()),
                });
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
            let segments = k
                .counts
                .iter()
                .map(|(_, n, tone)| div().h_full().bg(tone.color(cx)).w(relative(*n as f32 / total.max(1) as f32)));
            v_flex()
                .w(px(170.))
                .gap_1()
                .child(div().text_base().child(k.title))
                .child(
                    h_flex().h(px(12.)).w_full().rounded_sm().overflow_hidden().bg(theme.secondary).children(segments),
                )
                .child(
                    v_flex().text_xs().children(
                        k.counts
                            .iter()
                            .map(|(label, n, tone)| div().text_color(tone.color(cx)).child(format!("{n} {label}"))),
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
                        div().text_xs().text_color(muted).child(format!(
                            "{} active in the last hour",
                            crate::plural::count(data.warnings.len(), "reason")
                        )),
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
                    .children(section_error(&data.pods_errors, theme.danger))
                    .children(section_error(&data.usage_errors, theme.warning)),
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
            // No all-clear while some usage could not be read.
            .when(
                hot_rows.is_empty() && !data.hot_skipped && data.pods_errors.is_empty() && data.usage_errors.is_empty(),
                |d| {
                    d.child(div().py_2().text_sm().text_color(muted).child(if data.loading_details {
                        "Loading…"
                    } else {
                        "Nothing close to its limits."
                    }))
                },
            )
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

/// Timing aid for very large clusters (`RECOUVRANCE_SMOKE=ctx RECOUVRANCE_DASH=1`): prints each
/// part of the overview as it arrives, with its figures (counts only, never a name), so two
/// builds can be compared on one cluster. Whether every part was read.
#[cfg(feature = "dev")]
pub async fn debug_load(client: Client) -> bool {
    let (tx, mut rx) = futures::channel::mpsc::unbounded();
    let started = std::time::Instant::now();
    let print = async {
        let mut ok = true;
        while let Some(piece) = futures::StreamExt::next(&mut rx).await {
            ok &= !matches!(piece, Piece::Failed(..) | Piece::Kind(_, Err(_)));
            println!("  {:>7.2} s  {}", started.elapsed().as_secs_f64(), describe(&piece));
        }
        ok
    };
    let ((), ok) = futures::join!(load(client, None, "debug".into(), tx), print);
    ok
}

/// One part of the overview in a line: a kind's counts, how many warnings, restarts and
/// containers near their limits there are, or what failed.
#[cfg(feature = "dev")]
fn describe(piece: &Piece) -> String {
    let counts = |c: &[(String, usize, Tone)]| {
        if c.is_empty() {
            return "none".to_string();
        }
        c.iter().map(|(label, n, _)| format!("{n} {label}")).collect::<Vec<_>>().join(", ")
    };
    match piece {
        Piece::Kind(slot, Ok(c)) => format!("{}: {}", SLOTS[*slot].0, counts(c)),
        Piece::Kind(slot, Err(e)) => format!("{}: {e}", SLOTS[*slot].0),
        Piece::Warnings(w) => {
            format!(
                "warnings: {} reasons active in the last hour, ×{} in all",
                w.len(),
                w.iter().map(|w| w.count).sum::<i64>()
            )
        }
        Piece::Restarts(r) => format!("restarts: {} in the last hour", r.len()),
        Piece::Hot(h, skipped) => {
            format!("near their limits: {}{}", h.len(), if *skipped { " (too many pods to check)" } else { "" })
        }
        Piece::Failed(section, e) => {
            let what = match section {
                Section::Warnings => "warnings",
                Section::Pods => "pods",
                Section::Usage => "usage",
            };
            format!("{what} failed: {e}")
        }
        Piece::Done(took) => format!("done in {took:?}"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use futures::StreamExt as _;
    use proptest::prelude::*;
    use serde_json::{Value, json};

    use super::{
        Counts, HpaLite, PdbLite, Piece, PodDetail, SLOTS, Tbl, Tone, helm_counts, hpa_counts, pdb_counts, pod_counts,
        restart_ago, restarts_and_hot, tally,
    };
    use crate::k8s;

    #[test]
    fn pdb_without_pods_is_not_bad() {
        let pdb = |allowed: i64, expected: i64| -> PdbLite {
            serde_json::from_value(json!({"status": {"disruptionsAllowed": allowed, "expectedPods": expected}}))
                .unwrap()
        };
        let mut counts = pdb_counts(&[pdb(0, 0), pdb(0, 2), pdb(1, 3)]);
        counts.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            counts,
            [
                ("DisruptionAllowed".to_string(), 1, Tone::Good),
                ("InsufficientPods".to_string(), 1, Tone::Bad),
                ("NoPods".to_string(), 1, Tone::Done),
            ]
        );
    }

    #[test]
    fn pod_counts_follow_status_tones() {
        let defs = vec![k8s::ColumnDef { name: "Status".into(), priority: 0 }];
        let row = |status: &str| {
            Arc::new(k8s::Row {
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
            ("Completed", Tone::Done),
            ("Init:Error", Tone::Bad),
            ("Initializing", Tone::Busy),
            ("NotReady", Tone::Bad),
            ("Pending", Tone::Busy),
            ("Running", Tone::Good),
            ("Unknown", Tone::Busy),
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

    /// How the overview counted whole HPAs before it read them lean.
    fn hpa_counts_of_values(hpas: &[Value]) -> Counts {
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
                ("ScalingLimited", Tone::Busy)
            } else if cond("ScalingActive") == "True" {
                ("AtTarget", Tone::Good)
            } else {
                ("Unknown", Tone::Busy)
            }
        }))
    }

    /// How the overview counted whole budgets before it read them lean.
    fn pdb_counts_of_values(pdbs: &[Value]) -> Counts {
        let n = |v: &Value, p: &str| v.pointer(p).and_then(Value::as_i64).unwrap_or(0);
        tally(pdbs.iter().map(|p| {
            if crate::overview::pdb_blocks(p) {
                ("InsufficientPods", Tone::Bad)
            } else if n(p, "/status/expectedPods") == 0 {
                ("NoPods", Tone::Done)
            } else {
                ("DisruptionAllowed", Tone::Good)
            }
        }))
    }

    /// An HPA as the API server sends it: conditions of these types and statuses (or without
    /// one), maybe no conditions or no status at all, and fields the overview never reads.
    fn whole_hpa() -> impl Strategy<Value = Value> {
        const TYPES: [&str; 4] = ["ScalingLimited", "ScalingActive", "AbleToScale", ""];
        const STATUSES: [&str; 4] = ["True", "False", "Unknown", ""];
        let condition = (0..TYPES.len(), 0..STATUSES.len()).prop_map(|(t, s)| {
            let mut c = json!({"reason": "R", "message": "m", "lastTransitionTime": "2026-01-01T00:00:00Z"});
            if !TYPES[t].is_empty() {
                c["type"] = TYPES[t].into();
            }
            if !STATUSES[s].is_empty() {
                c["status"] = STATUSES[s].into();
            }
            c
        });
        prop::option::of(prop::option::of(prop::collection::vec(condition, 0..4))).prop_map(|status| {
            let mut h = json!({"metadata": {"name": "web", "managedFields": [{"manager": "kube-controller-manager"}]},
                "spec": {"minReplicas": 1, "maxReplicas": 5}});
            match status {
                None => {}
                Some(None) => h["status"] = json!({"currentReplicas": 1}),
                Some(Some(conditions)) => h["status"] = json!({"currentReplicas": 1, "conditions": conditions}),
            }
            h
        })
    }

    /// A budget as the API server sends it, its numbers maybe missing, or its whole status.
    fn whole_pdb() -> impl Strategy<Value = Value> {
        (prop::option::of(0i64..3), prop::option::of(0i64..3), any::<bool>()).prop_map(|(allowed, expected, status)| {
            let mut p = json!({"metadata": {"name": "web"}, "spec": {"minAvailable": 1}});
            if status {
                p["status"] = json!({"currentHealthy": 1, "desiredHealthy": 1, "observedGeneration": 2});
                if let Some(a) = allowed {
                    p["status"]["disruptionsAllowed"] = a.into();
                }
                if let Some(e) = expected {
                    p["status"]["expectedPods"] = e.into();
                }
            }
            p
        })
    }

    /// 2026-01-01T03:00:00Z: containers of `pods_and_usage` ended in the two hours before.
    fn base() -> jiff::Timestamp {
        jiff::Timestamp::from_second(1_767_236_400).unwrap()
    }

    /// Pods as the API server sends them, with usage for some of their containers: containers
    /// `c0`..`c2` with or without limits, statuses that ended minutes before `base` (or never,
    /// or at a time that doesn't read), and fields the overview never reads.
    fn pods_and_usage() -> impl Strategy<Value = (Vec<Value>, k8s::PodContainerUsage)> {
        // (name, CPU limit in millicores, memory limit in MiB, usage of both)
        let container = (
            0usize..3,
            prop::option::of(1u32..2000),
            prop::option::of(1u32..2000),
            prop::option::of((0u32..2000, 0u32..2000)),
        );
        // (name, restarts, when it ended in minutes before `base`, its reason, whether that time reads)
        let status = (0usize..3, 0i64..20, prop::option::of((0i64..120, 0usize..3, any::<bool>())));
        let pod = (0usize..2, prop::collection::vec(container, 0..3), prop::collection::vec(status, 0..3));
        prop::collection::vec(pod, 0..6).prop_map(|pods| {
            const REASONS: [&str; 3] = ["OOMKilled", "Error", "Completed"];
            let mut usage = k8s::PodContainerUsage::new();
            let mut values = Vec::new();
            for (i, (ns, containers, statuses)) in pods.into_iter().enumerate() {
                let (namespace, name) = (format!("ns{ns}"), format!("pod{i}"));
                let mut specs = Vec::new();
                for (c, cpu, mem, used) in containers {
                    let mut limits = serde_json::Map::new();
                    if let Some(cpu) = cpu {
                        limits.insert("cpu".into(), format!("{cpu}m").into());
                    }
                    if let Some(mem) = mem {
                        limits.insert("memory".into(), format!("{mem}Mi").into());
                    }
                    if let Some((cpu, mem)) = used {
                        let at = usage.entry((namespace.clone(), name.clone())).or_default();
                        at.insert(format!("c{c}"), (f64::from(cpu), f64::from(mem) * 1024.0 * 1024.0));
                    }
                    specs.push(json!({"name": format!("c{c}"), "image": "app:1", "env": [{"name": "A", "value": "b"}],
                        "resources": {"limits": limits, "requests": {"cpu": "10m"}},
                        "volumeMounts": [{"name": "v", "mountPath": "/v"}]}));
                }
                let states: Vec<Value> = statuses
                    .into_iter()
                    .map(|(c, restarts, ended)| {
                        let last = match ended {
                            Some((ago, reason, reads)) => {
                                let at = (base() - jiff::SignedDuration::from_mins(ago)).to_string();
                                let at = if reads { at } else { "a while ago".to_string() };
                                json!({"terminated": {"exitCode": 1, "reason": REASONS[reason], "finishedAt": at}})
                            }
                            None => json!({}),
                        };
                        json!({"name": format!("c{c}"), "restartCount": restarts, "ready": true, "lastState": last,
                            "state": {"running": {"startedAt": "2026-01-01T00:00:00Z"}}})
                    })
                    .collect();
                values.push(json!({
                    "metadata": {"name": name, "namespace": namespace, "managedFields": [{"manager": "kubelet"}]},
                    "spec": {"containers": specs, "nodeName": "node-1"},
                    "status": {"phase": "Running", "containerStatuses": states},
                }));
            }
            (values, usage)
        })
    }

    /// Restarts as (namespace, pod, container, reason, count, when) and containers near their
    /// limits as (namespace, pod, container, CPU, memory).
    type Found = (
        Vec<(String, String, String, String, i64, jiff::Timestamp)>,
        Vec<(String, String, String, (f64, Option<f64>), (f64, Option<f64>))>,
    );

    /// A pod as the overview read it before: each container and its status a JSON tree.
    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct OldPod {
        metadata: OldMeta,
        spec: OldSpec,
        status: OldStatus,
    }

    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct OldMeta {
        name: String,
        namespace: String,
    }

    #[derive(serde::Deserialize, Default)]
    #[serde(default)]
    struct OldSpec {
        containers: Vec<Value>,
    }

    #[derive(serde::Deserialize, Default)]
    #[serde(default, rename_all = "camelCase")]
    struct OldStatus {
        container_statuses: Vec<Value>,
    }

    /// What the overview found in whole pods before it read them lean.
    fn found_in_values(pods: &[Value], usage: &k8s::PodContainerUsage, since: jiff::Timestamp) -> Found {
        use crate::json::{i64_at, str_or_empty as st, time_at as ts};
        let pods: Vec<OldPod> = pods.iter().map(|p| serde_json::from_value(p.clone()).unwrap()).collect();
        let (mut restarts, mut hot) = (Vec::new(), Vec::new());
        for p in &pods {
            let key = (p.metadata.namespace.clone(), p.metadata.name.clone());
            let usage = usage.get(&key);
            let (namespace, pod) = key;
            for c in &p.status.container_statuses {
                if let Some(when) = ts(c, "/lastState/terminated/finishedAt").filter(|t| *t >= since) {
                    let reason = st(c, "/lastState/terminated/reason").to_string();
                    let count = i64_at(c, "/restartCount").unwrap_or(0);
                    restarts.push((namespace.clone(), pod.clone(), st(c, "/name").to_string(), reason, count, when));
                }
            }
            for c in &p.spec.containers {
                let name = st(c, "/name");
                let Some(&(cpu, mem)) = usage.and_then(|u| u.get(name)) else { continue };
                let cpu_limit = c.pointer("/resources/limits/cpu").and_then(Value::as_str).map(k8s::parse_cpu);
                let mem_limit = c.pointer("/resources/limits/memory").and_then(Value::as_str).map(k8s::parse_bytes);
                let near = |v: f64, l: Option<f64>| l.is_some_and(|l| l > 0.0 && v >= 0.95 * l);
                if near(cpu, cpu_limit) || near(mem, mem_limit) {
                    hot.push((namespace.clone(), pod.clone(), name.to_string(), (cpu, cpu_limit), (mem, mem_limit)));
                }
            }
        }
        restarts.sort_by_key(|r| std::cmp::Reverse(r.5));
        (restarts, hot)
    }

    /// What the overview finds in the same pods read lean.
    fn found_lean(pods: &[Value], usage: &k8s::PodContainerUsage, since: jiff::Timestamp) -> Found {
        let pods: Vec<PodDetail> = pods.iter().map(|p| serde_json::from_value(p.clone()).unwrap()).collect();
        let (restarts, hot) = restarts_and_hot(&pods, usage, "ctx", since);
        (
            restarts.into_iter().map(|r| (r.namespace, r.pod, r.container, r.reason, r.count, r.when)).collect(),
            hot.into_iter().map(|h| (h.namespace, h.pod, h.container, h.cpu, h.mem)).collect(),
        )
    }

    proptest! {
        /// HPAs and budgets read lean count as whole ones did, whatever their status holds.
        #[test]
        fn lean_hpas_and_budgets_count_as_whole_ones_did(
            hpas in prop::collection::vec(whole_hpa(), 0..8),
            pdbs in prop::collection::vec(whole_pdb(), 0..8),
        ) {
            let lean: Vec<HpaLite> = hpas.iter().map(|h| serde_json::from_value(h.clone()).unwrap()).collect();
            prop_assert_eq!(hpa_counts(&lean), hpa_counts_of_values(&hpas));
            let lean: Vec<PdbLite> = pdbs.iter().map(|p| serde_json::from_value(p.clone()).unwrap()).collect();
            prop_assert_eq!(pdb_counts(&lean), pdb_counts_of_values(&pdbs));
        }

        /// Pods read lean show the restarts and the containers near their limits that whole
        /// ones did, in the same order.
        #[test]
        fn lean_pods_show_what_whole_ones_did((pods, usage) in pods_and_usage()) {
            let since = base() - jiff::SignedDuration::from_mins(60);
            prop_assert_eq!(found_lean(&pods, &usage, since), found_in_values(&pods, &usage, since));
        }

        /// Asking for current revisions only counts the same: Helm marks a revision superseded
        /// once a newer one replaces it, so a release's newest revision never is.
        #[test]
        fn current_revisions_count_as_every_revision(
            releases in prop::collection::vec((0usize..2, prop::collection::vec(0usize..8, 0..5), 1usize..8), 0..8),
        ) {
            const STATUSES: [&str; 8] = [
                "superseded", "deployed", "failed", "uninstalled", "uninstalling", "pending-install", "pending-upgrade",
                "pending-rollback",
            ];
            let mut every = Vec::new();
            for (i, (ns, older, newest)) in releases.iter().enumerate() {
                let statuses = older.iter().chain([newest]).map(|&s| STATUSES[s]);
                for (v, status) in statuses.enumerate() {
                    let labels: BTreeMap<String, String> = [
                        ("name", format!("r{i}")),
                        ("owner", "helm".to_string()),
                        ("status", status.to_string()),
                        ("version", (v + 1).to_string()),
                    ]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect();
                    every.push((Some(format!("ns{ns}")), labels));
                }
            }
            let current: Vec<_> = every.iter().filter(|(_, l)| l["status"] != "superseded").cloned().collect();
            prop_assert_eq!(helm_counts(&current), helm_counts(&every));
        }
    }

    /// The context the fixture cluster is loaded as.
    const CONTEXT: &str = "fixture";

    /// A request the fixture cluster was sent: its path, query and Accept header.
    type Asked = (String, String, String);

    /// An API server with a few objects of each kind the overview counts, answering in the
    /// form asked: kubectl's table, whole objects or their metadata. Records each request.
    #[derive(Clone, Default)]
    struct Cluster(Arc<Mutex<Vec<Asked>>>);

    impl Cluster {
        fn asked(&self) -> Vec<Asked> {
            self.0.lock().unwrap().clone()
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
            let query = req.uri().query().unwrap_or_default().to_string();
            let accept = req.headers().get(http::header::ACCEPT).and_then(|v| v.to_str().ok()).unwrap_or_default();
            let body = if accept.contains("as=Table") { table(&path) } else { objects(&path, &query) };
            self.0.lock().unwrap().push((path, query, accept.to_string()));
            let code = if body["kind"] == "Status" { 404 } else { 200 };
            let response = http::Response::builder().status(code).body(body.to_string().into_bytes().into()).unwrap();
            std::future::ready(Ok(response))
        }
    }

    /// A query parameter's value, decoded.
    fn param(query: &str, key: &str) -> Option<String> {
        let raw = query.split('&').find_map(|p| p.strip_prefix(key)?.strip_prefix('='))?;
        let (bytes, mut out, mut i) = (raw.as_bytes(), Vec::new(), 0);
        while i < bytes.len() {
            if bytes[i] == b'%' {
                out.push(u8::from_str_radix(&raw[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
                i += 1;
            }
        }
        Some(String::from_utf8(out).unwrap())
    }

    /// kubectl's table of a kind on the fixture cluster: its columns and its rows' cells.
    fn table(path: &str) -> Value {
        let (columns, rows) = match path {
            "/api/v1/pods" => (
                vec!["Name", "Ready", "Status", "Restarts", "Age"],
                vec![
                    vec!["web-1", "1/1", "Running", "3 (10m ago)", "5d"],
                    vec!["web-2", "0/1", "CrashLoopBackOff", "7 (2h ago)", "5d"],
                    vec!["db-0", "2/2", "Running", "0", "30d"],
                    vec!["migrate-x", "0/1", "Completed", "0", "1d"],
                ],
            ),
            "/apis/apps/v1/deployments" => (
                vec!["Name", "Ready", "Up-to-date", "Available", "Age"],
                vec![
                    vec!["web", "2/2", "2", "2", "5d"],
                    vec!["api", "1/3", "3", "1", "5d"],
                    vec!["batch", "0/0", "0", "0", "5d"],
                ],
            ),
            "/apis/apps/v1/replicasets" => (
                vec!["Name", "Desired", "Current", "Ready", "Age"],
                vec![vec!["web-abc", "2", "2", "2", "5d"], vec!["web-old", "0", "0", "0", "9d"]],
            ),
            "/apis/apps/v1/statefulsets" => (vec!["Name", "Ready", "Age"], vec![vec!["db", "1/1", "30d"]]),
            "/apis/apps/v1/daemonsets" => (
                vec!["Name", "Desired", "Current", "Ready", "Up-to-date", "Available", "Node Selector", "Age"],
                vec![vec!["agent", "3", "3", "2", "3", "2", "<none>", "30d"]],
            ),
            "/apis/batch/v1/cronjobs" => (
                vec!["Name", "Schedule", "Timezone", "Suspend", "Active", "Last Schedule", "Age"],
                vec![
                    vec!["nightly", "0 3 * * *", "<none>", "False", "0", "5h", "30d"],
                    vec!["paused", "0 4 * * *", "<none>", "True", "0", "9d", "30d"],
                ],
            ),
            "/apis/batch/v1/jobs" => (
                vec!["Name", "Status", "Completions", "Duration", "Age"],
                vec![vec!["migrate-x", "Complete", "1/1", "20s", "1d"], vec!["broken", "Failed", "0/1", "5m", "2d"]],
            ),
            "/api/v1/persistentvolumeclaims" => (
                vec![
                    "Name",
                    "Status",
                    "Volume",
                    "Capacity",
                    "Access Modes",
                    "StorageClass",
                    "VolumeAttributesClass",
                    "Age",
                ],
                vec![
                    vec!["data-db-0", "Bound", "pv-1", "10Gi", "RWO", "standard", "<unset>", "30d"],
                    vec!["scratch", "Pending", "", "", "", "standard", "<unset>", "1m"],
                ],
            ),
            _ => return json!({"kind": "Status", "code": 404}),
        };
        json!({"kind": "Table", "apiVersion": "meta.k8s.io/v1", "metadata": {"resourceVersion": "7"},
            "columnDefinitions": columns.iter().map(|c| json!({"name": c, "type": "string"})).collect::<Vec<_>>(),
            "rows": rows.iter().map(|cells| json!({"cells": cells})).collect::<Vec<_>>()})
    }

    /// Managed fields, most of a stored object's metadata, which the overview never reads.
    fn managed() -> Value {
        json!([{"manager": "kubelet", "operation": "Update", "fieldsType": "FieldsV1", "fieldsV1": {"f:status": {}}}])
    }

    /// A container with `limits`, and what a real one holds besides.
    fn container(name: &str, limits: Value) -> Value {
        json!({"name": name, "image": format!("{name}:1"), "env": [{"name": "LOG", "value": "info"}],
            "resources": {"limits": limits, "requests": {"cpu": "10m"}},
            "volumeMounts": [{"name": "data", "mountPath": "/data"}],
            "livenessProbe": {"httpGet": {"path": "/health", "port": 8080}}})
    }

    fn pod(ns: &str, name: &str, containers: Value, statuses: Value) -> Value {
        json!({"metadata": {"name": name, "namespace": ns, "uid": format!("{ns}-{name}"), "labels": {"app": name},
                "managedFields": managed()},
            "spec": {"containers": containers, "nodeName": "node-1", "volumes": [{"name": "data", "emptyDir": {}}]},
            "status": {"phase": "Running", "containerStatuses": statuses, "podIP": "10.0.0.1"}})
    }

    /// The fixture cluster's objects of a kind, whole (Helm's Secrets: their metadata), as the
    /// query selects them.
    fn objects(path: &str, query: &str) -> Value {
        let list = |kind: &str, items: Vec<Value>| json!({"kind": kind, "apiVersion": "v1", "metadata": {"resourceVersion": "7"}, "items": items});
        let ago = |mins: i64| (jiff::Timestamp::now() - jiff::SignedDuration::from_mins(mins)).to_string();
        let ended =
            |reason: &str, mins: i64| json!({"terminated": {"exitCode": 1, "reason": reason, "finishedAt": ago(mins)}});
        match path {
            "/api/v1/pods" => list(
                "PodList",
                vec![
                    pod(
                        "shop",
                        "web-1",
                        json!([container("app", json!({"cpu": "500m", "memory": "128Mi"}))]),
                        json!([{"name": "app", "restartCount": 3, "ready": true, "lastState": ended("OOMKilled", 10)}]),
                    ),
                    pod(
                        "shop",
                        "web-2",
                        json!([container("app", json!({"memory": "128Mi"}))]),
                        json!([{"name": "app", "restartCount": 7, "ready": false, "lastState": ended("Error", 120)}]),
                    ),
                    pod(
                        "data",
                        "db-0",
                        json!([container("db", json!({"memory": "1Gi"})), container("sidecar", json!({}))]),
                        json!([{"name": "db", "restartCount": 0, "ready": true, "lastState": {}},
                        {"name": "sidecar", "restartCount": 0, "ready": true, "lastState": {}}]),
                    ),
                    pod(
                        "data",
                        "migrate-x",
                        json!([container("migrate", json!({}))]),
                        json!([{"name": "migrate", "restartCount": 0, "ready": false, "lastState": {}}]),
                    ),
                ],
            ),
            "/apis/metrics.k8s.io/v1beta1/pods" => list(
                "PodMetricsList",
                vec![
                    json!({"metadata": {"name": "web-1", "namespace": "shop"},
                    "containers": [{"name": "app", "usage": {"cpu": "100m", "memory": "127Mi"}}]}),
                    json!({"metadata": {"name": "db-0", "namespace": "data"},
                    "containers": [{"name": "db", "usage": {"cpu": "2", "memory": "100Mi"}},
                        {"name": "sidecar", "usage": {"cpu": "5m", "memory": "900Mi"}}]}),
                ],
            ),
            "/api/v1/secrets" => {
                // Every revision Helm keeps; the API server applies the selector.
                let current =
                    param(query, "labelSelector").is_some_and(|s| s.split(',').any(|s| s == "status!=superseded"));
                let revisions = [
                    ("shop", "web", 1, "superseded"),
                    ("shop", "web", 2, "superseded"),
                    ("shop", "web", 3, "deployed"),
                    ("data", "db", 1, "deployed"),
                    ("data", "db", 2, "failed"),
                    ("tools", "old", 1, "superseded"),
                    ("tools", "old", 2, "uninstalled"),
                ];
                let items: Vec<Value> = revisions
                    .iter()
                    .filter(|r| !current || r.3 != "superseded")
                    .map(|(ns, name, v, status)| json!({"apiVersion": "meta.k8s.io/v1", "kind": "PartialObjectMetadata",
                        "metadata": {"name": format!("sh.helm.release.v1.{name}.v{v}"), "namespace": ns, "managedFields": managed(),
                            "labels": {"name": name, "owner": "helm", "status": status, "version": v.to_string()}}}))
                    .collect();
                json!({"kind": "PartialObjectMetadataList", "apiVersion": "meta.k8s.io/v1",
                    "metadata": {"resourceVersion": "7"}, "items": items})
            }
            "/apis/autoscaling/v2/horizontalpodautoscalers" => {
                let hpa = |limited: &str| {
                    json!({"metadata": {"name": "web", "managedFields": managed()},
                    "spec": {"minReplicas": 1, "maxReplicas": 5}, "status": {"currentReplicas": 2, "conditions": [
                        {"type": "AbleToScale", "status": "True"}, {"type": "ScalingActive", "status": "True"},
                        {"type": "ScalingLimited", "status": limited}]}})
                };
                list(
                    "HorizontalPodAutoscalerList",
                    vec![hpa("False"), hpa("True"), json!({"metadata": {"name": "new"}})],
                )
            }
            "/apis/policy/v1/poddisruptionbudgets" => {
                let pdb = |allowed: i64, expected: i64| {
                    json!({"metadata": {"name": "db", "managedFields": managed()},
                    "spec": {"minAvailable": 1}, "status": {"disruptionsAllowed": allowed, "expectedPods": expected,
                        "currentHealthy": expected, "desiredHealthy": 1}})
                };
                list("PodDisruptionBudgetList", vec![pdb(0, 2), pdb(1, 3), pdb(0, 0)])
            }
            "/api/v1/events" => {
                let warnings_only = param(query, "fieldSelector").as_deref() == Some("type=Warning");
                let event = |kind: &str, reason: &str, times: Value| {
                    let mut e = json!({"metadata": {"name": reason, "namespace": "shop", "managedFields": managed()},
                        "involvedObject": {"kind": "Pod", "name": "web-1"}, "type": kind, "reason": reason, "message": "m"});
                    e.as_object_mut().unwrap().extend(times.as_object().unwrap().clone());
                    e
                };
                let events = [
                    event("Warning", "BackOff", json!({"count": 12, "lastTimestamp": ago(5)})),
                    event("Warning", "BackOff", json!({"count": 3, "lastTimestamp": ago(20)})),
                    // Recorded through events.k8s.io: its repeats are in `series`.
                    event(
                        "Warning",
                        "FailedScheduling",
                        json!({"eventTime": ago(120), "lastTimestamp": null,
                        "series": {"count": 40, "lastObservedTime": ago(2)}}),
                    ),
                    // Over an hour ago: left out.
                    event("Warning", "Unhealthy", json!({"count": 5, "lastTimestamp": ago(180)})),
                    event("Normal", "Pulled", json!({"count": 1, "lastTimestamp": ago(1)})),
                ];
                list("EventList", events.into_iter().filter(|e| !warnings_only || e["type"] == "Warning").collect())
            }
            _ => json!({"kind": "Status", "code": 404}),
        }
    }

    /// The overview of a small cluster: the same figures as from whole objects, from lean
    /// requests only. Counts from kubectl's cells alone, never an object's metadata; whole
    /// pods, HPAs and budgets and Helm's Secrets (their metadata, current revisions only) from
    /// the API server's cache; Warning events only, a page at a time; each list asked once.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_overview_reads_only_what_it_shows() {
        let cluster = Cluster::default();
        let client = kube::Client::new(cluster.clone(), "default");
        let (tx, rx) = futures::channel::mpsc::unbounded();
        super::load(client, None, CONTEXT.into(), tx).await;
        let pieces: Vec<Piece> = rx.collect().await;
        let mut kinds = BTreeMap::new();
        let (mut warnings, mut restarts, mut hot, mut failed) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for piece in pieces {
            match piece {
                Piece::Kind(slot, counts) => {
                    kinds.insert(SLOTS[slot].0, counts);
                }
                Piece::Warnings(w) => warnings.extend(w.into_iter().map(|w| (w.reason, w.count))),
                Piece::Restarts(r) => {
                    restarts.extend(r.into_iter().map(|r| (r.namespace, r.pod, r.container, r.reason, r.count)));
                }
                Piece::Hot(h, skipped) => {
                    assert!(!skipped);
                    hot.extend(h.into_iter().map(|h| (h.pod, h.container)));
                }
                Piece::Failed(_, e) => failed.push(e),
                Piece::Done(_) => {}
            }
        }
        assert!(failed.is_empty(), "{failed:?}");
        let counts = |c: &[(&str, usize, Tone)]| -> Result<Counts, String> {
            Ok(c.iter().map(|&(label, n, tone)| (label.to_string(), n, tone)).collect())
        };
        let (good, done, busy, bad) = (Tone::Good, Tone::Done, Tone::Busy, Tone::Bad);
        assert_eq!(
            kinds["Pods"],
            counts(&[("Running", 2, good), ("Completed", 1, done), ("CrashLoopBackOff", 1, bad)])
        );
        assert_eq!(kinds["Deployments"], counts(&[("Running", 1, good), ("Idle", 1, done), ("Unavailable", 1, bad)]));
        assert_eq!(
            kinds["Helm Releases"],
            counts(&[("Deployed", 1, good), ("Superseded", 1, done), ("Failed", 1, bad)])
        );
        assert_eq!(kinds["ReplicaSets"], counts(&[("Running", 1, good), ("Idle", 1, done)]));
        assert_eq!(kinds["StatefulSets"], counts(&[("Running", 1, good)]));
        assert_eq!(kinds["DaemonSets"], counts(&[("Unavailable", 1, bad)]));
        assert_eq!(kinds["CronJobs"], counts(&[("Scheduled", 1, good), ("Suspended", 1, done)]));
        assert_eq!(kinds["Jobs"], counts(&[("Complete", 1, done), ("Failed", 1, bad)]));
        assert_eq!(kinds["HPAs"], counts(&[("AtTarget", 1, good), ("ScalingLimited", 1, busy), ("Unknown", 1, busy)]));
        let budgets = counts(&[("DisruptionAllowed", 1, good), ("NoPods", 1, done), ("InsufficientPods", 1, bad)]);
        assert_eq!(kinds["Disruption Budgets"], budgets);
        assert_eq!(kinds["PVCs"], counts(&[("Bound", 1, good), ("Pending", 1, busy)]));
        assert_eq!(warnings, [("FailedScheduling".to_string(), 40), ("BackOff".to_string(), 15)]);
        let restarted = ("shop".to_string(), "web-1".to_string(), "app".to_string(), "OOMKilled".to_string(), 3);
        assert_eq!(restarts, [restarted]);
        // Usage is read unless this machine's settings turned metrics off.
        let near: &[(&str, &str)] = if crate::settings::metrics_on(CONTEXT) { &[("web-1", "app")] } else { &[] };
        assert_eq!(hot.iter().map(|(p, c)| (p.as_str(), c.as_str())).collect::<Vec<_>>(), near);

        let asked = cluster.asked();
        let (tables, lists): (Vec<&Asked>, Vec<&Asked>) = asked
            .iter()
            .filter(|(path, ..)| !path.starts_with("/apis/metrics.k8s.io/"))
            .partition(|(_, _, accept)| accept.contains("as=Table"));
        // Pods (their first page held them all), six kinds of workloads, PVCs: cells alone.
        assert_eq!(tables.len(), 8, "{asked:?}");
        assert!(tables.iter().all(|(_, q, _)| param(q, "includeObject").as_deref() == Some("None")), "{asked:?}");
        let mut listed: Vec<&str> = lists.iter().map(|(path, ..)| path.as_str()).collect();
        listed.sort_unstable();
        let expected = [
            "/api/v1/events",
            "/api/v1/pods",
            "/api/v1/secrets",
            "/apis/autoscaling/v2/horizontalpodautoscalers",
            "/apis/policy/v1/poddisruptionbudgets",
        ];
        assert_eq!(listed, expected, "{asked:?}");
        let (events, cached): (Vec<&Asked>, Vec<&Asked>) =
            lists.into_iter().partition(|(path, ..)| path == "/api/v1/events");
        assert!(cached.iter().all(|(_, q, _)| param(q, "resourceVersion").as_deref() == Some("0")), "{asked:?}");
        let (_, helm_query, helm_accept) = cached.iter().find(|(path, ..)| path == "/api/v1/secrets").unwrap();
        assert!(helm_accept.contains("as=PartialObjectMetadataList"), "{helm_accept}");
        assert_eq!(param(helm_query, "labelSelector").as_deref(), Some("owner=helm,status!=superseded"));
        // The API server keeps no cache of events: they come a page at a time, Warnings only.
        assert_eq!(param(&events[0].1, "fieldSelector").as_deref(), Some("type=Warning"));
        assert_eq!(param(&events[0].1, "limit").as_deref(), Some("5000"));
    }
}
