//! Argo CD, read from the cluster it runs in (the hub), which is often not one of the
//! clusters open: which contexts serve Applications, the Application that deployed an object
//! (its tracking id), and the ones that deploy into a namespace.
//!
//! Everything here reads. Refresh and Sync go through `argo_refresh` / `argo_sync` with the
//! hub's UI client, so the write gate applies to them like to any write.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures::StreamExt as _;
use kube::Client;
use kube::discovery::ApiResource;
use serde_json::Value;

use super::{ContextTarget, Kind, ObjectRef};
use crate::status::Tone;

/// The annotation Argo CD puts on what it deploys.
pub const TRACKING_ID: &str = "argocd.argoproj.io/tracking-id";
/// The label Argo CD's older tracking mode sets (Helm sets it too, so it is only a hint).
const INSTANCE_LABEL: &str = "app.kubernetes.io/instance";
/// How Applications name the cluster Argo CD runs in.
const IN_CLUSTER: &str = "https://kubernetes.default.svc";
/// Where Argo CD keeps its Applications by default (a tracking id without `<ns>_`).
const DEFAULT_NAMESPACE: &str = "argocd";
/// A context that did not answer is left alone this long before it is tried again.
const RETRY: Duration = Duration::from_secs(300);
/// The longest wait for a context to connect, then to answer whether it serves Applications.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// A hub's Applications are listed at most this often, however many pages ask.
const LIST_TTL: Duration = Duration::from_secs(30);
/// Contexts probed at the same time.
const PROBES_AT_ONCE: usize = 8;

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
    let dest = |p: &str| app.pointer(p).and_then(Value::as_str).filter(|s| !s.is_empty());
    match (dest("/spec/destination/server"), dest("/spec/destination/name")) {
        (Some(s), _) if server_key(s) == server_key(IN_CLUSTER) => same_cluster(hub, here),
        (Some(s), _) => same_server(Some(s), here.server.as_deref()),
        (None, Some("in-cluster")) => same_cluster(hub, here),
        (None, Some(n)) => n == here.name || n == here.cluster,
        (None, None) => false,
    }
}

fn resources(app: &Value) -> impl Iterator<Item = &Value> {
    app.pointer("/status/resources").and_then(Value::as_array).into_iter().flatten()
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

// MARK: What is shown

/// One past deployment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Deployment {
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
    /// The last deployments, newest first.
    pub history: Vec<Deployment>,
    /// Conditions as (type, message): errors and warnings Argo CD reports.
    pub conditions: Vec<(String, String)>,
    pub urls: Vec<String>,
    /// Being deleted since (a stuck finalizer keeps it so).
    pub deleting: Option<String>,
    /// This object's own (sync, health, health message) in it, on an object's page.
    pub own: Option<(String, String, Option<String>)>,
}

/// A revision as shown: a git sha cut to 8 characters, anything else (a chart version) whole.
pub fn short_revision(rev: &str) -> String {
    if rev.len() == 40 && rev.chars().all(|c| c.is_ascii_hexdigit()) { rev[..8].to_string() } else { rev.to_string() }
}

/// `status.sync.revision`, or each of `status.sync.revisions` (several sources).
fn revisions(v: &Value) -> Option<String> {
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

impl Summary {
    pub fn of(app: &Value) -> Self {
        let s = |p: &str| app.pointer(p).and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from);
        let automated = app
            .pointer("/spec/syncPolicy/automated")
            .is_some_and(|a| a.is_object() && a.get("enabled").and_then(Value::as_bool) != Some(false));
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
            history,
            conditions,
            urls,
            deleting: s("/metadata/deletionTimestamp"),
            own: None,
        }
    }

    /// The same, with this object's own sync and health in it.
    fn with_own(mut self, app: &Value, group: &str, kind: &str, ns: &str, name: &str) -> Self {
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

// MARK: Hubs

/// A context that serves Argo CD's Application API, with a client for it.
#[derive(Clone)]
pub struct Hub {
    pub context: String,
    pub client: Client,
}

/// The hubs found, and the contexts that could not be checked with why.
#[derive(Clone, Default)]
pub struct Hubs {
    pub found: Vec<Hub>,
    pub failed: Vec<(String, String)>,
}

#[derive(Clone)]
enum Probe {
    Serves(Client),
    No,
    Failed(String, Instant),
}

/// What each context answered, for the session.
static PROBES: LazyLock<Mutex<HashMap<String, Probe>>> = LazyLock::new(Default::default);

/// What the kubeconfig's contexts point to, read again only when the files change.
fn targets() -> Arc<Vec<ContextTarget>> {
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

fn target(targets: &[ContextTarget], context: &str) -> ContextTarget {
    targets
        .iter()
        .find(|t| t.name == context)
        .cloned()
        .unwrap_or_else(|| ContextTarget { name: context.to_string(), ..Default::default() })
}

/// The contexts that are not connected and may be probed without asking anything of the
/// user: no exec plugin, or the same one as a connected context (whose login is done, so it
/// opens no browser). One context per cluster, and none for a cluster already connected.
pub fn probe_candidates(targets: &[ContextTarget], connected: &[String]) -> Vec<String> {
    let open: Vec<&ContextTarget> = targets.iter().filter(|t| connected.contains(&t.name)).collect();
    let mut seen: Vec<String> = open.iter().filter_map(|t| t.server.as_deref().map(server_key)).collect();
    let mut out = Vec::new();
    for t in targets.iter().filter(|t| !connected.contains(&t.name)) {
        let Some(server) = t.server.as_deref().map(server_key) else { continue };
        let quiet = t.exec.as_ref().is_none_or(|cmd| open.iter().any(|o| o.exec.as_ref() == Some(cmd)));
        if quiet && !seen.contains(&server) {
            seen.push(server);
            out.push(t.name.clone());
        }
    }
    out
}

/// Whether the context serves Applications: one discovery request, bounded in time.
async fn probe(context: String, client: Option<Client>) -> (String, Probe) {
    let failed = |why: String| Probe::Failed(why, Instant::now());
    let client = match client {
        Some(c) => c,
        None => match tokio::time::timeout(PROBE_TIMEOUT, super::connect(context.clone())).await {
            Ok(Ok(c)) => c,
            Ok(Err(e)) => return (context, failed(super::short_error(&e))),
            Err(_) => return (context, failed("did not answer".into())),
        },
    };
    let request = http::Request::get("/apis/argoproj.io/v1alpha1").body(Vec::new());
    let answer = match request {
        Ok(r) => tokio::time::timeout(PROBE_TIMEOUT, client.request::<Value>(r)).await,
        Err(e) => return (context, failed(e.to_string())),
    };
    let probe = match answer {
        Ok(Ok(v)) => {
            let serves = v["resources"].as_array().into_iter().flatten().any(|r| r["name"] == "applications");
            if serves { Probe::Serves(client) } else { Probe::No }
        }
        Ok(Err(e)) => {
            let e = anyhow::Error::from(e);
            if super::is_not_found(&e) { Probe::No } else { failed(super::lists::load_error(&e)) }
        }
        Err(_) => failed("did not answer".into()),
    };
    (context, probe)
}

/// Probe the contexts not answered yet (or that failed a while ago); what each answered.
async fn probe_all(contexts: Vec<String>, clients: &[(String, Client)]) -> Vec<(String, Probe)> {
    let known: HashMap<String, Probe> = PROBES.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let mut out = Vec::new();
    let mut ask = Vec::new();
    for c in contexts {
        match known.get(&c) {
            Some(Probe::Failed(_, at)) if at.elapsed() >= RETRY => ask.push(c),
            Some(p) => out.push((c, p.clone())),
            None => ask.push(c),
        }
    }
    let answers: Vec<(String, Probe)> = futures::stream::iter(ask)
        .map(|c| {
            let client = clients.iter().find(|(n, _)| *n == c).map(|(_, cl)| cl.clone());
            probe(c, client)
        })
        .buffer_unordered(PROBES_AT_ONCE)
        .collect()
        .await;
    PROBES.lock().unwrap_or_else(PoisonError::into_inner).extend(answers.iter().cloned());
    out.extend(answers);
    out
}

fn hubs_of(probes: Vec<(String, Probe)>, clients: &[(String, Client)]) -> Hubs {
    let mut hubs = Hubs::default();
    for (context, p) in probes {
        match p {
            // A connected cluster's own client, as the rest of the app uses.
            Probe::Serves(client) => {
                let client = clients.iter().find(|(n, _)| *n == context).map_or(client, |(_, c)| c.clone());
                hubs.found.push(Hub { context, client });
            }
            Probe::Failed(why, _) => hubs.failed.push((context, why)),
            Probe::No => {}
        }
    }
    hubs.found.sort_by(|a, b| a.context.cmp(&b.context));
    hubs
}

/// The contexts that serve Applications: those set in Settings, else the connected clusters
/// that do, else the other contexts of the kubeconfig that can be asked quietly. What each
/// context answered is kept for the session; one that did not answer is asked again later.
pub async fn hubs(connected: Vec<(String, Client)>) -> Hubs {
    // One search at a time: pages opened together wait for the first one's answers.
    static SEARCH: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(Default::default);
    super::run(async move {
        let _one = SEARCH.lock().await;
        let chosen = crate::settings::with(|s| s.argocd_contexts.clone());
        if !chosen.is_empty() {
            return Ok(hubs_of(probe_all(chosen, &connected).await, &connected));
        }
        let names: Vec<String> = connected.iter().map(|(n, _)| n.clone()).collect();
        let open = hubs_of(probe_all(names.clone(), &connected).await, &connected);
        if !open.found.is_empty() {
            return Ok(open);
        }
        let others = probe_candidates(&targets(), &names);
        let mut hubs = hubs_of(probe_all(others, &connected).await, &connected);
        hubs.failed.extend(open.failed);
        Ok(hubs)
    })
    .await
    .unwrap_or_default()
}

// MARK: Reading Applications

type Lists = HashMap<String, (Instant, Arc<Vec<Value>>)>;

/// Each hub's Applications, with when they were listed.
static LISTS: LazyLock<Mutex<Lists>> = LazyLock::new(Default::default);

/// Drop a hub's list of Applications kept for a while: the next page asking lists them anew.
pub fn forget_list(hub: &str) {
    LISTS.lock().unwrap_or_else(PoisonError::into_inner).remove(hub);
}

/// Every Application on a hub, shared by the pages that ask within `LIST_TTL`.
pub async fn applications(hub: &Hub) -> Result<Arc<Vec<Value>>> {
    static LISTING: LazyLock<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = LazyLock::new(Default::default);
    let cached = || {
        let lists = LISTS.lock().unwrap_or_else(PoisonError::into_inner);
        lists.get(&hub.context).filter(|(at, _)| at.elapsed() < LIST_TTL).map(|(_, l)| l.clone())
    };
    let lock = LISTING.lock().unwrap_or_else(PoisonError::into_inner).entry(hub.context.clone()).or_default().clone();
    // Pages asking together wait for one list.
    let _one = lock.lock().await;
    if let Some(list) = cached() {
        return Ok(list);
    }
    let mut list = super::list_values(hub.client.clone(), application_kind().ar, None, None).await?;
    for app in &mut list {
        if let Some(meta) = app.get_mut("metadata").and_then(Value::as_object_mut) {
            meta.remove("managedFields");
        }
    }
    let list = Arc::new(list);
    LISTS.lock().unwrap_or_else(PoisonError::into_inner).insert(hub.context.clone(), (Instant::now(), list.clone()));
    Ok(list)
}

async fn get_application(hub: &Hub, ns: &str, name: &str) -> Result<Value> {
    let obj = ObjectRef {
        kind: application_kind(),
        name: name.to_string(),
        namespace: Some(ns.to_string()),
        uid: String::new(),
    };
    super::get_object(hub.client.clone(), obj).await
}

/// An Application found, with the hub it was read on.
#[derive(Clone)]
pub struct Found {
    pub hub: Hub,
    pub app: Summary,
}

/// What a page's Argo CD section shows: the Applications found, and why some could not be
/// read, in one line. Both empty: nothing to show (no Argo CD, or it deploys nothing here).
#[derive(Clone, Default)]
pub struct Lookup {
    pub apps: Vec<Found>,
    pub note: Option<String>,
}

fn failed_note(hubs: &Hubs) -> String {
    let why: Vec<String> = hubs.failed.iter().map(|(c, e)| format!("{c}: {e}")).collect();
    if why.is_empty() { String::new() } else { format!(" ({})", why.join("; ")) }
}

/// The Application that deployed this object (on the page of context `here`): by its
/// tracking id, else by the instance label when an Application tracks the object under
/// that name. Nothing is asked of any cluster for an object with neither.
pub async fn for_object(
    connected: Vec<(String, Client)>,
    here: String,
    group: String,
    kind: String,
    obj: Value,
) -> Lookup {
    // Off the UI thread: a hub's Applications can be many and big.
    super::run(async move { Ok(object_lookup(connected, here, group, kind, obj).await) }).await.unwrap_or_default()
}

async fn object_lookup(
    connected: Vec<(String, Client)>,
    here: String,
    group: String,
    kind: String,
    obj: Value,
) -> Lookup {
    let tracking = tracking_of(&group, &kind, &obj);
    let ns = obj.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default().to_string();
    let name = obj.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string();
    // Pods are not deployed by Argo CD but by their workloads, and carry its label.
    let label = obj
        .pointer("/metadata/labels")
        .and_then(|l| l.get(INSTANCE_LABEL))
        .and_then(Value::as_str)
        .filter(|_| tracking.is_none() && !(group.is_empty() && kind == "Pod"))
        .map(String::from);
    if tracking.is_none() && label.is_none() {
        return Lookup::default();
    }
    let hubs = hubs(connected).await;
    if hubs.found.is_empty() {
        let note = tracking.map(|t| {
            format!(
                "Deployed by Argo CD Application {}, but no context in your kubeconfig serves Argo CD{}",
                t.app,
                failed_note(&hubs)
            )
        });
        return Lookup { apps: Vec::new(), note };
    }
    let targets = targets();
    let here = target(&targets, &here);
    let own = |s: Summary, app: &Value| s.with_own(app, &group, &kind, &ns, &name);
    let mut errors = Vec::new();
    // Read on each hub; one whose destination is this cluster wins over one that may not be.
    let mut found: Vec<(bool, Found)> = Vec::new();
    for hub in &hubs.found {
        let hub_target = target(&targets, &hub.context);
        let apps: Vec<Value> = if let Some(t) = &tracking {
            let app_ns = t.app_ns.as_deref().unwrap_or(DEFAULT_NAMESPACE);
            match get_application(hub, app_ns, &t.app).await {
                Ok(app) => vec![app],
                // Argo CD may keep its Applications in another namespace than the default one.
                Err(e) if t.app_ns.is_none() && matches!(super::api_status(&e), Some(403 | 404)) => {
                    match applications(hub).await {
                        Ok(list) => list
                            .iter()
                            .filter(|a| a.pointer("/metadata/name").and_then(Value::as_str) == Some(t.app.as_str()))
                            .cloned()
                            .collect(),
                        Err(e) => {
                            errors.push(format!("{}: {}", hub.context, super::lists::load_error(&e)));
                            Vec::new()
                        }
                    }
                }
                Err(e) if super::is_not_found(&e) => Vec::new(),
                Err(e) => {
                    errors.push(format!("{}: {}", hub.context, super::lists::load_error(&e)));
                    Vec::new()
                }
            }
        } else {
            let label = label.as_deref().unwrap_or_default();
            match applications(hub).await {
                Ok(list) => list
                    .iter()
                    .filter(|a| a.pointer("/metadata/name").and_then(Value::as_str) == Some(label))
                    .filter(|a| {
                        targets_cluster(a, &hub_target, &here) && resource_of(a, &group, &kind, &ns, &name).is_some()
                    })
                    .cloned()
                    .collect(),
                // A label is only a hint: a hub that can't be listed says nothing about this object.
                Err(_) => Vec::new(),
            }
        };
        for app in apps {
            let here_too = targets_cluster(&app, &hub_target, &here);
            found.push((here_too, Found { hub: hub.clone(), app: own(Summary::of(&app), &app) }));
        }
    }
    // Several hubs may hold an Application of that name: keep those deploying here, else
    // one that can only be it.
    let mut apps: Vec<Found> = found.iter().filter(|(h, _)| *h).map(|(_, f)| f.clone()).collect();
    if apps.is_empty() && found.len() == 1 {
        apps = found.into_iter().map(|(_, f)| f).collect();
    }
    let note = match (&tracking, apps.is_empty(), errors.is_empty()) {
        (_, _, false) => Some(format!("Argo CD could not be read: {}", errors.join("; "))),
        (Some(t), true, true) => {
            let on: Vec<&str> = hubs.found.iter().map(|h| h.context.as_str()).collect();
            Some(format!("Deployed by Argo CD Application {}, which is not on {}", t.app, on.join(", ")))
        }
        _ => None,
    };
    Lookup { apps, note }
}

/// The Applications that deploy into this namespace of the context `here`. Quiet when no
/// context serves Argo CD.
pub async fn for_namespace(connected: Vec<(String, Client)>, here: String, ns: String) -> Lookup {
    super::run(async move { Ok(namespace_lookup(connected, here, ns).await) }).await.unwrap_or_default()
}

async fn namespace_lookup(connected: Vec<(String, Client)>, here: String, ns: String) -> Lookup {
    let hubs = hubs(connected).await;
    let targets = targets();
    let here = target(&targets, &here);
    let mut lookup = Lookup::default();
    let mut errors = Vec::new();
    for hub in &hubs.found {
        let hub_target = target(&targets, &hub.context);
        match applications(hub).await {
            Ok(list) => lookup.apps.extend(
                list.iter()
                    .filter(|a| targets_cluster(a, &hub_target, &here) && targets_namespace(a, &ns))
                    .map(|a| Found { hub: hub.clone(), app: Summary::of(a) }),
            ),
            Err(e) => errors.push(format!("{}: {}", hub.context, super::lists::load_error(&e))),
        }
    }
    lookup.apps.sort_by(|a, b| (&a.app.name, &a.hub.context).cmp(&(&b.app.name, &b.hub.context)));
    if !errors.is_empty() {
        lookup.note = Some(format!("Argo CD could not be read: {}", errors.join("; ")));
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
pub fn parents(apps: &[Value]) -> Vec<Option<usize>> {
    fn meta<'a>(app: &'a Value, field: &str) -> &'a str {
        app.get("metadata").and_then(|m| m.get(field)).and_then(Value::as_str).unwrap_or_default()
    }
    let mut by_key: HashMap<(&str, &str), usize> = HashMap::new();
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
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

/// One Application of the Applications page.
#[derive(Clone)]
pub struct FleetApp {
    pub hub: Hub,
    pub app: Summary,
    pub dest: Destination,
    /// The context its destination is shown on (a connected one first), if any reaches it.
    pub on: Option<String>,
    /// The Application that made it (an app of apps), as an index in the fleet.
    pub parent: Option<usize>,
}

/// Every Application of every hub, and why some hubs could not be read.
#[derive(Clone, Default)]
pub struct Fleet {
    pub apps: Vec<FleetApp>,
    /// "context: why", for each hub not read and each context that could not be checked.
    pub errors: Vec<String>,
    /// The hubs found, read or not.
    pub hubs: Vec<String>,
}

/// Every Application, read with one list per hub (kept `LIST_TTL`, shared with the pages'
/// Argo CD sections). One hub that can't be read is reported; the others are shown.
pub async fn fleet(connected: Vec<(String, Client)>) -> Fleet {
    super::run(async move { Ok(read_fleet(connected).await) })
        .await
        .unwrap_or_else(|e| Fleet { errors: vec![format!("Argo CD could not be read: {e:#}")], ..Default::default() })
}

async fn read_fleet(connected: Vec<(String, Client)>) -> Fleet {
    let names: Vec<String> = connected.iter().map(|(n, _)| n.clone()).collect();
    let hubs = hubs(connected).await;
    let targets = targets();
    let lists =
        futures::future::join_all(hubs.found.iter().map(|hub| async move { (hub, applications(hub).await) })).await;
    let mut fleet = Fleet {
        errors: hubs.failed.iter().map(|(c, e)| format!("{c}: {e}")).collect(),
        hubs: hubs.found.iter().map(|h| h.context.clone()).collect(),
        ..Default::default()
    };
    for (hub, list) in lists {
        let list = match list {
            Ok(list) => list,
            Err(e) => {
                fleet.errors.push(format!("{}: {}", hub.context, super::lists::load_error(&e)));
                continue;
            }
        };
        let hub_target = target(&targets, &hub.context);
        let base = fleet.apps.len();
        for (app, parent) in list.iter().zip(parents(&list)) {
            let dest = destination(app, &hub_target, &targets);
            let on = dest.context(&names).map(String::from);
            fleet.apps.push(FleetApp {
                hub: hub.clone(),
                app: Summary::of(app),
                dest,
                on,
                parent: parent.map(|p| base + p),
            });
        }
    }
    fleet
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
    fn only_quiet_contexts_are_probed_once_per_cluster() {
        let targets = vec![
            ctx("work", "https://work.example", Some("kubelogin get-token --x")),
            ctx("hub", "https://hub.example", Some("kubelogin get-token --x")),
            ctx("hub-admin", "https://hub.example:443/", None),
            ctx("eks", "https://eks.example", Some("aws eks get-token --cluster-name eks")),
            ctx("kind", "https://127.0.0.1:6443", None),
            ctx("work-again", "https://work.example", None),
        ];
        // The same login as a connected context; no login; never another login, nor a
        // second context for a cluster already asked or connected.
        assert_eq!(probe_candidates(&targets, &["work".into()]), ["hub", "kind"]);
        // Nothing connected: only contexts without a login.
        assert_eq!(probe_candidates(&targets, &[]), ["hub-admin", "kind", "work-again"]);
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
                "summary": {"externalURLs": ["https://web.example"]},
                "resources": [{"group": "apps", "kind": "Deployment", "namespace": "shop", "name": "web", "status": "Synced", "health": {"status": "Healthy"}}]
            }
        });
        let s = Summary::of(&app).with_own(&app, "apps", "Deployment", "shop", "web");
        assert_eq!((s.sync.as_str(), s.health.as_str(), s.automated), ("OutOfSync", "Degraded", true));
        assert_eq!(s.revision.as_deref(), Some("01234567"));
        assert_eq!(s.history[0].revision, "01234567, 2.0.0");
        assert_eq!(s.history[0].by.as_deref(), Some("ana"));
        assert_eq!(s.history[1].by.as_deref(), Some("auto-sync"));
        assert_eq!(s.operation.as_ref().map(|o| o.0.as_str()), Some("Failed"));
        assert_eq!(s.conditions, [("SyncError".to_string(), "boom".to_string())]);
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
        assert_eq!(parents(&apps), [None, Some(0), Some(0), Some(0), None, None, Some(7), None]);
        // The parent's namespace isn't `argocd`: found by its name when only one has it.
        let mut moved = apps;
        moved[0]["metadata"]["namespace"] = json!("gitops");
        assert_eq!(parents(&moved)[1], Some(0));
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
    fn out_of_sync_is_only_a_warning_with_auto_sync() {
        assert_eq!(sync_tone("OutOfSync", false), Tone::Done);
        assert_eq!(sync_tone("OutOfSync", true), Tone::Busy);
        assert_eq!(sync_tone("Synced", false), Tone::Good);
        assert_eq!(health_tone("Missing"), Tone::Bad);
        assert_eq!(health_tone("Suspended"), Tone::Done);
        assert_eq!(operation_tone("Failed"), Tone::Bad);
    }
}

/// Rules that hold for every server URL and tracking id.
#[cfg(test)]
mod properties {
    use super::{parse_tracking, server_key};
    use proptest::prelude::*;

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
            let prefix = app_ns.as_ref().map(|n| format!("{n}_")).unwrap_or_default();
            let t = parse_tracking(&format!("{prefix}{app}:{group}/{kind}:{ns}/{name}")).unwrap();
            prop_assert_eq!(t.app_ns, app_ns);
            prop_assert_eq!((t.app, t.group, t.kind, t.namespace, t.name), (app, group, kind, ns, name));
        }
    }
}
