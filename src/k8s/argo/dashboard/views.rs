//! The environment page's full-width views, worked out from what the dashboard read: its
//! resources by category, the last sync's result per resource, and the Application's details.

use std::collections::HashMap;

use jiff::Timestamp;
use serde_json::Value;

use super::super::diagnosis::Res;
use super::super::journal::Step;
use super::{info, pod_state, res_of_node, sources_of, who};
use crate::json::{arr_at as arr, nonempty_at as s, time_at as time};
use crate::status::Tone;

/// The annotation that orders a sync: lower waves first.
pub const SYNC_WAVE: &str = "argocd.argoproj.io/sync-wave";

// MARK: Resources

/// What a resource is for, the columns of the Resources view.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    Workloads,
    Jobs,
    Config,
    Networking,
    Storage,
    Others,
}

impl Category {
    pub const ALL: [Self; 6] =
        [Self::Workloads, Self::Jobs, Self::Config, Self::Networking, Self::Storage, Self::Others];

    pub fn title(self) -> &'static str {
        match self {
            Self::Workloads => "Workloads",
            Self::Jobs => "Jobs & CronJobs",
            Self::Config => "Config & Secrets",
            Self::Networking => "Networking",
            Self::Storage => "Storage",
            Self::Others => "Operators and others",
        }
    }

    /// The category of a kind of the API group `group`.
    pub fn of(group: &str, kind: &str) -> Self {
        match (group, kind) {
            ("apps", "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet")
            | ("", "Pod" | "ReplicationController")
            | ("argoproj.io", "Rollout")
            | ("autoscaling", "HorizontalPodAutoscaler")
            | ("policy", "PodDisruptionBudget")
            | ("keda.sh", "ScaledObject") => Self::Workloads,
            ("batch", "Job" | "CronJob") | ("argoproj.io", "Workflow" | "CronWorkflow") | ("keda.sh", "ScaledJob") => {
                Self::Jobs
            }
            ("", "ConfigMap" | "Secret" | "ServiceAccount")
            | ("rbac.authorization.k8s.io", _)
            | ("external-secrets.io", "ExternalSecret" | "SecretStore")
            | ("bitnami.com", "SealedSecret") => Self::Config,
            ("", "Service" | "Endpoints")
            | (
                "networking.k8s.io"
                | "discovery.k8s.io"
                | "gateway.networking.k8s.io"
                | "networking.istio.io"
                | "traefik.io"
                | "traefik.containo.us",
                _,
            )
            | ("cert-manager.io", "Certificate") => Self::Networking,
            ("", "PersistentVolumeClaim" | "PersistentVolume") | ("storage.k8s.io" | "snapshot.storage.k8s.io", _) => {
                Self::Storage
            }
            _ => Self::Others,
        }
    }
}

/// Kinds left out of the view: what a workload makes, shown as its pods ready.
fn hidden(group: &str, kind: &str) -> bool {
    matches!(
        (group, kind),
        ("", "Pod" | "Endpoints")
            | ("apps", "ReplicaSet" | "ControllerRevision")
            | ("discovery.k8s.io", "EndpointSlice")
    )
}

/// A resource of the Application, with its health.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub res: Res,
    pub category: Category,
    /// Argo CD's word for its health (`Healthy`, `Degraded`…), when it has one.
    pub health: Option<String>,
    pub tone: Tone,
    pub message: Option<String>,
    /// What its pods are doing, when it has pods.
    pub pods: Option<Pods>,
}

/// The pods under an object, by what they are doing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pods {
    /// Running and ready.
    pub ready: usize,
    /// Run to completion: never ready, and not meant to be.
    pub done: usize,
    /// In trouble: failed, crashing, evicted…
    pub failed: usize,
    pub all: usize,
}

impl Pods {
    /// One pod node of the tree.
    fn of_pod(pod: &Value) -> Self {
        let (reason, tone) = pod_state(pod);
        let mut out = Self { all: 1, ..Self::default() };
        match tone {
            Tone::Good => out.ready = 1,
            Tone::Bad => out.failed = 1,
            Tone::Done if matches!(reason.as_str(), "Completed" | "Succeeded") => out.done = 1,
            _ => {}
        }
        out
    }

    fn add(&mut self, o: Self) {
        self.ready += o.ready;
        self.done += o.done;
        self.failed += o.failed;
        self.all += o.all;
    }

    /// Those that should be ready: all but those run to completion.
    pub fn serving(&self) -> usize {
        self.all - self.done
    }

    /// Those neither done nor failed: starting or running.
    fn running(&self) -> usize {
        self.all - self.done - self.failed
    }

    /// Whether to say them as runs (done, running, failed) rather than ready of all: for what
    /// runs to completion (`jobs`), or when none of them should be ready.
    fn as_runs(&self, jobs: bool) -> bool {
        jobs || self.serving() == 0
    }

    /// What the row says of them, and its tone. For what runs to completion (`jobs`): how many
    /// are done, running and failed (`2 done, 1 failed`). For what serves: ready of those that
    /// should be (`1/2`), a pod run to completion not counted.
    pub fn summary(&self, jobs: bool) -> Option<(String, Tone)> {
        if self.as_runs(jobs) {
            let parts: Vec<String> = [(self.done, "done"), (self.running(), "running"), (self.failed, "failed")]
                .iter()
                .filter(|(n, _)| *n > 0)
                .map(|(n, w)| format!("{n} {w}"))
                .collect();
            let tone = if self.failed > 0 {
                Tone::Bad
            } else if self.running() > 0 {
                Tone::Busy
            } else {
                Tone::Done
            };
            return (!parts.is_empty()).then(|| (parts.join(", "), tone));
        }
        let tone = if self.ready < self.serving() { Tone::Busy } else { Tone::Done };
        Some((format!("{}/{}", self.ready, self.serving()), tone))
    }

    /// The tooltip's line: `1 of 2 pods ready, 3 completed`, or `Pods: 2 done` for runs.
    pub fn tip(&self, jobs: bool) -> Option<String> {
        if self.as_runs(jobs) {
            return self.summary(jobs).map(|(s, _)| format!("Pods: {s}"));
        }
        let mut out = format!("{} of {} ready", self.ready, crate::plural::count(self.serving(), "pod"));
        if self.done > 0 {
            out.push_str(&format!(", {} completed", self.done));
        }
        Some(out)
    }
}

impl Node {
    /// Whether it is in trouble: Degraded or Missing.
    pub fn in_trouble(&self) -> bool {
        self.tone == Tone::Bad
    }

    /// What the row says of its pods, and its tone: runs for a Job or a CronJob, whose pods
    /// complete; ready of all for what serves.
    pub fn pods_summary(&self) -> Option<(String, Tone)> {
        self.pods?.summary(self.category == Category::Jobs)
    }

    /// The tooltip's line on its pods.
    pub fn pods_tip(&self) -> Option<String> {
        self.pods?.tip(self.category == Category::Jobs)
    }

    /// Whether the view keeps it: `query` (lowercase words) in its kind or name, in trouble
    /// when only those are asked, of the health picked.
    pub fn keeps(&self, query: &str, only_problems: bool, health: Option<&str>) -> bool {
        let text = format!("{} {}", self.res.kind, self.res.name).to_lowercase();
        query.split_whitespace().all(|w| text.contains(w))
            && (!only_problems || self.in_trouble())
            && health.is_none_or(|h| self.health.as_deref() == Some(h))
    }
}

/// The tone of an Argo CD health.
pub fn health_tone(health: Option<&str>) -> Tone {
    match health {
        Some("Healthy") => Tone::Good,
        Some("Progressing") => Tone::Busy,
        Some("Degraded" | "Missing") => Tone::Bad,
        _ => Tone::Done,
    }
}

/// The resources of the tree, but pods and what only holds them (their counts go to the
/// objects above them), by category then kind and name.
pub fn nodes_from_tree(tree: &Value) -> Vec<Node> {
    let nodes = arr(tree, "/nodes");
    let by_uid: HashMap<&str, &Value> = nodes.iter().filter_map(|n| Some((s(n, "/uid")?, n))).collect();
    // Each pod counted for every object above it.
    let mut pods: HashMap<&str, Pods> = HashMap::new();
    for n in nodes.iter().filter(|n| s(n, "/kind") == Some("Pod") && s(n, "/group").unwrap_or("").is_empty()) {
        let this = Pods::of_pod(n);
        let mut at: &Value = n;
        for _ in 0..8 {
            let Some(parent) = arr(at, "/parentRefs").iter().find_map(|p| by_uid.get(s(p, "/uid")?).copied()) else {
                break;
            };
            if let Some(uid) = s(parent, "/uid") {
                pods.entry(uid).or_default().add(this);
            }
            at = parent;
        }
    }
    let mut out: Vec<Node> = nodes
        .iter()
        .filter(|n| !hidden(s(n, "/group").unwrap_or(""), s(n, "/kind").unwrap_or("")))
        .map(|n| {
            let res = res_of_node(n);
            let health = s(n, "/health/status").map(String::from);
            Node {
                category: Category::of(&res.group, &res.kind),
                tone: health_tone(health.as_deref()),
                health,
                message: s(n, "/health/message").or_else(|| info(n, "Status Reason")).map(String::from),
                pods: s(n, "/uid").and_then(|u| pods.get(u).copied()),
                res,
            }
        })
        .collect();
    sort_nodes(&mut out);
    out
}

/// The resources as the Application's status lists them (no tree read): no pods known.
pub fn nodes_from_status(app: &Value) -> Vec<Node> {
    let mut out: Vec<Node> = arr(app, "/status/resources")
        .iter()
        .filter(|r| r["hook"] != true)
        .filter_map(|r| {
            let res = Res::of(r)?;
            let health = s(r, "/health/status").map(String::from);
            Some(Node {
                category: Category::of(&res.group, &res.kind),
                tone: health_tone(health.as_deref()),
                health,
                message: s(r, "/health/message").map(String::from),
                pods: None,
                res,
            })
        })
        .collect();
    sort_nodes(&mut out);
    out
}

fn sort_nodes(nodes: &mut [Node]) {
    nodes.sort_by(|a, b| (a.category, &a.res.kind, &a.res.name).cmp(&(b.category, &b.res.kind, &b.res.name)));
}

/// A category of the Resources view with the resources it shows, and how many of them are in
/// trouble.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section<'a> {
    pub category: Category,
    pub nodes: Vec<&'a Node>,
    pub troubled: usize,
}

/// How bad a resource is, to list the worst first: in trouble, then busy, then the rest.
fn badness(n: &Node) -> u8 {
    match n.tone {
        Tone::Bad => 0,
        Tone::Busy => 1,
        _ => 2,
    }
}

/// The resources kept, by category for the Resources view: the categories with resources in
/// trouble first (then in their usual order), and in each the worst first, then by kind and
/// name. Empty categories are left out.
pub fn sections<'a>(kept: &[&'a Node]) -> Vec<Section<'a>> {
    let mut out: Vec<Section<'a>> = Category::ALL
        .iter()
        .filter_map(|&category| {
            let mut nodes: Vec<&Node> = kept.iter().copied().filter(|n| n.category == category).collect();
            if nodes.is_empty() {
                return None;
            }
            nodes.sort_by(|a, b| (badness(a), &a.res.kind, &a.res.name).cmp(&(badness(b), &b.res.kind, &b.res.name)));
            let troubled = nodes.iter().filter(|n| n.in_trouble()).count();
            Some(Section { category, nodes, troubled })
        })
        .collect();
    // Stable: the usual order among those with trouble, and among those without.
    out.sort_by_key(|s| s.troubled == 0);
    out
}

// MARK: Sync status

/// One resource of the last sync's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncRow {
    pub step: Step,
    /// Its sync wave (0 when it says none).
    pub wave: i64,
    /// Its health now, when known.
    pub health: Option<String>,
    pub hook: bool,
}

impl SyncRow {
    /// Whether the view keeps it, by the status, health and hook picked (None: any).
    pub fn keeps(&self, status: Option<&str>, health: Option<&str>, hook: Option<bool>) -> bool {
        status.is_none_or(|st| self.step.result() == st)
            && health.is_none_or(|h| self.health.as_deref().unwrap_or("None") == h)
            && hook.is_none_or(|h| self.hook == h)
    }
}

/// The operation of the last sync, as its header says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    /// `Sync`, `Sync (dry run)` or `Rollback`.
    pub kind: String,
    pub phase: String,
    pub tone: Tone,
    /// Its message, whole.
    pub message: Option<String>,
    pub started: Option<Timestamp>,
    pub finished: Option<Timestamp>,
    pub revision: Option<String>,
    pub by: Option<String>,
    pub retries: Option<i64>,
}

/// The last operation of `app`, whole.
pub fn operation(app: &Value) -> Option<Operation> {
    let op = app.pointer("/status/operationState")?;
    let phase = s(op, "/phase")?.to_string();
    let dry = op.pointer("/operation/sync/dryRun").and_then(Value::as_bool) == Some(true);
    let kind = if op.pointer("/operation/rollback").is_some_and(|r| !r.is_null()) {
        "Rollback"
    } else if dry {
        "Sync (dry run)"
    } else {
        "Sync"
    };
    Some(Operation {
        kind: kind.to_string(),
        tone: super::super::operation_tone(&phase),
        phase,
        message: s(op, "/message").map(String::from),
        started: time(op, "/startedAt"),
        finished: time(op, "/finishedAt"),
        revision: op
            .pointer("/syncResult")
            .and_then(super::super::revisions)
            .or_else(|| op.pointer("/operation/sync").and_then(super::super::revisions)),
        by: op.pointer("/operation/initiatedBy").and_then(who),
        retries: op.get("retryCount").and_then(Value::as_i64).filter(|n| *n > 0),
    })
}

/// The sync wave each resource says in its manifest, from the managed resources (their
/// target state, else their live one): by kind, namespace and name.
pub fn waves(managed: Option<&Value>) -> HashMap<(String, String, String), i64> {
    let mut out = HashMap::new();
    for item in managed.map_or(&[][..], |m| arr(m, "/items")) {
        let state = ["targetState", "liveState"]
            .iter()
            .filter_map(|k| item.get(*k).and_then(Value::as_str))
            // Most say none: read only those that do.
            .find(|t| t.contains(SYNC_WAVE));
        let Some(state) = state else { continue };
        let Ok(v) = serde_json::from_str::<Value>(state) else { continue };
        let wave = v.pointer("/metadata/annotations").and_then(|a| a.get(SYNC_WAVE)).and_then(Value::as_str);
        if let Some(wave) = wave.and_then(|w| w.trim().parse::<i64>().ok()) {
            let key = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
            out.insert((key("kind"), key("namespace"), key("name")), wave);
        }
    }
    out
}

/// The sync wave each resource of `app` has, as its status says it (`status.resources[].syncWave`,
/// kept by Argo CD for the resources it manages), with those its manifests say (`managed`, see
/// [`waves`]) over them: by kind, namespace and name.
pub fn all_waves(app: &Value, managed: Option<&Value>) -> HashMap<(String, String, String), i64> {
    let key = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    let mut out: HashMap<(String, String, String), i64> = arr(app, "/status/resources")
        .iter()
        .filter_map(|r| Some(((key(r, "kind"), key(r, "namespace"), key(r, "name")), r.get("syncWave")?.as_i64()?)))
        .collect();
    out.extend(waves(managed));
    out
}

/// Whether a step of a sync failed.
fn failed(row: &SyncRow) -> bool {
    super::super::steps::tone(&row.step) == Tone::Bad
}

/// The rows of a sync followed live: in Argo CD's order while it runs; once it ended, those
/// that failed first (the rest keep Argo CD's order).
pub fn failed_first(rows: &mut [SyncRow], ended: bool) {
    if ended {
        rows.sort_by_key(|r| !failed(r));
    }
}

/// Where a sync phase comes, Argo CD's order.
fn phase_rank(phase: &str) -> u8 {
    match phase {
        "PreSync" => 0,
        "PostSync" => 2,
        "SyncFail" => 3,
        _ => 1,
    }
}

/// The last sync's result per resource (its `steps`), each with its wave and its health now,
/// in the order Argo CD runs them: by phase, then wave, hooks first.
pub fn sync_rows(
    steps: &[Step],
    waves: &HashMap<(String, String, String), i64>,
    health: impl Fn(&Step) -> Option<String>,
) -> Vec<SyncRow> {
    let mut rows: Vec<(usize, SyncRow)> = steps
        .iter()
        .enumerate()
        .map(|(i, st)| {
            let key = (st.kind.clone(), st.namespace.clone(), st.name.clone());
            let row = SyncRow {
                wave: waves.get(&key).copied().unwrap_or(0),
                health: health(st),
                hook: !st.hook_type.is_empty(),
                step: st.clone(),
            };
            (i, row)
        })
        .collect();
    rows.sort_by_key(|(i, r)| {
        let phase = if r.step.sync_phase.is_empty() { &r.step.hook_type } else { &r.step.sync_phase };
        (phase_rank(phase), r.wave, !r.hook, *i)
    });
    rows.into_iter().map(|(_, r)| r).collect()
}

/// The health of each resource now, by kind, namespace and name: the tree's, else the
/// Application's status'.
pub fn healths(app: &Value, tree: Option<&Value>) -> HashMap<(String, String, String), String> {
    let mut out = HashMap::new();
    let key = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    for r in arr(app, "/status/resources") {
        if let Some(h) = s(r, "/health/status") {
            out.insert((key(r, "kind"), key(r, "namespace"), key(r, "name")), h.to_string());
        }
    }
    for n in tree.map_or(&[][..], |t| arr(t, "/nodes")) {
        if let Some(h) = s(n, "/health/status") {
            out.insert((key(n, "kind"), key(n, "namespace"), key(n, "name")), h.to_string());
        }
    }
    out
}

// MARK: Details

/// What the Details panel says of the Application.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Details {
    pub project: String,
    pub labels: Vec<(String, String)>,
    pub annotations: Vec<(String, String)>,
    /// Its destination: Argo CD's name of the cluster, its server, the namespace.
    pub cluster: String,
    pub server: String,
    pub namespace: String,
    pub created: Option<Timestamp>,
    /// `On (prune, self heal)` or `Off`.
    pub auto_sync: String,
    pub sync_options: Vec<String>,
    /// How a failed sync is tried again, when it is.
    pub retry: Option<String>,
    pub sync: String,
    pub revision: Option<String>,
    pub health: String,
    pub health_message: Option<String>,
    pub urls: Vec<String>,
    pub images: Vec<String>,
    /// Each source in one line: repository, path or chart, target revision.
    pub sources: Vec<String>,
    /// Its manifest (no status), as YAML; or why it couldn't be written.
    pub manifest: String,
}

fn pairs(v: Option<&Value>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = v
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), String::from)))
        .collect();
    out.sort();
    out
}

/// A source in one line: `repo · path · revision` (`chart name` for a Helm chart).
pub fn source_line(src: &Value) -> String {
    let mut parts = vec![s(src, "/repoURL").unwrap_or("?").to_string()];
    if let Some(chart) = s(src, "/chart") {
        parts.push(format!("chart {chart}"));
    }
    if let Some(path) = s(src, "/path") {
        parts.push(path.to_string());
    }
    parts.push(s(src, "/targetRevision").unwrap_or("HEAD").to_string());
    if let Some(r) = s(src, "/ref") {
        parts.push(format!("ref {r}"));
    }
    parts.join(" · ")
}

/// How a failed sync is tried again: `5 times, every 5s ×2 up to 3m`.
fn retry_text(retry: &Value) -> String {
    let limit = retry.get("limit").and_then(Value::as_i64);
    let mut out = match limit {
        Some(n) if n < 0 => "without end".to_string(),
        Some(n) => crate::plural::count(n as usize, "time"),
        None => "with the default limit".to_string(),
    };
    if let Some(b) = retry.get("backoff") {
        let mut how = vec![format!("every {}", s(b, "/duration").unwrap_or("5s"))];
        if let Some(f) = b.get("factor").and_then(Value::as_i64) {
            how.push(format!("×{f}"));
        }
        if let Some(m) = s(b, "/maxDuration") {
            how.push(format!("up to {m}"));
        }
        out.push_str(&format!(", {}", how.join(" ")));
    }
    out
}

/// The Details of `app` (read whole; a list's copy says less).
pub fn details(app: &Value) -> Details {
    let auto = app.pointer("/spec/syncPolicy/automated");
    let auto_sync = if super::super::automated_on(auto) {
        let flags: Vec<&str> = [("prune", "prune"), ("selfHeal", "self heal"), ("allowEmpty", "allow empty")]
            .iter()
            .filter(|(k, _)| auto.and_then(|a| a.get(*k)).and_then(Value::as_bool) == Some(true))
            .map(|(_, w)| *w)
            .collect();
        if flags.is_empty() { "On".to_string() } else { format!("On ({})", flags.join(", ")) }
    } else {
        "Off".to_string()
    };
    let mut shown = app.clone();
    if let Some(o) = shown.as_object_mut() {
        o.remove("status");
        o.remove("operation");
    }
    if let Some(m) = shown.get_mut("metadata").and_then(Value::as_object_mut) {
        m.remove("managedFields");
    }
    let manifest = crate::k8s::to_yaml(&shown, false, false).unwrap_or_else(|e| format!("Not written as YAML: {e}"));
    let strings = |p: &str| arr(app, p).iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>();
    Details {
        project: s(app, "/spec/project").unwrap_or("default").to_string(),
        labels: pairs(app.pointer("/metadata/labels")),
        annotations: pairs(app.pointer("/metadata/annotations")),
        cluster: s(app, "/spec/destination/name").unwrap_or_default().to_string(),
        server: s(app, "/spec/destination/server").unwrap_or_default().to_string(),
        namespace: s(app, "/spec/destination/namespace").unwrap_or_default().to_string(),
        created: time(app, "/metadata/creationTimestamp"),
        auto_sync,
        sync_options: strings("/spec/syncPolicy/syncOptions"),
        retry: app.pointer("/spec/syncPolicy/retry").filter(|r| r.is_object()).map(retry_text),
        sync: s(app, "/status/sync/status").unwrap_or("Unknown").to_string(),
        revision: app.pointer("/status/sync").and_then(super::super::revisions),
        health: s(app, "/status/health/status").unwrap_or("Unknown").to_string(),
        health_message: s(app, "/status/health/message").map(String::from),
        urls: strings("/status/summary/externalURLs"),
        images: strings("/status/summary/images"),
        sources: app.pointer("/spec").map_or_default(|spec| sources_of(spec).into_iter().map(source_line).collect()),
        manifest,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn each_kind_has_its_category() {
        let cases = [
            ("apps", "Deployment", Category::Workloads),
            ("argoproj.io", "Rollout", Category::Workloads),
            ("autoscaling", "HorizontalPodAutoscaler", Category::Workloads),
            ("batch", "CronJob", Category::Jobs),
            ("batch", "Job", Category::Jobs),
            ("", "Secret", Category::Config),
            ("external-secrets.io", "ExternalSecret", Category::Config),
            ("rbac.authorization.k8s.io", "RoleBinding", Category::Config),
            ("", "Service", Category::Networking),
            ("networking.k8s.io", "Ingress", Category::Networking),
            ("networking.k8s.io", "NetworkPolicy", Category::Networking),
            ("", "PersistentVolumeClaim", Category::Storage),
            ("postgresql.cnpg.io", "Cluster", Category::Others),
            // A CRD named as a built-in kind is not one.
            ("example.com", "Deployment", Category::Others),
            ("", "Namespace", Category::Others),
        ];
        for (group, kind, want) in cases {
            assert_eq!(Category::of(group, kind), want, "{group}/{kind}");
        }
    }

    /// The tree's objects with their pods ready, a CronJob's Jobs listed, pods and ReplicaSets
    /// counted rather than listed.
    #[test]
    fn the_tree_lists_objects_with_their_pods() {
        let node = |kind: &str, group: &str, name: &str, uid: &str, parent: Option<&str>, health: Option<&str>| {
            let mut n = json!({"kind": kind, "group": group, "namespace": "shop", "name": name, "uid": uid});
            if let Some(p) = parent {
                n["parentRefs"] = json!([{"uid": p}]);
            }
            if let Some(h) = health {
                n["health"] = json!({"status": h});
            }
            n
        };
        let tree = json!({"nodes": [
            node("Deployment", "apps", "web", "d", None, Some("Healthy")),
            node("ReplicaSet", "apps", "web-1", "rs", Some("d"), Some("Healthy")),
            node("Pod", "", "web-1-a", "p1", Some("rs"), Some("Healthy")),
            node("Pod", "", "web-1-b", "p2", Some("rs"), Some("Degraded")),
            node("CronJob", "batch", "nightly", "cj", None, Some("Healthy")),
            node("Job", "batch", "nightly-1", "j", Some("cj"), Some("Degraded")),
            node("Service", "", "web", "s", None, Some("Healthy")),
        ]});
        let nodes = nodes_from_tree(&tree);
        let names: Vec<(&str, Category)> = nodes.iter().map(|n| (n.res.name.as_str(), n.category)).collect();
        assert_eq!(
            names,
            [
                ("web", Category::Workloads),
                ("nightly", Category::Jobs),
                ("nightly-1", Category::Jobs),
                ("web", Category::Networking)
            ]
        );
        assert_eq!(nodes[0].pods_summary(), Some(("1/2".to_string(), Tone::Busy)));
        assert!(nodes[2].in_trouble());
        assert!(nodes[2].keeps("job night", true, None) && !nodes[0].keeps("", true, None));
        assert!(nodes[0].keeps("", false, Some("Healthy")) && !nodes[2].keeps("", false, Some("Healthy")));
    }

    /// The Resources view lists the categories with trouble first, and in each the worst
    /// first, then by kind and name; an empty category is left out.
    #[test]
    fn the_resources_are_listed_problems_first() {
        let node = |group: &str, kind: &str, name: &str, health: &str| {
            let res = Res { group: group.into(), kind: kind.into(), name: name.into(), ..Default::default() };
            Node {
                category: Category::of(group, kind),
                tone: health_tone(Some(health)),
                health: Some(health.into()),
                message: None,
                pods: None,
                res,
            }
        };
        let nodes = [
            node("apps", "Deployment", "api", "Healthy"),
            node("apps", "Deployment", "web", "Progressing"),
            node("", "ConfigMap", "a", "Healthy"),
            node("", "Secret", "b", "Missing"),
            node("", "ConfigMap", "z", "Degraded"),
            node("", "Service", "api", "Healthy"),
        ];
        let kept: Vec<&Node> = nodes.iter().collect();
        let got: Vec<(Category, Vec<&str>, usize)> = sections(&kept)
            .into_iter()
            .map(|s| (s.category, s.nodes.iter().map(|n| n.res.name.as_str()).collect(), s.troubled))
            .collect();
        assert_eq!(
            got,
            [
                (Category::Config, vec!["z", "b", "a"], 2),
                (Category::Workloads, vec!["web", "api"], 0),
                (Category::Networking, vec!["api"], 0),
            ]
        );
        assert!(sections(&[]).is_empty());
    }

    /// A pod run to completion is never ready, and isn't meant to be: a CronJob and its Jobs
    /// say their runs (done, running, failed), not ready of all; what serves doesn't count it.
    #[test]
    fn completed_pods_are_runs_done_not_pods_unready() {
        let node = |kind: &str, group: &str, uid: &str, parent: Option<&str>, health: &str, reason: Option<&str>| {
            let mut n = json!({"kind": kind, "group": group, "namespace": "shop", "name": uid, "uid": uid,
                               "health": {"status": health}});
            if let Some(p) = parent {
                n["parentRefs"] = json!([{"uid": p}]);
            }
            if let Some(r) = reason {
                n["info"] = json!([{"name": "Status Reason", "value": r}]);
            }
            n
        };
        let tree = json!({"nodes": [
            node("CronJob", "batch", "cj", None, "Healthy", None),
            node("Job", "batch", "j1", Some("cj"), "Healthy", None),
            node("Pod", "", "j1-a", Some("j1"), "Healthy", Some("Completed")),
            node("Job", "batch", "j2", Some("cj"), "Healthy", None),
            node("Pod", "", "j2-a", Some("j2"), "Healthy", Some("Completed")),
            node("Job", "batch", "j3", Some("cj"), "Degraded", None),
            node("Pod", "", "j3-a", Some("j3"), "Degraded", Some("Error")),
            node("Job", "batch", "j4", None, "Progressing", None),
            node("Pod", "", "j4-a", Some("j4"), "Progressing", Some("Running")),
            node("Deployment", "apps", "web", None, "Healthy", None),
            node("ReplicaSet", "apps", "web-1", Some("web"), "Healthy", None),
            node("Pod", "", "web-1-a", Some("web-1"), "Healthy", Some("Running")),
            node("Pod", "", "web-1-b", Some("web-1"), "Healthy", Some("Completed")),
        ]});
        let nodes = nodes_from_tree(&tree);
        let summary = |name: &str| {
            let n = nodes.iter().find(|n| n.res.name == name).expect(name);
            n.pods_summary()
        };
        let say = |s: &str, t: Tone| Some((s.to_string(), t));
        assert_eq!(summary("cj"), say("2 done, 1 failed", Tone::Bad));
        assert_eq!(summary("j1"), say("1 done", Tone::Done));
        assert_eq!(summary("j3"), say("1 failed", Tone::Bad));
        assert_eq!(summary("j4"), say("1 running", Tone::Busy));
        assert_eq!(summary("web"), say("1/1", Tone::Done));
        let web = nodes.iter().find(|n| n.res.name == "web").unwrap();
        assert_eq!(web.pods_tip().as_deref(), Some("1 of 1 pod ready, 1 completed"));
        let j1 = nodes.iter().find(|n| n.res.name == "j1").unwrap();
        assert_eq!(j1.pods_tip().as_deref(), Some("Pods: 1 done"));
        // What serves with only completed pods says them as runs, not 0/0.
        let only_done = Pods { done: 2, all: 2, ..Pods::default() };
        assert_eq!(only_done.summary(false), say("2 done", Tone::Done));
        assert_eq!(Pods::default().summary(true), None);
    }

    /// The sync's result in Argo CD's order: by phase, then wave (from the manifests), hooks
    /// first in a wave; a resource that says no wave is in wave 0.
    #[test]
    fn the_sync_result_is_ordered_by_phase_and_wave() {
        let step = |kind: &str, name: &str, phase: &str, hook: &str| Step {
            kind: kind.into(),
            namespace: "shop".into(),
            name: name.into(),
            status: "Synced".into(),
            sync_phase: phase.into(),
            hook_type: hook.into(),
            ..Default::default()
        };
        let steps = [
            step("Deployment", "web", "Sync", ""),
            step("Job", "smoke", "PostSync", "PostSync"),
            step("ConfigMap", "config", "Sync", ""),
            step("Job", "migrate", "PreSync", "PreSync"),
            step("Job", "seed", "Sync", "Sync"),
        ];
        let state = |wave: &str| json!({"metadata": {"annotations": {SYNC_WAVE: wave}}}).to_string();
        let managed = json!({"items": [
            {"kind": "ConfigMap", "namespace": "shop", "name": "config", "targetState": state("-1")},
            {"kind": "Deployment", "namespace": "shop", "name": "web", "targetState": "{}"},
        ]});
        let waves = waves(Some(&managed));
        let rows = sync_rows(&steps, &waves, |s| (s.kind == "Deployment").then(|| "Healthy".to_string()));
        let order: Vec<(&str, i64)> = rows.iter().map(|r| (r.step.name.as_str(), r.wave)).collect();
        assert_eq!(order, [("migrate", 0), ("config", -1), ("seed", 0), ("web", 0), ("smoke", 0)]);
        assert!(rows[0].hook && !rows[1].hook);
        assert!(rows[3].keeps(Some("Synced"), Some("Healthy"), Some(false)));
        assert!(!rows[3].keeps(None, None, Some(true)) && rows[1].keeps(None, Some("None"), None));
    }

    #[test]
    fn the_details_say_what_argo_cd_s_panel_says() {
        let app = json!({
            "metadata": {"name": "web", "labels": {"team": "shop"}, "creationTimestamp": "2026-01-01T00:00:00Z",
                         "managedFields": [{"manager": "x"}]},
            "spec": {
                "project": "shop",
                "destination": {"name": "prod", "server": "https://prod.example", "namespace": "web"},
                "sources": [
                    {"repoURL": "https://git.example.com/shop/deploy.git", "path": "web", "targetRevision": "main"},
                    {"repoURL": "https://charts.example.com", "chart": "web", "targetRevision": "1.2.0"},
                ],
                "syncPolicy": {"automated": {"prune": true, "selfHeal": true}, "syncOptions": ["CreateNamespace=true"],
                               "retry": {"limit": 5, "backoff": {"duration": "5s", "factor": 2, "maxDuration": "3m"}}},
            },
            "status": {"sync": {"status": "Synced", "revisions": ["abcdef1234567", "1.2.0"]}, "health": {"status": "Healthy"},
                       "summary": {"externalURLs": ["https://web.example.com"], "images": ["r.example/web:1"]}},
        });
        let d = details(&app);
        assert_eq!(d.project, "shop");
        assert_eq!(d.labels, [("team".to_string(), "shop".to_string())]);
        assert_eq!(
            (d.cluster.as_str(), d.server.as_str(), d.namespace.as_str()),
            ("prod", "https://prod.example", "web")
        );
        assert_eq!(d.auto_sync, "On (prune, self heal)");
        assert_eq!(d.sync_options, ["CreateNamespace=true"]);
        assert_eq!(d.retry.as_deref(), Some("5 times, every 5s ×2 up to 3m"));
        assert_eq!(
            d.sources,
            ["https://git.example.com/shop/deploy.git · web · main", "https://charts.example.com · chart web · 1.2.0"]
        );
        assert_eq!(d.urls, ["https://web.example.com"]);
        assert!(d.manifest.contains("project: shop") && !d.manifest.contains("externalURLs"));
        assert!(!d.manifest.contains("managedFields"));
        assert_eq!(details(&json!({})).auto_sync, "Off");
    }
}
