//! Upgrade check: what needs action before a cluster moves to a newer minor version, most
//! urgent first. For the cluster overview's Upgrade tab and the `upgrade_check` MCP tool.
//!
//! - Removed APIs still used: objects last written in an API version the target no longer
//!   serves (`metadata.managedFields` keeps the version each writer used, and `kubectl apply`
//!   its last-applied configuration), Helm releases whose last manifest names one, and the
//!   API server's `apiserver_requested_deprecated_apis` metric.
//! - Version skew: kubelets and kube-proxies against the target, by the skew policy.
//! - Drains that would block: budgets allowing no disruption, pods under several budgets.
//! - Feature gates the API server reports as deprecated, or alpha and turned on.
//! - Add-ons: CRD versions marked deprecated.

pub mod removed;

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context as _, Result};
use k8s_openapi::api::core::v1::{Node, Pod};
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::k8s::{self, Kind, Selection};
pub use crate::problems::Severity;
use removed::Removal;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Area {
    RemovedApis,
    Skew,
    Drains,
    FeatureGates,
    AddOns,
}

impl Area {
    pub fn label(self) -> &'static str {
        match self {
            Self::RemovedApis => "Removed APIs",
            Self::Skew => "Version skew",
            Self::Drains => "Drains",
            Self::FeatureGates => "Feature gates",
            Self::AddOns => "Add-ons",
        }
    }
}

/// The Helm release whose last manifest names an object.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HelmRef {
    pub namespace: String,
    pub release: String,
    pub revision: i64,
}

impl HelmRef {
    /// The Secret Helm keeps the revision in.
    pub fn secret_name(&self) -> String {
        format!("sh.helm.release.v1.{}.v{}", self.release, self.revision)
    }
}

/// An object a finding is about, to link to.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Object {
    /// kubectl-style kind: `horizontalpodautoscalers.autoscaling`, `pods`.
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
    /// How it is concerned: "written by helm in autoscaling/v2beta2", "on node a".
    pub note: String,
    /// Named by this Helm release's manifest (the object itself may not exist).
    pub helm: Option<HelmRef>,
}

impl Object {
    fn new(kind: &str, namespace: Option<&str>, name: &str, note: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            namespace: namespace.map(String::from),
            name: name.into(),
            note: note.into(),
            helm: None,
        }
    }

    /// `namespace/name`, or the name of a cluster-scoped object.
    pub fn path(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("{ns}/{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// One thing to act on. Its title never names an object (the objects say which), so the MCP
/// tool can leave out the ones an agent may not see.
#[derive(Clone, Debug)]
pub struct Finding {
    pub severity: Severity,
    pub area: Area,
    pub title: String,
    /// What to do.
    pub action: String,
    /// What was seen without an object to show (the API server's metrics).
    pub details: Vec<String>,
    pub objects: Vec<Object>,
}

pub struct Report {
    /// The cluster's minor version (1.x) and the one checked against.
    pub current: u32,
    pub target: u32,
    /// Most urgent first.
    pub findings: Vec<Finding>,
    /// The parts that could not be checked, and why: "Pods: forbidden (… not checked)".
    pub skipped: Vec<String>,
}

/// The minor version of a Kubernetes 1.x version: `v1.30.4-eks-1a2b`, `1.31`, `v1.29.0+k3s1`.
pub fn minor_of(version: &str) -> Option<u32> {
    let version = version.trim();
    let rest = version.strip_prefix('v').unwrap_or(version).strip_prefix("1.")?;
    rest[..rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len())].parse().ok()
}

/// How many minor versions a kubelet or kube-proxy may be older than the API server: three,
/// two for those older than 1.25 (the version skew policy).
fn max_lag(minor: u32) -> u32 {
    if minor >= 25 { 3 } else { 2 }
}

/// The oldest kubelet or kube-proxy minor version an API server at `target` works with.
pub fn oldest_node_version(target: u32) -> u32 {
    (0..=target).find(|m| target - m <= max_lag(*m)).unwrap_or(target)
}

/// Check a cluster against minor version `target` (1.x; the next one when None). `kinds` are
/// the types the cluster serves, to find the objects written in removed API versions.
pub async fn check(client: Client, kinds: Result<Vec<Kind>>, target: Option<u32>) -> Result<Report> {
    let c = client.clone();
    let version: Value =
        k8s::run(async move { Ok(c.request(http::Request::get("/version").body(Vec::new())?).await?) }).await?;
    let current = version
        .get("gitVersion")
        .and_then(Value::as_str)
        .and_then(minor_of)
        .context("the API server's version is not a Kubernetes 1.x version")?;
    let target = target.unwrap_or(current + 1);
    anyhow::ensure!(target > current, "1.{target} is not newer than the cluster's 1.{current}");
    let (live, helm, metrics, nodes, pods, pdbs, crds) = futures::join!(
        live_uses(client.clone(), kinds, current, target),
        helm_uses(client.clone(), target),
        api_metrics(client.clone()),
        k8s::lists::list_lean::<NodeLite>(client.clone(), k8s::ar_of::<Node>(), None),
        k8s::lists::list_lean_cached::<PodLite>(client.clone(), k8s::ar_of::<Pod>(), None),
        k8s::list_values(client.clone(), k8s::ar_of::<PodDisruptionBudget>(), None, None),
        k8s::lists::list_lean::<CrdLite>(client, k8s::ar_of::<CustomResourceDefinition>(), None),
    );
    Ok(assemble(Answers { current, target, live, helm, metrics, nodes, pods, pdbs, crds }))
}

/// A removed API version used by an object.
type Use = (&'static Removal, Object);

/// What a cluster answered, each part on its own.
struct Answers {
    current: u32,
    target: u32,
    /// Objects written in a removed API version, and the kinds that could not be listed.
    live: Result<(Vec<Use>, Vec<String>)>,
    /// Objects of Helm manifests in a removed API version, and how many releases could
    /// not be read.
    helm: Result<(Vec<Use>, usize)>,
    metrics: Result<ApiMetrics>,
    nodes: Result<Vec<NodeLite>>,
    pods: Result<Vec<PodLite>>,
    pdbs: Result<Vec<Value>>,
    crds: Result<Vec<CrdLite>>,
}

/// "Pods: forbidden (drains not checked)".
fn not_checked(what: &str, e: &anyhow::Error, missing: &str) -> String {
    format!("{what}: {} ({missing} not checked)", k8s::lists::load_error(e))
}

/// The findings from what was answered: a part that failed is said, and doesn't hide the
/// others (a user who can't list pods still sees the removed APIs).
fn assemble(a: Answers) -> Report {
    let (current, target) = (a.current, a.target);
    let mut skipped = Vec::new();
    let mut uses = Vec::new();
    match a.live {
        Ok((found, failed)) => {
            uses.extend(found);
            skipped.extend(failed);
        }
        Err(e) => skipped.push(not_checked("Resource types", &e, "objects in removed API versions")),
    }
    match a.helm {
        Ok((found, unreadable)) => {
            uses.extend(found);
            if unreadable > 0 {
                skipped.push(format!("Helm: {unreadable} release(s) could not be read (their manifests not checked)"));
            }
        }
        Err(e) => skipped.push(not_checked("Helm releases", &e, "their manifests")),
    }
    let (calls, gates) = match a.metrics {
        Ok(m) => (m.deprecated, Some(m.gates)),
        Err(e) => {
            skipped.push(not_checked("API server metrics", &e, "deprecated API calls and feature gates"));
            (Vec::new(), None)
        }
    };
    let mut findings = removed_api_findings(current, target, uses, &calls);
    findings.extend(control_plane_hop(current, target));
    let nodes = match a.nodes {
        Ok(nodes) => Some(nodes),
        Err(e) => {
            skipped.push(not_checked("Nodes", &e, "kubelet versions"));
            None
        }
    };
    let pods = match a.pods {
        Ok(pods) => Some(pods),
        Err(e) => {
            skipped.push(not_checked("Pods", &e, "kube-proxy versions and pods under several budgets"));
            None
        }
    };
    if let Some(nodes) = &nodes {
        findings.extend(kubelet_skew(target, nodes));
    }
    if let Some(pods) = &pods {
        findings.extend(kube_proxy_skew(target, pods, nodes.as_deref()));
    }
    match a.pdbs {
        Ok(pdbs) => findings.extend(drain_findings(&pdbs, pods.as_deref())),
        Err(e) => skipped.push(not_checked("PodDisruptionBudgets", &e, "drains")),
    }
    if let Some(gates) = gates {
        findings.extend(gate_findings(target, &gates));
    }
    match a.crds {
        Ok(crds) => findings.extend(crd_findings(&crds)),
        Err(e) => skipped.push(not_checked("CRDs", &e, "add-on versions")),
    }
    findings.sort_by(|x, y| (x.severity, x.area, &x.title).cmp(&(y.severity, y.area, &y.title)));
    Report { current, target, findings, skipped }
}

// MARK: Removed APIs

/// One finding per removed API version and kind, with every object using it and what the
/// API server's metrics say of it; a metric for an API the table doesn't know is its own.
fn removed_api_findings(current: u32, target: u32, uses: Vec<Use>, calls: &[DeprecatedCall]) -> Vec<Finding> {
    let index = |r: &Removal| removed::REMOVALS.iter().position(|o| std::ptr::eq(o, r)).unwrap_or(usize::MAX);
    let mut by: BTreeMap<usize, (Vec<Object>, Vec<String>)> = BTreeMap::new();
    for (r, o) in uses {
        by.entry(index(r)).or_default().0.push(o);
    }
    let mut findings = Vec::new();
    for c in calls {
        // Not removed (yet), or gone already: nobody can be calling it then.
        let Some(removed_in) = minor_of(&c.removed_release).filter(|m| *m > current && *m <= target) else {
            continue;
        };
        let gv = if c.group.is_empty() { c.version.clone() } else { format!("{}/{}", c.group, c.version) };
        let resource =
            if c.subresource.is_empty() { c.resource.clone() } else { format!("{}/{}", c.resource, c.subresource) };
        let line = format!(
            "Clients asked the API server for {resource} in {gv} since it started \
             (apiserver_requested_deprecated_apis): its audit log names them"
        );
        match removed::find_resource(&c.group, &c.version, &c.resource) {
            Some(r) => by.entry(index(r)).or_default().1.push(line),
            None => findings.push(Finding {
                severity: Severity::Critical,
                area: Area::RemovedApis,
                title: format!("{resource} {gv}: removed in 1.{removed_in}"),
                action: format!("Move the clients that call {resource} {gv} to a version 1.{removed_in} serves"),
                details: vec![line],
                objects: Vec::new(),
            }),
        }
    }
    for (i, (mut objects, mut details)) in by {
        let Some(r) = removed::REMOVALS.get(i) else { continue };
        objects.sort();
        objects.dedup();
        details.dedup();
        let mut action = r.advice(target);
        // Said in the action, not the details: details are what is shown without objects.
        if objects.iter().any(|o| o.helm.is_some()) {
            action.push_str(
                ". Helm builds an upgrade from the release's last manifest: upgrade those charts to versions that \
                 use the new API before the cluster, or fix their stored manifests with the helm-mapkubeapis plugin",
            );
        }
        let already = if r.removed_in <= current { " already" } else { "" };
        findings.push(Finding {
            severity: Severity::Critical,
            area: Area::RemovedApis,
            title: format!("{} {}: removed in 1.{}{already}", r.kind, r.api_version, r.removed_in),
            action,
            details,
            objects,
        });
    }
    findings
}

// Only the fields the check reads: the objects of a kind are never held as JSON.

#[derive(Deserialize, Default)]
#[serde(default)]
struct ObjectLite {
    metadata: ObjectMetaLite,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ObjectMetaLite {
    name: String,
    namespace: Option<String>,
    managed_fields: Vec<ManagedLite>,
    annotations: AnnotationsLite,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct ManagedLite {
    manager: String,
    api_version: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AnnotationsLite {
    #[serde(rename = "kubectl.kubernetes.io/last-applied-configuration")]
    last_applied: Option<String>,
}

/// An object's API version, kind and name, from a manifest.
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct Head {
    api_version: String,
    kind: String,
    metadata: HeadMeta,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct HeadMeta {
    name: String,
    namespace: Option<String>,
}

/// The kind a removal is about as the cluster serves it, at a version not removed when it
/// can: in the removed version's group or the replacement's (extensions → apps).
fn served_kind<'a>(kinds: &'a [Kind], r: &Removal) -> Option<&'a Kind> {
    let to = r.replacement.and_then(|v| v.split_once('/')).map_or("", |(g, _)| g);
    // Core Events are the same objects as events.k8s.io's, which discovery leaves out.
    let core = (r.kind == "Event").then_some("");
    kinds
        .iter()
        .filter(|k| {
            k.ar.kind == r.kind && (k.ar.group == r.group() || k.ar.group == to || Some(k.ar.group.as_str()) == core)
        })
        .min_by_key(|k| removed::find(&k.ar.api_version, &k.ar.kind).is_some())
}

/// The objects of the kinds removals after `current` up to `target` are about, each kind
/// listed once, that were written in a removed version.
async fn live_uses(
    client: Client,
    kinds: Result<Vec<Kind>>,
    current: u32,
    target: u32,
) -> Result<(Vec<Use>, Vec<String>)> {
    let kinds = kinds?;
    let mut wanted: BTreeMap<String, (Kind, Vec<&'static Removal>)> = BTreeMap::new();
    for r in removed::between(current, target) {
        // A kind not served (TokenReview and the other create-only reviews) stores nothing.
        if let Some(kind) = served_kind(&kinds, r) {
            wanted.entry(kind.key()).or_insert_with(|| (kind.clone(), Vec::new())).1.push(r);
        }
    }
    let lists = futures::future::join_all(wanted.into_values().map(|(kind, removals)| {
        let client = client.clone();
        async move {
            let items = k8s::lists::list_lean::<ObjectLite>(client, kind.ar.clone(), None).await;
            (kind, removals, items)
        }
    }))
    .await;
    let (mut found, mut failed) = (Vec::new(), Vec::new());
    for (kind, removals, items) in lists {
        match items {
            Ok(items) => found.extend(object_uses(&kind, &removals, &items)),
            Err(e) => failed.push(not_checked(&kind.label, &e, "their API versions")),
        }
    }
    Ok((found, failed))
}

/// The objects of `kind` written in one of `removals`' API versions: by a field manager
/// (`metadata.managedFields` keeps the version each one wrote with) or by `kubectl apply`
/// (its last-applied configuration).
fn object_uses(kind: &Kind, removals: &[&'static Removal], items: &[ObjectLite]) -> Vec<Use> {
    let mut out = Vec::new();
    for item in items {
        let m = &item.metadata;
        let applied = m.annotations.last_applied.as_deref().and_then(|t| serde_json::from_str::<Head>(t).ok());
        for r in removals.iter().filter(|r| r.kind == kind.ar.kind) {
            let mut how: Vec<String> = m
                .managed_fields
                .iter()
                .filter(|f| f.api_version == r.api_version)
                .map(|f| {
                    if f.manager.is_empty() {
                        "written by a client".into()
                    } else {
                        format!("written by {}", f.manager)
                    }
                })
                .collect();
            how.sort();
            how.dedup();
            if applied.as_ref().is_some_and(|h| h.api_version == r.api_version) {
                how.push("kubectl apply's last configuration".into());
            }
            if !how.is_empty() {
                let note = format!("{} in {}", how.join(", "), r.api_version);
                out.push((*r, Object::new(&kind.kubectl_name(), m.namespace.as_deref(), &m.name, note)));
            }
        }
    }
    out
}

/// The objects of Helm manifests in an API version removed up to `target`, before the
/// cluster's version too: Helm can't upgrade a release whose last manifest names one.
async fn helm_uses(client: Client, target: u32) -> Result<(Vec<Use>, usize)> {
    let (releases, unreadable) = crate::helm::scan_manifests(client, move |r| {
        // An uninstalled release kept for its history is not upgraded.
        if r.status == "uninstalled" { Vec::new() } else { manifest_uses(r, target) }
    })
    .await?;
    Ok((releases.into_iter().flat_map(|(_, uses)| uses).collect(), unreadable))
}

/// The objects of a release's manifest in an API version removed up to `target`.
fn manifest_uses(release: &crate::helm::Release, target: u32) -> Vec<Use> {
    let mut out = Vec::new();
    for (_, doc) in crate::ui::apply_view::split_docs(&release.manifest) {
        // Most documents are in a version still served: parse only the others.
        let Some(version) = doc.lines().find_map(|l| l.strip_prefix("apiVersion:")) else { continue };
        if !removed::removes_version(version.trim().trim_matches(['"', '\'']), target) {
            continue;
        }
        let Ok(head) = crate::yaml::from_str::<Head>(&doc) else { continue };
        let Some(r) = removed::find(&head.api_version, &head.kind).filter(|r| r.removed_in <= target) else {
            continue;
        };
        let namespace =
            r.namespaced.then(|| head.metadata.namespace.clone().unwrap_or_else(|| release.namespace.clone()));
        let helm =
            HelmRef { namespace: release.namespace.clone(), release: release.name.clone(), revision: release.revision };
        let note = format!("in Helm release {}/{} (revision {})", helm.namespace, helm.release, helm.revision);
        let mut object = Object::new(&r.kubectl_kind(), namespace.as_deref(), &head.metadata.name, note);
        object.helm = Some(helm);
        out.push((r, object));
    }
    out
}

// MARK: The API server's metrics

#[derive(Default, Debug, PartialEq)]
struct ApiMetrics {
    deprecated: Vec<DeprecatedCall>,
    gates: Vec<Gate>,
}

/// A deprecated API the API server was asked for since it started.
#[derive(Default, Debug, PartialEq)]
struct DeprecatedCall {
    group: String,
    version: String,
    resource: String,
    subresource: String,
    /// "1.32", or empty when no release removes it yet.
    removed_release: String,
}

#[derive(Debug, PartialEq)]
struct Gate {
    name: String,
    /// ALPHA, BETA, DEPRECATED, or empty once GA.
    stage: String,
    enabled: bool,
}

impl ApiMetrics {
    /// Keep what the check reads from one line of the API server's `/metrics`.
    fn add(&mut self, line: &str) {
        if let Some((labels, value)) = sample(line, "apiserver_requested_deprecated_apis") {
            let get = |k: &str| labels.iter().find(|(l, _)| l == k).map_or_else(String::new, |(_, v)| v.clone());
            if value > 0.0 {
                self.deprecated.push(DeprecatedCall {
                    group: get("group"),
                    version: get("version"),
                    resource: get("resource"),
                    subresource: get("subresource"),
                    removed_release: get("removed_release"),
                });
            }
        } else if let Some((labels, value)) = sample(line, "kubernetes_feature_enabled") {
            let get = |k: &str| labels.iter().find(|(l, _)| l == k).map_or_else(String::new, |(_, v)| v.clone());
            self.gates.push(Gate { name: get("name"), stage: get("stage"), enabled: value > 0.0 });
        }
    }
}

/// The labels and value of a sample of metric `name`: `name{a="b",c="d"} 1`.
fn sample(line: &str, name: &str) -> Option<(Vec<(String, String)>, f64)> {
    let mut rest = line.strip_prefix(name)?.strip_prefix('{')?;
    let mut labels = Vec::new();
    loop {
        rest = rest.trim_start_matches([',', ' ']);
        if let Some(after) = rest.strip_prefix('}') {
            rest = after;
            break;
        }
        let (key, after) = rest.split_once("=\"")?;
        let mut value = String::new();
        let mut chars = after.char_indices();
        let end = loop {
            match chars.next()? {
                (i, '"') => break i,
                (_, '\\') => match chars.next()?.1 {
                    'n' => value.push('\n'),
                    c => value.push(c),
                },
                (_, c) => value.push(c),
            }
        };
        labels.push((key.trim().to_string(), value));
        rest = &after[end + 1..];
    }
    Some((labels, rest.split_whitespace().next()?.parse().ok()?))
}

/// The deprecated APIs called and the feature gates, read line by line as `/metrics` arrives
/// (it runs to megabytes).
async fn api_metrics(client: Client) -> Result<ApiMetrics> {
    k8s::run(async move {
        use futures::{AsyncBufReadExt as _, StreamExt as _};
        let body = client.request_stream(http::Request::get("/metrics").body(Vec::new())?).await?;
        let mut lines = std::pin::pin!(body.lines());
        let mut metrics = ApiMetrics::default();
        while let Some(line) = lines.next().await {
            metrics.add(&line?);
        }
        Ok(metrics)
    })
    .await
}

/// Gates deprecated (a later release removes them, and an API server whose flags name a
/// removed gate does not start), and alpha ones turned on.
fn gate_findings(target: u32, gates: &[Gate]) -> Vec<Finding> {
    let names = |pick: &dyn Fn(&Gate) -> bool| -> Vec<String> {
        let mut names: Vec<String> = gates
            .iter()
            .filter(|g| pick(g))
            .map(|g| format!("{} ({})", g.name, if g.enabled { "on" } else { "off" }))
            .collect();
        names.sort();
        names.dedup();
        names
    };
    let mut out = Vec::new();
    let deprecated = names(&|g: &Gate| g.stage == "DEPRECATED");
    if !deprecated.is_empty() {
        out.push(Finding {
            severity: Severity::Info,
            area: Area::FeatureGates,
            title: format!("{} deprecated feature gate(s) on the API server", deprecated.len()),
            action: "A later release removes them, and an API server whose --feature-gates names a removed gate \
                     does not start: take them out of its flags"
                .into(),
            details: deprecated,
            objects: Vec::new(),
        });
    }
    let alpha = names(&|g: &Gate| g.stage == "ALPHA" && g.enabled);
    if !alpha.is_empty() {
        out.push(Finding {
            severity: Severity::Info,
            area: Area::FeatureGates,
            title: format!("{} alpha feature gate(s) turned on", alpha.len()),
            action: format!(
                "Alpha features change or go away in any release: check the 1.{target} release notes for them"
            ),
            details: alpha,
            objects: Vec::new(),
        });
    }
    out
}

// MARK: Version skew

#[derive(Deserialize, Default)]
#[serde(default)]
struct NameLite {
    name: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NodeLite {
    metadata: NameLite,
    status: NodeStatusLite,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct NodeStatusLite {
    node_info: NodeInfoLite,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct NodeInfoLite {
    kubelet_version: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PodLite {
    metadata: PodMetaLite,
    spec: PodSpecLite,
    status: PodStatusLite,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PodMetaLite {
    name: String,
    namespace: String,
    labels: serde_json::Map<String, Value>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct PodSpecLite {
    node_name: String,
    containers: Vec<ContainerLite>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ContainerLite {
    image: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PodStatusLite {
    phase: String,
}

/// The control plane goes up one minor version at a time.
fn control_plane_hop(current: u32, target: u32) -> Option<Finding> {
    (target > current + 1).then(|| Finding {
        severity: Severity::Warning,
        area: Area::Skew,
        title: format!("1.{current} → 1.{target} skips {} minor version(s)", target - current - 1),
        action: format!(
            "The control plane upgrades one minor version at a time: {}",
            (current..=target).map(|m| format!("1.{m}")).collect::<Vec<_>>().join(" → ")
        ),
        details: Vec::new(),
        objects: Vec::new(),
    })
}

/// Nodes whose kubelet is too old for the target, or newer than it, by version.
fn kubelet_skew(target: u32, nodes: &[NodeLite]) -> Vec<Finding> {
    let mut by_minor: BTreeMap<u32, Vec<&str>> = BTreeMap::new();
    for n in nodes {
        if let Some(m) = minor_of(&n.status.node_info.kubelet_version) {
            by_minor.entry(m).or_default().push(&n.metadata.name);
        }
    }
    let oldest = oldest_node_version(target);
    let mut out = Vec::new();
    for (m, names) in by_minor {
        let objects = names.iter().map(|n| Object::new("nodes", None, n, format!("kubelet 1.{m}"))).collect();
        if m < oldest {
            out.push(Finding {
                severity: Severity::Critical,
                area: Area::Skew,
                title: format!(
                    "{} node(s) on kubelet 1.{m}, {} minor versions behind 1.{target}",
                    names.len(),
                    target - m
                ),
                action: format!(
                    "Upgrade or replace them to kubelet 1.{oldest} or newer before the control plane goes to \
                     1.{target}: a kubelet may be at most {} minor versions older than the API server",
                    max_lag(m)
                ),
                details: Vec::new(),
                objects,
            });
        } else if m > target {
            out.push(Finding {
                severity: Severity::Warning,
                area: Area::Skew,
                title: format!("{} node(s) on kubelet 1.{m}, newer than 1.{target}", names.len()),
                action: format!("A kubelet must not be newer than the API server: go to 1.{m} or later"),
                details: Vec::new(),
                objects,
            });
        }
    }
    out
}

/// The minor version of a kube-proxy image: `registry.k8s.io/kube-proxy:v1.30.2`,
/// `…/eks/kube-proxy:v1.29.0-minimal-eksbuild.1`, `gke.gcr.io/kube-proxy-amd64:v1.29.1-gke.1`.
fn kube_proxy_minor(image: &str) -> Option<u32> {
    let image = image.split('@').next()?;
    let (name, tag) = image.rsplit('/').next()?.split_once(':')?;
    name.starts_with("kube-proxy").then(|| minor_of(tag)).flatten()
}

/// kube-proxy pods (in kube-system, by their image) too old or too new for the target, or
/// more than three minor versions from the kubelet next to them.
fn kube_proxy_skew(target: u32, pods: &[PodLite], nodes: Option<&[NodeLite]>) -> Vec<Finding> {
    let kubelets: HashMap<&str, u32> = nodes
        .into_iter()
        .flatten()
        .filter_map(|n| Some((n.metadata.name.as_str(), minor_of(&n.status.node_info.kubelet_version)?)))
        .collect();
    let oldest = oldest_node_version(target);
    let mut too_old: BTreeMap<u32, Vec<Object>> = BTreeMap::new();
    let mut too_new: BTreeMap<u32, Vec<Object>> = BTreeMap::new();
    let mut apart = Vec::new();
    for p in pods.iter().filter(|p| p.metadata.namespace == "kube-system") {
        let Some(m) = p.spec.containers.iter().find_map(|c| kube_proxy_minor(&c.image)) else { continue };
        let node = &p.spec.node_name;
        let object = Object::new("pods", Some("kube-system"), &p.metadata.name, format!("kube-proxy 1.{m} on {node}"));
        if m < oldest {
            too_old.entry(m).or_default().push(object.clone());
        } else if m > target {
            too_new.entry(m).or_default().push(object.clone());
        }
        if let Some(k) = kubelets.get(node.as_str())
            && k.abs_diff(m) > 3
        {
            apart.push(Object { note: format!("kube-proxy 1.{m}, kubelet 1.{k} on {node}"), ..object });
        }
    }
    let mut out = Vec::new();
    for (m, objects) in too_old {
        out.push(Finding {
            severity: Severity::Critical,
            area: Area::Skew,
            title: format!("kube-proxy 1.{m}: {} minor versions behind 1.{target}", target - m),
            action: format!(
                "Upgrade kube-proxy to 1.{oldest} or newer before the control plane goes to 1.{target}: it may be \
                 at most {} minor versions older than the API server",
                max_lag(m)
            ),
            details: Vec::new(),
            objects,
        });
    }
    for (m, objects) in too_new {
        out.push(Finding {
            severity: Severity::Warning,
            area: Area::Skew,
            title: format!("kube-proxy 1.{m}, newer than 1.{target}"),
            action: format!("kube-proxy must not be newer than the API server: go to 1.{m} or later"),
            details: Vec::new(),
            objects,
        });
    }
    if !apart.is_empty() {
        out.push(Finding {
            severity: Severity::Warning,
            area: Area::Skew,
            title: "kube-proxy more than three minor versions from its node's kubelet".into(),
            action: "Bring kube-proxy and the kubelet within three minor versions of each other".into(),
            details: Vec::new(),
            objects: apart,
        });
    }
    out
}

// MARK: Drains

const PDB_KIND: &str = "poddisruptionbudgets.policy";

fn pdb_kind() -> Kind {
    Kind {
        ar: k8s::ar_of::<PodDisruptionBudget>(),
        namespaced: true,
        section: "Configuration",
        label: "Disruption Budgets".into(),
    }
}

/// Whether a PodDisruptionBudget selects a pod with these labels, as the eviction API decides.
fn pdb_selects(kind: &Kind, pdb: &Value, labels: &serde_json::Map<String, Value>) -> bool {
    match k8s::pod_selection(kind, pdb) {
        Selection::Nothing => false,
        Selection::Everything => true,
        Selection::Labels(_) => pdb.pointer("/spec/selector").is_some_and(|s| k8s::selector_matches(s, labels)),
    }
}

/// "minAvailable 1", "maxUnavailable 0".
fn budget_text(pdb: &Value) -> String {
    ["minAvailable", "maxUnavailable"]
        .iter()
        .filter_map(|k| {
            let v = pdb.pointer(&format!("/spec/{k}"))?;
            Some(format!("{k} {}", v.as_str().map_or_else(|| v.to_string(), String::from)))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Budgets that allow no disruption now (a drain waits on their pods), and pods several
/// budgets select (the eviction API refuses them). Pods unknown: only the budgets.
fn drain_findings(pdbs: &[Value], pods: Option<&[PodLite]>) -> Vec<Finding> {
    let kind = pdb_kind();
    let text = |v: &Value, p: &str| v.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string();
    let int = |v: &Value, p: &str| v.pointer(p).and_then(Value::as_i64);
    // Finished pods are evicted whatever the budgets say.
    let live: Vec<&PodLite> =
        pods.into_iter().flatten().filter(|p| !matches!(p.status.phase.as_str(), "Succeeded" | "Failed")).collect();
    let mut pods_by_ns: HashMap<&str, Vec<&PodLite>> = HashMap::new();
    for &p in &live {
        pods_by_ns.entry(p.metadata.namespace.as_str()).or_default().push(p);
    }
    let pod_object = |p: &PodLite| {
        let on = if p.spec.node_name.is_empty() { String::new() } else { format!("on node {}", p.spec.node_name) };
        Object::new("pods", Some(p.metadata.namespace.as_str()), &p.metadata.name, on)
    };
    let mut findings = Vec::new();
    let mut by_ns: HashMap<String, Vec<(String, &Value)>> = HashMap::new();
    for pdb in pdbs {
        let (ns, name) = (text(pdb, "/metadata/namespace"), text(pdb, "/metadata/name"));
        by_ns.entry(ns.clone()).or_default().push((name.clone(), pdb));
        // Not reconciled yet, or guarding no pod: nothing to block.
        let (Some(allowed), Some(expected)) =
            (int(pdb, "/status/disruptionsAllowed"), int(pdb, "/status/expectedPods"))
        else {
            continue;
        };
        if allowed > 0 || expected == 0 {
            continue;
        }
        let mut objects = vec![Object::new(PDB_KIND, Some(ns.as_str()), &name, budget_text(pdb))];
        let guarded = pods_by_ns.get(ns.as_str()).into_iter().flatten();
        objects.extend(guarded.filter(|p| pdb_selects(&kind, pdb, &p.metadata.labels)).map(|&p| pod_object(p)));
        let (title, action) = if expected == 1 {
            (
                "A PodDisruptionBudget guards a single pod: draining its node waits forever".to_string(),
                "Run 2 or more replicas, or let the budget allow 1 unavailable pod (maxUnavailable: 1) during the \
                 upgrade",
            )
        } else {
            let healthy = int(pdb, "/status/currentHealthy").unwrap_or(0);
            (
                format!("A PodDisruptionBudget allows no disruption ({healthy} of {expected} pods healthy)"),
                "Drains stop on its pods until it allows one: fix its unhealthy pods, or loosen minAvailable or \
                 maxUnavailable for the upgrade",
            )
        };
        findings.push(Finding {
            severity: Severity::Warning,
            area: Area::Drains,
            title,
            action: action.into(),
            details: Vec::new(),
            objects,
        });
    }
    // Pods under several budgets, grouped by the budgets that select them.
    let mut shared: BTreeMap<(String, Vec<String>), Vec<Object>> = BTreeMap::new();
    for p in live {
        let ns = p.metadata.namespace.as_str();
        let mut names: Vec<String> = by_ns
            .get(ns)
            .into_iter()
            .flatten()
            .filter(|(_, pdb)| pdb_selects(&kind, pdb, &p.metadata.labels))
            .map(|(name, _)| name.clone())
            .collect();
        if names.len() > 1 {
            names.sort();
            shared.entry((ns.to_string(), names)).or_default().push(pod_object(p));
        }
    }
    for ((ns, names), pods) in shared {
        let mut objects: Vec<Object> = names.iter().map(|n| Object::new(PDB_KIND, Some(ns.as_str()), n, "")).collect();
        let count = pods.len();
        objects.extend(pods);
        findings.push(Finding {
            severity: Severity::Warning,
            area: Area::Drains,
            title: format!("{count} pod(s) selected by {} PodDisruptionBudgets", names.len()),
            action: "The eviction API refuses a pod that more than one budget selects, so drains stop on these: make \
                     the budgets' selectors disjoint"
                .into(),
            details: Vec::new(),
            objects,
        });
    }
    findings
}

// MARK: Add-ons

#[derive(Deserialize, Default)]
#[serde(default)]
struct CrdLite {
    metadata: NameLite,
    spec: CrdSpecLite,
    status: CrdStatusLite,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CrdSpecLite {
    group: String,
    names: CrdNamesLite,
    versions: Vec<CrdVersionLite>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CrdNamesLite {
    kind: String,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct CrdVersionLite {
    name: String,
    served: bool,
    deprecated: bool,
    deprecation_warning: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct CrdStatusLite {
    stored_versions: Vec<String>,
}

/// CRDs serving versions their add-on marks deprecated.
fn crd_findings(crds: &[CrdLite]) -> Vec<Finding> {
    let mut out = Vec::new();
    for crd in crds {
        let deprecated: Vec<&CrdVersionLite> = crd.spec.versions.iter().filter(|v| v.served && v.deprecated).collect();
        if deprecated.is_empty() {
            continue;
        }
        let versions: Vec<&str> = deprecated.iter().map(|v| v.name.as_str()).collect();
        let to = (crd.spec.versions.iter())
            .filter(|v| v.served && !v.deprecated)
            .max_by_key(|v| kube::core::Version::parse(&v.name).priority());
        let action = match to {
            Some(v) => format!("Move manifests and clients to {}/{}", crd.spec.group, v.name),
            None => "The add-on deprecates every version it serves: check its upgrade notes".into(),
        };
        let mut notes = vec![crd.spec.names.kind.clone()];
        notes.extend(deprecated.iter().filter_map(|v| v.deprecation_warning.clone()));
        let stored: Vec<&str> =
            versions.iter().copied().filter(|v| crd.status.stored_versions.iter().any(|s| s == v)).collect();
        if !stored.is_empty() {
            notes.push(format!(
                "objects may still be stored in {}: rewrite them, then drop it from status.storedVersions, before \
                 the add-on stops serving it",
                stored.join(", ")
            ));
        }
        out.push(Finding {
            severity: Severity::Warning,
            area: Area::AddOns,
            title: format!("Deprecated custom resource versions served: {}", versions.join(", ")),
            action,
            details: Vec::new(),
            objects: vec![Object::new(
                "customresourcedefinitions.apiextensions.k8s.io",
                None,
                &crd.metadata.name,
                notes.join(" · "),
            )],
        });
    }
    out
}

// MARK: Text

/// Objects listed per finding in the text.
const TEXT_OBJECTS: usize = 50;

/// The report as text, for the `upgrade_check` tool, with only the objects `visible` lets
/// through: a finding left with none (and nothing else to say) is counted, not shown.
pub fn to_text(context: &str, report: &Report, visible: impl Fn(&Object) -> bool) -> String {
    let mut out = format!("Upgrade check of {context}: Kubernetes 1.{} → 1.{}\n", report.current, report.target);
    let (mut shown, mut left_out) = (0, 0);
    for f in &report.findings {
        let objects: Vec<&Object> = f.objects.iter().filter(|o| visible(o)).collect();
        let hidden = f.objects.len() - objects.len();
        if objects.is_empty() && hidden > 0 && f.details.is_empty() {
            left_out += 1;
            continue;
        }
        shown += 1;
        let severity = format!("{:?}", f.severity).to_uppercase();
        out.push_str(&format!("\n{severity} · {} · {}\n  To do: {}\n", f.area.label(), f.title, f.action));
        for d in &f.details {
            out.push_str(&format!("  {d}\n"));
        }
        for o in objects.iter().take(TEXT_OBJECTS) {
            let note = if o.note.is_empty() { String::new() } else { format!(": {}", o.note) };
            out.push_str(&format!("  - {} {}{note}\n", o.kind, o.path()));
        }
        if objects.len() > TEXT_OBJECTS {
            out.push_str(&format!("  … and {} more\n", objects.len() - TEXT_OBJECTS));
        }
        if hidden > 0 {
            out.push_str(&format!("  ({hidden} more you may not see)\n"));
        }
    }
    if shown == 0 {
        out.push_str(if report.skipped.is_empty() {
            "\nNothing to do found.\n"
        } else {
            "\nNothing to do found in what could be read.\n"
        });
    }
    if left_out > 0 {
        out.push_str(&format!("\n{left_out} finding(s) about objects you may not see are left out.\n"));
    }
    if !report.skipped.is_empty() {
        out.push_str(&format!("\nNot checked: {}\n", report.skipped.join("; ")));
    }
    out
}

#[cfg(test)]
mod tests;
