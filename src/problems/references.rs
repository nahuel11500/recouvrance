//! References to objects that don't exist. They break quietly: the pod waits on a missing
//! ConfigMap, the Ingress answers 503, the Service has no endpoints. The Problems scan names
//! what is missing on the object that points to it.
//!
//! Pods, workloads and PVCs are read only when their row is already a problem (a pod stuck
//! creating, a Deployment with no ready replica, a PVC pending), and the reason says which
//! reference is missing. Services, Ingresses and HTTP routes are checked from their lists,
//! with the Services listed once for the three. Those lists, and the pods and workloads read
//! for a Service, are parsed lean: only the fields the checks read are kept.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use k8s_openapi::api::{apps::v1 as apps, core::v1 as core, storage::v1 as storage};
use kube::Client;
use kube::api::{Api, DynamicObject};
use kube::discovery::ApiResource;
use serde::Deserialize;
use serde::de::{DeserializeOwned, IgnoredAny};
use serde_json::Value;

use super::{HELD_PARALLEL, Problem, Scan, Severity, cell, since};
use crate::json::nonempty_at as text;
use crate::k8s::{self, ColumnDef, Kind, Row, Selection};

/// The Gateway API's group, for HTTP routes.
pub(crate) const GATEWAY: &str = "gateway.networking.k8s.io";

/// Kinds checked with the Services they point to (`traffic`), not on their own.
pub(crate) const WITH_SERVICES: [(&str, &str); 2] = [("networking.k8s.io", "Ingress"), (GATEWAY, "HTTPRoute")];

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
    /// The kind's name: `ConfigMap`, `PersistentVolumeClaim`.
    pub fn kind(self) -> &'static str {
        match self {
            Self::ConfigMap => "ConfigMap",
            Self::Secret => "Secret",
            Self::Claim => "PersistentVolumeClaim",
            Self::StorageClass => "StorageClass",
            Self::ServiceAccount => "ServiceAccount",
            Self::Service => "Service",
        }
    }

    fn plural(self) -> &'static str {
        match self {
            Self::ConfigMap => "ConfigMaps",
            Self::Secret => "Secrets",
            Self::Claim => "PersistentVolumeClaims",
            Self::StorageClass => "StorageClasses",
            Self::ServiceAccount => "ServiceAccounts",
            Self::Service => "Services",
        }
    }

    fn ar(self) -> ApiResource {
        match self {
            Self::ConfigMap => k8s::ar_of::<core::ConfigMap>(),
            Self::Secret => k8s::ar_of::<core::Secret>(),
            Self::Claim => k8s::ar_of::<core::PersistentVolumeClaim>(),
            Self::StorageClass => k8s::ar_of::<storage::StorageClass>(),
            Self::ServiceAccount => k8s::ar_of::<core::ServiceAccount>(),
            Self::Service => k8s::ar_of::<core::Service>(),
        }
    }

    fn namespaced(self) -> bool {
        self != Self::StorageClass
    }

    /// The kind's API group: `storage.k8s.io` for a StorageClass, the core group (`""`) else.
    pub fn group(self) -> &'static str {
        if self == Self::StorageClass { "storage.k8s.io" } else { "" }
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

fn items(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

/// What a pod needs to start: the ConfigMaps, Secrets and PVCs of its volumes (projected ones
/// too) and of its containers' and init containers' environment. A reference marked
/// `optional: true` isn't needed.
pub fn pod_refs(pod: &Value) -> Vec<Target> {
    spec_refs(text(pod, "/metadata/namespace"), &pod["spec"], false)
}

/// The objects a pod spec (a pod's, or a workload's template) names in `namespace`: the
/// ConfigMaps, Secrets and PVCs of its volumes (projected ones too) and of its containers' and
/// init containers' environment. Without `all`, what a pod needs to start (`pod_refs`): the
/// references marked `optional: true` are left out. With `all`, every object it names: the
/// optional ones too, its image pull Secrets and its ServiceAccount (what the Map shows), but
/// not the `kube-api-access-*` volume the API server adds to every pod (its token and the
/// cluster's `kube-root-ca.crt`): nothing the manifest asked for.
pub fn spec_refs(namespace: Option<&str>, spec: &Value, all: bool) -> Vec<Target> {
    let mut out = BTreeSet::new();
    let mut add = |of: Of, r: &Value, key: &str| {
        let name = r.get(key).and_then(Value::as_str).filter(|n| !n.is_empty());
        if let Some(name) = name
            && (all || r.get("optional").and_then(Value::as_bool) != Some(true))
        {
            out.insert(Target { of, namespace: namespace.map(String::from), name: name.to_string() });
        }
    };
    for v in items(&spec["volumes"]) {
        if all && v["name"].as_str().is_some_and(|n| n.starts_with("kube-api-access-")) {
            continue;
        }
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
    if all {
        for p in items(&spec["imagePullSecrets"]) {
            add(Of::Secret, p, "name");
        }
        // `serviceAccount` is the deprecated spelling, still written by some manifests.
        let account = ["serviceAccountName", "serviceAccount"]
            .into_iter()
            .find(|k| spec.get(*k).and_then(Value::as_str).is_some_and(|n| !n.is_empty()));
        if let Some(key) = account {
            add(Of::ServiceAccount, spec, key);
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
            match k8s::list_light(client.clone(), kind, ns.clone(), true, None).await {
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
/// doesn't exist to their reasons (`CreateContainerConfigError: missing ConfigMap app`).
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
    // Each object gives way to its references as soon as it is read: whole, they would all be
    // held at once.
    let read: Vec<(usize, anyhow::Result<Option<Vec<Target>>>)> = futures::stream::iter(which.iter().copied())
        .map(|i| {
            let (client, kind, ns, name) =
                (client.clone(), kind.clone(), found[i].namespace.clone(), found[i].name.clone());
            async move {
                let ar = kind.ar.clone();
                let obj = k8s::run(async move {
                    let api: Api<DynamicObject> = match &ns {
                        Some(ns) => Api::namespaced_with(client, ns, &ar),
                        None => Api::all_with(client, &ar),
                    };
                    Ok(api.get_opt(&name).await?.map(serde_json::to_value).transpose()?)
                })
                .await;
                (i, obj.map(|obj| obj.map(|obj| refs_of(&kind, &obj))))
            }
        })
        .buffer_unordered(HELD_PARALLEL)
        .collect()
        .await;
    let mut skipped = Vec::new();
    let mut refs = Vec::new();
    for (i, targets) in read {
        match targets {
            Ok(Some(targets)) => refs.push((i, targets)),
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

// MARK: Services, Ingresses and HTTP routes

/// What the traffic checks read of an object's metadata. Like the types below, it is parsed as
/// its list streams in, and what it doesn't name (managedFields, annotations, status) is
/// skipped while parsing: a Service or Deployment held whole as JSON takes ten times its size.
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Meta {
    name: Option<String>,
    namespace: Option<String>,
    creation_timestamp: Option<String>,
    /// Only whether it is set.
    deletion_timestamp: Option<IgnoredAny>,
    /// With the resourceVersion, what a Service's verdict is kept by (`VERDICTS`).
    uid: Option<String>,
    resource_version: Option<String>,
}

impl Meta {
    fn name(&self) -> Option<&str> {
        self.name.as_deref().filter(|n| !n.is_empty())
    }

    fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref().filter(|n| !n.is_empty())
    }

    fn created(&self) -> Option<jiff::Timestamp> {
        self.creation_timestamp.as_deref()?.parse().ok()
    }

    /// Not being deleted, and older than `SETTLE` (an unknown age counts as old).
    fn settled(&self) -> bool {
        self.deletion_timestamp.is_none() && since(self.created(), SETTLE)
    }

    /// Its (namespace, name), `""` for what it lacks.
    fn key(&self) -> (String, String) {
        (self.namespace().unwrap_or_default().to_string(), self.name().unwrap_or_default().to_string())
    }

    /// Its (uid, resourceVersion), when it has both.
    fn version(&self) -> Option<(&str, &str)> {
        let uid = self.uid.as_deref().filter(|s| !s.is_empty())?;
        let version = self.resource_version.as_deref().filter(|s| !s.is_empty())?;
        Some((uid, version))
    }
}

/// A Service: the pods it selects, and its ports.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Service {
    metadata: Meta,
    spec: ServiceSpec,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ServiceSpec {
    #[serde(rename = "type")]
    kind: Option<String>,
    /// Its labels as written, for `k8s::service_selection`.
    selector: Option<Value>,
    #[serde(deserialize_with = "k8s::null_default")]
    ports: Vec<ServicePort>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ServicePort {
    name: Option<String>,
    port: Option<i64>,
}

impl Service {
    fn selection(&self) -> Selection {
        k8s::service_selection(self.spec.selector.as_ref())
    }
}

/// A pod a Service selects: whether it ran to completion, is ready, is being deleted.
#[derive(Deserialize, Default)]
#[serde(default)]
struct SelectedPod {
    metadata: Meta,
    status: PodStatus,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PodStatus {
    phase: Option<String>,
    #[serde(deserialize_with = "k8s::null_default")]
    conditions: Vec<Condition>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Condition {
    #[serde(rename = "type")]
    kind: Option<String>,
    status: Option<String>,
}

impl SelectedPod {
    /// It may serve: it didn't run to completion.
    fn live(&self) -> bool {
        !matches!(self.status.phase.as_deref(), Some("Succeeded" | "Failed"))
    }

    /// Ready (its `Ready` condition `True`, as `k8s::pod_ready` reads it), and not being deleted.
    fn ready(&self) -> bool {
        let ready = |c: &Condition| c.kind.as_deref() == Some("Ready") && c.status.as_deref() == Some("True");
        self.metadata.deletion_timestamp.is_none() && self.status.conditions.iter().any(ready)
    }
}

/// A Deployment or StatefulSet: its replicas, and the labels of the pods it runs.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Workload {
    metadata: Meta,
    spec: WorkloadSpec,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct WorkloadSpec {
    replicas: Option<i64>,
    template: PodTemplate,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PodTemplate {
    metadata: TemplateMeta,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TemplateMeta {
    labels: Option<serde_json::Map<String, Value>>,
}

/// A workload scaled to 0, with the labels of the pods it would run.
struct Scaled {
    kind: &'static str,
    namespace: Option<String>,
    name: String,
    labels: serde_json::Map<String, Value>,
}

impl Workload {
    /// It, of `kind`, when scaled to 0 with labels for its pods; None otherwise: no Service
    /// can be idle because of it.
    fn scaled(self, kind: &'static str) -> Option<Scaled> {
        if self.spec.replicas != Some(0) {
            return None;
        }
        Some(Scaled {
            kind,
            namespace: self.metadata.namespace().map(String::from),
            name: self.metadata.name().unwrap_or_default().to_string(),
            labels: self.spec.template.metadata.labels?,
        })
    }
}

/// An Ingress: its backends' Services, and its TLS Secrets.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Ingress {
    metadata: Meta,
    spec: IngressSpec,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct IngressSpec {
    default_backend: Option<Backend>,
    #[serde(deserialize_with = "k8s::null_default")]
    rules: Vec<IngressRule>,
    #[serde(deserialize_with = "k8s::null_default")]
    tls: Vec<IngressTls>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct IngressRule {
    http: Option<IngressHttp>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct IngressHttp {
    #[serde(deserialize_with = "k8s::null_default")]
    paths: Vec<IngressPath>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct IngressPath {
    backend: Backend,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Backend {
    service: Option<ServiceBackend>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ServiceBackend {
    name: Option<String>,
    port: Option<BackendPort>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct BackendPort {
    number: Option<i64>,
    name: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct IngressTls {
    secret_name: Option<String>,
}

impl Ingress {
    /// The Services its backends send to, the default backend's first.
    fn backends(&self) -> impl Iterator<Item = &ServiceBackend> {
        let paths = self.spec.rules.iter().filter_map(|r| r.http.as_ref()).flat_map(|h| &h.paths);
        self.spec.default_backend.iter().chain(paths.map(|p| &p.backend)).filter_map(|b| b.service.as_ref())
    }
}

/// An HTTPRoute: the backends of its rules.
#[derive(Deserialize, Default)]
#[serde(default)]
struct Route {
    metadata: Meta,
    spec: RouteSpec,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RouteSpec {
    #[serde(deserialize_with = "k8s::null_default")]
    rules: Vec<RouteRule>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct RouteRule {
    #[serde(deserialize_with = "k8s::null_default")]
    backend_refs: Vec<BackendRef>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct BackendRef {
    group: Option<String>,
    kind: Option<String>,
    name: Option<String>,
    namespace: Option<String>,
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

/// A Service whose pods are `selected` (listed by its selector): selecting none is a warning,
/// minor when `scaled` names its workload scaled to 0 on purpose; none of them ready, a
/// warning. Pods that ran to completion don't count.
fn judge_service(selected: &[SelectedPod], scaled: Option<&str>) -> Option<(Severity, String)> {
    let live = selected.iter().filter(|p| p.live()).count();
    if live == 0 {
        return Some(match scaled {
            Some(w) => (Severity::Info, format!("selects no pod ({w} scaled to 0)")),
            None => (Severity::Warning, "selects no pod".into()),
        });
    }
    let ready = selected.iter().filter(|p| p.live() && p.ready()).count();
    (ready == 0).then(|| (Severity::Warning, format!("no ready pod (0/{live} ready)")))
}

/// The workload scaled to 0 whose pods a Service would select, as "Deployment web".
fn scaled_to_zero(service: &Service, scaled: &[Scaled]) -> Option<String> {
    let ns = service.metadata.namespace();
    let selector = service.spec.selector.as_ref()?;
    let selector = serde_json::json!({ "matchLabels": selector });
    scaled
        .iter()
        .find(|w| w.namespace.as_deref() == ns && k8s::selector_matches(&selector, &w.labels))
        .map(|w| format!("{} {}", w.kind, w.name))
}

/// What an Ingress sends to and names: its backends' Services (the default backend's first,
/// each once) and its TLS Secrets.
pub fn ingress_refs(obj: &Value) -> (Vec<Target>, Vec<Target>) {
    let Ok(ing) = Ingress::deserialize(obj) else { return Default::default() };
    let ns = ing.metadata.namespace().map(String::from);
    let mut services: Vec<Target> = Vec::new();
    for name in ing.backends().filter_map(|b| b.name.as_deref().filter(|n| !n.is_empty())) {
        let t = Target { of: Of::Service, namespace: ns.clone(), name: name.to_string() };
        if !services.contains(&t) {
            services.push(t);
        }
    }
    (services, ingress_secrets(&ing))
}

/// The Services an HTTPRoute sends to (other kinds of backend aren't Services), in their
/// namespace: the route's own unless the backend names another.
pub fn route_refs(obj: &Value) -> Vec<Target> {
    Route::deserialize(obj).map_or_default(|r| route_services(&r))
}

fn route_services(route: &Route) -> Vec<Target> {
    let ns = route.metadata.namespace();
    let mut targets = BTreeSet::new();
    for b in route.spec.rules.iter().flat_map(|r| &r.backend_refs) {
        let core = b.group.as_deref().unwrap_or_default().is_empty();
        let service = b.kind.as_deref().unwrap_or("Service") == "Service";
        let Some(name) = b.name.as_deref().filter(|n| !n.is_empty()) else { continue };
        if core && service {
            let namespace = b.namespace.as_deref().filter(|n| !n.is_empty()).or(ns).map(String::from);
            targets.insert(Target { of: Of::Service, namespace, name: name.to_string() });
        }
    }
    targets.into_iter().collect()
}

/// The TLS Secrets an Ingress names.
fn ingress_secrets(ing: &Ingress) -> Vec<Target> {
    let ns = ing.metadata.namespace().map(String::from);
    let names: BTreeSet<&str> =
        ing.spec.tls.iter().filter_map(|t| t.secret_name.as_deref().filter(|n| !n.is_empty())).collect();
    names.into_iter().map(|name| Target { of: Of::Secret, namespace: ns.clone(), name: name.to_string() }).collect()
}

/// The port an Ingress backend asks for when the Service doesn't have it: "8080", "named
/// http". An `ExternalName` Service forwards elsewhere, on any port.
fn port_missing(service: &Service, port: Option<&BackendPort>) -> Option<String> {
    if service.spec.kind.as_deref() == Some("ExternalName") {
        return None;
    }
    let (port, mut ports) = (port?, service.spec.ports.iter());
    if let Some(n) = port.number {
        return (!ports.any(|p| p.port == Some(n))).then(|| n.to_string());
    }
    let name = port.name.as_deref().filter(|n| !n.is_empty())?;
    (!ports.any(|p| p.name.as_deref() == Some(name))).then(|| format!("named {name}"))
}

/// What an Ingress points to that is missing: its backend Services or their ports, and its
/// TLS Secrets (`secret`: whether one exists, None when that could not be read).
fn judge_ingress(ing: &Ingress, services: &Listed<&Service>, secret: impl Fn(&Target) -> Option<bool>) -> Vec<String> {
    let ns = ing.metadata.namespace();
    let mut out = Vec::new();
    for b in ing.backends() {
        let Some(name) = b.name.as_deref().filter(|n| !n.is_empty()) else { continue };
        let why = match services.get(ns, name) {
            None => continue,
            Some(None) => format!("missing Service {name}"),
            Some(Some(s)) => match port_missing(s, b.port.as_ref()) {
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
fn judge_route(route: &Route, services: &Listed<&Service>) -> Vec<String> {
    let targets = route_services(route);
    missing(&targets, route.metadata.namespace(), |t| services.exists(t)).into_iter().collect()
}

/// Services by namespace and name.
fn services_by_name<'a>(scope: Option<&str>, services: &'a [Service]) -> Listed<&'a Service> {
    Listed::new(
        scope,
        services.iter().filter_map(|s| Some((s.metadata.namespace()?.to_string(), s.metadata.name()?.to_string(), s))),
    )
}

/// Every object of `kind` in `namespace` (all when None), parsed lean as `T`, or None when the
/// cluster doesn't serve it.
async fn list_all<T>(client: Client, kind: Option<Kind>, namespace: Option<String>) -> Option<anyhow::Result<Vec<T>>>
where
    T: DeserializeOwned + Send + 'static,
{
    let kind = kind?;
    Some(k8s::lists::list_lean_cached::<T>(client, kind.ar, namespace).await)
}

fn problem(kind: &Kind, meta: &Meta, severity: Severity, reason: String) -> Option<Problem> {
    Some(Problem {
        severity,
        kind: kind.kubectl_name(),
        label: kind.ar.kind.clone(),
        namespace: meta.namespace().map(String::from),
        name: meta.name()?.to_string(),
        reason,
        age: meta.created(),
        on_hub: None,
    })
}

/// A namespace's Deployments and StatefulSets scaled to 0, to tell a Service whose workload was
/// scaled to 0 on purpose; the others are dropped as each list is read. Unreadable ones are
/// left out: the Service stays a warning, as if none was.
async fn workloads_of(client: Client, ns: &str) -> Vec<Scaled> {
    let list = |ar| k8s::lists::list_lean_cached::<Workload>(client.clone(), ar, Some(ns.into()));
    let (deployments, sets) =
        futures::join!(list(k8s::ar_of::<apps::Deployment>()), list(k8s::ar_of::<apps::StatefulSet>()));
    let mut scaled = Vec::new();
    for (kind, listed) in [("Deployment", deployments), ("StatefulSet", sets)] {
        if let Ok(listed) = listed {
            scaled.extend(listed.into_iter().filter_map(|w| w.scaled(kind)));
        }
    }
    scaled
}

/// Services (with a selector) without a ready endpoint: their pods say whether they select
/// none or none is ready. One pod list a Service (by its selector), and the workloads of a
/// namespace listed once, only where a Service selects no pod. A Service unchanged since it
/// was judged in the last `VERDICT_KEPT` keeps its verdict, and nothing is listed for it,
/// unless it was judged before `judged_since`: Refresh, or an agent checking a fix with
/// `max_age_seconds: 0`, judges it again (pods fixed through their Deployment leave the
/// Service unchanged).
async fn unserved(
    client: &Client,
    kind: &Kind,
    services: &[Service],
    namespace: Option<String>,
    judged_since: Option<Instant>,
) -> Scan {
    let mut out = Scan::default();
    // By their index in `services`, with the label selector their pods are listed by.
    let selecting: Vec<(usize, Option<String>)> = services
        .iter()
        .enumerate()
        .filter(|(_, s)| s.metadata.settled())
        .filter_map(|(i, s)| Some((i, s.selection().list_selector()?)))
        .collect();
    if selecting.is_empty() {
        return out;
    }
    // Every Service has its Endpoints, where the ready addresses are listed: only those
    // without one are looked into, their pods (the truth) listed.
    let endpoints =
        Kind { ar: k8s::ar_of::<core::Endpoints>(), namespaced: true, section: "", label: "Endpoints".into() };
    let ready = match k8s::list_light(client.clone(), endpoints, namespace, true, None).await {
        Ok((defs, rows)) => served(&defs, &rows),
        Err(e) => {
            out.skipped.push((kind.label.clone(), e));
            return out;
        }
    };
    // Collected first, and each future owns what it reads: closures over references held
    // across an await would make the scan's future not `Send` for the agents' server.
    let candidates: Vec<(usize, String, Option<String>)> = selecting
        .into_iter()
        .filter(|(i, _)| !ready.contains(&services[*i].metadata.key()))
        .filter_map(|(i, selector)| Some((i, services[i].metadata.namespace()?.to_string(), selector)))
        .collect();
    // A Service judged lately and unchanged since keeps its verdict: on a big cluster most of
    // them are the same idle Services scan after scan, a pod list each.
    let now = Instant::now();
    let mut known: Vec<(usize, Option<(Severity, String)>)> = Vec::new();
    let mut unready: Vec<(usize, String, Option<String>)> = Vec::new();
    // Kept as selecting no pod because a workload is scaled to 0: it stands only while one
    // still is, as the namespace's workloads say (one list a namespace, not one a Service).
    let mut scaled_kept: Vec<(usize, String, Option<String>)> = Vec::new();
    {
        let kept = verdicts();
        for (i, ns, selector) in candidates {
            let reused = services[i].metadata.version().and_then(|(uid, version)| {
                kept.get(uid)
                    .filter(|k| reusable(k, version, now) && judged_since.is_none_or(|since| k.at >= since))
                    .map(|k| k.verdict.clone())
            });
            match reused {
                Some((Severity::Info, _)) => scaled_kept.push((i, ns, selector)),
                Some(verdict) => known.push((i, Some(verdict))),
                None => unready.push((i, ns, selector)),
            }
        }
    }
    let mut workloads = workloads_in(client, scaled_kept.iter().map(|(_, ns, _)| ns.clone()).collect()).await;
    for (i, ns, selector) in scaled_kept {
        match workloads.get(&ns).and_then(|w| scaled_to_zero(&services[i], w)) {
            Some(w) => known.push((i, judge_service(&[], Some(w.as_str())))),
            None => unready.push((i, ns, selector)),
        }
    }
    let listed: Vec<(usize, anyhow::Result<Vec<SelectedPod>>)> = futures::stream::iter(unready)
        .map(|(i, ns, selector)| {
            let client = client.clone();
            async move {
                let pod = k8s::ar_of::<core::Pod>();
                let pods = k8s::lists::list_lean_cached_by::<SelectedPod>(client, pod, Some(ns), selector).await;
                (i, pods)
            }
        })
        .buffer_unordered(HELD_PARALLEL)
        .collect()
        .await;
    let idle: BTreeSet<String> = listed
        .iter()
        .filter(|(_, pods)| pods.as_ref().is_ok_and(|p| !p.iter().any(SelectedPod::live)))
        .map(|(i, _)| services[*i].metadata.key().0)
        .filter(|ns| !workloads.contains_key(ns))
        .collect();
    workloads.extend(workloads_in(client, idle).await);
    let mut failed = None;
    let mut judged = Vec::new();
    for (i, pods) in listed {
        let pods = match pods {
            Ok(pods) => pods,
            Err(e) => {
                failed = failed.or(Some(e));
                continue;
            }
        };
        let svc = &services[i];
        let scaled = workloads.get(&svc.metadata.key().0).and_then(|w| scaled_to_zero(svc, w));
        let verdict = judge_service(&pods, scaled.as_deref());
        // "A ready pod after all" is not kept: the Endpoints lagged, and the pods may go
        // unready by the next scan.
        if let (Some((uid, version)), Some(kept)) = (svc.metadata.version(), &verdict) {
            judged.push((uid.to_string(), Verdict { version: version.to_string(), at: now, verdict: kept.clone() }));
        }
        known.push((i, verdict));
    }
    for (i, verdict) in known {
        if let Some((severity, why)) = verdict {
            out.problems.extend(problem(kind, &services[i].metadata, severity, why));
        }
    }
    {
        let mut kept = verdicts();
        kept.retain(|_, k| now.saturating_duration_since(k.at) < VERDICT_KEPT);
        kept.extend(judged);
    }
    if let Some(e) = failed {
        out.skipped.push(("pods of Services".into(), e));
    }
    out
}

/// The workloads scaled to 0 in each of `namespaces`.
async fn workloads_in(client: &Client, namespaces: BTreeSet<String>) -> HashMap<String, Vec<Scaled>> {
    futures::stream::iter(namespaces)
        .map(|ns| {
            let client = client.clone();
            async move {
                let scaled = workloads_of(client, &ns).await;
                (ns, scaled)
            }
        })
        .buffer_unordered(HELD_PARALLEL)
        .collect()
        .await
}

/// A Service's verdict on its pods, as judged at `at` for its resourceVersion `version`.
/// Only a problem is kept: a Service with a ready pod is judged again each time.
struct Verdict {
    version: String,
    at: Instant,
    verdict: (Severity, String),
}

/// How long a Service's verdict is kept at most: the pods it selects can change without the
/// Service. One that selects no pod because a workload is scaled to 0 keeps it only while
/// that workload still is.
const VERDICT_KEPT: Duration = Duration::from_secs(10 * 60);

/// The verdicts of the Services judged lately, by uid (unique across clusters too).
static VERDICTS: LazyLock<Mutex<HashMap<String, Verdict>>> = LazyLock::new(Default::default);

fn verdicts() -> MutexGuard<'static, HashMap<String, Verdict>> {
    VERDICTS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Whether a verdict `kept` stands at `now` for a Service now at resourceVersion `version`:
/// the Service hasn't changed, and it was judged less than `VERDICT_KEPT` ago.
fn reusable(kept: &Verdict, version: &str, now: Instant) -> bool {
    kept.version == version && now.saturating_duration_since(kept.at) < VERDICT_KEPT
}

/// Ingresses whose backend Services, ports or TLS Secrets are missing.
async fn ingresses(
    client: &Client,
    kind: &Kind,
    listed: &[Ingress],
    services: &Listed<&Service>,
    scope: Option<&str>,
) -> Scan {
    let mut out = Scan::default();
    let listed: Vec<&Ingress> = listed.iter().filter(|i| i.metadata.settled()).collect();
    let secrets = listed.iter().flat_map(|i| ingress_secrets(i)).collect();
    let (exists, skipped) = lookup(client, scope, secrets).await;
    out.skipped = skipped;
    for ing in listed {
        let why = judge_ingress(ing, services, |t| exists.get(t).copied());
        if !why.is_empty() {
            out.problems.extend(problem(kind, &ing.metadata, Severity::Warning, why.join("; ")));
        }
    }
    out
}

/// The Ingress and HTTPRoute kinds among `kinds`, those the cluster serves.
pub(super) fn with_services(kinds: &[Kind]) -> (Option<Kind>, Option<Kind>) {
    let [(ig, ik), (rg, rk)] = WITH_SERVICES;
    (kinds.iter().find(|x| x.is(ig, ik)).cloned(), kinds.iter().find(|x| x.is(rg, rk)).cloned())
}

/// Services that select no pod or no ready one, and Ingresses and HTTP routes whose Services
/// (or TLS Secrets) are missing, in `namespace` or the whole cluster. A kind the cluster
/// doesn't serve (`ingress`, `route` None) is left out, and one that can't be read is
/// skipped alone. `judged_since`: a Service judged before keeps no verdict (see `unserved`).
pub(super) async fn traffic(
    client: Client,
    service: &Kind,
    ingress: Option<&Kind>,
    route: Option<&Kind>,
    namespace: Option<String>,
    judged_since: Option<Instant>,
) -> Scan {
    let (services, ingress_list, route_list) = futures::join!(
        list_all::<Service>(client.clone(), Some(service.clone()), namespace.clone()),
        list_all::<Ingress>(client.clone(), ingress.cloned(), namespace.clone()),
        list_all::<Route>(client.clone(), route.cloned(), namespace.clone()),
    );
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
        futures::join!(unserved(&client, service, &services, namespace.clone(), judged_since), ingress_scan);
    out.absorb(service_scan);
    out.absorb(ingress_scan);
    match (route, route_list) {
        (Some(kind), Some(Ok(listed))) => {
            for r in listed.iter().filter(|r| r.metadata.settled()) {
                let why = judge_route(r, &by_name);
                if !why.is_empty() {
                    out.problems.extend(problem(kind, &r.metadata, Severity::Warning, why.join("; ")));
                }
            }
        }
        (Some(kind), Some(Err(e))) => out.skipped.push((kind.label.clone(), e)),
        _ => {}
    }
    out
}

#[cfg(test)]
mod same_findings;

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn kind(group: &str, k: &str) -> Kind {
        Kind { label: format!("{k}s"), ..Kind::fake(group, k) }
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

    /// An object as its list is read: only what the checks use is kept.
    fn lean<T: DeserializeOwned>(obj: Value) -> T {
        serde_json::from_value(obj).unwrap()
    }

    fn pod(ns: &str, phase: &str, ready: bool) -> Value {
        json!({"metadata": {"namespace": ns}, "status": {"phase": phase,
            "conditions": [{"type": "Ready", "status": if ready { "True" } else { "False" }}]}})
    }

    #[test]
    fn a_service_without_pods_or_ready_ones() {
        let judge = |pods: Vec<Value>, scaled: Option<&str>| {
            judge_service(&pods.into_iter().map(lean::<SelectedPod>).collect::<Vec<_>>(), scaled)
        };
        assert_eq!(judge(vec![], None), Some((Severity::Warning, "selects no pod".into())));
        assert_eq!(
            judge(vec![], Some("Deployment web")),
            Some((Severity::Info, "selects no pod (Deployment web scaled to 0)".into()))
        );
        // Pods of finished runs don't serve.
        assert_eq!(judge(vec![pod("app", "Succeeded", false)], None).unwrap().1, "selects no pod");
        assert_eq!(
            judge(vec![pod("app", "Running", false), pod("app", "Pending", false)], None),
            Some((Severity::Warning, "no ready pod (0/2 ready)".into()))
        );
        assert_eq!(judge(vec![pod("app", "Running", false), pod("app", "Running", true)], None), None);
        // Being deleted: not serving any more.
        let mut leaving = pod("app", "Running", true);
        leaving["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
        assert_eq!(judge(vec![leaving], None).unwrap().1, "no ready pod (0/1 ready)");
    }

    #[test]
    fn a_service_s_workload_scaled_to_zero() {
        let svc: Service =
            lean(json!({"metadata": {"name": "web", "namespace": "app"}, "spec": {"selector": {"app": "web"}}}));
        let workload = |kind: &'static str, ns: &str, replicas: i64, app: &str| -> Vec<Scaled> {
            let obj = json!({"metadata": {"name": "web", "namespace": ns}, "spec": {"replicas": replicas,
                "template": {"metadata": {"labels": {"app": app, "tier": "front"}}}}});
            lean::<Workload>(obj).scaled(kind).into_iter().collect()
        };
        assert_eq!(scaled_to_zero(&svc, &workload("Deployment", "app", 0, "web")).as_deref(), Some("Deployment web"));
        // Running replicas aren't kept; another app, or the same in another namespace: not it.
        assert!(workload("Deployment", "app", 2, "web").is_empty());
        assert_eq!(scaled_to_zero(&svc, &workload("Deployment", "app", 0, "api")), None);
        assert_eq!(scaled_to_zero(&svc, &workload("StatefulSet", "other", 0, "web")), None);
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

    fn service(ns: &str, name: &str, ports: Value) -> Service {
        lean(json!({"metadata": {"namespace": ns, "name": name}, "spec": {"ports": ports}}))
    }

    #[test]
    fn an_ingress_misses_its_services_ports_and_secrets() {
        let services = [
            service("app", "web", json!([{"name": "http", "port": 80}])),
            service("other", "api", json!([{"port": 80}])),
        ];
        let by_name = services_by_name(None, &services);
        let ing = |backend: Value, tls: &str| -> Ingress {
            lean(json!({"metadata": {"namespace": "app", "name": "front"}, "spec": {
                "tls": [{"hosts": ["a.example.com"], "secretName": tls}],
                "rules": [{"http": {"paths": [{"path": "/", "backend": {"service": backend}}]}}]}}))
        };
        let secrets = listed(&[("app", "front-tls"), ("other", "other-tls")]);
        let check = |i: &Ingress| judge_ingress(i, &by_name, |t| secrets.exists(t));
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
        let external: [Service; 1] =
            [lean(json!({"metadata": {"namespace": "app", "name": "out"}, "spec": {"type": "ExternalName"}}))];
        let by_name = services_by_name(Some("app"), &external);
        let default: Ingress = lean(
            json!({"metadata": {"namespace": "app"}, "spec": {"defaultBackend": {"service": {"name": "out", "port": {"number": 443}}}}}),
        );
        assert!(judge_ingress(&default, &by_name, |_| None).is_empty());
        let default: Ingress = lean(
            json!({"metadata": {"namespace": "app"}, "spec": {"defaultBackend": {"service": {"name": "gone", "port": {"number": 1}}}}}),
        );
        assert_eq!(judge_ingress(&default, &by_name, |_| None), ["missing Service gone"]);
    }

    #[test]
    fn an_http_route_misses_its_services() {
        let services = [service("app", "web", json!([])), service("other", "api", json!([]))];
        let route = |refs: Value| -> Route {
            lean(json!({"metadata": {"namespace": "app", "name": "r"}, "spec": {"rules": [{"backendRefs": refs}]}}))
        };
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

    /// A cluster with Services, their Endpoints and pods, Ingresses and HTTP routes in
    /// namespace `app`, whole as an API server sends them (managedFields, annotations,
    /// status, `null`s), so the lean reads skip what they don't use; records the paths it was
    /// asked for. Each cluster has uids of its own, as real ones do (Services' verdicts are
    /// kept by uid); `.2` is the resourceVersion of its objects; `.3` scales the Deployment
    /// `idle` up to 1.
    #[derive(Clone)]
    struct Cluster(
        std::sync::Arc<Mutex<Vec<String>>>,
        u64,
        std::sync::Arc<std::sync::atomic::AtomicU64>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    );

    impl Default for Cluster {
        fn default() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Self(std::sync::Arc::default(), id, std::sync::Arc::new(4711.into()), std::sync::Arc::default())
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
            self.0.lock().unwrap().push(path.clone());
            let old = "2026-01-01T00:00:00Z";
            let (id, version) = (self.1, self.2.load(std::sync::atomic::Ordering::Relaxed).to_string());
            let meta = |name: &str| {
                json!({"name": name, "namespace": "app", "creationTimestamp": old, "uid": format!("uid-{id}-{name}"),
                    "resourceVersion": version, "labels": {"app": name},
                    "annotations": {"kubectl.kubernetes.io/last-applied-configuration": format!("{{\"metadata\":{{\"name\":\"{name}\"}}}}")},
                    "managedFields": [{"manager": "kubectl", "operation": "Update", "apiVersion": "v1", "time": old,
                        "fieldsType": "FieldsV1", "fieldsV1": {"f:metadata": {"f:labels": {".": {}, "f:app": {}}}, "f:spec": {"f:ports": {}}}}]})
            };
            let list = |items: Vec<Value>| json!({"kind": "List", "apiVersion": "v1", "metadata": {"resourceVersion": "4711"}, "items": items});
            let svc = |name: &str, app: &str| {
                json!({"metadata": meta(name), "spec": {"selector": {"app": app}, "clusterIP": "10.96.0.10", "type": "ClusterIP",
                    "ports": [{"name": "http", "port": 80, "targetPort": "http", "protocol": "TCP"}]},
                    "status": {"loadBalancer": {}}})
            };
            let body = match path.as_str() {
                "/api/v1/namespaces/app/services" => list(vec![
                    svc("web", "web"),
                    svc("idle", "idle"),
                    svc("down", "down"),
                    svc("orphan", "orphan"),
                    json!({"metadata": meta("external"), "spec": {"type": "ExternalName", "externalName": "x.example.com",
                        "selector": null, "ports": null}}),
                ]),
                "/api/v1/namespaces/app/endpoints" => {
                    let row = |name: &str, endpoints: &str| json!({"cells": [name, endpoints, "1d"], "object": {"metadata": {"name": name, "namespace": "app", "uid": name}}});
                    json!({"kind": "Table", "columnDefinitions": [{"name": "Name"}, {"name": "Endpoints"}, {"name": "Age"}],
                        "rows": [row("web", "10.0.0.1:80"), row("down", "<none>"), row("idle", "<none>")]})
                }
                "/api/v1/namespaces/app/pods" if query.contains("down") => {
                    list(vec![json!({"metadata": meta("down-1"),
                    "spec": {"nodeName": "n1", "containers": [{"name": "main", "image": "busybox:1.37", "env": [{"name": "A", "value": "1"}]}]},
                    "status": {"phase": "Running", "podIP": "10.0.0.9", "conditions": [
                        {"type": "PodScheduled", "status": "True", "lastProbeTime": null, "lastTransitionTime": old},
                        {"type": "Ready", "status": "False", "lastProbeTime": null, "reason": "ContainersNotReady"}],
                        "containerStatuses": [{"name": "main", "ready": false, "restartCount": 3, "state": {"running": {"startedAt": old}}}]}})])
                }
                "/api/v1/namespaces/app/pods" => list(vec![]),
                "/apis/apps/v1/namespaces/app/deployments" => list(vec![json!({"metadata": meta("idle"),
                    "spec": {"replicas": u8::from(self.3.load(std::sync::atomic::Ordering::Relaxed)),
                        "selector": {"matchLabels": {"app": "idle"}}, "strategy": {"type": "RollingUpdate"},
                        "template": {"metadata": {"creationTimestamp": null, "labels": {"app": "idle"}},
                            "spec": {"containers": [{"name": "main", "image": "busybox:1.37", "resources": {}}]}}},
                    "status": {"observedGeneration": 3, "conditions": [{"type": "Available", "status": "True"}]}})]),
                "/apis/apps/v1/namespaces/app/statefulsets" => list(vec![]),
                "/apis/networking.k8s.io/v1/namespaces/app/ingresses" => {
                    list(vec![json!({"metadata": meta("front"), "spec": {"ingressClassName": "nginx",
                    "tls": [{"hosts": ["a.example.com"], "secretName": "front-tls"}, {"secretName": "gone-tls"}],
                    "rules": [{"host": "a.example.com", "http": {"paths": [
                        {"path": "/", "pathType": "Prefix", "backend": {"service": {"name": "web", "port": {"name": "http"}}}},
                        {"path": "/x", "pathType": "Prefix", "backend": {"service": {"name": "nothing", "port": {"number": 80}}}},
                    ]}}, {"host": "b.example.com"}]},
                    "status": {"loadBalancer": {"ingress": [{"ip": "192.0.2.1"}]}}})])
                }
                "/api/v1/namespaces/app/secrets/front-tls" => {
                    json!({"apiVersion": "meta.k8s.io/v1", "kind": "PartialObjectMetadata", "metadata": meta("front-tls")})
                }
                "/apis/gateway.networking.k8s.io/v1/namespaces/app/httproutes" => {
                    list(vec![json!({"metadata": meta("route"),
                    "spec": {"parentRefs": [{"name": "gateway", "namespace": "infra"}], "hostnames": ["a.example.com"],
                        "rules": [{"matches": [{"path": {"type": "PathPrefix", "value": "/"}}],
                            "backendRefs": [{"group": "", "kind": "Service", "name": "web", "port": 80, "weight": 1},
                                {"name": "gone", "port": 80}]}]},
                    "status": {"parents": [{"parentRef": {"name": "gateway"}, "controllerName": "example.com/gateway",
                        "conditions": [{"type": "Accepted", "status": "True"}]}]}})])
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
        let scan = traffic(client, &service, Some(&ingress), Some(&route), Some("app".into()), None).await;
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
        // Two Services select no pod in `app`: its workloads are listed once for both.
        assert_eq!(asked.iter().filter(|p| p.ends_with("/deployments")).count(), 1, "{asked:?}");
        assert_eq!(asked.iter().filter(|p| p.ends_with("/statefulsets")).count(), 1, "{asked:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cluster_without_gateway_api_still_checks_the_rest() {
        let client = Client::new(Cluster::default(), "default");
        let service = kind("", "Service");
        let ingress = kind("networking.k8s.io", "Ingress");
        let scan = traffic(client, &service, Some(&ingress), None, Some("app".into()), None).await;
        assert!(scan.skipped.is_empty(), "{:?}", scan.skipped_text());
        assert!(scan.problems.iter().all(|p| p.label != "HTTPRoute"));
        assert_eq!(scan.problems.len(), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_unchanged_service_keeps_its_verdict() {
        let cluster = Cluster::default();
        let client = Client::new(cluster.clone(), "default");
        let service = kind("", "Service");
        let asked = |end: &str| cluster.0.lock().unwrap().iter().filter(|p| p.ends_with(end)).count();
        let verdicts = |scan: Scan| {
            let mut found: Vec<(String, Severity, String)> =
                scan.problems.into_iter().map(|p| (p.name, p.severity, p.reason)).collect();
            found.sort();
            found
        };
        let first = verdicts(traffic(client.clone(), &service, None, None, Some("app".into()), None).await);
        assert_eq!(first.len(), 3);
        assert_eq!((asked("/pods"), asked("/deployments")), (3, 1));
        // Nothing changed: the same verdicts, and no pod listed again. The Service kept as
        // selecting no pod because `idle` is scaled to 0 checks that it still is: the
        // namespace's workloads, listed once.
        let second = verdicts(traffic(client.clone(), &service, None, None, Some("app".into()), None).await);
        assert_eq!(second, first);
        assert_eq!((asked("/pods"), asked("/deployments")), (3, 2));
        // `idle` scaled up, its Service unchanged: no longer a minor finding, its pods listed.
        cluster.3.store(true, std::sync::atomic::Ordering::Relaxed);
        let third = verdicts(traffic(client.clone(), &service, None, None, Some("app".into()), None).await);
        let idle = third.iter().find(|(name, ..)| name == "idle").unwrap();
        assert_eq!((idle.1, idle.2.as_str()), (Severity::Warning, "selects no pod"));
        assert_eq!((asked("/pods"), asked("/deployments")), (4, 3));
        // The Services changed: their pods are listed again.
        cluster.2.store(4712, std::sync::atomic::Ordering::Relaxed);
        let fourth = verdicts(traffic(client.clone(), &service, None, None, Some("app".into()), None).await);
        assert_eq!(fourth, third);
        assert_eq!((asked("/pods"), asked("/deployments")), (7, 4));
        // Refresh, or an agent checking a fix: every Service is judged again, as the first
        // time after a change, though none changed.
        let now = Some(Instant::now());
        let fifth = verdicts(traffic(client.clone(), &service, None, None, Some("app".into()), now).await);
        assert_eq!(fifth, fourth);
        assert_eq!((asked("/pods"), asked("/deployments")), (10, 5));
        // Verdicts made since then stand.
        let sixth = verdicts(traffic(client, &service, None, None, Some("app".into()), now).await);
        assert_eq!(sixth, fifth);
        assert_eq!(asked("/pods"), 10, "no pod listed again");
    }

    proptest! {
        /// A verdict stands only for the resourceVersion it was judged at, and for
        /// `VERDICT_KEPT` at most.
        #[test]
        fn a_verdict_stands_while_its_service_is_unchanged(
            kept in prop::sample::select(&["1", "2"][..]),
            now in prop::sample::select(&["1", "2"][..]),
            age in 0u64..2_000,
        ) {
            let at = Instant::now();
            let verdict = Verdict { version: kept.into(), at, verdict: (Severity::Warning, "selects no pod".into()) };
            let later = at + Duration::from_secs(age);
            prop_assert_eq!(reusable(&verdict, now, later), kept == now && Duration::from_secs(age) < VERDICT_KEPT);
        }
    }
}
