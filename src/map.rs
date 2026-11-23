//! The Map tab of every detail page: what an object is linked to, from what the app already
//! knows about references:
//!
//! - owner references both ways (its owners, and what it owns);
//! - the pods a Service, PDB, NetworkPolicy or workload selects (`k8s::pod_selected`);
//! - what the Problems page checks (`problems::references`): the ConfigMaps, Secrets, PVCs and
//!   ServiceAccount a pod spec names, an Ingress's or `HTTPRoute`'s Services and TLS Secrets;
//! - an HPA's or VPA's target, a PVC's volume and class, a binding's role and accounts;
//! - operators' objects: custom resources naming another object in their spec, by known rules
//!   (`RULES`, `CloudNativePG` first) or the generic one (`names_in_spec`), and what is
//!   labelled with an instance's name (`INSTANCE_LABELS`).
//!
//! It is read only when the tab opens (`load`, off the UI thread): in the object's namespace,
//! from the API server's cache (`resourceVersion=0`), lean (metadata alone where that is
//! enough), next to what the page holds already (`Known`, taken from `Related` off the UI
//! thread). A cluster-scoped object reads nothing more. The relations are worked out off the
//! UI thread too (`sections`); each shows at most `SHOWN_AT_MOST` objects and counts the rest.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use futures::StreamExt as _;
use k8s_openapi::api::{apps::v1 as apps, autoscaling::v2 as autoscaling, batch::v1 as batch, core::v1 as core};
use k8s_openapi::api::{networking::v1 as networking, policy::v1 as policy};
use kube::Client;
use kube::api::{Api, DynamicObject, ListParams};
use kube::discovery::ApiResource;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::json::{arr_at as arr, nonempty_at as text};
use crate::k8s::{self, Kind};
use crate::overview::Related;
use crate::problems::references::{self as refs, Target};

/// Objects shown under one relation; the rest are counted.
pub const SHOWN_AT_MOST: usize = 50;

/// Backend Services of an Ingress or `HTTPRoute` whose pods are listed.
const BACKENDS_READ: usize = 10;

/// Kinds of an operator's group read to find what points to one of its objects.
const GROUP_KINDS_READ: usize = 12;

/// Above this many objects of an operator's kind in the namespace, only their metadata is
/// kept (owners, labels): their specs aren't read to find what they name.
const SPECS_READ_AT_MOST: usize = 200;

/// `CloudNativePG`'s API group.
const CNPG: &str = "postgresql.cnpg.io";

/// How one object relates to the one shown, in the order the tab lists them. An object is
/// shown once, under the first: a label match is the weakest, last.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Rel {
    OwnedBy,
    Owns,
    Uses,
    UsedBy,
    PointsTo,
    PointedToBy,
    Selects,
    SelectedBy,
    SendsTo,
    ReceivesFrom,
    Scales,
    ScaledBy,
    Runs,
    Binds,
    /// Labelled with the instance's name (`INSTANCE_LABELS`).
    Labelled,
}

impl Rel {
    pub fn label(self) -> &'static str {
        match self {
            Self::OwnedBy => "Owned by",
            Self::Owns => "Owns",
            Self::Uses => "Uses",
            Self::UsedBy => "Used by",
            Self::PointsTo => "Points to",
            Self::PointedToBy => "Pointed to by",
            Self::Selects => "Selects",
            Self::SelectedBy => "Selected by",
            Self::SendsTo => "Sends traffic to",
            Self::ReceivesFrom => "Receives traffic from",
            Self::Scales => "Scales",
            Self::ScaledBy => "Scaled by",
            Self::Runs => "Runs",
            Self::Binds => "Binds",
            Self::Labelled => "Labelled with its name",
        }
    }
}

/// An object on the map: its kind (group and name), namespace (None when cluster-scoped)
/// and name, with a note ("Running", "TLS"), the objects shown under it (a backend Service's
/// pods) and how many more of those there are.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Item {
    pub group: String,
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
    pub note: Option<String>,
    pub under: Vec<Self>,
    pub more_under: usize,
}

impl Item {
    fn new(group: &str, kind: &str, namespace: Option<&str>, name: &str) -> Self {
        Self {
            group: group.into(),
            kind: kind.into(),
            namespace: namespace.map(String::from),
            name: name.into(),
            ..Default::default()
        }
    }

    /// A listed object of this kind, a pod with its phase.
    fn of(group: &str, kind: &str, obj: &Value) -> Option<Self> {
        let mut item = Self::new(group, kind, text(obj, "/metadata/namespace"), text(obj, "/metadata/name")?);
        item.note = text(obj, "/status/phase").map(String::from);
        Some(item)
    }

    fn target(t: &Target) -> Self {
        Self::new(t.of.group(), t.of.kind(), t.namespace.as_deref(), &t.name)
    }

    /// An object named by `apiVersion` (or a bare group) and `kind`.
    fn by_version(api_version: &str, kind: &str, namespace: Option<&str>, name: &str) -> Self {
        let group =
            api_version.split_once('/').map_or(if api_version.contains('.') { api_version } else { "" }, |(g, _)| g);
        Self::new(group, kind, namespace, name)
    }

    fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// The kind as a link opens it (`k8s::find_kind`): `secret`, `certificate.cert-manager.io`.
    pub fn link_kind(&self) -> String {
        let kind = self.kind.to_lowercase();
        if self.group.is_empty() { kind } else { format!("{kind}.{}", self.group) }
    }
}

/// The objects of one relation, at most `SHOWN_AT_MOST`, and how many more there are.
#[derive(Debug, PartialEq, Eq)]
pub struct Section {
    pub rel: Rel,
    pub items: Vec<Item>,
    pub more: usize,
}

/// An object in a custom resource's spec that names another one, as an operator documents it.
#[derive(Clone, Copy)]
enum At {
    /// The name at this path.
    Path(&'static str),
    /// A reference at this path (`name`, and `kind` when it has one, which must be `to`'s).
    Ref(&'static str),
    /// The Secret an `ExternalSecret` writes (`external_secrets::target_of`).
    ExternalSecretTarget,
}

/// A custom resource of kind `from` names an object of kind `to` (group, kind) in its namespace.
struct Rule {
    from: (&'static str, &'static str),
    to: (&'static str, &'static str),
    at: At,
}

const CNPG_CLUSTER: (&str, &str) = (CNPG, "Cluster");
const SECRET: (&str, &str) = ("", "Secret");
const ES: &str = crate::external_secrets::GROUP;

/// Known operators' references, as data. `CloudNativePG` first: what works on a Cluster names
/// it in `spec.cluster.name`.
const RULES: &[Rule] = &[
    Rule { from: (CNPG, "Backup"), to: CNPG_CLUSTER, at: At::Path("/spec/cluster/name") },
    Rule { from: (CNPG, "ScheduledBackup"), to: CNPG_CLUSTER, at: At::Path("/spec/cluster/name") },
    Rule { from: (CNPG, "Pooler"), to: CNPG_CLUSTER, at: At::Path("/spec/cluster/name") },
    Rule { from: (CNPG, "Database"), to: CNPG_CLUSTER, at: At::Path("/spec/cluster/name") },
    Rule { from: (CNPG, "Publication"), to: CNPG_CLUSTER, at: At::Path("/spec/cluster/name") },
    Rule { from: (CNPG, "Subscription"), to: CNPG_CLUSTER, at: At::Path("/spec/cluster/name") },
    Rule { from: ("cert-manager.io", "Certificate"), to: SECRET, at: At::Path("/spec/secretName") },
    Rule {
        from: ("cert-manager.io", "Certificate"),
        to: ("cert-manager.io", "Issuer"),
        at: At::Ref("/spec/issuerRef"),
    },
    Rule { from: (ES, "ExternalSecret"), to: SECRET, at: At::ExternalSecretTarget },
    Rule { from: (ES, "ExternalSecret"), to: (ES, "SecretStore"), at: At::Ref("/spec/secretStoreRef") },
];

impl Rule {
    /// The name `obj` (of kind `from`) gives to its `to` object.
    fn name<'a>(&self, obj: &'a Value) -> Option<&'a str> {
        match self.at {
            At::Path(p) => text(obj, p),
            At::Ref(p) => {
                let r = obj.pointer(p)?;
                let kind = r["kind"].as_str().filter(|k| !k.is_empty()).unwrap_or(self.to.1);
                (kind == self.to.1).then(|| r["name"].as_str().filter(|n| !n.is_empty())).flatten()
            }
            At::ExternalSecretTarget => crate::external_secrets::target_of(obj),
        }
    }
}

/// Labels whose value is an operator instance's name, and the kind of that instance (None:
/// any custom resource, unless Helm manages the labelled object: there the label is the
/// release's name, shared by everything the release installs). `CloudNativePG`'s first.
const INSTANCE_LABELS: &[(&str, Option<(&str, &str)>)] = &[
    ("cnpg.io/cluster", Some(CNPG_CLUSTER)),
    ("clickhouse.altinity.com/chi", Some(("clickhouse.altinity.com", "ClickHouseInstallation"))),
    ("strimzi.io/cluster", Some(("kafka.strimzi.io", "Kafka"))),
    ("postgres-operator.crunchydata.com/cluster", Some(("postgres-operator.crunchydata.com", "PostgresCluster"))),
    ("app.kubernetes.io/instance", None),
];

/// Whether these labels point at the instance `name` of `kind` (`INSTANCE_LABELS`).
fn labelled_with(kind: &Kind, name: &str, labels: &serde_json::Map<String, Value>) -> bool {
    let helm = labels.get("app.kubernetes.io/managed-by").and_then(Value::as_str) == Some("Helm");
    INSTANCE_LABELS.iter().any(|(label, of)| {
        labels.get(*label).and_then(Value::as_str) == Some(name)
            && match of {
                Some((g, k)) => kind.is(g, k),
                None => !helm,
            }
    })
}

/// What an operator's custom resource is read with to find what it owns or what is labelled
/// with its name: the kinds operators create, metadata only.
fn created_by_operators() -> [ApiResource; 10] {
    [
        k8s::ar_of::<core::Pod>(),
        k8s::ar_of::<core::Service>(),
        k8s::ar_of::<core::Secret>(),
        k8s::ar_of::<core::ConfigMap>(),
        k8s::ar_of::<core::PersistentVolumeClaim>(),
        k8s::ar_of::<apps::Deployment>(),
        k8s::ar_of::<apps::StatefulSet>(),
        k8s::ar_of::<apps::DaemonSet>(),
        k8s::ar_of::<batch::Job>(),
        k8s::ar_of::<batch::CronJob>(),
    ]
}

/// Where a kind keeps its pod spec, for the kinds that have one.
pub fn pod_spec_path(kind: &Kind) -> Option<&'static str> {
    match (kind.ar.group.as_str(), kind.ar.kind.as_str()) {
        ("", "Pod") => Some("/spec"),
        ("batch", "CronJob") => Some("/spec/jobTemplate/spec/template/spec"),
        ("apps", "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet") | ("batch", "Job") => {
            Some("/spec/template/spec")
        }
        _ => None,
    }
}

/// The labels of the pods a kind runs (a pod's own).
fn pod_labels<'a>(kind: &Kind, obj: &'a Value) -> Option<&'a serde_json::Map<String, Value>> {
    let path = match pod_spec_path(kind)? {
        "/spec" => "/metadata/labels".to_string(),
        spec => format!("{}/metadata/labels", spec.trim_end_matches("/spec")),
    };
    obj.pointer(&path).and_then(Value::as_object)
}

/// Kinds whose Map starts with the tree of their pods (`overview::relation_rows`): it shows
/// their Services, what their pods use, their ReplicaSets, pods and nodes.
pub fn has_tree(kind: &Kind) -> bool {
    kind.ar.group == "apps" || kind.is("batch", "Job") || kind.is("", "Pod")
}

/// A kind known by its group and name alone, for the rules that read only those.
fn kind_named(group: &str, kind: &str) -> Kind {
    let plural = format!("{}s", kind.to_lowercase());
    let api_version = if group.is_empty() { "v1".to_string() } else { format!("{group}/v1") };
    let ar = ApiResource { group: group.into(), version: "v1".into(), api_version, kind: kind.into(), plural };
    Kind { ar, namespaced: true, section: "", label: kind.into() }
}

fn custom(kind: &Kind) -> bool {
    !k8s::is_builtin_group(&kind.ar.group)
}

/// Kinds whose Map has rules of its own (`sections`), besides owners.
fn has_rules(kind: &Kind) -> bool {
    let own = matches!(
        (kind.ar.group.as_str(), kind.ar.kind.as_str()),
        ("batch", "CronJob")
            | ("", "Secret" | "ConfigMap" | "ServiceAccount" | "PersistentVolumeClaim" | "PersistentVolume")
            | ("", "Service" | "Node")
            | ("networking.k8s.io", "Ingress" | "NetworkPolicy")
            | ("autoscaling", "HorizontalPodAutoscaler")
            | ("autoscaling.k8s.io", "VerticalPodAutoscaler")
            | ("policy", "PodDisruptionBudget")
            | ("rbac.authorization.k8s.io", "RoleBinding" | "ClusterRoleBinding")
    ) || kind.is(refs::GATEWAY, "HTTPRoute");
    own || has_tree(kind)
        || custom(kind)
        || RULES.iter().any(|r| kind.is(r.to.0, r.to.1) || kind.is(r.from.0, r.from.1))
}

/// Only owners are known for this kind (a Gateway, a StorageClass, a ClusterRole…): the tab
/// says so rather than "nothing found".
pub fn only_owners(kind: &Kind) -> bool {
    !has_rules(kind)
}

/// Kinds that are cluster-scoped though nothing says so (the served kinds weren't known).
fn cluster_scoped(kinds: &[Kind], group: &str, kind: &str) -> bool {
    match kinds.iter().find(|k| k.is(group, kind)) {
        Some(k) => !k.namespaced,
        None => matches!(
            (group, kind),
            ("", "Node" | "Namespace" | "PersistentVolume")
                | ("storage.k8s.io", "StorageClass")
                | ("rbac.authorization.k8s.io", "ClusterRole" | "ClusterRoleBinding")
        ),
    }
}

/// How a list is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// Whole, lean (what the rules read).
    Full,
    /// Metadata alone.
    Meta,
    /// Metadata, then with specs when there are at most `SPECS_READ_AT_MOST` of them.
    SpecsIfFew,
}

/// A list to read in the object's namespace.
#[derive(Clone, Debug)]
pub struct Want {
    pub ar: ApiResource,
    pub read: Read,
}

fn served<'a>(served: &'a [Kind], group: &str, kind: &str) -> Option<&'a Kind> {
    served.iter().find(|k| k.is(group, kind))
}

/// The lists the Map of `obj` reads, in its namespace; none for a cluster-scoped object.
/// `kinds` are those its cluster serves (a custom kind is read only when served).
pub fn wants(kind: &Kind, obj: &Value, kinds: &[Kind]) -> Vec<Want> {
    if text(obj, "/metadata/namespace").is_none() {
        return Vec::new();
    }
    let full = |ar: ApiResource| Want { ar, read: Read::Full };
    let mut out: Vec<Want> = Vec::new();
    let traffic = |out: &mut Vec<Want>| {
        out.push(full(k8s::ar_of::<networking::Ingress>()));
        if let Some(k) = served(kinds, refs::GATEWAY, "HTTPRoute") {
            out.push(full(k.ar.clone()));
        }
    };
    match (kind.ar.group.as_str(), kind.ar.kind.as_str()) {
        ("", "Pod") => {
            out.push(full(k8s::ar_of::<policy::PodDisruptionBudget>()));
            traffic(&mut out);
        }
        _ if has_tree(kind) => {
            out.push(full(k8s::ar_of::<policy::PodDisruptionBudget>()));
            out.push(full(k8s::ar_of::<networking::NetworkPolicy>()));
            out.push(full(k8s::ar_of::<autoscaling::HorizontalPodAutoscaler>()));
            traffic(&mut out);
        }
        ("", "Secret") => {
            out.push(full(k8s::ar_of::<core::ServiceAccount>()));
            out.push(full(k8s::ar_of::<networking::Ingress>()));
        }
        ("", "ServiceAccount") => {
            out.push(full(k8s::ar_of::<apps::Deployment>()));
            out.push(full(k8s::ar_of::<apps::StatefulSet>()));
            out.push(full(k8s::ar_of::<apps::DaemonSet>()));
            out.push(full(k8s::ar_of::<batch::CronJob>()));
        }
        ("", "Service") => traffic(&mut out),
        ("networking.k8s.io", "Ingress") => out.push(full(k8s::ar_of::<core::Service>())),
        (g, "HTTPRoute") if g == refs::GATEWAY => out.push(full(k8s::ar_of::<core::Service>())),
        _ if custom(kind) => {
            out.extend(created_by_operators().into_iter().map(|ar| Want { ar, read: Read::Meta }));
            // The operator's other kinds: what it owns, and what names this object in its spec.
            let group = kinds
                .iter()
                .filter(|k| k.ar.group == kind.ar.group && k.namespaced && k.ar.kind != kind.ar.kind)
                .take(GROUP_KINDS_READ);
            out.extend(group.map(|k| Want { ar: k.ar.clone(), read: Read::SpecsIfFew }));
        }
        _ => {}
    }
    // Known operators' kinds that name this kind.
    for rule in RULES.iter().filter(|r| kind.is(r.to.0, r.to.1)) {
        if let Some(k) = served(kinds, rule.from.0, rule.from.1) {
            out.push(Want { ar: k.ar.clone(), read: Read::SpecsIfFew });
        }
    }
    let mut seen = BTreeSet::new();
    out.retain(|w| seen.insert((w.ar.group.clone(), w.ar.kind.clone())));
    out
}

/// What was read for the Map, by (group, kind), and what could not be.
#[derive(Default, Debug)]
pub struct Lists {
    objects: HashMap<(String, String), Vec<Value>>,
    /// The pods of an Ingress's or `HTTPRoute`'s backend Services, by Service name.
    backends: HashMap<String, Vec<Value>>,
    /// "ingresses: forbidden", for each list that failed.
    pub errors: Vec<String>,
    /// Lists read in part: "1500 applications: too many to read what each names".
    pub partial: Vec<String>,
    /// The kinds read, as their plural ("pods, services"), for the tab to say where it looked.
    pub read: Vec<String>,
}

impl Lists {
    fn of(&self, group: &str, kind: &str) -> &[Value] {
        self.objects.get(&(group.to_string(), kind.to_string())).map_or(&[], Vec::as_slice)
    }

    fn all(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.objects.iter().flat_map(|((g, k), vs)| vs.iter().map(move |v| (g.as_str(), k.as_str(), v)))
    }

    #[cfg(test)]
    pub fn with(mut self, group: &str, kind: &str, objects: Vec<Value>) -> Self {
        self.objects.insert((group.into(), kind.into()), objects);
        self
    }

    #[cfg(test)]
    pub fn with_backend(mut self, service: &str, pods: Vec<Value>) -> Self {
        self.backends.insert(service.into(), pods);
        self
    }
}

/// What the Map keeps of an object it lists: its names, labels and owners.
#[derive(Deserialize, Serialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct LeanMeta {
    name: Option<String>,
    namespace: Option<String>,
    uid: Option<String>,
    #[serde(deserialize_with = "k8s::null_default")]
    labels: BTreeMap<String, String>,
    #[serde(deserialize_with = "k8s::null_default")]
    owner_references: Vec<Value>,
}

/// A listed object: its metadata, its spec, and a ServiceAccount's Secrets (outside its spec).
#[derive(Deserialize, Serialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Lean {
    metadata: LeanMeta,
    spec: Value,
    #[serde(skip_serializing_if = "Value::is_null")]
    secrets: Value,
    #[serde(skip_serializing_if = "Value::is_null")]
    image_pull_secrets: Value,
}

/// A backend's pod: its names and labels, and its phase.
#[derive(Deserialize, Serialize, Default)]
#[serde(default)]
struct LeanPod {
    metadata: LeanMeta,
    status: PodPhase,
}

#[derive(Deserialize, Serialize, Default)]
#[serde(default)]
struct PodPhase {
    phase: Option<String>,
}

fn values<T: Serialize>(items: Vec<T>) -> Vec<Value> {
    items.into_iter().filter_map(|i| serde_json::to_value(i).ok()).collect()
}

/// Metadata alone (names, labels, owners), from the API server's cache.
async fn list_meta(client: Client, ar: ApiResource, ns: String) -> anyhow::Result<Vec<Value>> {
    let api: Api<DynamicObject> = Api::namespaced_with(client, &ns, &ar);
    let list = api.list_metadata(&ListParams::default().match_any()).await?;
    Ok(list
        .items
        .into_iter()
        .map(|o| {
            let m = o.metadata;
            json!({ "metadata": {
                "name": m.name, "namespace": m.namespace, "uid": m.uid,
                "labels": m.labels, "ownerReferences": m.owner_references,
            } })
        })
        .collect())
}

async fn list_full(client: Client, ar: ApiResource, ns: String) -> anyhow::Result<Vec<Value>> {
    k8s::lists::list_lean_cached::<Lean>(client, ar, Some(ns)).await.map(values)
}

/// Read what the Map of `obj` needs (`wants`), in its namespace, from the API server's
/// cache; then the pods of an Ingress's or `HTTPRoute`'s backend Services, by their selector.
/// A kind the cluster doesn't serve is skipped; one that fails is said, the others still show.
/// Runs on the I/O runtime, parsing included; dropping it stops the reads.
pub async fn load(client: Client, kind: Kind, obj: Value, kinds: Vec<Kind>) -> Lists {
    k8s::run_abortable(async move { Ok(read_lists(client, kind, obj, kinds).await) })
        .await
        .unwrap_or_else(|e| Lists { errors: vec![format!("failed: {}", k8s::short_error(&e))], ..Default::default() })
}

async fn read_lists(client: Client, kind: Kind, obj: Value, kinds: Vec<Kind>) -> Lists {
    let mut lists = Lists::default();
    let Some(ns) = text(&obj, "/metadata/namespace").map(String::from) else { return lists };
    let reads = wants(&kind, &obj, &kinds).into_iter().map(|w| {
        let (client, ns) = (client.clone(), ns.clone());
        async move {
            let read = match w.read {
                Read::Full => list_full(client, w.ar.clone(), ns).await.map(|v| (v, None)),
                Read::Meta => list_meta(client, w.ar.clone(), ns).await.map(|v| (v, None)),
                Read::SpecsIfFew => match list_meta(client.clone(), w.ar.clone(), ns.clone()).await {
                    Ok(meta) if meta.len() > SPECS_READ_AT_MOST => {
                        let note = format!("{} {}: too many to read what each names", meta.len(), w.ar.plural);
                        Ok((meta, Some(note)))
                    }
                    Ok(_) => list_full(client, w.ar.clone(), ns).await.map(|v| (v, None)),
                    Err(e) => Err(e),
                },
            };
            (w, read)
        }
    });
    let read: Vec<_> = futures::stream::iter(reads).buffer_unordered(8).collect().await;
    for (w, result) in read {
        match result {
            Ok((items, partial)) => {
                lists.read.push(w.ar.plural.clone());
                lists.partial.extend(partial);
                lists.objects.insert((w.ar.group, w.ar.kind), items);
            }
            Err(e) if k8s::api_status(&e) == Some(404) => {}
            Err(e) => lists.errors.push(format!("{}: {}", w.ar.plural, k8s::lists::load_error(&e))),
        }
    }
    lists.read.sort();
    lists.errors.sort();
    // The pods behind an Ingress's or HTTPRoute's Services in this namespace.
    let backends: Vec<Target> = match (kind.ar.group.as_str(), kind.ar.kind.as_str()) {
        ("networking.k8s.io", "Ingress") => refs::ingress_refs(&obj).0,
        (g, "HTTPRoute") if g == refs::GATEWAY => refs::route_refs(&obj),
        _ => Vec::new(),
    };
    let service = Kind { ar: k8s::ar_of::<core::Service>(), namespaced: true, section: "", label: "Service".into() };
    let selectors: Vec<(String, String)> = backends
        .iter()
        .filter(|t| t.namespace.as_deref() == Some(ns.as_str()))
        .take(BACKENDS_READ)
        .filter_map(|t| {
            let svc = lists.of("", "Service").iter().find(|s| text(s, "/metadata/name") == Some(t.name.as_str()))?;
            Some((t.name.clone(), k8s::pod_selection(&service, svc).list_selector().flatten()?))
        })
        .collect();
    let pods = selectors.into_iter().map(|(name, selector)| {
        let (client, ns) = (client.clone(), ns.clone());
        async move {
            let ar = k8s::ar_of::<core::Pod>();
            (name, k8s::lists::list_lean_cached_by::<LeanPod>(client, ar, Some(ns), Some(selector)).await)
        }
    });
    let pods: Vec<_> = futures::stream::iter(pods).buffer_unordered(4).collect().await;
    for (name, result) in pods {
        match result {
            Ok(p) => {
                lists.backends.insert(name, values(p));
            }
            Err(e) => lists.errors.push(format!("pods of Service {name}: {}", k8s::lists::load_error(&e))),
        }
    }
    lists
}

/// What the Map uses of what its page already holds (`Related`), taken off the UI thread each
/// time the page reloads it: pods as names and phases (a node can run a thousand), Services as
/// their selectors, and why a list the page reads could not be.
#[derive(Clone, Debug, Default)]
pub struct Known {
    /// The pods the page lists (a Service's, a node's, a ServiceAccount's…).
    pub pods: Vec<Item>,
    /// The namespace's Services, by name and selector (a pod's or workload's page).
    pub services: Vec<Value>,
    /// A CronJob's Jobs.
    pub jobs: Vec<Item>,
    /// What uses a Secret or ConfigMap, by plural (`Related::referenced_by`).
    pub referenced_by: BTreeMap<String, Vec<String>>,
    /// The NetworkPolicies selecting a pod (None: not read).
    pub netpols: Option<Vec<Value>>,
    /// "pods: forbidden", for what the page could not read that the Map draws from.
    pub errors: Vec<String>,
}

/// Kinds whose Map lists the pods their page holds.
fn lists_pods(kind: &Kind) -> bool {
    matches!(
        (kind.ar.group.as_str(), kind.ar.kind.as_str()),
        ("", "ServiceAccount" | "PersistentVolumeClaim" | "Service" | "Node")
            | ("policy", "PodDisruptionBudget")
            | ("networking.k8s.io", "NetworkPolicy")
    )
}

impl Known {
    /// What the Map of a `kind` takes from its page's `related` (the tree of a pod or workload
    /// says its own load errors).
    pub fn of(kind: &Kind, related: &Related) -> Self {
        let mut known = Self::default();
        let failed = |section: &str, what: &str, errors: &mut Vec<String>| {
            if let Some(e) = related.errors.get(section) {
                errors.push(format!("{what}: {e}"));
            }
        };
        let mut errors = Vec::new();
        if lists_pods(kind) {
            known.pods = related.pods.iter().filter_map(|p| Item::of("", "Pod", p)).collect();
            failed("pods", "pods", &mut errors);
        }
        if has_tree(kind) {
            known.services = related
                .services
                .iter()
                .map(|s| json!({ "metadata": { "name": s.pointer("/metadata/name") }, "spec": { "selector": s.pointer("/spec/selector") } }))
                .collect();
        }
        if kind.is("", "Pod") {
            known.netpols = related.netpols.clone();
            if known.netpols.is_none() {
                errors.push("NetworkPolicies: could not be read".into());
            }
        }
        if kind.is("batch", "CronJob") {
            known.jobs = related.extra.iter().filter_map(|j| Item::of("batch", "Job", j)).collect();
            failed("extra", "jobs", &mut errors);
        }
        if kind.is("", "Secret") || kind.is("", "ConfigMap") {
            known.referenced_by = related.referenced_by.clone();
            failed("referenced_by", "what uses it", &mut errors);
        }
        known.errors = errors;
        known
    }
}

/// The relations, grouped and in `Rel` order, each object once (under its first relation),
/// sorted by kind and name, capped at `SHOWN_AT_MOST`.
fn sections_of(mut found: Vec<(Rel, Item)>) -> Vec<Section> {
    found.sort_by_key(|f| f.0);
    let mut seen = BTreeSet::new();
    let mut by: BTreeMap<Rel, Vec<Item>> = BTreeMap::new();
    for (rel, item) in found {
        let key = (item.group.clone(), item.kind.clone(), item.namespace.clone(), item.name.clone());
        if seen.insert(key) {
            by.entry(rel).or_default().push(item);
        }
    }
    by.into_iter()
        .map(|(rel, mut items)| {
            items.sort_by(|a, b| (&a.kind, &a.name).cmp(&(&b.kind, &b.name)));
            let more = items.len().saturating_sub(SHOWN_AT_MOST);
            items.truncate(SHOWN_AT_MOST);
            Section { rel, items, more }
        })
        .collect()
}

/// The group and kind a plural of `Related::referenced_by` names.
fn referrer_kind(plural: &str) -> Option<(&'static str, &'static str)> {
    Some(match plural {
        "deployments" => ("apps", "Deployment"),
        "statefulsets" => ("apps", "StatefulSet"),
        "daemonsets" => ("apps", "DaemonSet"),
        "cronjobs" => ("batch", "CronJob"),
        "pods" => ("", "Pod"),
        _ => return None,
    })
}

/// The kind a spec field names, as the end of the kind's name (lowercase): `cluster`,
/// `clusterName` and `clusterRef` a `…Cluster`; any other `xRef` a kind ending with x
/// (`issuerRef` an Issuer, `secretStoreRef` a `SecretStore`, `storeRef` one too).
fn kind_named_by(key: &str) -> Option<String> {
    match key {
        "cluster" | "clusterName" | "clusterRef" => Some("cluster".into()),
        k => k.strip_suffix("Ref").filter(|x| !x.is_empty()).map(str::to_lowercase),
    }
}

/// Whether `spec` names an object called `name` of kind `kind`, the generic way operators do:
/// a `cluster` or `clusterName` field holding the name, or a `cluster` / `…Ref` object whose
/// `name` it is. The kind must be the one the field names (`kind_named_by`); a `kind` written
/// in the reference wins. Looked at down to a few levels, arrays included.
pub fn names_in_spec(spec: &Value, kind: &str, name: &str) -> bool {
    fn walk(v: &Value, kind: &str, name: &str, depth: usize) -> bool {
        if depth > 5 {
            return false;
        }
        let ends = |suffix: &str| kind.to_lowercase().ends_with(suffix);
        match v {
            Value::Array(items) => items.iter().any(|i| walk(i, kind, name, depth + 1)),
            Value::Object(map) => map.iter().any(|(k, v)| {
                let named = match (v, kind_named_by(k)) {
                    (Value::String(s), Some(suffix)) => k != "clusterRef" && s == name && ends(&suffix),
                    (Value::Object(r), Some(suffix)) => {
                        let said = r.get("kind").and_then(Value::as_str).filter(|k| !k.is_empty());
                        r.get("name").and_then(Value::as_str) == Some(name)
                            && said.map_or_else(|| ends(&suffix), |said| said == kind)
                    }
                    _ => false,
                };
                named || walk(v, kind, name, depth + 1)
            }),
            _ => false,
        }
    }
    walk(spec, kind, name, 0)
}

/// What the Map of `obj` shows, from the object, what its page holds (`known`), what the tab
/// read (`lists`) and the kinds its cluster serves (to tell a cluster-scoped owner). For a kind
/// with a tree (`has_tree`), what the tree doesn't show.
pub fn sections(kind: &Kind, obj: &Value, known: &Known, lists: &Lists, kinds: &[Kind]) -> Vec<Section> {
    let ns = text(obj, "/metadata/namespace");
    let name = text(obj, "/metadata/name").unwrap_or_default();
    let uid = text(obj, "/metadata/uid");
    let mut found: Vec<(Rel, Item)> = Vec::new();
    let mut add = |rel: Rel, item: Item| found.push((rel, item));

    for o in arr(obj, "/metadata/ownerReferences") {
        if let (Some(v), Some(k), Some(n)) = (text(o, "/apiVersion"), text(o, "/kind"), text(o, "/name")) {
            let mut owner = Item::by_version(v, k, ns, n);
            if cluster_scoped(kinds, &owner.group, k) {
                owner.namespace = None;
            }
            add(Rel::OwnedBy, owner);
        }
    }
    // Anything read that it owns.
    if let Some(uid) = uid {
        for (g, k, v) in lists.all() {
            if arr(v, "/metadata/ownerReferences").iter().any(|o| text(o, "/uid") == Some(uid))
                && let Some(item) = Item::of(g, k, v)
            {
                add(Rel::Owns, item);
            }
        }
    }
    // Known operators' references: what it names, and what names it.
    for rule in RULES {
        if kind.is(rule.from.0, rule.from.1)
            && let Some(n) = rule.name(obj)
        {
            add(Rel::PointsTo, Item::new(rule.to.0, rule.to.1, ns, n));
        }
        if kind.is(rule.to.0, rule.to.1) {
            for v in lists.of(rule.from.0, rule.from.1) {
                if rule.name(v) == Some(name)
                    && let Some(item) = Item::of(rule.from.0, rule.from.1, v)
                {
                    add(Rel::PointedToBy, item);
                }
            }
        }
    }
    // What its pods use, for a kind without the tree (a CronJob).
    if !has_tree(kind)
        && let Some(spec) = pod_spec_path(kind).and_then(|p| obj.pointer(p))
    {
        for t in refs::spec_refs(ns, spec, true) {
            add(Rel::Uses, Item::target(&t));
        }
    }

    let pods = |rel: Rel, add: &mut dyn FnMut(Rel, Item)| {
        for p in &known.pods {
            add(rel, p.clone());
        }
    };
    let group = kind.ar.group.as_str();
    match (group, kind.ar.kind.as_str()) {
        _ if has_tree(kind) => {
            let labels = pod_labels(kind, obj);
            let empty_labels = serde_json::Map::new();
            let labels = labels.unwrap_or(&empty_labels);
            let selected =
                |g: &str, k: &str, v: &Value| !labels.is_empty() && k8s::pod_selected(&kind_named(g, k), v, labels);
            for v in lists.of("policy", "PodDisruptionBudget") {
                if selected("policy", "PodDisruptionBudget", v)
                    && let Some(item) = Item::of("policy", "PodDisruptionBudget", v)
                {
                    add(Rel::SelectedBy, item);
                }
            }
            let netpols: &[Value] = match &known.netpols {
                Some(p) if kind.is("", "Pod") => p,
                _ => lists.of("networking.k8s.io", "NetworkPolicy"),
            };
            for v in netpols {
                if selected("networking.k8s.io", "NetworkPolicy", v)
                    && let Some(item) = Item::of("networking.k8s.io", "NetworkPolicy", v)
                {
                    add(Rel::SelectedBy, item);
                }
            }
            for v in lists.of("autoscaling", "HorizontalPodAutoscaler") {
                if text(v, "/spec/scaleTargetRef/kind") == Some(kind.ar.kind.as_str())
                    && text(v, "/spec/scaleTargetRef/name") == Some(name)
                    && let Some(item) = Item::of("autoscaling", "HorizontalPodAutoscaler", v)
                {
                    add(Rel::ScaledBy, item);
                }
            }
            // Ingresses and HTTPRoutes that send to the Services selecting its pods.
            let services: BTreeSet<&str> = known
                .services
                .iter()
                .filter(|s| selected("", "Service", s))
                .filter_map(|s| text(s, "/metadata/name"))
                .collect();
            traffic_to(&services, ns, lists, &mut add);
        }
        ("batch", "CronJob") => {
            for j in &known.jobs {
                add(Rel::Owns, j.clone());
            }
        }
        ("", "Secret" | "ConfigMap") => {
            for (plural, names) in &known.referenced_by {
                let Some((g, k)) = referrer_kind(plural) else { continue };
                for n in names {
                    add(Rel::UsedBy, Item::new(g, k, ns, n));
                }
            }
            if kind.ar.kind == "Secret" {
                for sa in lists.of("", "ServiceAccount") {
                    let mut names = arr(sa, "/secrets").iter().chain(arr(sa, "/imagePullSecrets"));
                    if names.any(|r| text(r, "/name") == Some(name))
                        && let Some(item) = Item::of("", "ServiceAccount", sa)
                    {
                        add(Rel::UsedBy, item);
                    }
                }
                for ing in lists.of("networking.k8s.io", "Ingress") {
                    if refs::ingress_refs(ing).1.iter().any(|t| t.name == name)
                        && let Some(item) = Item::of("networking.k8s.io", "Ingress", ing)
                    {
                        add(Rel::UsedBy, item.with_note("TLS"));
                    }
                }
                let annotation = |k: &str| text(obj, &format!("/metadata/annotations/{}", k.replace('/', "~1")));
                if let Some(cert) = annotation("cert-manager.io/certificate-name") {
                    add(Rel::PointedToBy, Item::new("cert-manager.io", "Certificate", ns, cert).with_note("issued it"));
                }
                if let Some(sa) = annotation("kubernetes.io/service-account.name") {
                    add(Rel::UsedBy, Item::new("", "ServiceAccount", ns, sa).with_note("token"));
                }
            }
        }
        ("", "ServiceAccount") => {
            for r in arr(obj, "/secrets").iter().chain(arr(obj, "/imagePullSecrets")) {
                if let Some(n) = text(r, "/name") {
                    add(Rel::Uses, Item::new("", "Secret", ns, n));
                }
            }
            pods(Rel::UsedBy, &mut add);
            let me = Target { of: refs::Of::ServiceAccount, namespace: ns.map(String::from), name: name.to_string() };
            for (g, k) in [("apps", "Deployment"), ("apps", "StatefulSet"), ("apps", "DaemonSet"), ("batch", "CronJob")]
            {
                let path = pod_spec_path(&kind_named(g, k)).unwrap_or("/spec/template/spec");
                for w in lists.of(g, k) {
                    let spec = w.pointer(path).unwrap_or(&Value::Null);
                    if refs::spec_refs(ns, spec, true).contains(&me)
                        && let Some(item) = Item::of(g, k, w)
                    {
                        add(Rel::UsedBy, item);
                    }
                }
            }
        }
        ("", "PersistentVolumeClaim") => {
            if let Some(pv) = text(obj, "/spec/volumeName") {
                add(Rel::Uses, Item::new("", "PersistentVolume", None, pv));
            }
            if let Some(class) = text(obj, "/spec/storageClassName") {
                add(Rel::Uses, Item::new("storage.k8s.io", "StorageClass", None, class));
            }
            pods(Rel::UsedBy, &mut add);
        }
        ("", "PersistentVolume") => {
            if let Some(claim) = text(obj, "/spec/claimRef/name") {
                add(Rel::UsedBy, Item::new("", "PersistentVolumeClaim", text(obj, "/spec/claimRef/namespace"), claim));
            }
            if let Some(class) = text(obj, "/spec/storageClassName") {
                add(Rel::Uses, Item::new("storage.k8s.io", "StorageClass", None, class));
            }
        }
        ("", "Service") => {
            pods(Rel::Selects, &mut add);
            traffic_to(&BTreeSet::from([name]), ns, lists, &mut add);
        }
        ("networking.k8s.io", "Ingress") => {
            let (services, secrets) = refs::ingress_refs(obj);
            for t in services {
                add(Rel::SendsTo, backend(&t, lists));
            }
            for t in secrets {
                add(Rel::Uses, Item::target(&t).with_note("TLS"));
            }
            if let Some(class) = text(obj, "/spec/ingressClassName") {
                add(Rel::Uses, Item::new("networking.k8s.io", "IngressClass", None, class));
            }
        }
        (g, "HTTPRoute") if g == refs::GATEWAY => {
            for t in refs::route_refs(obj) {
                let item = if t.namespace.as_deref() == ns { backend(&t, lists) } else { Item::target(&t) };
                add(Rel::SendsTo, item);
            }
            for p in arr(obj, "/spec/parentRefs") {
                let Some(n) = text(p, "/name") else { continue };
                let group = text(p, "/group").unwrap_or(refs::GATEWAY);
                let k = text(p, "/kind").unwrap_or("Gateway");
                add(Rel::Uses, Item::new(group, k, text(p, "/namespace").or(ns), n));
            }
        }
        ("autoscaling", "HorizontalPodAutoscaler") | ("autoscaling.k8s.io", "VerticalPodAutoscaler") => {
            let at = if group == "autoscaling" { "/spec/scaleTargetRef" } else { "/spec/targetRef" };
            if let (Some(k), Some(n)) = (text(obj, &format!("{at}/kind")), text(obj, &format!("{at}/name"))) {
                let version = text(obj, &format!("{at}/apiVersion")).unwrap_or("");
                add(Rel::Scales, Item::by_version(version, k, ns, n));
            }
        }
        ("policy", "PodDisruptionBudget") | ("networking.k8s.io", "NetworkPolicy") => pods(Rel::Selects, &mut add),
        ("", "Node") => pods(Rel::Runs, &mut add),
        ("rbac.authorization.k8s.io", "RoleBinding" | "ClusterRoleBinding") => {
            if let (Some(k), Some(n)) = (text(obj, "/roleRef/kind"), text(obj, "/roleRef/name")) {
                let role_ns = if k == "Role" { ns } else { None };
                add(Rel::Uses, Item::new("rbac.authorization.k8s.io", k, role_ns, n));
            }
            for s in arr(obj, "/subjects") {
                if text(s, "/kind") == Some("ServiceAccount")
                    && let Some(n) = text(s, "/name")
                {
                    add(Rel::Binds, Item::new("", "ServiceAccount", text(s, "/namespace").or(ns), n));
                }
            }
        }
        _ if custom(kind) => {
            for (g, k, v) in lists.all() {
                // The operator's other kinds that name it in their spec.
                if g == group
                    && v.get("spec").is_some_and(|s| names_in_spec(s, &kind.ar.kind, name))
                    && let Some(item) = Item::of(g, k, v)
                {
                    add(Rel::PointedToBy, item);
                }
                // Objects labelled with its name, by a label for its kind or any instance.
                if v.pointer("/metadata/labels")
                    .and_then(Value::as_object)
                    .is_some_and(|l| labelled_with(kind, name, l))
                    && let Some(item) = Item::of(g, k, v)
                {
                    add(Rel::Labelled, item);
                }
            }
        }
        _ => {}
    }
    sections_of(found)
}

/// An Ingress's or `HTTPRoute`'s backend Service, with its pods when they were read.
fn backend(t: &Target, lists: &Lists) -> Item {
    let mut item = Item::target(t);
    let pods: Vec<Item> =
        lists.backends.get(&t.name).into_iter().flatten().filter_map(|p| Item::of("", "Pod", p)).collect();
    item.more_under = pods.len().saturating_sub(SHOWN_AT_MOST);
    item.under = pods.into_iter().take(SHOWN_AT_MOST).collect();
    item
}

/// The Ingresses and `HTTPRoutes` read that send to one of `services` (in `ns`).
fn traffic_to(services: &BTreeSet<&str>, ns: Option<&str>, lists: &Lists, add: &mut dyn FnMut(Rel, Item)) {
    if services.is_empty() {
        return;
    }
    let hits = |targets: Vec<Target>| {
        targets
            .into_iter()
            .find(|t| t.namespace.as_deref() == ns && services.contains(t.name.as_str()))
            .map(|t| format!("to Service {}", t.name))
    };
    for ing in lists.of("networking.k8s.io", "Ingress") {
        if let Some(note) = hits(refs::ingress_refs(ing).0)
            && let Some(item) = Item::of("networking.k8s.io", "Ingress", ing)
        {
            add(Rel::ReceivesFrom, item.with_note(note));
        }
    }
    for route in lists.of(refs::GATEWAY, "HTTPRoute") {
        if let Some(note) = hits(refs::route_refs(route))
            && let Some(item) = Item::of(refs::GATEWAY, "HTTPRoute", route)
        {
            add(Rel::ReceivesFrom, item.with_note(note));
        }
    }
}

#[cfg(test)]
mod tests;
