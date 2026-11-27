//! Argo CD, read only through its API (`api`): what an Application is (`Summary`), where it
//! deploys among the kubeconfig's contexts, the Application that deployed an object (its
//! tracking id), the ones that deploy into a namespace, and every Application of the
//! servers (`from_api`).
//!
//! Everything here reads. The actions go through `api::act` (the window's) and
//! `api::act_for_agent`.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use anyhow::Result;
use kube::discovery::ApiResource;
use serde_json::Value;

use super::{ContextTarget, Kind};
use crate::status::Tone;

pub mod api;
pub mod connect;
pub mod dashboard;
pub mod diagnosis;
pub mod environments;
pub mod grouped_diff;
pub mod grouped_sync;
pub mod grouping;
pub mod journal;
pub mod ops;
pub mod release;
mod revision;
pub mod server;
pub mod steps;
pub mod sync_diff;
pub mod sync_phases;
pub mod windows;

/// The annotation Argo CD puts on what it deploys.
pub const TRACKING_ID: &str = "argocd.argoproj.io/tracking-id";
/// The label Argo CD's older tracking mode sets (Helm sets it too, so it is only a hint).
const INSTANCE_LABEL: &str = "app.kubernetes.io/instance";
/// How Applications name the cluster Argo CD runs in.
const IN_CLUSTER: &str = "https://kubernetes.default.svc";
/// Where Argo CD keeps its Applications by default (a tracking id without `<ns>_`).
pub const DEFAULT_NAMESPACE: &str = "argocd";
/// The longest wait for a context to connect, then to answer whether it serves Applications.
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// Argo CD's Application kind.
pub fn application_kind() -> Kind {
    Kind {
        ar: ApiResource {
            group: "argoproj.io".into(),
            version: "v1alpha1".into(),
            api_version: "argoproj.io/v1alpha1".into(),
            kind: "Application".into(),
            plural: "applications".into(),
        },
        namespaced: true,
        section: super::crd_section("argoproj.io"),
        label: "Application".into(),
    }
}

// MARK: Tracking ids

/// What a tracking id says: `<app-ns>_<app>:<group>/<kind>:<namespace>/<name>`, the
/// `<app-ns>_` only when the Application is outside Argo CD's own namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tracking {
    pub app_ns: Option<String>,
    pub app: String,
    pub group: String,
    pub kind: String,
    /// Empty for a cluster-scoped object.
    pub namespace: String,
    pub name: String,
}

/// Read a tracking id; None when it is not one.
pub fn parse_tracking(id: &str) -> Option<Tracking> {
    let mut parts = id.trim().split(':');
    let (app, group_kind, ns_name) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    let (group, kind) = group_kind.split_once('/')?;
    let (namespace, name) = ns_name.split_once('/')?;
    // Names and namespaces never hold `_`: the first one ends the namespace.
    let (app_ns, app) = match app.split_once('_') {
        Some((ns, app)) => (Some(ns), app),
        None => (None, app),
    };
    if app.is_empty() || kind.is_empty() || name.is_empty() || app_ns == Some("") {
        return None;
    }
    Some(Tracking {
        app_ns: app_ns.map(String::from),
        app: app.to_string(),
        group: group.to_string(),
        kind: kind.to_string(),
        namespace: namespace.to_string(),
        name: name.to_string(),
    })
}

/// The object's tracking id, when it is about this very object: a copy of another object's
/// annotation (`kubectl get -o yaml | kubectl apply`) is not Argo CD's, and Argo CD ignores it too.
pub fn tracking_of(group: &str, kind: &str, obj: &Value) -> Option<Tracking> {
    let id = obj.pointer("/metadata/annotations")?.get(TRACKING_ID)?.as_str()?;
    let t = parse_tracking(id)?;
    let ns = obj.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default();
    let name = obj.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default();
    (t.group == group && t.kind == kind && t.name == name && (t.namespace.is_empty() || t.namespace == ns)).then_some(t)
}

/// How an object names the Application that deployed it, per Argo CD's tracking method.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TrackedBy {
    /// Its tracking id (`annotation` and `annotation+label` tracking): about this very object.
    Id(Tracking),
    /// Its `app.kubernetes.io/instance` label (`label` tracking): only a hint, as Helm sets
    /// the same label.
    Label(String),
}

/// The Application an object says deployed it: its tracking id, else its instance label. A
/// pod carries its workload's label but is deployed by the workload, not by Argo CD.
pub fn tracked_by(group: &str, kind: &str, obj: &Value) -> Option<TrackedBy> {
    if let Some(t) = tracking_of(group, kind, obj) {
        return Some(TrackedBy::Id(t));
    }
    if group.is_empty() && kind == "Pod" {
        return None;
    }
    let label = obj.pointer("/metadata/labels").and_then(|l| l.get(INSTANCE_LABEL)).and_then(Value::as_str)?;
    (!label.is_empty()).then(|| TrackedBy::Label(label.to_string()))
}

// MARK: Destinations

/// A server URL as compared: host in lowercase, no trailing slash, no default port.
pub fn server_key(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    let (scheme, rest) = url.split_once("://").unwrap_or(("https", url));
    let scheme = scheme.to_ascii_lowercase();
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let host = host.to_ascii_lowercase();
    let default_port = if scheme == "http" { ":80" } else { ":443" };
    let host = host.strip_suffix(default_port).unwrap_or(&host);
    if path.is_empty() { format!("{scheme}://{host}") } else { format!("{scheme}://{host}/{path}") }
}

fn same_server(a: Option<&str>, b: Option<&str>) -> bool {
    matches!((a, b), (Some(a), Some(b)) if server_key(a) == server_key(b))
}

/// Whether two contexts reach the same cluster.
fn same_cluster(a: &ContextTarget, b: &ContextTarget) -> bool {
    a.name == b.name || same_server(a.server.as_deref(), b.server.as_deref())
}

/// Whether an Application, read on the hub `hub`, deploys to the cluster of the context `here`.
/// A destination given by name matches the context's name or its kubeconfig cluster's name
/// (Argo CD's own cluster names are in Secrets, which are not read).
pub fn targets_cluster(app: &Value, hub: &ContextTarget, here: &ContextTarget) -> bool {
    app_deploys_to(app, hub, here) == Some(true)
}

/// `targets_cluster` for a destination's `server` and `name` as written.
pub fn deploys_to(server: Option<&str>, name: Option<&str>, hub: &ContextTarget, here: &ContextTarget) -> bool {
    destination_is(server, name, hub, here) == Some(true)
}

/// [`targets_cluster`], None when it can't be told: a destination named neither `in-cluster`
/// nor as the context or its kubeconfig cluster may be the name of an Argo CD cluster Secret
/// for this very cluster.
pub fn app_deploys_to(app: &Value, hub: &ContextTarget, here: &ContextTarget) -> Option<bool> {
    let dest = |p: &str| app.pointer(p).and_then(Value::as_str);
    destination_is(dest("/spec/destination/server"), dest("/spec/destination/name"), hub, here)
}

/// Whether the destination `server` / `name` is the cluster of `here`; None when it can't be
/// told (see [`app_deploys_to`]).
fn destination_is(server: Option<&str>, name: Option<&str>, hub: &ContextTarget, here: &ContextTarget) -> Option<bool> {
    match (server.filter(|s| !s.is_empty()), name.filter(|s| !s.is_empty())) {
        (Some(s), _) if server_key(s) == server_key(IN_CLUSTER) => Some(same_cluster(hub, here)),
        (Some(s), _) => Some(same_server(Some(s), here.server.as_deref())),
        (None, Some("in-cluster")) => Some(same_cluster(hub, here)),
        (None, Some(n)) if n == here.name || n == here.cluster => Some(true),
        (None, Some(_)) => None,
        (None, None) => Some(false),
    }
}

fn resources(app: &Value) -> impl Iterator<Item = &Value> {
    app.pointer("/status/resources").and_then(Value::as_array).into_iter().flatten()
}

/// Whether a sync with prune deletes the resource `r` (an entry of `status.resources`): in the
/// cluster, no longer in git, and out of sync. Argo CD also sets `requiresPruning` on a hook
/// left from the last sync, which it never prunes and gives no sync, and on every resource
/// while it can't load the manifests, whose sync is then `Unknown`. Its own UI shows the
/// prune mark only on what is out of sync too.
pub(crate) fn to_prune(r: &Value) -> bool {
    r["requiresPruning"] == true && r["status"] == "OutOfSync"
}

/// Whether an Application deploys into the namespace: its destination, or anything it tracks there.
pub fn targets_namespace(app: &Value, ns: &str) -> bool {
    app.pointer("/spec/destination/namespace").and_then(Value::as_str) == Some(ns)
        || resources(app).any(|r| r["namespace"].as_str() == Some(ns))
}

/// The object's line in the Application's `status.resources`.
pub fn resource_of<'a>(app: &'a Value, group: &str, kind: &str, ns: &str, name: &str) -> Option<&'a Value> {
    resources(app).find(|r| {
        r["group"].as_str().unwrap_or_default() == group
            && r["kind"].as_str() == Some(kind)
            && r["namespace"].as_str().unwrap_or_default() == ns
            && r["name"].as_str() == Some(name)
    })
}

// MARK: Resources table

/// One line of an Application's resources table, from its `status.resources`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ResourceRow {
    pub res: diagnosis::Res,
    /// `Synced`, `OutOfSync`, or `Unknown` when Argo CD couldn't compare it (it can't load
    /// the manifests, or the project doesn't permit the kind); empty for a hook, whose sync
    /// Argo CD doesn't track.
    pub sync: String,
    /// Empty when Argo CD keeps none for it: a hook, a kind without a health check
    /// (`ConfigMap`, Secret...), or any kind when it keeps no health per resource in the
    /// Application.
    pub health: String,
    pub message: Option<String>,
    /// Out of sync, in the cluster and no longer in git: a sync with prune deletes it
    /// ([`to_prune`]).
    pub prune: bool,
    /// A hook (a Job run before or after a sync), not something it applies.
    pub hook: bool,
}

impl ResourceRow {
    /// What to look at before a sync: out of sync (to change, to create or to prune), or
    /// Degraded or Missing. Progressing is on its way, Suspended is on purpose, and `Unknown`
    /// is Argo CD failing to compare, which the diagnosis says once for the Application.
    pub fn needs_attention(&self) -> bool {
        self.sync == "OutOfSync" || matches!(self.health.as_str(), "Degraded" | "Missing")
    }
}

/// An Application's resources, in the order Argo CD lists them. Read from the Application
/// itself: nothing is asked of any cluster, however many there are.
pub fn resource_rows(app: &Value) -> Vec<ResourceRow> {
    resources(app)
        .filter_map(|r| {
            let s = |p: &str| r.pointer(p).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
            Some(ResourceRow {
                res: diagnosis::Res::of(r)?,
                sync: s("/status").unwrap_or_default(),
                health: s("/health/status").unwrap_or_default(),
                message: s("/health/message"),
                prune: to_prune(r),
                hook: r["hook"] == true,
            })
        })
        .collect()
}

/// Whether Argo CD keeps no health per resource in the Application: it says so with
/// `status.resourceHealthSource: appTree`, the default from Argo CD 3.0 (it keeps them in its
/// resource tree then), unless `controller.resource.health.persist` is on. Older versions
/// don't set the field: they keep each resource's health in the Application.
pub fn keeps_no_health(app: &Value) -> bool {
    app.pointer("/status/resourceHealthSource").and_then(Value::as_str) == Some("appTree")
}

// MARK: What is shown

/// One past deployment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Deployment {
    /// Its id in `status.history`: what a rollback names.
    pub id: Option<i64>,
    pub revision: String,
    pub deployed_at: Option<String>,
    /// Who started it: a user, or "auto-sync".
    pub by: Option<String>,
}

/// What a page shows of an Application, worked out once when it is read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub name: String,
    pub namespace: String,
    /// Its Argo CD project (`default` when it names none).
    pub project: String,
    pub sync: String,
    pub health: String,
    pub health_message: Option<String>,
    /// Auto-sync is on: out of sync is then something to look at, not the normal state.
    pub automated: bool,
    /// The revision(s) deployed, short.
    pub revision: Option<String>,
    /// The last operation: phase, message, when it finished.
    pub operation: Option<(String, Option<String>, Option<String>)>,
    /// An operation is running or waiting to start (`operation` is set): it can be terminated.
    pub in_progress: bool,
    /// The last operation: when it started, who started it (`dashboard::who`), the revision(s) it
    /// synced to (short).
    pub operation_started: Option<String>,
    pub operation_by: Option<String>,
    pub operation_revision: Option<String>,
    /// The last deployments, newest first.
    pub history: Vec<Deployment>,
    /// Conditions as (type, message): errors and warnings Argo CD reports.
    pub conditions: Vec<(String, String)>,
    pub urls: Vec<String>,
    /// Being deleted since (a stuck finalizer keeps it so).
    pub deleting: Option<String>,
    /// This object's own (sync, health, health message) in it, on an object's page.
    pub own: Option<(String, String, Option<String>)>,
    /// Its labels, sorted by key.
    pub labels: Vec<(String, String)>,
    /// The images it runs, as Argo CD sums them up (`status.summary.images`).
    pub images: Vec<String>,
    /// Its destination as written: the server URL, the cluster name, the namespace.
    pub dest_server: String,
    pub dest_name: String,
    pub dest_namespace: String,
}

pub use revision::short_revision;

/// `status.sync.revision`, or each of `status.sync.revisions` (several sources).
pub(crate) fn revisions(v: &Value) -> Option<String> {
    let one = v.get("revision").and_then(Value::as_str).filter(|s| !s.is_empty()).map(short_revision);
    let many: Vec<String> = v
        .get("revisions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(short_revision)
        .collect();
    one.or_else(|| (!many.is_empty()).then(|| many.join(", ")))
}

/// How many past deployments are shown.
const HISTORY: usize = 5;

/// Whether auto-sync is on: `syncPolicy.automated` set, and not turned off with
/// `enabled: false` (newer Argo CD).
pub fn auto_sync(app: &Value) -> bool {
    automated_on(app.pointer("/spec/syncPolicy/automated"))
}

/// Whether `syncPolicy.automated`, as written, turns auto-sync on.
pub fn automated_on(automated: Option<&Value>) -> bool {
    automated.is_some_and(|a| a.is_object() && a.get("enabled").and_then(Value::as_bool) != Some(false))
}

impl Summary {
    pub fn of(app: &Value) -> Self {
        let s = |p: &str| app.pointer(p).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
        let automated = auto_sync(app);
        let operation = s("/status/operationState/phase")
            .map(|phase| (phase, s("/status/operationState/message"), s("/status/operationState/finishedAt")));
        let history = app
            .pointer("/status/history")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .rev()
            .take(HISTORY)
            .map(|h| Deployment {
                id: h["id"].as_i64(),
                revision: revisions(h).unwrap_or_default(),
                deployed_at: h["deployedAt"].as_str().map(String::from),
                by: if h.pointer("/initiatedBy/automated").and_then(Value::as_bool) == Some(true) {
                    Some("auto-sync".into())
                } else {
                    h.pointer("/initiatedBy/username").and_then(Value::as_str).map(String::from)
                },
            })
            .collect();
        let conditions = app
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| {
                Some((c["type"].as_str()?.to_string(), c["message"].as_str().unwrap_or_default().to_string()))
            })
            .collect();
        let urls = app
            .pointer("/status/summary/externalURLs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            // Set by annotations in the manifests: only web links are kept, never a file,
            // share or another program's scheme for the desktop to launch.
            .filter(|u| crate::forwards::is_web_link(u))
            .map(String::from)
            .collect();
        Self {
            name: s("/metadata/name").unwrap_or_default(),
            namespace: s("/metadata/namespace").unwrap_or_default(),
            project: s("/spec/project").unwrap_or_else(|| "default".into()),
            sync: s("/status/sync/status").unwrap_or_else(|| "Unknown".into()),
            health: s("/status/health/status").unwrap_or_else(|| "Unknown".into()),
            health_message: s("/status/health/message"),
            automated,
            revision: app.pointer("/status/sync").and_then(revisions),
            operation,
            in_progress: ops::in_progress(app),
            operation_started: s("/status/operationState/startedAt"),
            operation_by: app.pointer("/status/operationState/operation/initiatedBy").and_then(dashboard::who),
            operation_revision: app
                .pointer("/status/operationState/syncResult")
                .and_then(revisions)
                .or_else(|| app.pointer("/status/operationState/operation/sync").and_then(revisions)),
            history,
            conditions,
            urls,
            deleting: s("/metadata/deletionTimestamp"),
            own: None,
            labels: app
                .pointer("/metadata/labels")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect(),
            images: app
                .pointer("/status/summary/images")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect(),
            dest_server: s("/spec/destination/server").unwrap_or_default(),
            dest_name: s("/spec/destination/name").unwrap_or_default(),
            dest_namespace: s("/spec/destination/namespace").unwrap_or_default(),
        }
    }

    /// The same, with this object's own sync and health in it.
    pub fn with_own(mut self, app: &Value, group: &str, kind: &str, ns: &str, name: &str) -> Self {
        self.own = resource_of(app, group, kind, ns, name).map(|r| {
            let s = |p: &str| r.pointer(p).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
            (
                s("/status").unwrap_or_else(|| "Unknown".into()),
                s("/health/status").unwrap_or_default(),
                s("/health/message"),
            )
        });
        self
    }
}

/// Healthy is good, Progressing on its way, Degraded and Missing wrong; Suspended and
/// Unknown say nothing either way.
pub fn health_tone(health: &str) -> Tone {
    match health {
        "Healthy" => Tone::Good,
        "Progressing" => Tone::Busy,
        "Degraded" | "Missing" => Tone::Bad,
        _ => Tone::Done,
    }
}

/// Synced is good. Out of sync is the normal state of an Application synced by hand, so it
/// is only a warning when auto-sync is on.
pub fn sync_tone(sync: &str, automated: bool) -> Tone {
    match sync {
        "Synced" => Tone::Good,
        "OutOfSync" if automated => Tone::Busy,
        _ => Tone::Done,
    }
}

/// A sync that failed is wrong; one running is on its way.
pub fn operation_tone(phase: &str) -> Tone {
    match phase {
        "Succeeded" => Tone::Good,
        "Running" | "Terminating" => Tone::Busy,
        "Failed" | "Error" => Tone::Bad,
        _ => Tone::Done,
    }
}

/// An Application's health in a list of them. Missing is what an Application synced by hand
/// shows for what it has not deployed yet, so it is only wrong when auto-sync is on.
pub fn listed_health_tone(health: &str, automated: bool) -> Tone {
    match health {
        "Missing" if !automated => Tone::Done,
        _ => health_tone(health),
    }
}

/// Whether the last sync failed.
pub fn sync_failed(app: &Summary) -> bool {
    app.operation.as_ref().is_some_and(|(phase, _, _)| operation_tone(phase) == Tone::Bad)
}

/// How an Application reads in a list of them: red for a failed sync, an error Argo CD
/// reports (a manifest it can't render) or Degraded health; amber while it is on its way or
/// out of sync with auto-sync on. Out of sync with auto-sync off is never red on its own: it
/// is how an Application synced by hand (`*-workload`) waits for its next sync.
pub fn attention(app: &Summary) -> Tone {
    let error = app.conditions.iter().any(|(ty, _)| ty.ends_with("Error"));
    let running = app.operation.as_ref().is_some_and(|(phase, _, _)| operation_tone(phase) == Tone::Busy);
    let health = listed_health_tone(&app.health, app.automated);
    if sync_failed(app) || error || health == Tone::Bad {
        Tone::Bad
    } else if running || health == Tone::Busy || sync_tone(&app.sync, app.automated) == Tone::Busy {
        Tone::Busy
    } else if app.sync == "Synced" && health == Tone::Good {
        Tone::Good
    } else {
        Tone::Done
    }
}

/// What makes Applications need a look, the one rule of the Argo CD page's *Needs a look*
/// (its filter, its headings' and types' counts, its Overview) and of the agents'
/// `problems_only`: a failed sync, an error Argo CD reports, Degraded (or Missing with
/// auto-sync on), a sync running for over 10 minutes (`diagnosis::STUCK_AFTER`), or out of
/// sync with auto-sync on. Out of sync or Missing with auto-sync off is how an Application
/// synced by hand waits for its next sync, and a sync running is on its way: neither counts.
/// Several Applications' (an environment's) add up with `add`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Look {
    pub failed: bool,
    pub error: bool,
    pub degraded: bool,
    pub missing: bool,
    /// When the oldest sync still running started, in seconds.
    pub running_since: Option<i64>,
    pub auto_out_of_sync: bool,
}

impl Look {
    pub fn of(app: &Summary) -> Self {
        let health = listed_health_tone(&app.health, app.automated) == Tone::Bad;
        let running = app.operation.as_ref().is_some_and(|(phase, _, _)| operation_tone(phase) == Tone::Busy);
        Self {
            failed: sync_failed(app),
            error: app.conditions.iter().any(|(ty, _)| ty.ends_with("Error")),
            degraded: health && app.health != "Missing",
            missing: health && app.health == "Missing",
            running_since: app
                .operation_started
                .as_deref()
                .filter(|_| running)
                .and_then(|t| t.parse::<jiff::Timestamp>().ok())
                .map(jiff::Timestamp::as_second),
            auto_out_of_sync: app.automated && app.sync == "OutOfSync",
        }
    }

    /// Both together: an environment's from its Applications'.
    #[must_use]
    pub fn add(self, other: Self) -> Self {
        Self {
            failed: self.failed || other.failed,
            error: self.error || other.error,
            degraded: self.degraded || other.degraded,
            missing: self.missing || other.missing,
            running_since: match (self.running_since, other.running_since) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
            auto_out_of_sync: self.auto_out_of_sync || other.auto_out_of_sync,
        }
    }

    /// Why it needs a look at `now`, worst first; empty when it doesn't.
    pub fn why(self, now: jiff::Timestamp) -> Vec<(String, Tone)> {
        let mut out = Vec::new();
        for (on, text) in [
            (self.failed, "Sync failed"),
            (self.error, "Error"),
            (self.degraded, "Degraded"),
            (self.missing, "Missing"),
        ] {
            if on {
                out.push((text.to_string(), Tone::Bad));
            }
        }
        if let Some(secs) = self.running_since.map(|t| now.as_second() - t).filter(|&s| s >= diagnosis::STUCK_AFTER) {
            out.push((format!("Syncing for {}", super::duration_text(secs)), Tone::Busy));
        }
        if self.auto_out_of_sync {
            out.push(("Out of sync, auto-sync on".to_string(), Tone::Busy));
        }
        out
    }

    pub fn needs_a_look(self, now: jiff::Timestamp) -> bool {
        !self.why(now).is_empty()
    }
}

// MARK: Contexts

/// What the kubeconfig's contexts point to, read again only when the files change.
pub(crate) fn targets() -> Arc<Vec<ContextTarget>> {
    type Cached = (Vec<(std::path::PathBuf, Option<std::time::SystemTime>, u64)>, Arc<Vec<ContextTarget>>);
    static CACHE: Mutex<Option<Cached>> = Mutex::new(None);
    let signature = super::kubeconfig_signature();
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((sig, targets)) = cache.as_ref()
        && *sig == signature
    {
        return targets.clone();
    }
    let targets = Arc::new(super::context_targets().unwrap_or_default());
    *cache = Some((signature, targets.clone()));
    targets
}

pub(crate) fn target(targets: &[ContextTarget], context: &str) -> ContextTarget {
    targets
        .iter()
        .find(|t| t.name == context)
        .cloned()
        .unwrap_or_else(|| ContextTarget { name: context.to_string(), ..Default::default() })
}

// MARK: For the Problems scan

/// What the Problems scan reads of an Application: the rest of each one is skipped while the
/// list is parsed, never held.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct LeanApp {
    pub metadata: LeanAppMeta,
    pub spec: LeanAppSpec,
    pub status: Option<LeanAppStatus>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct LeanAppMeta {
    pub name: String,
    pub namespace: String,
    #[serde(rename = "creationTimestamp")]
    pub created: Option<String>,
    #[serde(rename = "deletionTimestamp")]
    pub deleted: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct LeanAppSpec {
    pub destination: Option<LeanDestination>,
    #[serde(rename = "syncPolicy")]
    pub sync_policy: Option<LeanSyncPolicy>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct LeanDestination {
    pub server: Option<String>,
    pub name: Option<String>,
    pub namespace: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct LeanSyncPolicy {
    pub automated: Option<Value>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct LeanAppStatus {
    pub health: Option<LeanHealth>,
    #[serde(rename = "operationState")]
    pub operation: Option<LeanOperation>,
    pub conditions: Option<Vec<LeanCondition>>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct LeanHealth {
    pub status: Option<String>,
    pub message: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct LeanOperation {
    pub phase: Option<String>,
    pub message: Option<String>,
    #[serde(rename = "startedAt")]
    pub started: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
pub struct LeanCondition {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub message: Option<String>,
}

impl LeanApp {
    /// Whether auto-sync is on (see `auto_sync`).
    pub fn automated(&self) -> bool {
        automated_on(self.spec.sync_policy.as_ref().and_then(|p| p.automated.as_ref()))
    }

    /// Whether it deploys to the cluster of the context `here` (see `targets_cluster`).
    pub fn deploys_to(&self, hub: &ContextTarget, here: &ContextTarget) -> bool {
        let dest = self.spec.destination.as_ref();
        deploys_to(dest.and_then(|d| d.server.as_deref()), dest.and_then(|d| d.name.as_deref()), hub, here)
    }

    /// The namespace it deploys into, if it names one.
    pub fn dest_namespace(&self) -> Option<&str> {
        self.spec.destination.as_ref()?.namespace.as_deref().filter(|n| !n.is_empty())
    }
}

/// An Application found, with the Argo CD server it was read from.
#[derive(Clone)]
pub struct Found {
    /// The Argo CD API server it was read from.
    pub server: Arc<api::Endpoint>,
    pub app: Summary,
}

/// What a page's Argo CD section shows: the Applications found, and why some could not be
/// read, in one line. Both empty: nothing to show (no Argo CD, or it deploys nothing here).
#[derive(Clone, Default)]
pub struct Lookup {
    pub apps: Vec<Found>,
    pub note: Option<String>,
    /// No Argo CD server could be read: how many Applications there are is unknown, not 0.
    pub unknown: bool,
    /// `note` says which servers could not be read (not what was found).
    pub note_unread: bool,
    /// On an Application's own page: the Argo CD server holding it (same `metadata.uid`), or
    /// why none does. None until read, and on other pages.
    pub own: Option<Result<Arc<api::Endpoint>, String>>,
}

/// The Application that deployed this object (on the page of context `here`): by its
/// tracking id, else by the instance label when an Application tracks the object under
/// that name. Nothing is asked of any cluster for an object with neither.
/// Found in what Argo CD's API feeds hold; with no server connected, a tracking id says so.
pub async fn for_object(here: String, group: String, kind: String, obj: Value) -> Lookup {
    // Off the UI thread: the servers' config is read, and their feeds may start.
    let read = async move {
        let Some(by) = tracked_by(&group, &kind, &obj) else { return Ok(Lookup::default()) };
        if let Some(found) = api_object_lookup(&here, &by, &group, &kind, &obj).await {
            return Ok(found);
        }
        let note = match &by {
            TrackedBy::Id(t) => Some(format!("Deployed by Argo CD Application {}. {NO_SERVER_READ}", t.app)),
            TrackedBy::Label(_) => None,
        };
        Ok(Lookup { note, ..Lookup::default() })
    };
    super::run(read).await.unwrap_or_default()
}

/// Said when an object names an Application but no Argo CD server is set or logged in to.
const NO_SERVER_READ: &str =
    "No Argo CD server is connected to read it: connect one (Settings → Argo CD, or Connect to Argo CD…)";

/// The Argo CD server holding the Application whose `metadata.uid` is `uid` (one opened as a
/// Kubernetes object), from what the servers' feeds hold (a feed is started when none runs),
/// or why none does: no server connected, or none has it (and those that could not be read).
pub async fn api_by_uid(uid: String) -> Result<Arc<api::Endpoint>, String> {
    let read = async move {
        if uid.is_empty() {
            return Ok(Err("it has no uid".to_string()));
        }
        let held = api::held_apps(
            |_| true,
            true,
            API_WAIT,
            |_, a| a.value.pointer("/metadata/uid").and_then(Value::as_str) == Some(uid.as_str()),
        )
        .await;
        Ok(own_in(held))
    };
    super::run(read).await.unwrap_or_else(|e| Err(format!("{e:#}")))
}

/// Which server's feed holds the Application looked for (`held`: each server's part), or
/// why none does.
fn own_in(held: Vec<api::Held>) -> Result<Arc<api::Endpoint>, String> {
    if held.is_empty() {
        return Err(NO_SERVER.into());
    }
    if let Some(h) = held.iter().find(|h| h.apps.as_ref().is_ok_and(|a| !a.is_empty())) {
        return Ok(h.endpoint.clone());
    }
    let read: Vec<String> = held.iter().filter(|h| h.apps.is_ok()).map(|h| h.endpoint.name()).collect();
    let failed: Vec<String> =
        held.iter().filter_map(|h| h.apps.as_ref().err().map(|e| format!("{}: {e}", h.endpoint.name()))).collect();
    let mut why = if read.is_empty() {
        "No Argo CD server could be read".to_string()
    } else {
        format!("Not on the Argo CD servers connected ({})", read.join(", "))
    };
    if !failed.is_empty() {
        why.push_str(&format!("; not read: {}", failed.join("; ")));
    }
    Err(why)
}

/// Said on an Application's page when no Argo CD server is set or logged in to.
const NO_SERVER: &str =
    "No Argo CD server is connected: connect one (Settings → Argo CD) to open this Application there";

/// The longest wait for an Argo CD server's first list in a lookup (one that failed answers
/// at once: see `api::held_apps`), and for an Application read whole.
const API_WAIT: Duration = PROBE_TIMEOUT;

/// Whether the Application `app`, read by Argo CD's API, is the one `by` names: a tracking id
/// names it by name, and by namespace when it gives one; an instance label by name.
pub fn names_app(by: &TrackedBy, app: &Summary) -> bool {
    match by {
        TrackedBy::Id(t) => app.name == t.app && t.app_ns.as_ref().is_none_or(|ns| *ns == app.namespace),
        TrackedBy::Label(l) => app.name == *l,
    }
}

/// Whether an Application `by` names deployed the object: by a tracking id, when it deploys to
/// the object's cluster (`here`) or tracks the object; by a label (Helm sets it too), when it
/// does both.
pub fn fits(by: &TrackedBy, here: Option<bool>, tracks: bool) -> bool {
    match by {
        TrackedBy::Id(_) => here == Some(true) || tracks,
        TrackedBy::Label(_) => here == Some(true) && tracks,
    }
}

/// An Application named for an object, read by Argo CD's API.
pub struct ApiCandidate {
    pub endpoint: Arc<api::Endpoint>,
    /// The Application whole, or what the feed holds of it when it couldn't be read whole.
    pub app: Value,
    /// It deploys to the object's cluster; None when that can't be told (`app_deploys_to`).
    pub here: Option<bool>,
    /// Its `status.resources` lists the object.
    pub tracks: bool,
}

/// What the Argo CD servers say of the object `obj` (of `group`/`kind`, on the context `here`)
/// that `by` names an Application for: the Applications of that name found in the feeds
/// (`api::held_apps`, no list of their own), each read whole (its resources say whether it
/// tracks the object), and why some servers said nothing. A label (Helm sets it too) leaves
/// out at once those that deploy elsewhere. Only the servers `use_server` accepts are read,
/// with `start` (a feed started when none runs), and the Applications `may_read` lets through
/// (their server, the Application): the others are said.
/// None when no server is set, logged in to or turned on.
pub async fn api_candidates(
    here: &str,
    by: &TrackedBy,
    obj: (&str, &str, &Value),
    use_server: impl Fn(&str) -> bool,
    start: bool,
    may_read: impl Fn(&api::Endpoint, &Summary) -> bool,
) -> Option<(Vec<ApiCandidate>, Vec<String>, Vec<String>)> {
    let (group, kind, obj) = obj;
    let ns = obj.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default();
    let name = obj.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default();
    let targets = targets();
    let here = target(&targets, here);
    let at = |_: &api::Endpoint| &*API_HOME;
    let held = api::held_apps(use_server, start, API_WAIT, |e, a| {
        names_app(by, &a.summary)
            && (matches!(by, TrackedBy::Id(_)) || app_deploys_to(&a.value, at(e), &here) != Some(false))
    })
    .await;
    if held.is_empty() {
        return None;
    }
    let (mut notes, mut read, mut wanted) = (Vec::new(), Vec::new(), Vec::new());
    for h in held {
        let server = h.endpoint.name();
        match h.apps {
            Err(e) => notes.push(format!("{server}: {e}")),
            Ok(apps) => {
                for a in apps {
                    if may_read(&h.endpoint, &a.summary) {
                        wanted.push((h.endpoint.clone(), a));
                    } else {
                        let (ns, name) = (&a.summary.namespace, &a.summary.name);
                        notes.push(format!("{server}: your rules don't let you read Application {ns}/{name}"));
                    }
                }
                read.push(server);
            }
        }
    }
    // Each read whole, all at once, each bounded: one slow server holds no other.
    let whole = futures::future::join_all(wanted.iter().map(|(e, a)| {
        let get = api::read_app(e.clone(), a.summary.namespace.clone(), a.summary.name.clone());
        async move { tokio::time::timeout(API_WAIT, get).await.unwrap_or_else(|_| Err("did not answer".into())) }
    }))
    .await;
    let mut found = Vec::new();
    for ((endpoint, held), whole) in wanted.into_iter().zip(whole) {
        let app = match whole {
            Ok(app) => app,
            // A label is only a hint: without the Application's resources, it says nothing.
            Err(_) if matches!(by, TrackedBy::Label(_)) => continue,
            Err(e) => {
                let (ns, name) = (&held.summary.namespace, &held.summary.name);
                notes.push(format!("{}: Application {ns}/{name} could not be read whole: {e}", endpoint.name()));
                held.value.clone()
            }
        };
        let here = app_deploys_to(&app, at(&endpoint), &here);
        let tracks = resource_of(&app, group, kind, ns, name).is_some();
        found.push(ApiCandidate { endpoint, app, here, tracks });
    }
    Some((found, notes, read))
}

/// [`for_object`] on Argo CD's API servers; None when there is none to read.
async fn api_object_lookup(here: &str, by: &TrackedBy, group: &str, kind: &str, obj: &Value) -> Option<Lookup> {
    let (found, errors, read) = api_candidates(here, by, (group, kind, obj), |_| true, true, |_, _| true).await?;
    let ns = obj.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default();
    let name = obj.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default();
    let summary = |c: &ApiCandidate| {
        let mut s = Summary::of(&c.app).with_own(&c.app, group, kind, ns, name);
        if s.namespace.is_empty() {
            s.namespace = DEFAULT_NAMESPACE.into();
        }
        Found { server: c.endpoint.clone(), app: s }
    };
    let mut apps: Vec<Found> = found.iter().filter(|c| fits(by, c.here, c.tracks)).map(summary).collect();
    // The only one of that name, whose destination can't be told: it can only be it.
    if apps.is_empty() && matches!(by, TrackedBy::Id(_)) && found.len() == 1 && found[0].here.is_none() {
        apps = found.iter().map(summary).collect();
    }
    let note_unread = !errors.is_empty();
    let note = match (by, apps.is_empty(), errors.is_empty()) {
        (_, _, false) => Some(format!("Argo CD could not be read: {}", errors.join("; "))),
        (TrackedBy::Id(t), true, true) => {
            Some(format!("Deployed by Argo CD Application {}, which is not on {}", t.app, read.join(", ")))
        }
        _ => None,
    };
    Some(Lookup { apps, note, note_unread, ..Lookup::default() })
}

/// The Applications that deploy into this namespace of the context `here`, from what Argo
/// CD's API feeds hold. Quiet when no Argo CD server is connected.
pub async fn for_namespace(here: String, ns: String) -> Lookup {
    let read = async move { Ok(api_namespace_lookup(&here, &ns).await.unwrap_or_default()) };
    super::run(read).await.unwrap_or_default()
}

/// [`for_namespace`] on Argo CD's API servers, from what their feeds hold: those whose
/// destination is this namespace of this cluster. None when there is no server to read.
async fn api_namespace_lookup(here: &str, ns: &str) -> Option<Lookup> {
    let targets = targets();
    let here = target(&targets, here);
    let held = api::held_apps(
        |_| true,
        true,
        API_WAIT,
        |_, a| targets_namespace(&a.value, ns) && targets_cluster(&a.value, &API_HOME, &here),
    )
    .await;
    if held.is_empty() {
        return None;
    }
    Some(namespace_lookup(held))
}

/// What the servers' feeds (`held`) say of a namespace: their Applications, and those that
/// could not be read. None read: how many there are is unknown; some: the count leaves the
/// others out, and says so.
fn namespace_lookup(held: Vec<api::Held>) -> Lookup {
    let mut lookup = Lookup::default();
    let (mut errors, mut read) = (Vec::new(), 0);
    for h in held {
        match h.apps {
            Ok(apps) => {
                read += 1;
                lookup.apps.extend(apps.iter().map(|a| Found { server: h.endpoint.clone(), app: a.summary.clone() }));
            }
            Err(e) => errors.push(format!("{}: {e}", h.endpoint.name())),
        }
    }
    lookup.apps.sort_by_cached_key(|a| (a.app.name.clone(), a.server.name()));
    if !errors.is_empty() {
        lookup.unknown = read == 0;
        let lead = if read == 0 { "Argo CD could not be read" } else { "Not counted, Argo CD could not be read" };
        lookup.note = Some(format!("{lead}: {}", errors.join("; ")));
    }
    lookup
}

// MARK: Every Application

/// Where an Application deploys: the kubeconfig's contexts that reach its destination
/// cluster (several may), and the server URL or cluster name it gives, shown when none does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Destination {
    pub contexts: Vec<String>,
    pub cluster: String,
    pub namespace: String,
}

impl Destination {
    /// The context to show it on and open it with: a connected one first.
    pub fn context(&self, connected: &[String]) -> Option<&str> {
        self.contexts.iter().find(|c| connected.contains(c)).or(self.contexts.first()).map(String::as_str)
    }
}

/// "cluster / namespace" as shown, the cluster alone for none.
pub fn destination_text(cluster: &str, namespace: &str) -> String {
    if namespace.is_empty() { cluster.to_string() } else { format!("{cluster} / {namespace}") }
}

/// Where an Application read on the hub `hub` deploys, among the contexts `targets`.
pub fn destination(app: &Value, hub: &ContextTarget, targets: &[ContextTarget]) -> Destination {
    let dest = |p: &str| app.pointer(p).and_then(Value::as_str).filter(|s| !s.is_empty());
    let cluster = dest("/spec/destination/server").or_else(|| dest("/spec/destination/name")).unwrap_or_default();
    Destination {
        contexts: targets.iter().filter(|t| targets_cluster(app, hub, t)).map(|t| t.name.clone()).collect(),
        cluster: cluster.to_string(),
        namespace: dest("/spec/destination/namespace").unwrap_or_default().to_string(),
    }
}

/// Where the Application `app`, read on the hub `hub`, deploys, and the context shown for it
/// (a connected one first). The kubeconfig is read off the UI thread.
pub async fn destination_of(app: Value, hub: String, connected: Vec<String>) -> (Destination, Option<String>) {
    let read = super::run(async move {
        let targets = targets();
        let dest = destination(&app, &target(&targets, &hub), &targets);
        let on = dest.context(&connected).map(String::from);
        Ok((dest, on))
    });
    read.await.unwrap_or_default()
}

/// The Application that made each one (an app of apps), as an index in `apps` (one hub's):
/// the one its tracking id names, else the one listing it among its resources, else the one
/// its instance label names (label tracking), when only one has that name.
pub fn parents<V: Borrow<Value>>(apps: &[V]) -> Vec<Option<usize>> {
    fn meta<'a>(app: &'a Value, field: &str) -> &'a str {
        app.get("metadata").and_then(|m| m.get(field)).and_then(Value::as_str).unwrap_or_default()
    }
    let mut by_key: HashMap<(&str, &str), usize> = HashMap::new();
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    let apps: Vec<&Value> = apps.iter().map(|a| <V as Borrow<Value>>::borrow(a)).collect();
    for (i, a) in apps.iter().enumerate() {
        by_key.insert((meta(a, "namespace"), meta(a, "name")), i);
        by_name.entry(meta(a, "name")).or_default().push(i);
    }
    let only = |name: &str| match by_name.get(name).map(Vec::as_slice) {
        Some([one]) => Some(*one),
        _ => None,
    };
    // Who lists whom: the first parent found wins.
    let mut listed: HashMap<(&str, &str), usize> = HashMap::new();
    for (i, a) in apps.iter().enumerate() {
        for r in resources(a).filter(|r| r["group"] == "argoproj.io" && r["kind"] == "Application") {
            let (ns, name) =
                (r["namespace"].as_str().unwrap_or(DEFAULT_NAMESPACE), r["name"].as_str().unwrap_or_default());
            listed.entry((ns, name)).or_insert(i);
        }
    }
    apps.iter()
        .enumerate()
        .map(|(i, a)| {
            let key = (meta(a, "namespace"), meta(a, "name"));
            let tracked = tracking_of("argoproj.io", "Application", a).and_then(|t| {
                let ns = t.app_ns.as_deref().unwrap_or(DEFAULT_NAMESPACE);
                // Without `<ns>_`, the parent is in Argo CD's own namespace, which may not be `argocd`.
                by_key
                    .get(&(ns, t.app.as_str()))
                    .copied()
                    .or_else(|| t.app_ns.is_none().then(|| only(&t.app)).flatten())
            });
            let label = || {
                let name = a.pointer("/metadata/labels").and_then(|l| l.get(INSTANCE_LABEL)).and_then(Value::as_str)?;
                only(name)
            };
            tracked.or_else(|| listed.get(&key).copied()).or_else(label).filter(|p| *p != i)
        })
        .collect()
}

/// Each project's sync windows, by project name.
pub type ProjectWindows = Arc<std::collections::BTreeMap<String, Vec<windows::SyncWindow>>>;

/// One Application of the Applications page.
#[derive(Clone)]
pub struct FleetApp {
    /// The Argo CD API server it is read from.
    pub server: Arc<api::Endpoint>,
    /// Its Argo CD (`api::Endpoint::instance`).
    pub instance: String,
    pub app: Summary,
    pub dest: Destination,
    /// Its destination cluster as Argo CD names it (see `cluster_name`).
    pub cluster: String,
    /// The context its destination is shown on (a connected one first), if any reaches it.
    pub on: Option<String>,
    /// The Application that made it (an app of apps), as an index in the fleet.
    pub parent: Option<usize>,
    /// Its resourceVersion when read (empty when unknown): what its diff is kept under.
    pub version: String,
}

/// An Application's resourceVersion, empty when it has none.
fn resource_version(app: &Value) -> String {
    app.pointer("/metadata/resourceVersion").and_then(Value::as_str).unwrap_or_default().to_string()
}

/// Every Application of every hub, and why some hubs could not be read.
#[derive(Clone, Default)]
pub struct Fleet {
    pub apps: Vec<FleetApp>,
    /// "server: why", for each Argo CD server that could not be read.
    pub errors: Vec<String>,
    /// The Argo CD API servers read (see `api`), by name.
    pub servers: Vec<String>,
    /// Those of `servers` whose first list is still being read (none failed yet), by name.
    pub reading: Vec<String>,
    /// Those of `servers` that could not be read (and hold no list from before), by name:
    /// their Applications are unknown, not none.
    pub unread: Vec<String>,
    /// Each Argo CD's projects' sync windows (by `instance`), or why they couldn't be read.
    pub projects: HashMap<String, Result<ProjectWindows, String>>,
    /// The API servers whose login is missing, expired or refused: their address, and whether
    /// their certificate is not checked (to Reconnect as the argocd CLI's config says).
    pub relogin: Vec<(String, bool)>,
}

impl Fleet {
    /// The servers whose list is in: what the counts and "not found" cover.
    pub fn read(&self) -> Vec<String> {
        let out = |s: &String| self.reading.contains(s) || self.unread.contains(s);
        self.servers.iter().filter(|s| !out(s)).cloned().collect()
    }

    /// No server's list is in, and some could not be read: its Applications are unknown.
    pub fn none_read(&self) -> bool {
        !self.unread.is_empty() && self.read().is_empty()
    }

    /// When the next sync of `a` can start, from `now`, under its project's sync windows.
    pub fn next_sync(&self, a: &FleetApp, plan: &mut windows::Planner) -> windows::NextSync {
        // Auto-sync is held by every window; a manual sync goes through those that allow it.
        self.sync_window(a, plan, !a.app.automated)
    }

    /// When a sync of `a` asked by hand (`manual`), or one auto-sync starts, can start next.
    pub fn sync_window(&self, a: &FleetApp, plan: &mut windows::Planner, manual: bool) -> windows::NextSync {
        let all = match self.projects.get(&a.instance) {
            None => return windows::NextSync::Unknown("its project's sync windows are not read yet".into()),
            Some(Err(e)) => return windows::NextSync::Unknown(e.clone()),
            Some(Ok(projects)) => match projects.get(&a.app.project) {
                Some(w) => w,
                None => return windows::NextSync::Unknown(format!("project {} can't be read", a.app.project)),
            },
        };
        let s = &a.app;
        let applying: Vec<&windows::SyncWindow> =
            all.iter().filter(|w| w.applies(&s.name, &s.dest_namespace, &s.dest_server, &s.dest_name)).collect();
        plan.next_sync(&applying, manual)
    }
}

/// The contexts among `targets` that reach the destination of `app`, read by Argo CD's API.
/// Its `in-cluster` is the cluster Argo CD runs on, which no context is known to be.
pub fn api_contexts(app: &Summary, targets: &[ContextTarget]) -> Vec<String> {
    let (server, name) = (Some(app.dest_server.as_str()), Some(app.dest_name.as_str()));
    targets.iter().filter(|t| deploys_to(server, name, &API_HOME, t)).map(|t| t.name.clone()).collect()
}

/// The cluster an Argo CD read by its API runs on, as its `in-cluster` destination: not known
/// to be any context of the kubeconfig, so it matches none.
static API_HOME: LazyLock<ContextTarget> = LazyLock::new(ContextTarget::default);

/// What a page has read of one Argo CD API server.
pub struct ApiRead {
    pub endpoint: Arc<api::Endpoint>,
    pub state: api::FeedState,
}

/// A destination cluster as Argo CD names it: the name the Application gives, else the one
/// Argo CD's cluster list gives its server URL (`names`, by `server_key`), else `in-cluster`
/// for Argo CD's own, else the URL's host.
pub fn cluster_name(name: &str, server: &str, names: &std::collections::BTreeMap<String, String>) -> String {
    if !name.is_empty() {
        return name.to_string();
    }
    if server.is_empty() {
        return String::new();
    }
    let key = server_key(server);
    if let Some(n) = names.get(&key) {
        return n.clone();
    }
    if key == server_key(IN_CLUSTER) {
        return "in-cluster".into();
    }
    let host = key.split_once("://").map_or(key.as_str(), |(_, h)| h);
    host.split(['/', ':']).next().unwrap_or(host).to_string()
}

/// The contexts that reach a destination given by its `server` URL when no kubeconfig
/// `server:` is that URL (a context reaching the cluster by its IP address or another host
/// name): those named as Argo CD's cluster list names that server, or whose kubeconfig
/// cluster is, as `destination_is` matches a destination given by name.
fn named_contexts(
    server: &str,
    names: &std::collections::BTreeMap<String, String>,
    targets: &[ContextTarget],
) -> Vec<String> {
    if server.is_empty() {
        return Vec::new();
    }
    let Some(name) = names.get(&server_key(server)) else { return Vec::new() };
    targets.iter().filter(|t| t.name == *name || t.cluster == *name).map(|t| t.name.clone()).collect()
}

/// Every Application read by Argo CD's API, each server's under the app of apps that made it.
/// `targets` are the kubeconfig's contexts, `connected` those connected.
pub fn from_api(reads: Vec<ApiRead>, targets: &[ContextTarget], connected: &[String]) -> Fleet {
    let mut fleet = Fleet::default();
    for read in reads {
        let endpoint = read.endpoint;
        let instance = endpoint.instance();
        let name = endpoint.name();
        fleet.servers.push(name.clone());
        if !read.state.listed && read.state.error.is_none() {
            fleet.reading.push(name.clone());
        }
        if let Some(e) = read.state.error.as_ref().filter(|_| !read.state.live) {
            fleet.errors.push(format!("{name}: {e}"));
            if !read.state.listed {
                fleet.unread.push(name);
            }
        }
        if read.state.refused && !fleet.relogin.iter().any(|(b, _)| *b == endpoint.server.base) {
            fleet.relogin.push((endpoint.server.base.clone(), endpoint.insecure));
        }
        if let Some(p) = read.state.projects {
            fleet.projects.insert(instance.clone(), p);
        }
        let hub = &*API_HOME;
        let mut apps: Vec<&Arc<api::ApiApp>> = read.state.apps.values().collect();
        apps.sort_by(|a, b| (&a.summary.namespace, &a.summary.name).cmp(&(&b.summary.namespace, &b.summary.name)));
        let values: Vec<&Value> = apps.iter().map(|a| &a.value).collect();
        let base = fleet.apps.len();
        for (a, parent) in apps.iter().zip(parents(values.as_slice())) {
            let mut dest = destination(&a.value, hub, targets);
            let cluster = cluster_name(&a.summary.dest_name, &a.summary.dest_server, &read.state.clusters);
            if dest.contexts.is_empty() {
                dest.contexts = named_contexts(&a.summary.dest_server, &read.state.clusters, targets);
            }
            let on = dest.context(connected).map(String::from);
            fleet.apps.push(FleetApp {
                server: endpoint.clone(),
                instance: instance.clone(),
                app: a.summary.clone(),
                dest,
                cluster,
                on,
                parent: parent.map(|p| base + p),
                version: resource_version(&a.value),
            });
        }
    }
    fleet
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An Application opened as a Kubernetes object opens on the server whose feed holds one
    /// of its uid; none connected, none holding it, and those not read are each said.
    #[test]
    fn an_application_page_finds_its_argo_cd_by_uid() {
        let held = |url: &str, apps: Result<usize, &str>| api::Held {
            endpoint: endpoint(url),
            apps: apps.map(|n| (0..n).map(|_| api_app("argocd", "web", None)).collect()).map_err(String::from),
        };
        assert_eq!(own_in(Vec::new()).unwrap_err(), NO_SERVER);
        let found = own_in(vec![held("https://a.example", Ok(0)), held("https://b.example", Ok(1))]).unwrap();
        assert_eq!(found.name(), "b.example");
        let why = own_in(vec![held("https://a.example", Ok(0)), held("https://b.example", Err("did not answer"))])
            .unwrap_err();
        assert_eq!(why, "Not on the Argo CD servers connected (a.example); not read: b.example: did not answer");
        let why = own_in(vec![held("https://b.example", Err("expired"))]).unwrap_err();
        assert!(why.starts_with("No Argo CD server could be read; not read: b.example: expired"), "{why}");
    }

    /// A namespace's count says nothing when no server could be read, and leaves out, saying
    /// so, those not read when another was.
    #[test]
    fn a_namespace_s_applications_are_unknown_when_no_server_could_be_read() {
        let held = |url: &str, apps: Result<usize, &str>| api::Held {
            endpoint: endpoint(url),
            apps: apps.map(|n| (0..n).map(|_| api_app("argocd", "web", None)).collect()).map_err(String::from),
        };
        let none = namespace_lookup(vec![held("https://b.example", Err("HTTP 403"))]);
        assert!(none.unknown && none.apps.is_empty());
        assert_eq!(none.note.as_deref(), Some("Argo CD could not be read: b.example: HTTP 403"));
        let some = namespace_lookup(vec![held("https://a.example", Ok(1)), held("https://b.example", Err("HTTP 403"))]);
        assert!(!some.unknown);
        assert_eq!(some.apps.len(), 1);
        assert_eq!(some.note.as_deref(), Some("Not counted, Argo CD could not be read: b.example: HTTP 403"));
        let all = namespace_lookup(vec![held("https://a.example", Ok(0))]);
        assert!(!all.unknown && all.note.is_none());
    }

    fn endpoint(url: &str) -> Arc<api::Endpoint> {
        Arc::new(api::Endpoint {
            server: server::Server::parse(url).unwrap(),
            cli_context: None,
            in_settings: true,
            insecure: false,
            login: Err("no login in tests".into()),
            on: true,
            settings_token: false,
            renewable: false,
            cli: None,
            cli_renews: false,
        })
    }

    fn api_app(ns: &str, name: &str, tracked_by: Option<&str>) -> Arc<api::ApiApp> {
        let mut v = json!({"metadata": {"name": name, "namespace": ns}, "spec": {"project": "shop"}});
        if let Some(parent) = tracked_by {
            v["metadata"]["annotations"] =
                json!({TRACKING_ID: format!("{parent}:argoproj.io/Application:{ns}/{name}")});
        }
        Arc::new(api::ApiApp { summary: Summary::of(&v), value: v })
    }

    fn fleet_app(source: &Arc<api::Endpoint>, app: &api::ApiApp) -> FleetApp {
        FleetApp {
            server: source.clone(),
            instance: source.instance(),
            app: app.summary.clone(),
            dest: Destination::default(),
            cluster: String::new(),
            on: None,
            parent: None,
            version: String::new(),
        }
    }

    #[test]
    fn each_server_s_applications_are_under_their_own_parents() {
        let feed = |apps: Vec<Arc<api::ApiApp>>, projects| {
            let mut state = api::FeedState { projects, ..Default::default() };
            for a in apps {
                state.apps.insert((a.summary.namespace.clone(), a.summary.name.clone()), a);
            }
            state
        };
        let (one, two) = (endpoint("https://argocd.example.com"), endpoint("https://argocd.example.org"));
        let reads = vec![
            ApiRead {
                endpoint: one.clone(),
                state: feed(
                    vec![
                        api_app("argocd", "root", None),
                        api_app("argocd", "child", Some("root")),
                        api_app("argocd", "leaf", Some("child")),
                    ],
                    Some(Ok(Arc::new(std::collections::BTreeMap::new()))),
                ),
            },
            // The other server has a root of the same name: its child is under its own.
            ApiRead {
                endpoint: two.clone(),
                state: feed(vec![api_app("argocd", "root", None), api_app("argocd", "zchild", Some("root"))], None),
            },
        ];
        let fleet = from_api(reads, &[], &[]);
        let names: Vec<(&str, Option<usize>)> = fleet.apps.iter().map(|a| (a.app.name.as_str(), a.parent)).collect();
        assert_eq!(names, [("child", Some(2)), ("leaf", Some(0)), ("root", None), ("root", None), ("zchild", Some(3))]);
        assert_eq!(fleet.servers.len(), 2);
        assert!(fleet.projects[&one.instance()].is_ok());
        assert!(!fleet.projects.contains_key(&two.instance()), "not read yet");
    }

    /// #1072: a destination is named by Argo CD's cluster list, and reaches the context named
    /// so even when the context's `server:` is another address of the cluster.
    #[test]
    fn a_destination_is_named_as_argo_cd_names_its_cluster() {
        let source = endpoint("https://argocd.example.com");
        let app = |name: &str, dest: Value| {
            let v = json!({"metadata": {"name": name, "namespace": "argocd"},
                "spec": {"project": "shop", "destination": dest}});
            Arc::new(api::ApiApp { summary: Summary::of(&v), value: v })
        };
        let clusters = std::iter::once(("https://apiserver-prod.example".to_string(), "prod".to_string()));
        let mut state = api::FeedState { clusters: Arc::new(clusters.collect()), ..Default::default() };
        for a in [
            app("a", json!({"server": "https://apiserver-prod.example", "namespace": "web"})),
            app("b", json!({"server": "https://kubernetes.default.svc", "namespace": "argocd"})),
            app("c", json!({"server": "https://other.example:6443", "namespace": "web"})),
        ] {
            state.apps.insert((a.summary.namespace.clone(), a.summary.name.clone()), a);
        }
        let targets = [
            ContextTarget {
                name: "prod".into(),
                cluster: "prod".into(),
                server: Some("https://10.0.0.1".into()),
                exec: None,
            },
            ContextTarget {
                name: "else".into(),
                cluster: "else".into(),
                server: Some("https://10.0.0.2".into()),
                exec: None,
            },
        ];
        let fleet = from_api(vec![ApiRead { endpoint: source, state }], &targets, &[]);
        let rows: Vec<(&str, &str, Option<&str>)> =
            fleet.apps.iter().map(|a| (a.app.name.as_str(), a.cluster.as_str(), a.on.as_deref())).collect();
        assert_eq!(rows, [("a", "prod", Some("prod")), ("b", "in-cluster", None), ("c", "other.example", None)]);
    }

    /// #1076: while a server's first list is being read, the page says so, never `0 Applications`.
    #[test]
    fn a_server_whose_first_list_is_not_in_yet_is_said_to_be_read() {
        let (one, two) = (endpoint("https://argocd.example.com"), endpoint("https://argocd.example.org"));
        let pending = ApiRead { endpoint: one.clone(), state: api::FeedState::default() };
        let failed = api::FeedState { error: Some("HTTP 401".into()), ..Default::default() };
        let fleet = from_api(vec![pending, ApiRead { endpoint: two.clone(), state: failed }], &[], &[]);
        // The one that failed says why instead.
        assert_eq!(fleet.reading, [one.name()]);

        // One listed, the other still read: both said.
        let mut listed = api::FeedState { listed: true, ..Default::default() };
        let a = api_app("argocd", "web", None);
        listed.apps.insert((a.summary.namespace.clone(), a.summary.name.clone()), a);
        let pending = ApiRead { endpoint: one.clone(), state: api::FeedState::default() };
        let fleet = from_api(vec![ApiRead { endpoint: two, state: listed }, pending], &[], &[]);
        assert_eq!((fleet.apps.len(), fleet.reading), (1, vec![one.name()]));
    }

    #[test]
    fn the_next_sync_of_an_application_is_manual_when_auto_sync_is_off() {
        let server = endpoint("https://argocd.example.com");
        let mut fleet = Fleet::default();
        let nightly = windows::SyncWindow::of(&json!({"kind": "allow", "schedule": "0 22 * * *", "duration": "1h",
            "applications": ["*"], "manualSync": true}));
        let projects: std::collections::BTreeMap<String, Vec<windows::SyncWindow>> =
            std::iter::once(("shop".to_string(), vec![nightly])).collect();
        fleet.projects.insert(server.instance(), Ok(Arc::new(projects)));
        let mut by_hand = fleet_app(&server, &api_app("argocd", "web", None));
        let mut auto = by_hand.clone();
        auto.app.automated = true;
        by_hand.app.automated = false;
        let now: jiff::Timestamp = "2026-11-24T10:00:00Z".parse().unwrap();
        let mut plan = windows::Planner::new(now);
        // A manual sync goes through the window's manualSync; auto-sync waits for it to open.
        assert_eq!(fleet.next_sync(&by_hand, &mut plan), windows::NextSync::Now);
        assert_eq!(fleet.next_sync(&auto, &mut plan), windows::NextSync::At("2026-11-24T22:00:00Z".parse().unwrap()));
        // Another project, unread: unknown.
        auto.app.project = "other".into();
        assert!(matches!(fleet.next_sync(&auto, &mut plan), windows::NextSync::Unknown(_)));
    }

    fn ctx(name: &str, server: &str, exec: Option<&str>) -> ContextTarget {
        ContextTarget {
            name: name.into(),
            cluster: format!("{name}-cluster"),
            server: Some(server.into()),
            exec: exec.map(String::from),
        }
    }

    #[test]
    fn tracking_ids_read() {
        let t = parse_tracking("shop-argo_shop-prod:apps/Deployment:shop-prod/web").unwrap();
        assert_eq!(t.app_ns.as_deref(), Some("shop-argo"));
        assert_eq!((t.app.as_str(), t.group.as_str(), t.kind.as_str()), ("shop-prod", "apps", "Deployment"));
        assert_eq!((t.namespace.as_str(), t.name.as_str()), ("shop-prod", "web"));
        // In Argo CD's own namespace, a core kind, a cluster-scoped object.
        let t = parse_tracking("guestbook:/ConfigMap:default/cm").unwrap();
        assert_eq!((t.app_ns, t.app.as_str(), t.group.as_str()), (None, "guestbook", ""));
        let t = parse_tracking("guestbook:/Namespace:/team-a").unwrap();
        assert_eq!((t.namespace.as_str(), t.name.as_str()), ("", "team-a"));
        for bad in [
            "",
            "guestbook",
            "a:b:c",
            ":apps/Deployment:ns/web",
            "a:apps/Deployment:ns/",
            "_a:/X:ns/n",
            "a:/X:n/n:extra",
        ] {
            assert_eq!(parse_tracking(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_copied_tracking_id_is_not_this_objects() {
        let obj = |name: &str| json!({"metadata": {"name": name, "namespace": "prod", "annotations": {TRACKING_ID: "app:apps/Deployment:prod/web"}}});
        assert!(tracking_of("apps", "Deployment", &obj("web")).is_some());
        assert!(tracking_of("apps", "Deployment", &obj("web-copy")).is_none());
        assert!(tracking_of("apps", "StatefulSet", &obj("web")).is_none());
        assert!(tracking_of("apps", "Deployment", &json!({"metadata": {"name": "web"}})).is_none());
    }

    #[test]
    fn an_object_names_its_application_by_tracking_id_else_by_label() {
        let obj = |annotation: Option<&str>, label: Option<&str>| {
            json!({"metadata": {"name": "web", "namespace": "prod",
                "annotations": annotation.map(|a| json!({TRACKING_ID: a})),
                "labels": label.map(|l| json!({INSTANCE_LABEL: l}))}})
        };
        let id = tracked_by("apps", "Deployment", &obj(Some("shop:apps/Deployment:prod/web"), Some("other")));
        assert!(matches!(id, Some(TrackedBy::Id(t)) if t.app == "shop"), "the tracking id wins over the label");
        // A tracking id copied from another object is not this one's: the label is the hint left.
        let copied = tracked_by("apps", "Deployment", &obj(Some("shop:apps/Deployment:prod/api"), Some("shop")));
        assert_eq!(copied, Some(TrackedBy::Label("shop".into())));
        assert_eq!(tracked_by("apps", "Deployment", &obj(None, Some("shop"))), Some(TrackedBy::Label("shop".into())));
        assert_eq!(tracked_by("apps", "Deployment", &obj(None, Some(""))), None);
        assert_eq!(tracked_by("apps", "Deployment", &obj(None, None)), None);
        // A pod carries its workload's label; its workload is what Argo CD deployed.
        assert_eq!(tracked_by("", "Pod", &obj(None, Some("shop"))), None);
    }

    /// In Argo CD's API feeds, a tracking id names an Application by name, and by namespace
    /// when it gives one (`<ns>_<app>`); a label by name, in any namespace.
    #[test]
    fn a_tracking_id_or_a_label_names_an_application_read_by_the_api() {
        let app = |ns: &str, name: &str| Summary::of(&json!({"metadata": {"name": name, "namespace": ns}}));
        let id = |text: &str| TrackedBy::Id(parse_tracking(text).unwrap());
        let plain = id("shop:apps/Deployment:prod/web");
        assert!(names_app(&plain, &app("argocd", "shop")));
        assert!(names_app(&plain, &app("gitops", "shop")), "Argo CD's own namespace may not be argocd");
        assert!(!names_app(&plain, &app("argocd", "shop-2")));
        let in_ns = id("team-a_shop:apps/Deployment:prod/web");
        assert!(names_app(&in_ns, &app("team-a", "shop")));
        assert!(!names_app(&in_ns, &app("team-b", "shop")));
        assert!(!names_app(&in_ns, &app("argocd", "shop")));
        let label = TrackedBy::Label("shop".into());
        assert!(names_app(&label, &app("team-b", "shop")));
        assert!(!names_app(&label, &app("argocd", "web")));
    }

    /// A tracking id's Application deployed the object when it deploys here or tracks it; a
    /// label's (Helm sets the same label) only when it does both.
    #[test]
    fn a_label_needs_both_the_destination_and_the_resource() {
        let id = TrackedBy::Id(parse_tracking("shop:apps/Deployment:prod/web").unwrap());
        let label = TrackedBy::Label("shop".into());
        for here in [Some(true), Some(false), None] {
            for tracks in [true, false] {
                assert_eq!(fits(&id, here, tracks), here == Some(true) || tracks, "{here:?} {tracks}");
                assert_eq!(fits(&label, here, tracks), here == Some(true) && tracks, "{here:?} {tracks}");
            }
        }
    }

    #[test]
    fn servers_compare_without_default_port_case_or_slash() {
        assert_eq!(server_key("https://API.example.com:443/"), "https://api.example.com");
        assert_eq!(server_key("https://api.example.com:6443"), "https://api.example.com:6443");
        assert_eq!(server_key("http://h:80"), "http://h");
        assert_eq!(
            server_key("https://rancher.example/k8s/clusters/c-AbC"),
            "https://rancher.example/k8s/clusters/c-AbC"
        );
    }

    #[test]
    fn applications_match_their_destination_cluster() {
        let hub = ctx("hub", "https://hub.example", None);
        let prod = ctx("prod", "https://prod.example:443", None);
        let to = |dest: Value| json!({"spec": {"destination": dest}});
        assert!(targets_cluster(&to(json!({"server": "https://PROD.example"})), &hub, &prod));
        assert!(!targets_cluster(&to(json!({"server": "https://other.example"})), &hub, &prod));
        // The hub's own cluster.
        assert!(targets_cluster(&to(json!({"server": IN_CLUSTER})), &hub, &hub));
        assert!(!targets_cluster(&to(json!({"server": IN_CLUSTER})), &hub, &prod));
        assert!(targets_cluster(&to(json!({"name": "in-cluster"})), &hub, &hub));
        // By name: the context's or its cluster's.
        assert!(targets_cluster(&to(json!({"name": "prod"})), &hub, &prod));
        assert!(targets_cluster(&to(json!({"name": "prod-cluster"})), &hub, &prod));
        assert!(!targets_cluster(&to(json!({"name": "staging"})), &hub, &prod));
        assert!(!targets_cluster(&json!({}), &hub, &prod));
        // A name of Argo CD's own (a cluster Secret's) can't be told from another cluster's.
        assert_eq!(app_deploys_to(&to(json!({"name": "staging"})), &hub, &prod), None);
        assert_eq!(app_deploys_to(&to(json!({"name": "prod"})), &hub, &prod), Some(true));
        assert_eq!(app_deploys_to(&to(json!({"name": "in-cluster"})), &hub, &prod), Some(false));
        assert_eq!(app_deploys_to(&to(json!({"server": "https://other.example"})), &hub, &prod), Some(false));
        assert_eq!(app_deploys_to(&json!({}), &hub, &prod), Some(false));
    }

    #[test]
    fn applications_match_a_namespace_by_destination_or_what_they_track() {
        let app = json!({
            "spec": {"destination": {"namespace": "shop"}},
            "status": {"resources": [{"kind": "RoleBinding", "group": "rbac.authorization.k8s.io", "namespace": "monitoring", "name": "read"}]}
        });
        assert!(targets_namespace(&app, "shop"));
        assert!(targets_namespace(&app, "monitoring"));
        assert!(!targets_namespace(&app, "other"));
        assert!(resource_of(&app, "rbac.authorization.k8s.io", "RoleBinding", "monitoring", "read").is_some());
        assert!(resource_of(&app, "", "RoleBinding", "monitoring", "read").is_none());
    }

    #[test]
    fn a_summary_reads_status() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let app = json!({
            "metadata": {"name": "web", "namespace": "argocd", "deletionTimestamp": "2026-01-01T00:00:00Z"},
            "spec": {"syncPolicy": {"automated": {"prune": true}}},
            "status": {
                "sync": {"status": "OutOfSync", "revision": sha},
                "health": {"status": "Degraded", "message": "web: 0/2 ready"},
                "operationState": {"phase": "Failed", "message": "one or more objects failed", "finishedAt": "2026-01-01T00:00:00Z"},
                "history": [
                    {"revision": "1.0.0", "deployedAt": "2025-01-01T00:00:00Z", "initiatedBy": {"automated": true}},
                    {"revisions": [sha, "2.0.0"], "deployedAt": "2025-02-01T00:00:00Z", "initiatedBy": {"username": "ana"}}
                ],
                "conditions": [{"type": "SyncError", "message": "boom"}],
                "summary": {"externalURLs": ["https://web.example", "\\\\host@SSL\\x\\evil.exe", "search-ms:query=x", "file:///etc/passwd"]},
                "resources": [{"group": "apps", "kind": "Deployment", "namespace": "shop", "name": "web", "status": "Synced", "health": {"status": "Healthy"}}]
            }
        });
        let s = Summary::of(&app).with_own(&app, "apps", "Deployment", "shop", "web");
        assert_eq!((s.sync.as_str(), s.health.as_str(), s.automated), ("OutOfSync", "Degraded", true));
        assert_eq!(s.revision.as_deref(), Some("0123456"));
        assert_eq!(s.history[0].revision, "0123456, 2.0.0");
        assert_eq!(s.history[0].by.as_deref(), Some("ana"));
        assert_eq!(s.history[1].by.as_deref(), Some("auto-sync"));
        assert_eq!(s.operation.as_ref().map(|o| o.0.as_str()), Some("Failed"));
        assert_eq!(s.conditions, [("SyncError".to_string(), "boom".to_string())]);
        // Only the web link: the share, the search and the file are never offered.
        assert_eq!(s.urls, ["https://web.example"]);
        assert!(s.deleting.is_some());
        assert_eq!(s.own, Some(("Synced".into(), "Healthy".into(), None)));
        // Auto-sync turned off with `enabled: false` (newer Argo CD).
        let off = json!({"spec": {"syncPolicy": {"automated": {"enabled": false}}}});
        assert!(!Summary::of(&off).automated);
    }

    #[test]
    fn a_destination_is_the_contexts_that_reach_its_cluster() {
        let hub = ctx("mgmt", "https://mgmt.example", None);
        let targets = vec![
            hub.clone(),
            ctx("prod", "https://prod.example:443", None),
            ctx("prod-admin", "https://PROD.example/", None),
            ctx("staging", "https://staging.example", None),
        ];
        let to = |dest: Value| json!({"spec": {"destination": dest}});
        let prod = destination(&to(json!({"server": "https://prod.example", "namespace": "shop"})), &hub, &targets);
        assert_eq!(prod.contexts, ["prod", "prod-admin"]);
        assert_eq!((prod.cluster.as_str(), prod.namespace.as_str()), ("https://prod.example", "shop"));
        // A connected context first, else the kubeconfig's first.
        assert_eq!(prod.context(&["prod-admin".into()]), Some("prod-admin"));
        assert_eq!(prod.context(&[]), Some("prod"));
        // Argo CD's own cluster, by URL or by name.
        let own = destination(&to(json!({"server": IN_CLUSTER, "namespace": "argocd"})), &hub, &targets);
        assert_eq!(own.contexts, ["mgmt"]);
        assert_eq!(destination(&to(json!({"name": "in-cluster"})), &hub, &targets).contexts, ["mgmt"]);
        assert_eq!(destination(&to(json!({"name": "staging-cluster"})), &hub, &targets).contexts, ["staging"]);
        // A cluster no context reaches keeps what the Application names.
        let gone = destination(&to(json!({"server": "https://gone.example", "namespace": "x"})), &hub, &targets);
        assert!(gone.contexts.is_empty() && gone.context(&["prod".into()]).is_none());
        assert_eq!(gone.cluster, "https://gone.example");
        assert_eq!(destination_text(&gone.cluster, &gone.namespace), "https://gone.example / x");
        assert_eq!(destination_text("prod", ""), "prod");
    }

    #[test]
    fn applications_are_children_of_the_app_of_apps_that_made_them() {
        let app = |ns: &str, name: &str, meta: Value, children: &[(&str, &str)]| {
            let resources: Vec<Value> = children
                .iter()
                .map(|(ns, name)| json!({"group": "argoproj.io", "kind": "Application", "namespace": ns, "name": name}))
                .collect();
            let mut a = json!({"metadata": {"namespace": ns, "name": name}, "status": {"resources": resources}});
            if let (Some(m), Some(extra)) = (a["metadata"].as_object_mut(), meta.as_object()) {
                m.extend(extra.clone());
            }
            a
        };
        let tracked = |id: &str| json!({"annotations": {TRACKING_ID: id}});
        let apps = vec![
            // 0: the app of apps, in Argo CD's namespace; it lists 2 among its resources.
            app("argocd", "root", json!({}), &[("shop-argo", "shop-workload")]),
            // 1: made by root, says its tracking id (no `<ns>_`: Argo CD's own namespace).
            app("shop-argo", "shop-infra", tracked("root:argoproj.io/Application:shop-argo/shop-infra"), &[]),
            // 2: no tracking id, listed by root.
            app("shop-argo", "shop-workload", json!({}), &[]),
            // 3: label tracking.
            app("data-argo", "data", json!({"labels": {INSTANCE_LABEL: "root"}}), &[]),
            // 4: a label naming itself, and 5: a tracking id copied from another object.
            app("data-argo", "self", json!({"labels": {INSTANCE_LABEL: "self"}}), &[]),
            app("data-argo", "copy", tracked("root:argoproj.io/Application:shop-argo/shop-infra"), &[]),
            // 6: tracked by an app of apps in another namespace.
            app("team", "team-app", tracked("team_team-root:argoproj.io/Application:team/team-app"), &[]),
            app("team", "team-root", json!({}), &[]),
        ];
        assert_eq!(parents(apps.as_slice()), [None, Some(0), Some(0), Some(0), None, None, Some(7), None]);
        // The parent's namespace isn't `argocd`: found by its name when only one has it.
        let mut moved = apps;
        moved[0]["metadata"]["namespace"] = json!("gitops");
        assert_eq!(parents(moved.as_slice())[1], Some(0));
    }

    #[test]
    fn red_is_for_failures_not_for_waiting_for_a_manual_sync() {
        let app = |sync: &str, health: &str, automated: bool, phase: Option<&str>| Summary {
            sync: sync.into(),
            health: health.into(),
            automated,
            operation: phase.map(|p| (p.to_string(), None, None)),
            ..Default::default()
        };
        // A `*-workload` Application synced by hand: out of sync (and what it adds Missing) is not red.
        assert_eq!(attention(&app("OutOfSync", "Healthy", false, Some("Succeeded"))), Tone::Done);
        assert_eq!(attention(&app("OutOfSync", "Missing", false, None)), Tone::Done);
        assert_eq!(listed_health_tone("Missing", false), Tone::Done);
        // With auto-sync on, out of sync is on its way; Missing is wrong.
        assert_eq!(attention(&app("OutOfSync", "Healthy", true, Some("Succeeded"))), Tone::Busy);
        assert_eq!(attention(&app("OutOfSync", "Missing", true, None)), Tone::Bad);
        // A failed sync or Degraded health is red, auto-sync or not.
        assert_eq!(attention(&app("OutOfSync", "Healthy", false, Some("Failed"))), Tone::Bad);
        assert_eq!(attention(&app("Synced", "Degraded", false, Some("Succeeded"))), Tone::Bad);
        assert_eq!(attention(&app("Synced", "Healthy", true, Some("Error"))), Tone::Bad);
        // An error Argo CD reports (manifests it can't render) too; a warning is not.
        let mut broken = app("Unknown", "Healthy", false, None);
        broken.conditions = vec![("ComparisonError".into(), "helm template failed".into())];
        assert_eq!(attention(&broken), Tone::Bad);
        broken.conditions = vec![("SharedResourceWarning".into(), "shared".into())];
        assert_eq!(attention(&broken), Tone::Done);
        assert_eq!(attention(&app("Synced", "Healthy", false, Some("Running"))), Tone::Busy);
        assert_eq!(attention(&app("Synced", "Healthy", false, Some("Succeeded"))), Tone::Good);
        assert!(sync_failed(&app("Synced", "Healthy", false, Some("Failed"))));
    }

    #[test]
    fn the_resources_table_reads_what_argo_cd_lists() {
        let app = json!({"status": {"resources": [
            {"group": "apps", "version": "v1", "kind": "Deployment", "namespace": "shop", "name": "web",
             "status": "OutOfSync", "health": {"status": "Progressing", "message": "Waiting for rollout to finish"}},
            {"version": "v1", "kind": "ServiceAccount", "namespace": "shop", "name": "old", "status": "OutOfSync",
             "requiresPruning": true},
            // A hook left from the last sync (kept until the next one, Argo CD's default): it is
            // flagged `requiresPruning`, with no sync and no health, but a sync never prunes it.
            {"group": "batch", "version": "v1", "kind": "Job", "namespace": "shop", "name": "migrate", "hook": true,
             "requiresPruning": true},
            {"group": "rbac.authorization.k8s.io", "version": "v1", "kind": "ClusterRole", "name": "reader",
             "status": "Synced"},
            // Argo CD can't load the manifests: every resource is flagged, its sync Unknown.
            {"version": "v1", "kind": "Service", "namespace": "shop", "name": "api", "status": "Unknown",
             "requiresPruning": true},
            // Not a resource it can show: no name.
            {"version": "v1", "kind": "ConfigMap", "namespace": "shop"}
        ]}});
        let rows = resource_rows(&app);
        let names: Vec<String> = rows.iter().map(|r| r.res.label()).collect();
        assert_eq!(
            names,
            [
                "Deployment shop/web",
                "ServiceAccount shop/old",
                "Job shop/migrate",
                "ClusterRole reader",
                "Service shop/api"
            ]
        );
        let web = &rows[0];
        assert_eq!((web.res.group.as_str(), web.res.version.as_str()), ("apps", "v1"));
        assert_eq!((web.sync.as_str(), web.health.as_str()), ("OutOfSync", "Progressing"));
        assert_eq!(web.message.as_deref(), Some("Waiting for rollout to finish"));
        // Out of sync and no longer in git: the one row the table says a prune deletes.
        assert!(rows[1].prune && !rows[1].hook && rows[1].health.is_empty());
        let hook = &rows[2];
        assert!(hook.hook && hook.sync.is_empty() && hook.health.is_empty() && !hook.prune);
        let unknown = &rows[4];
        assert!(unknown.sync == "Unknown" && !unknown.prune);
        assert_eq!(rows.iter().map(|r| r.prune).collect::<Vec<_>>(), [false, true, false, false, false]);
        // Neither the hook nor what Argo CD couldn't compare is something to look at here.
        assert_eq!(
            rows.iter().map(ResourceRow::needs_attention).collect::<Vec<_>>(),
            [true, true, false, false, false]
        );
        assert!(resource_rows(&json!({})).is_empty());
    }

    #[test]
    fn a_resource_needs_attention_when_out_of_sync_or_unhealthy() {
        let row =
            |sync: &str, health: &str| ResourceRow { sync: sync.into(), health: health.into(), ..Default::default() };
        for wrong in [row("OutOfSync", ""), row("OutOfSync", "Healthy"), row("Synced", "Degraded"), row("", "Missing")]
        {
            assert!(wrong.needs_attention(), "{wrong:?}");
        }
        // On its way, suspended on purpose, fine, not compared (the diagnosis says why), or a
        // hook with nothing said.
        for fine in [
            row("Synced", "Progressing"),
            row("Synced", "Suspended"),
            row("Synced", "Healthy"),
            row("Unknown", ""),
            row("", ""),
        ] {
            assert!(!fine.needs_attention(), "{fine:?}");
        }
    }

    #[test]
    fn no_health_per_resource_is_what_the_application_says() {
        let app = |source: Option<&str>| {
            let mut app = json!({"status": {"resources": [
                {"group": "apps", "version": "v1", "kind": "Deployment", "namespace": "shop", "name": "web",
                 "status": "Synced"}
            ]}});
            if let Some(source) = source {
                app["status"]["resourceHealthSource"] = json!(source);
            }
            app
        };
        // Argo CD 3's default: in its resource tree, not in the Application.
        assert!(keeps_no_health(&app(Some("appTree"))));
        // Kept in the Application: older versions don't set the field, newer ones leave it empty.
        assert!(!keeps_no_health(&app(None)));
        assert!(!keeps_no_health(&app(Some(""))));
        assert!(!keeps_no_health(&json!({})));
    }

    #[test]
    fn out_of_sync_is_only_a_warning_with_auto_sync() {
        assert_eq!(sync_tone("OutOfSync", false), Tone::Done);
        assert_eq!(sync_tone("OutOfSync", true), Tone::Busy);
        assert_eq!(sync_tone("Synced", false), Tone::Good);
        assert_eq!(health_tone("Missing"), Tone::Bad);
        assert_eq!(health_tone("Suspended"), Tone::Done);
        assert_eq!(operation_tone("Failed"), Tone::Bad);
    }
}

/// Rules that hold for every server URL, tracking id and list of resources.
#[cfg(test)]
mod properties {
    use super::{parse_tracking, resource_rows, server_key};
    use proptest::prelude::*;
    use serde_json::{Value, json};

    /// An entry of `status.resources`, any of its fields left out or empty.
    fn entry() -> impl Strategy<Value = Value> {
        let word = |words: Vec<&'static str>| proptest::sample::select(words);
        (
            word(vec!["", "apps", "batch"]),
            word(vec!["", "Deployment", "Job"]),
            word(vec!["", "shop"]),
            word(vec!["", "a", "b"]),
            proptest::option::of(word(vec!["Synced", "OutOfSync", "Unknown"])),
            proptest::option::of(word(vec!["", "Healthy", "Progressing", "Degraded", "Missing", "Suspended"])),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(group, kind, namespace, name, sync, health, prune, hook)| {
                let mut e = json!({"group": group, "kind": kind, "namespace": namespace, "name": name});
                if let Some(sync) = sync {
                    e["status"] = json!(sync);
                }
                if let Some(health) = health {
                    e["health"] = json!({"status": health});
                }
                if prune {
                    e["requiresPruning"] = json!(true);
                }
                if hook {
                    e["hook"] = json!(true);
                }
                e
            })
    }

    proptest! {
        /// Every resource with a kind and a name gets one row, in Argo CD's order, with what it
        /// lists. It is to prune only when flagged `requiresPruning` and out of sync (not a hook
        /// with no sync, nor one Argo CD couldn't compare), and needs attention exactly when it
        /// is out of sync, Degraded or Missing.
        #[test]
        fn every_resource_gets_its_row(entries in proptest::collection::vec(entry(), 0..20)) {
            let app = json!({"status": {"resources": entries}});
            let rows = resource_rows(&app);
            let listed: Vec<&Value> =
                entries.iter().filter(|e| e["kind"] != "" && e["name"] != "").collect();
            prop_assert_eq!(rows.len(), listed.len());
            for (row, e) in rows.iter().zip(listed) {
                let s = |k: &str| e[k].as_str().unwrap_or_default();
                prop_assert_eq!(
                    (row.res.group.as_str(), row.res.kind.as_str(), row.res.namespace.as_str(), row.res.name.as_str()),
                    (s("group"), s("kind"), s("namespace"), s("name"))
                );
                prop_assert_eq!(row.sync.as_str(), s("status"));
                let health = e.pointer("/health/status").and_then(Value::as_str).unwrap_or_default();
                prop_assert_eq!(row.health.as_str(), health);
                let out_of_sync = s("status") == "OutOfSync";
                prop_assert_eq!(row.prune, e["requiresPruning"] == true && out_of_sync);
                prop_assert_eq!(row.hook, e["hook"] == true);
                let wrong = out_of_sync || matches!(health, "Degraded" | "Missing");
                prop_assert_eq!(row.needs_attention(), wrong);
            }
        }
    }

    proptest! {
        /// A URL compares the same with a trailing slash, its default port or its host in capitals.
        #[test]
        fn a_server_has_one_key(host in "[a-z0-9]{1,10}(\\.[a-z]{2,5}){0,2}", path in "(/[a-z0-9-]{1,8}){0,2}") {
            let url = format!("https://{host}{path}");
            let key = server_key(&url);
            prop_assert_eq!(&server_key(&format!("https://{host}:443{path}")), &key);
            prop_assert_eq!(&server_key(&format!("{url}/")), &key);
            prop_assert_eq!(&server_key(&format!("https://{}{path}", host.to_uppercase())), &key);
            prop_assert_eq!(&server_key(&key), &key);
            prop_assert_ne!(server_key(&format!("https://{host}:6443{path}")), key);
        }

        /// A tracking id reads back what it was made of, whatever the names.
        #[test]
        fn a_tracking_id_round_trips(
            app_ns in proptest::option::of("[a-z0-9][a-z0-9-]{0,10}"),
            app in "[a-z0-9][a-z0-9.-]{0,20}",
            group in "([a-z]{1,8}(\\.[a-z]{1,5}){0,2})?",
            kind in "[A-Z][A-Za-z]{0,12}",
            ns in "([a-z0-9][a-z0-9-]{0,10})?",
            name in "[a-z0-9][a-z0-9.-]{0,20}",
        ) {
            let prefix = app_ns.as_ref().map_or_default(|n| format!("{n}_"));
            let t = parse_tracking(&format!("{prefix}{app}:{group}/{kind}:{ns}/{name}")).unwrap();
            prop_assert_eq!(t.app_ns, app_ns);
            prop_assert_eq!((t.app, t.group, t.kind, t.namespace, t.name), (app, group, kind, ns, name));
        }
    }
}
