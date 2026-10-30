//! References to objects that don't exist. They break quietly: the pod waits on a missing
//! ConfigMap, the Ingress answers 503, the Service has no endpoints. The Problems scan names
//! what is missing on the object that points to it.
//!
//! Pods, workloads and PVCs are read only when their row is already a problem (a pod stuck
//! creating, a Deployment with no ready replica, a PVC pending), and the reason says which
//! reference is missing. Services, Ingresses and HTTPRoutes are checked whole, with the
//! Services listed once for the three.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use futures::StreamExt as _;
use k8s_openapi::api::{apps::v1 as apps, core::v1 as core, storage::v1 as storage};
use kube::Client;
use kube::api::{Api, DynamicObject};
use kube::discovery::ApiResource;
use serde_json::Value;

use super::{HELD_PARALLEL, Problem, Scan, Severity, cell, since};
use crate::k8s::{self, ColumnDef, Kind, Row, Selection};

/// The Gateway API's group, for HTTPRoutes.
pub(super) const GATEWAY: &str = "gateway.networking.k8s.io";

/// Kinds checked with the Services they point to (`traffic`), not on their own.
pub(super) const WITH_SERVICES: [(&str, &str); 2] = [("networking.k8s.io", "Ingress"), (GATEWAY, "HTTPRoute")];

/// Seconds an object may point to something missing before it is called a problem: what a
/// release creates with it (a TLS Secret being issued, a ServiceAccount) may still be coming.
const SETTLE: i64 = 300;

/// Above this many objects of one kind to look up, the kind is listed (kubectl's light rows)
/// rather than read one at a time.
const READ_AT_MOST: usize = 32;

/// A kind of object referred to by name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Of {
    ConfigMap,
    Secret,
    Claim,
    StorageClass,
    ServiceAccount,
    Service,
}

impl Of {
    fn kind(self) -> &'static str {
        match self {
            Of::ConfigMap => "ConfigMap",
            Of::Secret => "Secret",
            Of::Claim => "PersistentVolumeClaim",
            Of::StorageClass => "StorageClass",
            Of::ServiceAccount => "ServiceAccount",
            Of::Service => "Service",
        }
    }

    fn plural(self) -> &'static str {
        match self {
            Of::ConfigMap => "ConfigMaps",
            Of::Secret => "Secrets",
            Of::Claim => "PersistentVolumeClaims",
            Of::StorageClass => "StorageClasses",
            Of::ServiceAccount => "ServiceAccounts",
            Of::Service => "Services",
        }
    }

    fn ar(self) -> ApiResource {
        match self {
            Of::ConfigMap => k8s::ar_of::<core::ConfigMap>(),
            Of::Secret => k8s::ar_of::<core::Secret>(),
            Of::Claim => k8s::ar_of::<core::PersistentVolumeClaim>(),
            Of::StorageClass => k8s::ar_of::<storage::StorageClass>(),
            Of::ServiceAccount => k8s::ar_of::<core::ServiceAccount>(),
            Of::Service => k8s::ar_of::<core::Service>(),
        }
    }

    fn namespaced(self) -> bool {
        self != Of::StorageClass
    }

    /// What a scan says when this kind could not be read.
    fn skipped(self) -> String {
        format!("references to {}", self.plural())
    }
}

/// An object referred to: its kind, namespace (None for a cluster-scoped kind) and name.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Target {
    pub of: Of,
    pub namespace: Option<String>,
    pub name: String,
}

impl Target {
    /// "ConfigMap app", with its namespace when it isn't `home` (an HTTPRoute's backend
    /// elsewhere).
    fn text(&self, home: Option<&str>) -> String {
        match self.namespace.as_deref() {
            Some(ns) if Some(ns) != home => format!("{} {ns}/{}", self.of.kind(), self.name),
            _ => format!("{} {}", self.of.kind(), self.name),
        }
    }
}

fn text<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn items(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

fn created(obj: &Value) -> Option<jiff::Timestamp> {
    text(obj, "/metadata/creationTimestamp")?.parse().ok()
}

/// Not being deleted, and older than `SETTLE` (an unknown age counts as old).
fn settled(obj: &Value) -> bool {
    obj.pointer("/metadata/deletionTimestamp").is_none_or(Value::is_null) && since(created(obj), SETTLE)
}

/// What a pod needs to start: the ConfigMaps, Secrets and PVCs of its volumes (projected ones
/// too) and of its containers' and init containers' environment. A reference marked
/// `optional: true` isn't needed.
pub fn pod_refs(pod: &Value) -> Vec<Target> {
    let ns = text(pod, "/metadata/namespace");
    let mut out = BTreeSet::new();
    let mut add = |of: Of, r: &Value, key: &str| {
        let name = r.get(key).and_then(Value::as_str).filter(|n| !n.is_empty());
        if let Some(name) = name
            && r.get("optional").and_then(Value::as_bool) != Some(true)
        {
            out.insert(Target { of, namespace: ns.map(String::from), name: name.to_string() });
        }
    };
    let spec = &pod["spec"];
    for v in items(&spec["volumes"]) {
        add(Of::ConfigMap, &v["configMap"], "name");
        add(Of::Secret, &v["secret"], "secretName");
        add(Of::Claim, &v["persistentVolumeClaim"], "claimName");
        for s in items(&v["projected"]["sources"]) {
            add(Of::ConfigMap, &s["configMap"], "name");
            add(Of::Secret, &s["secret"], "name");
        }
    }
    for c in items(&spec["initContainers"]).chain(items(&spec["containers"])) {
        for e in items(&c["envFrom"]) {
            add(Of::ConfigMap, &e["configMapRef"], "name");
            add(Of::Secret, &e["secretRef"], "name");
        }
        for e in items(&c["env"]) {
            add(Of::ConfigMap, &e["valueFrom"]["configMapKeyRef"], "name");
            add(Of::Secret, &e["valueFrom"]["secretKeyRef"], "name");
        }
    }
    out.into_iter().collect()
}

/// The ServiceAccount a workload's pods run as, when named: without it the ReplicaSet (or
/// StatefulSet, DaemonSet) can't create them.
pub fn workload_refs(obj: &Value) -> Vec<Target> {
    let spec = &obj["spec"]["template"]["spec"];
    let name = [&spec["serviceAccountName"], &spec["serviceAccount"]]
        .into_iter()
        .find_map(|n| n.as_str().filter(|n| !n.is_empty()));
    let ns = text(obj, "/metadata/namespace").map(String::from);
    name.map(|name| Target { of: Of::ServiceAccount, namespace: ns, name: name.to_string() }).into_iter().collect()
}

/// The StorageClass a pending PVC waits on, when it names one (`""` is no class at all, and
/// none the cluster's default).
pub fn claim_refs(pvc: &Value) -> Vec<Target> {
    if text(pvc, "/status/phase").unwrap_or("Pending") != "Pending" {
        return Vec::new();
    }
    let class = text(pvc, "/spec/storageClassName");
    class.map(|name| Target { of: Of::StorageClass, namespace: None, name: name.to_string() }).into_iter().collect()
}

/// What an object of `kind` needs that may be missing. A pod only while Pending: the kubelet
/// reads them at start, so a running pod doesn't need them any more.
pub fn refs_of(kind: &Kind, obj: &Value) -> Vec<Target> {
    match (kind.ar.group.as_str(), kind.ar.kind.as_str()) {
        ("", "Pod") if text(obj, "/status/phase").unwrap_or("Pending") == "Pending" => pod_refs(obj),
        ("apps", "Deployment" | "StatefulSet" | "DaemonSet") => workload_refs(obj),
        ("", "PersistentVolumeClaim") => claim_refs(obj),
        _ => Vec::new(),
    }
}

/// Whether a problem row may come from a missing reference, so its object is read to say
/// which: a pod still creating (not one running or crashing), a workload short of ready
/// replicas, a pending PVC.
pub(super) fn may_dangle(kind: &Kind, defs: &[ColumnDef], row: &Row) -> bool {
    match (kind.ar.group.as_str(), kind.ar.kind.as_str()) {
        ("", "Pod") => {
            let status = cell(defs, row, "Status");
            matches!(status, "Pending" | "ContainerCreating" | "CreateContainerConfigError")
                || status.starts_with("Init:")
        }
        ("apps", "Deployment" | "StatefulSet" | "DaemonSet") | ("", "PersistentVolumeClaim") => true,
        _ => false,
    }
}

/// "missing ConfigMap a, Secret b" for the targets known not to exist; one that could not be
/// told (`exists` None) isn't reported. None when nothing is missing.
pub fn missing(targets: &[Target], home: Option<&str>, exists: impl Fn(&Target) -> Option<bool>) -> Option<String> {
    let gone: Vec<String> = targets.iter().filter(|&t| exists(t) == Some(false)).map(|t| t.text(home)).collect();
    (!gone.is_empty()).then(|| format!("missing {}", gone.join(", ")))
}

/// Objects of one kind listed in one namespace, or in all (`scope` None), by namespace and
/// name (`""` for a cluster-scoped kind).
pub struct Listed<T> {
    scope: Option<String>,
    objects: HashMap<(String, String), T>,
}

impl<T> Listed<T> {
    pub fn new(scope: Option<&str>, objects: impl IntoIterator<Item = (String, String, T)>) -> Self {
        Self {
            scope: scope.map(String::from),
            objects: objects.into_iter().map(|(ns, name, o)| ((ns, name), o)).collect(),
        }
    }

    /// The object, or Some(None) when it doesn't exist; None when its namespace wasn't listed
    /// (nothing can be said).
    pub fn get(&self, namespace: Option<&str>, name: &str) -> Option<Option<&T>> {
        if let (Some(scope), Some(ns)) = (self.scope.as_deref(), namespace)
            && scope != ns
        {
            return None;
        }
        Some(self.objects.get(&(namespace.unwrap_or_default().to_string(), name.to_string())))
    }

    pub fn exists(&self, t: &Target) -> Option<bool> {
        Some(self.get(t.namespace.as_deref(), &t.name)?.is_some())
    }
}

/// Services (whole objects) by namespace and name.
fn services_by_name<'a>(scope: Option<&str>, services: &'a [Value]) -> Listed<&'a Value> {
    Listed::new(
        scope,
        services.iter().filter_map(|s| {
            Some((text(s, "/metadata/namespace")?.to_string(), text(s, "/metadata/name")?.to_string(), s))
        }),
    )
}

/// Whether each target exists. A few are read one at a time (metadata only); more of one
/// kind than `READ_AT_MOST`, and that kind is listed in `scope` instead. A kind that could
/// not be read leaves its targets unknown, and is returned once.
async fn lookup(
    client: &Client,
    scope: Option<&str>,
    targets: BTreeSet<Target>,
) -> (HashMap<Target, bool>, Vec<(String, anyhow::Error)>) {
    let mut by_of: BTreeMap<Of, Vec<Target>> = BTreeMap::new();
    for t in targets {
        by_of.entry(t.of).or_default().push(t);
    }
    let mut exists = HashMap::new();
    let mut skipped = Vec::new();
    for (of, targets) in by_of {
        if targets.len() > READ_AT_MOST {
            let kind = Kind { ar: of.ar(), namespaced: of.namespaced(), section: "", label: of.plural().into() };
            let ns = scope.filter(|_| of.namespaced()).map(String::from);
            match k8s::list_light(client.clone(), kind, ns.clone(), true).await {
                Ok((_, rows)) => {
                    let listed = Listed::new(
                        ns.as_deref(),
                        rows.iter().map(|r| (r.namespace.clone().unwrap_or_default(), r.name.clone(), ())),
                    );
                    exists.extend(targets.into_iter().filter_map(|t| listed.exists(&t).map(|e| (t, e))));
                }
                Err(e) => skipped.push((of.skipped(), e)),
            }
            continue;
        }
        let read: Vec<(Target, anyhow::Result<bool>)> = futures::stream::iter(targets)
            .map(|t| {
                let client = client.clone();
                async move {
                    let (ar, ns, name) = (t.of.ar(), t.namespace.clone(), t.name.clone());
                    let found = k8s::run(async move {
                        let api: Api<DynamicObject> = match &ns {
                            Some(ns) => Api::namespaced_with(client, ns, &ar),
                            None => Api::all_with(client, &ar),
                        };
                        Ok(api.get_metadata_opt(&name).await?.is_some())
                    })
                    .await;
                    (t, found)
                }
            })
            .buffer_unordered(HELD_PARALLEL)
            .collect()
            .await;
        let mut failed = None;
        for (t, found) in read {
            match found {
                Ok(found) => {
                    exists.insert(t, found);
                }
                Err(e) => failed = failed.or(Some(e)),
            }
        }
        if let Some(e) = failed {
            skipped.push((of.skipped(), e));
        }
    }
    (exists, skipped)
}

/// Read the objects of the problems at `which` in `found` and add what they need that
/// doesn't exist to their reasons ("CreateContainerConfigError: missing ConfigMap app").
/// Returns what could not be read.
pub(super) async fn explain(
    client: &Client,
    kind: &Kind,
    scope: Option<&str>,
    found: &mut [Problem],
    which: &[usize],
) -> Vec<(String, anyhow::Error)> {
    if which.is_empty() {
        return Vec::new();
    }
    let read: Vec<(usize, anyhow::Result<Option<Value>>)> = futures::stream::iter(which.iter().copied())
        .map(|i| {
            let (client, ar, ns, name) =
                (client.clone(), kind.ar.clone(), found[i].namespace.clone(), found[i].name.clone());
            async move {
                let obj = k8s::run(async move {
                    let api: Api<DynamicObject> = match &ns {
                        Some(ns) => Api::namespaced_with(client, ns, &ar),
                        None => Api::all_with(client, &ar),
                    };
                    Ok(api.get_opt(&name).await?.map(serde_json::to_value).transpose()?)
                })
                .await;
                (i, obj)
            }
        })
        .buffer_unordered(HELD_PARALLEL)
        .collect()
        .await;
    let mut skipped = Vec::new();
    let mut refs = Vec::new();
    for (i, obj) in read {
        match obj {
            Ok(Some(obj)) => refs.push((i, refs_of(kind, &obj))),
            // Deleted in between.
            Ok(None) => {}
            Err(e) if skipped.is_empty() => skipped.push((format!("references from {}", kind.label), e)),
            Err(_) => {}
        }
    }
    let targets = refs.iter().flat_map(|(_, t)| t.iter().cloned()).collect();
    let (exists, more) = lookup(client, scope, targets).await;
    skipped.extend(more);
    for (i, targets) in refs {
        let p = &mut found[i];
        if let Some(gone) = missing(&targets, p.namespace.as_deref(), |t| exists.get(t).copied()) {
            p.reason = format!("{}: {gone}", p.reason);
        }
    }
    skipped
}

/// The Services with a ready endpoint, as (namespace, name), from the Endpoints' rows:
/// kubectl's column lists the ready addresses only, `<none>` without one. A few bytes a
/// Service, where its EndpointSlices are whole objects.
fn served(defs: &[ColumnDef], rows: &[std::sync::Arc<Row>]) -> HashSet<(String, String)> {
    rows.iter()
        .filter(|r| !matches!(cell(defs, r, "Endpoints"), "" | "<none>"))
        .filter_map(|r| Some((r.namespace.clone()?, r.name.clone())))
        .collect()
}

/// The pods that may serve: not those that ran to completion.
fn live(pods: &[Value]) -> Vec<&Value> {
    pods.iter().filter(|p| !matches!(text(p, "/status/phase"), Some("Succeeded" | "Failed"))).collect()
}

fn pod_ready(pod: &Value) -> bool {
    let deleting = pod.pointer("/metadata/deletionTimestamp").is_some_and(|t| !t.is_null());
    !deleting
        && items(&pod["status"]["conditions"]).any(|c| c["type"].as_str() == Some("Ready") && c["status"] == "True")
}

/// A Service whose pods are `selected` (listed by its selector): selecting none is a warning,
/// minor when `scaled` names its workload scaled to 0 on purpose; none of them ready, a
/// warning. Pods that ran to completion don't count.
fn judge_service(selected: &[Value], scaled: Option<&str>) -> Option<(Severity, String)> {
    let live = live(selected);
    if live.is_empty() {
        return Some(match scaled {
            Some(w) => (Severity::Info, format!("selects no pod ({w} scaled to 0)")),
            None => (Severity::Warning, "selects no pod".into()),
        });
    }
    let ready = live.iter().filter(|p| pod_ready(p)).count();
    (ready == 0).then(|| (Severity::Warning, format!("no ready pod (0/{} ready)", live.len())))
}

/// The workload scaled to 0 whose pods a Service would select, as "Deployment web".
/// `workloads`: (kind, object), in any namespace.
fn scaled_to_zero(service: &Value, workloads: &[(&str, Value)]) -> Option<String> {
    let ns = text(service, "/metadata/namespace");
    let selector = service.pointer("/spec/selector")?;
    let selector = serde_json::json!({ "matchLabels": selector });
    workloads.iter().find_map(|(kind, w)| {
        let labels = w.pointer("/spec/template/metadata/labels")?.as_object()?;
        let zero = w.pointer("/spec/replicas").and_then(Value::as_i64) == Some(0);
        (zero && text(w, "/metadata/namespace") == ns && k8s::selector_matches(&selector, labels))
            .then(|| format!("{kind} {}", text(w, "/metadata/name").unwrap_or_default()))
    })
}

/// The TLS Secrets an Ingress names.
fn ingress_secrets(ing: &Value) -> Vec<Target> {
    let ns = text(ing, "/metadata/namespace").map(String::from);
    let names: BTreeSet<&str> =
        items(&ing["spec"]["tls"]).filter_map(|t| t["secretName"].as_str().filter(|n| !n.is_empty())).collect();
    names.into_iter().map(|name| Target { of: Of::Secret, namespace: ns.clone(), name: name.to_string() }).collect()
}

/// The port an Ingress backend asks for when the Service doesn't have it: "8080", "named
/// http". An ExternalName Service forwards elsewhere, on any port.
fn port_missing(service: &Value, port: &Value) -> Option<String> {
    if text(service, "/spec/type") == Some("ExternalName") {
        return None;
    }
    let mut ports = items(&service["spec"]["ports"]);
    if let Some(n) = port["number"].as_i64() {
        return (!ports.any(|p| p["port"].as_i64() == Some(n))).then(|| n.to_string());
    }
    let name = port["name"].as_str().filter(|n| !n.is_empty())?;
    (!ports.any(|p| p["name"].as_str() == Some(name))).then(|| format!("named {name}"))
}

/// What an Ingress points to that is missing: its backend Services or their ports, and its
/// TLS Secrets (`secret`: whether one exists, None when that could not be read).
fn judge_ingress(ing: &Value, services: &Listed<&Value>, secret: impl Fn(&Target) -> Option<bool>) -> Vec<String> {
    let ns = text(ing, "/metadata/namespace");
    let spec = &ing["spec"];
    let backends = std::iter::once(&spec["defaultBackend"])
        .chain(items(&spec["rules"]).flat_map(|r| items(&r["http"]["paths"]).map(|p| &p["backend"])));
    let mut out = Vec::new();
    for b in backends {
        let svc = &b["service"];
        let Some(name) = svc["name"].as_str().filter(|n| !n.is_empty()) else { continue };
        let why = match services.get(ns, name) {
            None => continue,
            Some(None) => format!("missing Service {name}"),
            Some(Some(s)) => match port_missing(s, &svc["port"]) {
                Some(port) => format!("Service {name} has no port {port}"),
                None => continue,
            },
        };
        if !out.contains(&why) {
            out.push(why);
        }
    }
    for t in ingress_secrets(ing) {
        if secret(&t) == Some(false) {
            out.push(format!("missing TLS Secret {}", t.name));
        }
    }
    out
}

/// The Services an HTTPRoute sends to that are missing (other kinds of backend aren't
/// checked). A backend in a namespace that wasn't listed can't be told.
fn judge_route(route: &Value, services: &Listed<&Value>) -> Vec<String> {
    let ns = text(route, "/metadata/namespace");
    let mut targets = BTreeSet::new();
    for b in items(&route["spec"]["rules"]).flat_map(|r| items(&r["backendRefs"])) {
        let core = b["group"].as_str().unwrap_or_default().is_empty();
        let service = b["kind"].as_str().unwrap_or("Service") == "Service";
        let Some(name) = b["name"].as_str().filter(|n| !n.is_empty()) else { continue };
        if core && service {
            let namespace = b["namespace"].as_str().filter(|n| !n.is_empty()).or(ns).map(String::from);
            targets.insert(Target { of: Of::Service, namespace, name: name.to_string() });
        }
    }
    let targets: Vec<Target> = targets.into_iter().collect();
    missing(&targets, ns, |t| services.exists(t)).into_iter().collect()
}

fn problem(kind: &Kind, obj: &Value, severity: Severity, reason: String) -> Option<Problem> {
    Some(Problem {
        severity,
        kind: kind.kubectl_name(),
        label: kind.ar.kind.clone(),
        namespace: text(obj, "/metadata/namespace").map(String::from),
        name: text(obj, "/metadata/name")?.to_string(),
        reason,
        age: created(obj),
    })
}

/// How a Service with no ready endpoint stands, from its pods (listed by its selector): None
/// when they turn out ready (the Endpoints were behind).
async fn judge_unserved(client: Client, kind: &Kind, svc: &Value) -> anyhow::Result<Option<(Severity, String)>> {
    let (Some(ns), Some(selector)) = (text(svc, "/metadata/namespace"), k8s::pod_selection(kind, svc).list_selector())
    else {
        return Ok(None);
    };
    let pods = k8s::list_values(client.clone(), k8s::ar_of::<core::Pod>(), Some(ns.into()), selector).await?;
    if !live(&pods).is_empty() {
        return Ok(judge_service(&pods, None));
    }
    // Selects nothing: maybe its workload was scaled to 0 on purpose. Unreadable workloads
    // leave it a warning, as if none was.
    let (deployments, sets) = futures::join!(
        k8s::list_values(client.clone(), k8s::ar_of::<apps::Deployment>(), Some(ns.into()), None),
        k8s::list_values(client, k8s::ar_of::<apps::StatefulSet>(), Some(ns.into()), None),
    );
    let mut workloads = Vec::new();
    for (label, listed) in [("Deployment", deployments), ("StatefulSet", sets)] {
        if let Ok(listed) = listed {
            workloads.extend(listed.into_iter().map(|w| (label, w)));
        }
    }
    Ok(judge_service(&pods, scaled_to_zero(svc, &workloads).as_deref()))
}

/// Services (with a selector) without a ready endpoint: their pods say whether they select
/// none or none is ready.
async fn unserved(client: &Client, kind: &Kind, services: &[Value], namespace: Option<String>) -> Scan {
    let mut out = Scan::default();
    let selecting: Vec<&Value> =
        services.iter().filter(|s| settled(s) && k8s::pod_selection(kind, s) != Selection::Nothing).collect();
    if selecting.is_empty() {
        return out;
    }
    // Every Service has its Endpoints, where the ready addresses are listed: only those
    // without one are looked into, their pods (the truth) listed.
    let endpoints =
        Kind { ar: k8s::ar_of::<core::Endpoints>(), namespaced: true, section: "", label: "Endpoints".into() };
    let ready = match k8s::list_light(client.clone(), endpoints, namespace, true).await {
        Ok((defs, rows)) => served(&defs, &rows),
        Err(e) => {
            out.skipped.push((kind.label.clone(), e));
            return out;
        }
    };
    let key = |s: &Value| Some((text(s, "/metadata/namespace")?.to_string(), text(s, "/metadata/name")?.to_string()));
    let judged: Vec<(&Value, anyhow::Result<Option<(Severity, String)>>)> =
        futures::stream::iter(selecting.into_iter().filter(|&s| key(s).is_some_and(|k| !ready.contains(&k))))
            .map(|svc| {
                let client = client.clone();
                async move { (svc, judge_unserved(client, kind, svc).await) }
            })
            .buffer_unordered(HELD_PARALLEL)
            .collect()
            .await;
    let mut failed = None;
    for (svc, judged) in judged {
        match judged {
            Ok(Some((severity, why))) => out.problems.extend(problem(kind, svc, severity, why)),
            Ok(None) => {}
            Err(e) => failed = failed.or(Some(e)),
        }
    }
    if let Some(e) = failed {
        out.skipped.push(("pods of Services".into(), e));
    }
    out
}

/// Ingresses whose backend Services, ports or TLS Secrets are missing.
async fn ingresses(
    client: &Client,
    kind: &Kind,
    listed: &[Value],
    services: &Listed<&Value>,
    scope: Option<&str>,
) -> Scan {
    let mut out = Scan::default();
    let listed: Vec<&Value> = listed.iter().filter(|i| settled(i)).collect();
    let secrets = listed.iter().flat_map(|i| ingress_secrets(i)).collect();
    let (exists, skipped) = lookup(client, scope, secrets).await;
    out.skipped = skipped;
    for ing in listed {
        let why = judge_ingress(ing, services, |t| exists.get(t).copied());
        if !why.is_empty() {
            out.problems.extend(problem(kind, ing, Severity::Warning, why.join("; ")));
        }
    }
    out
}

/// Services that select no pod or no ready one, and Ingresses and HTTPRoutes whose Services
/// (or TLS Secrets) are missing, in `namespace` or the whole cluster. A kind the cluster
/// doesn't serve is left out, and one that can't be read is skipped alone.
pub(super) async fn traffic(client: Client, kinds: &[Kind], service: &Kind, namespace: Option<String>) -> Scan {
    let find = |(g, k): (&str, &str)| kinds.iter().find(|x| x.is(g, k));
    let (ingress, route) = (find(WITH_SERVICES[0]), find(WITH_SERVICES[1]));
    let list = |kind: Option<&Kind>| {
        let read = kind.map(|k| k8s::lists::list_lean_cached::<Value>(client.clone(), k.ar.clone(), namespace.clone()));
        async move {
            match read {
                Some(read) => Some(read.await),
                None => None,
            }
        }
    };
    let (services, ingress_list, route_list) = futures::join!(list(Some(service)), list(ingress), list(route));
    let mut out = Scan::default();
    let services = match services {
        Some(Ok(services)) => services,
        Some(Err(e)) => {
            out.skipped.push((service.label.clone(), e));
            // What points to Services can't be checked either.
            for k in [ingress, route].into_iter().flatten() {
                out.skipped.push((k.label.clone(), anyhow::anyhow!("Services could not be read")));
            }
            return out;
        }
        None => return out,
    };
    let scope = namespace.as_deref();
    let by_name = services_by_name(scope, &services);
    let ingress_scan = async {
        match (ingress, ingress_list) {
            (Some(kind), Some(Ok(listed))) => ingresses(&client, kind, &listed, &by_name, scope).await,
            (Some(kind), Some(Err(e))) => Scan { problems: Vec::new(), skipped: vec![(kind.label.clone(), e)] },
            _ => Scan::default(),
        }
    };
    let (service_scan, ingress_scan) =
        futures::join!(unserved(&client, service, &services, namespace.clone()), ingress_scan);
    out.absorb(service_scan);
    out.absorb(ingress_scan);
    match (route, route_list) {
        (Some(kind), Some(Ok(listed))) => {
            for r in listed.iter().filter(|r| settled(r)) {
                let why = judge_route(r, &by_name);
                if !why.is_empty() {
                    out.problems.extend(problem(kind, r, Severity::Warning, why.join("; ")));
                }
            }
        }
        (Some(kind), Some(Err(e))) => out.skipped.push((kind.label.clone(), e)),
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn kind(group: &str, k: &str) -> Kind {
        Kind {
            ar: kube::discovery::ApiResource {
                group: group.into(),
                version: "v1".into(),
                api_version: if group.is_empty() { "v1".into() } else { format!("{group}/v1") },
                kind: k.into(),
                plural: if k == "Ingress" { "ingresses".into() } else { format!("{}s", k.to_lowercase()) },
            },
            namespaced: true,
            section: "",
            label: format!("{k}s"),
        }
    }

    /// A fake list of objects of one kind, as (namespace, name), listed in every namespace.
    fn listed(objects: &[(&str, &str)]) -> Listed<()> {
        Listed::new(None, objects.iter().map(|(ns, name)| (ns.to_string(), name.to_string(), ())))
    }

    /// What a pod misses, given the ConfigMaps, Secrets and PVCs that exist.
    fn pod_missing(pod: &Value, config_maps: &[(&str, &str)], secrets: &[(&str, &str)]) -> Option<String> {
        let (cms, secrets, claims) = (listed(config_maps), listed(secrets), listed(&[("app", "data")]));
        missing(&pod_refs(pod), Some("app"), |t| match t.of {
            Of::ConfigMap => cms.exists(t),
            Of::Secret => secrets.exists(t),
            Of::Claim => claims.exists(t),
            _ => None,
        })
    }

    #[test]
    fn a_pod_misses_what_it_needs_in_its_own_namespace() {
        let pod = json!({
            "metadata": {"name": "web", "namespace": "app"},
            "spec": {
                "volumes": [
                    {"name": "a", "configMap": {"name": "settings"}},
                    {"name": "b", "secret": {"secretName": "tls", "optional": true}},
                    {"name": "c", "persistentVolumeClaim": {"claimName": "data"}},
                    {"name": "d", "projected": {"sources": [{"secret": {"name": "token"}}]}},
                ],
                "initContainers": [{"name": "init", "envFrom": [{"configMapRef": {"name": "init-env"}}]}],
                "containers": [{"name": "main", "env": [
                    {"name": "PASSWORD", "valueFrom": {"secretKeyRef": {"name": "db", "key": "password"}}},
                    {"name": "LEVEL", "valueFrom": {"configMapKeyRef": {"name": "flags", "key": "x", "optional": true}}},
                    {"name": "PLAIN", "value": "1"},
                ]}],
            },
        });
        // Present: nothing missing. The optional ones aren't needed.
        let all_cms = [("app", "settings"), ("app", "init-env")];
        assert_eq!(pod_missing(&pod, &all_cms, &[("app", "token"), ("app", "db")]), None);
        // Missing, and the same names in another namespace don't count.
        assert_eq!(
            pod_missing(&pod, &[("other", "settings"), ("app", "init-env")], &[("other", "db"), ("app", "token")]),
            Some("missing ConfigMap settings, Secret db".into())
        );
        // A PVC gone.
        let claim = json!({"metadata": {"namespace": "app"}, "spec": {"volumes": [{"persistentVolumeClaim": {"claimName": "logs"}}]}});
        assert_eq!(pod_missing(&claim, &[], &[]), Some("missing PersistentVolumeClaim logs".into()));
    }

    #[test]
    fn a_running_pod_needs_nothing_any_more() {
        let pod = kind("", "Pod");
        let mut obj = json!({"metadata": {"namespace": "app"}, "spec": {"volumes": [{"configMap": {"name": "x"}}]},
            "status": {"phase": "Running"}});
        assert!(refs_of(&pod, &obj).is_empty());
        obj["status"]["phase"] = json!("Pending");
        assert_eq!(refs_of(&pod, &obj).len(), 1);
    }

    #[test]
    fn a_workload_misses_its_service_account() {
        let deploy = kind("apps", "Deployment");
        let obj =
            |sa: Value| json!({"metadata": {"name": "web", "namespace": "app"}, "spec": {"template": {"spec": sa}}});
        let accounts = listed(&[("app", "builder"), ("other", "runner")]);
        let check = |o: &Value| missing(&refs_of(&deploy, o), Some("app"), |t| accounts.exists(t));
        assert_eq!(check(&obj(json!({"serviceAccountName": "builder"}))), None);
        assert_eq!(
            check(&obj(json!({"serviceAccountName": "runner"}))),
            Some("missing ServiceAccount runner".into()),
            "the one of another namespace doesn't count"
        );
        // The deprecated field, and none named (the namespace's default).
        assert_eq!(check(&obj(json!({"serviceAccount": "gone"}))), Some("missing ServiceAccount gone".into()));
        assert!(refs_of(&deploy, &obj(json!({}))).is_empty());
        // Not a kind with references.
        assert!(refs_of(&kind("batch", "CronJob"), &obj(json!({"serviceAccountName": "x"}))).is_empty());
    }

    #[test]
    fn a_pending_claim_misses_its_storage_class() {
        let pvc = kind("", "PersistentVolumeClaim");
        let obj = |class: Value, phase: &str| json!({"metadata": {"namespace": "app"}, "spec": {"storageClassName": class}, "status": {"phase": phase}});
        let classes = listed(&[("", "standard")]);
        let check = |o: &Value| missing(&refs_of(&pvc, o), Some("app"), |t| classes.exists(t));
        assert_eq!(check(&obj(json!("standard"), "Pending")), None);
        assert_eq!(check(&obj(json!("fast"), "Pending")), Some("missing StorageClass fast".into()));
        // No class at all, or the default one: nothing to miss.
        assert!(refs_of(&pvc, &obj(json!(""), "Pending")).is_empty());
        assert!(refs_of(&pvc, &obj(Value::Null, "Pending")).is_empty());
        // Bound already.
        assert!(refs_of(&pvc, &obj(json!("fast"), "Bound")).is_empty());
    }

    #[test]
    fn a_namespace_not_listed_says_nothing() {
        let one = Listed::new(Some("app"), [("app".to_string(), "a".to_string(), ())]);
        assert_eq!(one.get(Some("app"), "a"), Some(Some(&())));
        assert_eq!(one.get(Some("app"), "b"), Some(None));
        assert_eq!(one.get(Some("other"), "a"), None);
        let target = Target { of: Of::Service, namespace: Some("other".into()), name: "a".into() };
        assert_eq!(missing(&[target], Some("app"), |t| one.exists(t)), None);
    }

    fn pod(ns: &str, phase: &str, ready: bool) -> Value {
        json!({"metadata": {"namespace": ns}, "status": {"phase": phase,
            "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}]}})
    }

    #[test]
    fn a_service_without_pods_or_ready_ones() {
        assert_eq!(judge_service(&[], None), Some((Severity::Warning, "selects no pod".into())));
        assert_eq!(
            judge_service(&[], Some("Deployment web")),
            Some((Severity::Info, "selects no pod (Deployment web scaled to 0)".into()))
        );
        // Pods of finished runs don't serve.
        assert_eq!(judge_service(&[pod("app", "Succeeded", false)], None).unwrap().1, "selects no pod");
        assert_eq!(
            judge_service(&[pod("app", "Running", false), pod("app", "Pending", false)], None),
            Some((Severity::Warning, "no ready pod (0/2 ready)".into()))
        );
        assert_eq!(judge_service(&[pod("app", "Running", false), pod("app", "Running", true)], None), None);
        // Being deleted: not serving any more.
        let mut leaving = pod("app", "Running", true);
        leaving["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
        assert_eq!(judge_service(&[leaving], None).unwrap().1, "no ready pod (0/1 ready)");
    }

    #[test]
    fn a_service_s_workload_scaled_to_zero() {
        let svc = json!({"metadata": {"name": "web", "namespace": "app"}, "spec": {"selector": {"app": "web"}}});
        let workload = |ns: &str, replicas: i64, app: &str| {
            json!({"metadata": {"name": "web", "namespace": ns}, "spec": {"replicas": replicas,
                "template": {"metadata": {"labels": {"app": app, "tier": "front"}}}}})
        };
        assert_eq!(
            scaled_to_zero(&svc, &[("Deployment", workload("app", 0, "web"))]).as_deref(),
            Some("Deployment web")
        );
        // Running replicas, another app, or the same in another namespace: not it.
        assert_eq!(scaled_to_zero(&svc, &[("Deployment", workload("app", 2, "web"))]), None);
        assert_eq!(scaled_to_zero(&svc, &[("Deployment", workload("app", 0, "api"))]), None);
        assert_eq!(scaled_to_zero(&svc, &[("StatefulSet", workload("other", 0, "web"))]), None);
    }

    #[test]
    fn services_with_a_ready_endpoint() {
        let defs: Vec<ColumnDef> =
            ["Name", "Endpoints", "Age"].iter().map(|n| ColumnDef { name: n.to_string(), priority: 0 }).collect();
        let row = |ns: &str, name: &str, endpoints: &str| {
            std::sync::Arc::new(Row {
                uid: format!("{ns}/{name}"),
                name: name.into(),
                namespace: Some(ns.into()),
                created: None,
                cells: vec![name.into(), endpoints.into(), "1d".into()],
                labels: "".into(),
                deleting: false,
                deleted_at: None,
            })
        };
        let ready = served(
            &defs,
            &[
                row("app", "web", "10.0.0.1:80,10.0.0.2:80 + 1 more..."),
                row("app", "down", "<none>"),
                row("app", "headless", "10.0.0.3"),
            ],
        );
        let has = |ns: &str, svc: &str| ready.contains(&(ns.to_string(), svc.to_string()));
        assert!(has("app", "web"));
        assert!(has("app", "headless"), "no ports: the addresses alone");
        assert!(!has("app", "down"));
        assert!(!has("other", "web"), "another namespace's Service of the same name");
    }

    fn service(ns: &str, name: &str, ports: Value) -> Value {
        json!({"metadata": {"namespace": ns, "name": name}, "spec": {"ports": ports}})
    }

    #[test]
    fn an_ingress_misses_its_services_ports_and_secrets() {
        let services = [
            service("app", "web", json!([{"name": "http", "port": 80}])),
            service("other", "api", json!([{"port": 80}])),
        ];
        let by_name = services_by_name(None, &services);
        let ing = |backend: Value, tls: &str| {
            json!({"metadata": {"namespace": "app", "name": "front"}, "spec": {
                "tls": [{"hosts": ["a.example.com"], "secretName": tls}],
                "rules": [{"http": {"paths": [{"path": "/", "backend": {"service": backend}}]}}]}})
        };
        let secrets = listed(&[("app", "front-tls"), ("other", "other-tls")]);
        let check = |i: &Value| judge_ingress(i, &by_name, |t| secrets.exists(t));
        assert!(check(&ing(json!({"name": "web", "port": {"number": 80}}), "front-tls")).is_empty());
        assert!(check(&ing(json!({"name": "web", "port": {"name": "http"}}), "front-tls")).is_empty());
        assert_eq!(
            check(&ing(json!({"name": "web", "port": {"number": 8080}}), "front-tls")),
            ["Service web has no port 8080"]
        );
        assert_eq!(
            check(&ing(json!({"name": "web", "port": {"name": "grpc"}}), "front-tls")),
            ["Service web has no port named grpc"]
        );
        // The same names in another namespace don't count.
        assert_eq!(
            check(&ing(json!({"name": "api", "port": {"number": 80}}), "other-tls")),
            ["missing Service api", "missing TLS Secret other-tls"]
        );
        // Secrets that could not be read: not reported missing.
        let unknown = judge_ingress(&ing(json!({"name": "web", "port": {"number": 80}}), "x"), &by_name, |_| None);
        assert!(unknown.is_empty());
        // The default backend, and an ExternalName Service on any port.
        let external = [json!({"metadata": {"namespace": "app", "name": "out"}, "spec": {"type": "ExternalName"}})];
        let by_name = services_by_name(Some("app"), &external);
        let default = json!({"metadata": {"namespace": "app"}, "spec": {"defaultBackend": {"service": {"name": "out", "port": {"number": 443}}}}});
        assert!(judge_ingress(&default, &by_name, |_| None).is_empty());
        let default = json!({"metadata": {"namespace": "app"}, "spec": {"defaultBackend": {"service": {"name": "gone", "port": {"number": 1}}}}});
        assert_eq!(judge_ingress(&default, &by_name, |_| None), ["missing Service gone"]);
    }

    #[test]
    fn an_http_route_misses_its_services() {
        let services = [service("app", "web", json!([])), service("other", "api", json!([]))];
        let route = |refs: Value| json!({"metadata": {"namespace": "app", "name": "r"}, "spec": {"rules": [{"backendRefs": refs}]}});
        let all = services_by_name(None, &services);
        assert!(judge_route(&route(json!([{"name": "web", "port": 80}])), &all).is_empty());
        // Another namespace's Service of the same name doesn't count; one named there does.
        assert_eq!(judge_route(&route(json!([{"name": "api"}])), &all), ["missing Service api"]);
        assert!(judge_route(&route(json!([{"name": "api", "namespace": "other"}])), &all).is_empty());
        assert_eq!(
            judge_route(&route(json!([{"name": "web", "namespace": "other"}])), &all),
            ["missing Service other/web"]
        );
        // Not a Service: not checked.
        assert!(judge_route(&route(json!([{"name": "x", "group": "example.com", "kind": "Bucket"}])), &all).is_empty());
        // Scanned one namespace: a backend elsewhere can't be told.
        let one = services_by_name(Some("app"), &services[..1]);
        assert!(judge_route(&route(json!([{"name": "web", "namespace": "other"}])), &one).is_empty());
        assert_eq!(judge_route(&route(json!([{"name": "gone"}])), &one), ["missing Service gone"]);
    }

    proptest! {
        /// A pod's reference is missing exactly when it is needed somewhere (not only
        /// optional; a PVC always is) and nothing of its kind and name exists in the pod's
        /// namespace, wherever in the pod it is made.
        #[test]
        fn missing_exactly_when_nothing_answers(
            refs in prop::collection::vec((0..3usize, 0..3usize, any::<bool>(), 0..4usize), 0..8),
            existing in prop::collection::vec((0..3usize, 0..3usize, any::<bool>()), 0..10),
        ) {
            let ofs = [Of::ConfigMap, Of::Secret, Of::Claim];
            let names = ["a", "b", "c"];
            let (mut volumes, mut init, mut env) = (Vec::new(), Vec::new(), Vec::new());
            for &(of, name, optional, place) in &refs {
                let name = names[name];
                match (ofs[of], place) {
                    (Of::Claim, _) => volumes.push(json!({"persistentVolumeClaim": {"claimName": name}})),
                    (Of::ConfigMap, 0) => volumes.push(json!({"configMap": {"name": name, "optional": optional}})),
                    (Of::Secret, 0) => volumes.push(json!({"secret": {"secretName": name, "optional": optional}})),
                    (Of::ConfigMap, 1) => init.push(json!({"configMapRef": {"name": name, "optional": optional}})),
                    (Of::Secret, 1) => init.push(json!({"secretRef": {"name": name, "optional": optional}})),
                    (Of::ConfigMap, 2) => env.push(json!({"valueFrom": {"configMapKeyRef": {"name": name, "optional": optional}}})),
                    (Of::Secret, 2) => env.push(json!({"valueFrom": {"secretKeyRef": {"name": name, "optional": optional}}})),
                    (Of::ConfigMap, _) => volumes.push(json!({"projected": {"sources": [{"configMap": {"name": name, "optional": optional}}]}})),
                    (_, _) => volumes.push(json!({"projected": {"sources": [{"secret": {"name": name, "optional": optional}}]}})),
                }
            }
            let pod = json!({"metadata": {"namespace": "app"}, "spec": {"volumes": volumes,
                "initContainers": [{"envFrom": init}], "containers": [{"env": env}]}});
            let lists: Vec<Listed<()>> = ofs
                .iter()
                .map(|&of| {
                    Listed::new(
                        None,
                        existing
                            .iter()
                            .filter(|(o, _, _)| ofs[*o] == of)
                            .map(|&(_, name, home)| (if home { "app" } else { "other" }.to_string(), names[name].to_string(), ())),
                    )
                })
                .collect();
            let exists = |t: &Target| lists[ofs.iter().position(|o| *o == t.of).unwrap()].exists(t);
            let found: BTreeSet<(Of, String)> =
                pod_refs(&pod).into_iter().filter(|t| exists(t) == Some(false)).map(|t| (t.of, t.name)).collect();
            let expected: BTreeSet<(Of, String)> = refs
                .iter()
                .filter(|(of, _, optional, _)| ofs[*of] == Of::Claim || !optional)
                .map(|&(of, name, _, _)| (ofs[of], names[name].to_string()))
                .filter(|(of, name)| {
                    let at = ofs.iter().position(|o| o == of).unwrap();
                    !existing.iter().any(|&(o, n, home)| o == at && names[n] == name && home)
                })
                .collect();
            prop_assert_eq!(found, expected);
        }
    }

    /// A cluster with Services, their Endpoints and pods, Ingresses and HTTPRoutes in
    /// namespace `app`; records the paths it was asked for.
    #[derive(Clone, Default)]
    struct Cluster(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

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
            self.0.lock().unwrap().push(path.clone());
            let old = "2026-01-01T00:00:00Z";
            let meta = |name: &str| json!({"name": name, "namespace": "app", "creationTimestamp": old});
            let list = |items: Vec<Value>| json!({"kind": "List", "apiVersion": "v1", "metadata": {}, "items": items});
            let svc = |name: &str, app: &str| json!({"metadata": meta(name), "spec": {"selector": {"app": app}, "ports": [{"name": "http", "port": 80}]}});
            let body = match path.as_str() {
                "/api/v1/namespaces/app/services" => list(vec![
                    svc("web", "web"),
                    svc("idle", "idle"),
                    svc("down", "down"),
                    svc("orphan", "orphan"),
                    json!({"metadata": meta("external"), "spec": {"type": "ExternalName", "externalName": "x.example.com"}}),
                ]),
                "/api/v1/namespaces/app/endpoints" => {
                    let row = |name: &str, endpoints: &str| json!({"cells": [name, endpoints, "1d"], "object": {"metadata": {"name": name, "namespace": "app", "uid": name}}});
                    json!({"kind": "Table", "columnDefinitions": [{"name": "Name"}, {"name": "Endpoints"}, {"name": "Age"}],
                        "rows": [row("web", "10.0.0.1:80"), row("down", "<none>"), row("idle", "<none>")]})
                }
                "/api/v1/namespaces/app/pods" if query.contains("down") => list(vec![
                    json!({"metadata": meta("down-1"), "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "False"}]}}),
                ]),
                "/api/v1/namespaces/app/pods" => list(vec![]),
                "/apis/apps/v1/namespaces/app/deployments" => list(vec![json!({"metadata": meta("idle"),
                    "spec": {"replicas": 0, "template": {"metadata": {"labels": {"app": "idle"}}}}})]),
                "/apis/apps/v1/namespaces/app/statefulsets" => list(vec![]),
                "/apis/networking.k8s.io/v1/namespaces/app/ingresses" => {
                    list(vec![json!({"metadata": meta("front"), "spec": {
                    "tls": [{"secretName": "front-tls"}, {"secretName": "gone-tls"}],
                    "rules": [{"http": {"paths": [
                        {"backend": {"service": {"name": "web", "port": {"name": "http"}}}},
                        {"backend": {"service": {"name": "nothing", "port": {"number": 80}}}},
                    ]}}]}})])
                }
                "/api/v1/namespaces/app/secrets/front-tls" => {
                    json!({"apiVersion": "meta.k8s.io/v1", "kind": "PartialObjectMetadata", "metadata": meta("front-tls")})
                }
                "/apis/gateway.networking.k8s.io/v1/namespaces/app/httproutes" => {
                    list(vec![json!({"metadata": meta("route"),
                    "spec": {"rules": [{"backendRefs": [{"name": "web", "port": 80}, {"name": "gone", "port": 80}]}]}})])
                }
                _ => {
                    json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "code": 404, "reason": "NotFound"})
                }
            };
            let code = if body["kind"] == "Status" { 404 } else { 200 };
            let response = http::Response::builder().status(code).body(body.to_string().into_bytes().into()).unwrap();
            std::future::ready(Ok(response))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn finds_broken_traffic() {
        let cluster = Cluster::default();
        let client = Client::new(cluster.clone(), "default");
        let (service, ingress) = (kind("", "Service"), kind("networking.k8s.io", "Ingress"));
        let route = kind(GATEWAY, "HTTPRoute");
        let kinds = [service.clone(), ingress, route];
        let scan = traffic(client, &kinds, &service, Some("app".into())).await;
        assert!(scan.skipped.is_empty(), "{:?}", scan.skipped_text());
        let mut found: Vec<(String, String, Severity, String)> =
            scan.problems.into_iter().map(|p| (p.label, p.name, p.severity, p.reason)).collect();
        found.sort();
        let row =
            |label: &str, name: &str, severity, reason: &str| (label.into(), name.into(), severity, reason.into());
        assert_eq!(
            found,
            [
                row("HTTPRoute", "route", Severity::Warning, "missing Service gone"),
                row("Ingress", "front", Severity::Warning, "missing Service nothing; missing TLS Secret gone-tls"),
                row("Service", "down", Severity::Warning, "no ready pod (0/1 ready)"),
                row("Service", "idle", Severity::Info, "selects no pod (Deployment idle scaled to 0)"),
                row("Service", "orphan", Severity::Warning, "selects no pod"),
            ]
        );
        // The Services are listed once for the three kinds; the one served isn't looked into.
        let asked = cluster.0.lock().unwrap().clone();
        assert_eq!(asked.iter().filter(|p| p.ends_with("/services")).count(), 1, "{asked:?}");
        assert_eq!(asked.iter().filter(|p| p.ends_with("/pods")).count(), 3, "{asked:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cluster_without_gateway_api_still_checks_the_rest() {
        let client = Client::new(Cluster::default(), "default");
        let service = kind("", "Service");
        let scan =
            traffic(client, &[service.clone(), kind("networking.k8s.io", "Ingress")], &service, Some("app".into()))
                .await;
        assert!(scan.skipped.is_empty(), "{:?}", scan.skipped_text());
        assert!(scan.problems.iter().all(|p| p.label != "HTTPRoute"));
        assert_eq!(scan.problems.len(), 4);
    }
}
