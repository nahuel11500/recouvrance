//! API discovery: the resource types a cluster serves and where they go in the sidebar.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use futures::StreamExt as _;
use k8s_openapi::api::core::v1::Namespace;
use kube::Client;
use kube::api::{Api, ListParams};
use kube::core::discovery::v2::APIGroupDiscovery;
use kube::discovery::{ApiCapabilities, ApiResource, Scope, verbs};

use super::run;

#[derive(Clone, Debug)]
pub struct Kind {
    pub ar: ApiResource,
    pub namespaced: bool,
    /// Where the sidebar lists it (`SECTIONS`); `HIDDEN_SECTION` for a kind discovered that
    /// the sidebar leaves out.
    pub section: &'static str,
    pub label: String,
}

impl Kind {
    /// A namespaced kind of `group`/v1 for tests: plural from the kind (`Ingress` →
    /// `ingresses`), labelled with its kind.
    #[cfg(test)]
    pub fn fake(group: &str, kind: &str) -> Self {
        let lower = kind.to_lowercase();
        Self {
            ar: ApiResource {
                group: group.into(),
                version: "v1".into(),
                api_version: if group.is_empty() { "v1".into() } else { format!("{group}/v1") },
                kind: kind.into(),
                plural: if lower.ends_with("ss") { format!("{lower}es") } else { format!("{lower}s") },
            },
            namespaced: true,
            section: "",
            label: kind.into(),
        }
    }

    pub fn key(&self) -> String {
        format!("{}/{}", self.ar.api_version, self.ar.plural)
    }

    /// The name kubectl accepts: `pods`, `certificates.cert-manager.io`.
    pub fn kubectl_name(&self) -> String {
        if self.ar.group.is_empty() { self.ar.plural.clone() } else { format!("{}.{}", self.ar.plural, self.ar.group) }
    }

    pub fn is(&self, group: &str, kind: &str) -> bool {
        self.ar.group == group && self.ar.kind == kind
    }
}

/// Sidebar order and labels for the built-in kinds: (section, group, kind, label).
pub(super) const KNOWN: &[(&str, &str, &str, &str)] = &[
    ("Cluster", "", "Node", "Nodes"),
    ("Cluster", "", "Namespace", "Namespaces"),
    ("Cluster", "", "Event", "Events"),
    ("Cluster", "coordination.k8s.io", "Lease", "Lease"),
    ("Workloads", "", "Pod", "Pods"),
    ("Workloads", "apps", "Deployment", "Deployments"),
    ("Workloads", "apps", "StatefulSet", "StatefulSets"),
    ("Workloads", "apps", "DaemonSet", "DaemonSets"),
    ("Workloads", "apps", "ReplicaSet", "ReplicaSets"),
    ("Workloads", "batch", "Job", "Jobs"),
    ("Workloads", "batch", "CronJob", "CronJobs"),
    ("Networking", "", "Service", "Services"),
    ("Networking", "networking.k8s.io", "Ingress", "Ingresses"),
    ("Networking", "gateway.networking.k8s.io", "Gateway", "Gateways"),
    ("Networking", "gateway.networking.k8s.io", "HTTPRoute", "HTTP Routes"),
    ("Networking", "gateway.networking.k8s.io", "GRPCRoute", "GRPC Routes"),
    ("Networking", "gateway.networking.k8s.io", "TLSRoute", "TLS Routes"),
    ("Networking", "gateway.networking.k8s.io", "TCPRoute", "TCP Routes"),
    ("Networking", "networking.k8s.io", "NetworkPolicy", "Network Policies"),
    ("Networking", "discovery.k8s.io", "EndpointSlice", "Endpoint Slices"),
    ("Networking", "networking.k8s.io", "IngressClass", "Ingress Classes"),
    ("Configuration", "", "ConfigMap", "ConfigMaps"),
    ("Configuration", "", "Secret", "Secrets"),
    ("Configuration", "autoscaling", "HorizontalPodAutoscaler", "HPA"),
    ("Configuration", "", "ResourceQuota", "Resource Quotas"),
    ("Configuration", "", "LimitRange", "Limit Ranges"),
    ("Configuration", "policy", "PodDisruptionBudget", "Disruption Budgets"),
    ("Storage", "", "PersistentVolumeClaim", "Persistent Volume Claims"),
    ("Storage", "", "PersistentVolume", "Persistent Volumes"),
    ("Storage", "storage.k8s.io", "StorageClass", "Storage Classes"),
    ("Storage", "storage.k8s.io", "VolumeAttachment", "Volume Attachments"),
    ("Storage", "storage.k8s.io", "CSINode", "CSI Nodes"),
    ("Storage", "storage.k8s.io", "CSIDriver", "CSI Drivers"),
    ("Storage", "storage.k8s.io", "VolumeAttributesClass", "Volume Attributes Classes"),
    ("Access Control", "", "ServiceAccount", "Service Accounts"),
    ("Access Control", "rbac.authorization.k8s.io", "Role", "Roles"),
    ("Access Control", "rbac.authorization.k8s.io", "RoleBinding", "Role Bindings"),
    ("Access Control", "rbac.authorization.k8s.io", "ClusterRole", "Cluster Roles"),
    ("Access Control", "rbac.authorization.k8s.io", "ClusterRoleBinding", "Cluster Role Bindings"),
    ("Administration", "apiextensions.k8s.io", "CustomResourceDefinition", "CRDs"),
    ("Administration", "admissionregistration.k8s.io", "MutatingWebhookConfiguration", "Mutating Webhooks"),
    ("Administration", "admissionregistration.k8s.io", "ValidatingWebhookConfiguration", "Validating Webhooks"),
    ("Administration", "scheduling.k8s.io", "PriorityClass", "Priority Classes"),
    ("Administration", "node.k8s.io", "RuntimeClass", "Runtime Classes"),
];

/// Position of a built-in kind in the sidebar; unknown built-ins go last.
pub fn sidebar_rank(k: &Kind) -> usize {
    KNOWN.iter().position(|(_, g, kind, _)| *g == k.ar.group && *kind == k.ar.kind).unwrap_or(usize::MAX)
}

pub const CUSTOM_SECTION: &str = "Custom resources";

/// The section of the kinds the sidebar leaves out as noise (`is_noise`). Discovery keeps
/// them all the same: a manifest, a link or an agent can still name one.
pub const HIDDEN_SECTION: &str = "Hidden";

/// Whether the sidebar lists a kind (and the palette, which offers the same types).
pub fn in_sidebar(k: &Kind) -> bool {
    k.section != HIDDEN_SECTION
}

/// Sidebar sections, in order.
pub const SECTIONS: [&str; 12] = [
    "Cluster",
    "Workloads",
    "Networking",
    "Configuration",
    "Storage",
    "Data",
    "Observability",
    "Delivery",
    "Cloud",
    "Access Control",
    "Administration",
    CUSTOM_SECTION,
];

/// Where custom resources belong, by API group (the group itself or a parent domain).
pub(super) const CRD_SECTIONS: &[(&str, &str)] = &[
    // Networking: CNIs, meshes, gateways, DNS, load balancers.
    ("cilium.io", "Networking"),
    ("projectcalico.org", "Networking"),
    ("networking.istio.io", "Networking"),
    ("telemetry.istio.io", "Observability"),
    ("security.istio.io", "Access Control"),
    ("install.istio.io", "Administration"),
    ("linkerd.io", "Networking"),
    ("traefik.io", "Networking"),
    ("traefik.containo.us", "Networking"),
    ("containo.us", "Networking"),
    ("getambassador.io", "Networking"),
    ("envoyproxy.io", "Networking"),
    ("agentgateway.dev", "Networking"),
    ("externaldns.k8s.io", "Networking"),
    ("metallb.io", "Networking"),
    ("kuma.io", "Networking"),
    ("konghq.com", "Networking"),
    ("networking.gke.io", "Networking"),
    ("nginx.org", "Networking"),
    // Configuration: secrets, certificates, autoscaling.
    ("external-secrets.io", "Configuration"),
    ("secrets-store.csi.x-k8s.io", "Configuration"),
    ("bitnami.com", "Configuration"),
    ("secrets.hashicorp.com", "Configuration"),
    ("onepassword.com", "Configuration"),
    ("cert-manager.io", "Configuration"),
    ("keda.sh", "Configuration"),
    ("autoscaling.k8s.io", "Configuration"),
    ("autoscaling.gke.io", "Configuration"),
    ("internal.autoscaling.k8s.io", "Configuration"),
    // Storage and backups.
    ("velero.io", "Storage"),
    ("snapshot.storage.k8s.io", "Storage"),
    ("topolvm.io", "Storage"),
    ("longhorn.io", "Storage"),
    ("ceph.rook.io", "Storage"),
    ("rook.io", "Storage"),
    ("objectbucket.io", "Storage"),
    ("barmancloud.cnpg.io", "Storage"),
    ("openebs.io", "Storage"),
    ("topolvm.cybozu.com", "Storage"),
    // Cloud: cloud resources managed from Kubernetes.
    ("services.k8s.aws", "Cloud"),
    ("cnrm.cloud.google.com", "Cloud"),
    ("azure.com", "Cloud"),
    ("crossplane.io", "Cloud"),
    ("upbound.io", "Cloud"),
    ("aws.upbound.io", "Cloud"),
    // Data: database and messaging operators.
    ("postgresql.cnpg.io", "Data"),
    ("acid.zalan.do", "Data"),
    ("clickhouse.altinity.com", "Data"),
    ("clickhouse-keeper.altinity.com", "Data"),
    ("rabbitmq.com", "Data"),
    ("mongodbcommunity.mongodb.com", "Data"),
    ("mongodb.com", "Data"),
    ("redis.redis.opstreelabs.in", "Data"),
    ("databases.spotahome.com", "Data"),
    ("kafka.strimzi.io", "Data"),
    ("elasticsearch.k8s.elastic.co", "Data"),
    ("mysql.oracle.com", "Data"),
    ("dragonflydb.io", "Data"),
    ("minio.min.io", "Data"),
    // Observability.
    ("monitoring.coreos.com", "Observability"),
    ("monitoring.grafana.com", "Observability"),
    ("grafana.integreatly.org", "Observability"),
    ("opentelemetry.io", "Observability"),
    ("logging.banzaicloud.io", "Observability"),
    ("logging.kube-logging.dev", "Observability"),
    ("monitoring.kiali.io", "Observability"),
    ("giantswarm.io", "Observability"),
    ("pyrra.dev", "Observability"),
    ("loki.grafana.com", "Observability"),
    ("tempo.grafana.com", "Observability"),
    ("chaos-mesh.org", "Workloads"),
    // Delivery: GitOps and progressive delivery.
    ("toolkit.fluxcd.io", "Delivery"),
    ("argoproj.io", "Delivery"),
    ("fleet.cattle.io", "Delivery"),
    ("helm.cattle.io", "Delivery"),
    ("flagger.app", "Delivery"),
    ("tekton.dev", "Delivery"),
    // Workloads: batch, ML, serverless.
    ("batch.volcano.sh", "Workloads"),
    ("ray.io", "Workloads"),
    ("kubeflow.org", "Workloads"),
    ("sparkoperator.k8s.io", "Workloads"),
    ("serving.knative.dev", "Workloads"),
    ("dask.org", "Workloads"),
    // Access Control: identity and policy.
    ("kyverno.io", "Access Control"),
    ("gatekeeper.sh", "Access Control"),
    ("aquasecurity.github.io", "Access Control"),
    ("dex.coreos.com", "Access Control"),
    ("keycloak.org", "Access Control"),
    ("crowdsec.net", "Access Control"),
    ("rbacmanager.reactiveops.io", "Access Control"),
    // Cluster: nodes and cluster lifecycle.
    ("cluster.x-k8s.io", "Cluster"),
    ("karpenter.sh", "Cluster"),
    ("karpenter.k8s.aws", "Cluster"),
    ("nodemanagement.gke.io", "Cluster"),
    ("nvidia.com", "Cluster"),
    ("cattle.io", "Administration"),
    ("auto.gke.io", "Cluster"),
    ("hub.gke.io", "Administration"),
    ("cloud.google.com", "Administration"),
];

/// The sidebar section a custom resource's API group belongs to.
pub fn crd_section(group: &str) -> &'static str {
    // Longest match wins (security.istio.io before istio.io).
    CRD_SECTIONS
        .iter()
        .filter(|(g, _)| group == *g || group.ends_with(&format!(".{g}")))
        .max_by_key(|(g, _)| g.len())
        .map_or(CUSTOM_SECTION, |(_, s)| *s)
}

/// A custom resource (not a Kubernetes built-in API).
pub fn is_custom(k: &Kind) -> bool {
    sidebar_rank(k) == usize::MAX && (!is_builtin_group(&k.ar.group) || crd_section(&k.ar.group) != CUSTOM_SECTION)
}

/// Sidebar order: by section, built-ins first (in KNOWN order), then custom resources by group.
pub fn sort_for_sidebar(kinds: &mut [Kind]) {
    kinds.sort_by(|a, b| {
        let key = |k: &Kind| {
            let section = SECTIONS.iter().position(|s| *s == k.section).unwrap_or(SECTIONS.len());
            (section, is_custom(k), sidebar_rank(k), k.ar.group.clone(), k.ar.kind.clone())
        };
        key(a).cmp(&key(b))
    });
}

/// An add-on API group discovery could not read: its APIService is down, or its discovery
/// failed. Its kinds may be missing from what was found (by group, or aggregated with none
/// known) or the last ones known (aggregated, marked stale): not gone from the cluster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SkippedGroup {
    pub group: String,
    /// Why, with the group version that failed.
    pub error: String,
}

/// What discovery found: the kinds, and the groups it had to leave out.
#[derive(Clone, Debug, Default)]
pub struct Discovered {
    pub kinds: Vec<Kind>,
    pub skipped: Vec<SkippedGroup>,
}

/// The kinds a cluster serves; the groups that didn't answer are left out (see
/// [`discover_all`] to know which).
pub async fn discover(client: Client) -> Result<Vec<Kind>> {
    discover_all(client).await.map(|d| d.kinds)
}

/// The kinds a cluster serves, and the add-on groups that didn't answer: a group whose
/// APIService is down is left out, as kubectl does, and named, so that its kinds read as
/// unreadable for now rather than as gone.
pub async fn discover_all(client: Client) -> Result<Discovered> {
    run(async move {
        let (resources, skipped) = match aggregated(&client).await {
            Ok(found) => found,
            Err(_) => discover_by_group(&client).await?,
        };
        Ok(Discovered { kinds: kinds_of(resources), skipped })
    })
    .await
}

/// Aggregated discovery (Kubernetes 1.26 and later): two requests for every group.
async fn aggregated(client: &Client) -> kube::Result<(Vec<(ApiResource, ApiCapabilities)>, Vec<SkippedGroup>)> {
    let apis = client.list_api_groups_aggregated().await?;
    let core = client.list_core_api_versions_aggregated().await?;
    Ok(from_aggregated(core.items.into_iter().take(1).chain(apis.items)))
}

/// The resources of aggregated discovery's groups, every version (Gateway API prefers v1, but
/// `TCPRoute` is only in v1alpha2), each kind at its most stable one; and the groups with a
/// version marked stale: the API server could not reach its APIService, so the kinds listed
/// for it (the last ones known, or none) may not be all.
fn from_aggregated(
    groups: impl IntoIterator<Item = APIGroupDiscovery>,
) -> (Vec<(ApiResource, ApiCapabilities)>, Vec<SkippedGroup>) {
    let mut resources = Vec::new();
    let mut skipped: Vec<SkippedGroup> = Vec::new();
    for g in groups {
        let group = g.metadata.and_then(|m| m.name).unwrap_or_default();
        for v in g.versions {
            let version = v.version.unwrap_or_default();
            let api_version = if group.is_empty() { version.clone() } else { format!("{group}/{version}") };
            if v.freshness.as_deref() == Some("Stale") && !skipped.iter().any(|s| s.group == group) {
                let error = format!("{api_version}: stale, its API service is not available");
                skipped.push(SkippedGroup { group: group.clone(), error });
            }
            for r in v.resources {
                let ar = ApiResource {
                    group: group.clone(),
                    version: version.clone(),
                    api_version: api_version.clone(),
                    kind: r.response_kind.and_then(|k| k.kind).unwrap_or_default(),
                    plural: r.resource.unwrap_or_default(),
                };
                let scope = if r.scope.as_deref() == Some("Namespaced") { Scope::Namespaced } else { Scope::Cluster };
                resources.push((ar, ApiCapabilities { scope, subresources: Vec::new(), operations: r.verbs }));
            }
        }
    }
    (most_stable(resources), skipped)
}

/// The groups that didn't answer, for a message: `argoproj.io (why), metrics.k8s.io (why)`.
pub fn describe_skipped(skipped: &[SkippedGroup]) -> String {
    let named: Vec<String> = skipped.iter().map(|s| format!("{} ({})", group_name(&s.group), s.error)).collect();
    named.join(", ")
}

/// A group's name as people write it: the core group has none.
fn group_name(group: &str) -> &str {
    if group.is_empty() { "core" } else { group }
}

/// Added to "unknown type" when some groups didn't answer: the type may be one of theirs.
pub fn maybe_skipped(skipped: &[SkippedGroup]) -> String {
    if skipped.is_empty() {
        return String::new();
    }
    format!("; it may belong to an API group that couldn't be read: {}", describe_skipped(skipped))
}

/// The group of a kind saved by its kubectl name (`pods`, `applications.argoproj.io`).
pub fn group_of_name(name: &str) -> &str {
    name.split_once('.').map_or("", |(_, group)| group)
}

/// The listable types of `resources`, each in its sidebar section, in sidebar order. The
/// noise is kept, in `HIDDEN_SECTION` after all the others: Apply and the agents look types
/// up in this list (a manifest may hold a `FlowSchema`), and a lookup by name still finds the
/// shown kind first (the core Events before events.k8s.io's).
fn kinds_of(resources: Vec<(ApiResource, ApiCapabilities)>) -> Vec<Kind> {
    let mut kinds: Vec<Kind> = Vec::new();
    for (ar, caps) in resources {
        if !listable(&caps) || ar.plural.contains('/') {
            continue;
        }
        kinds.push(kind_of(ar, caps.scope == Scope::Namespaced));
    }
    sort_for_sidebar(&mut kinds);
    kinds
}

/// A kind discovery found, with its sidebar section and label: a built-in's from `KNOWN`,
/// the noise (old events.k8s.io duplicates, metrics, flow control) in `HIDDEN_SECTION`.
pub fn kind_of(ar: ApiResource, namespaced: bool) -> Kind {
    match KNOWN.iter().find(|(_, g, k, _)| *g == ar.group && *k == ar.kind) {
        Some((section, _, _, label)) => Kind { ar, namespaced, section, label: label.to_string() },
        None if is_noise(&ar) => Kind { label: ar.kind.clone(), ar, namespaced, section: HIDDEN_SECTION },
        // Remaining built-in APIs are cluster plumbing, not custom resources
        // (some CRD groups end in .k8s.io too: those are mapped above).
        None if is_builtin_group(&ar.group) && crd_section(&ar.group) == CUSTOM_SECTION => {
            Kind { label: ar.kind.clone(), ar, namespaced, section: "Administration" }
        }
        None => Kind { label: ar.kind.clone(), section: crd_section(&ar.group), ar, namespaced },
    }
}

/// Discovery for clusters without aggregated discovery (before 1.26, or turned off): every
/// version of each group, side by side, each kind kept at its most stable version. A group
/// that does not answer (an APIService that is down, like metrics.k8s.io) is left out, as
/// kubectl does, instead of failing them all, and returned among the skipped; one with
/// built-in kinds the sidebar lists (Pods, Deployments…) is asked again, then fails
/// discovery rather than leaving them out.
async fn discover_by_group(client: &Client) -> Result<(Vec<(ApiResource, ApiCapabilities)>, Vec<SkippedGroup>)> {
    let mut gvs = vec![kube::core::GroupVersion::gv("", "v1")];
    for g in client.list_api_groups().await?.groups {
        gvs.extend(g.versions.iter().map(|v| kube::core::GroupVersion::gv(&g.name, &v.version)));
    }
    let mut resources = Vec::new();
    let mut failed = Vec::new();
    for (gv, g) in pinned_groups(client, gvs).await {
        match g {
            Ok(g) => resources.extend(g.recommended_resources()),
            Err(e) => failed.push((gv, e)),
        }
    }
    // Once more for the ones that matter (a timeout, too many requests).
    let (retry, optional): (Vec<_>, Vec<_>) = failed.into_iter().partition(|(gv, _)| required(&gv.group));
    for (gv, g) in pinned_groups(client, retry.into_iter().map(|(gv, _)| gv).collect()).await {
        match g {
            Ok(g) => resources.extend(g.recommended_resources()),
            Err(e) => return Err(anyhow::Error::from(e).context(format!("discovery of {} failed", gv.api_version()))),
        }
    }
    let answered: HashSet<&str> = resources.iter().map(|(ar, _)| ar.group.as_str()).collect();
    let skipped = skipped_by_group(&optional, &answered);
    match optional.into_iter().next() {
        Some((_, e)) if resources.is_empty() => Err(e.into()),
        _ => Ok((most_stable(resources), skipped)),
    }
}

/// The groups none of whose versions answered (`failed`), once each, with the first version
/// that failed and why. A group another version of which answered (in `answered`) is not
/// skipped: its kinds are there, at that version.
fn skipped_by_group<E: std::fmt::Display>(
    failed: &[(kube::core::GroupVersion, E)],
    answered: &HashSet<&str>,
) -> Vec<SkippedGroup> {
    let mut skipped: Vec<SkippedGroup> = Vec::new();
    for (gv, e) in failed {
        if !answered.contains(gv.group.as_str()) && !skipped.iter().any(|s| s.group == gv.group) {
            skipped.push(SkippedGroup { group: gv.group.clone(), error: format!("{}: {e}", gv.api_version()) });
        }
    }
    skipped
}

/// Each group version's discovery, 16 at a time.
async fn pinned_groups(
    client: &Client,
    gvs: Vec<kube::core::GroupVersion>,
) -> Vec<(kube::core::GroupVersion, kube::Result<kube::discovery::ApiGroup>)> {
    let jobs = gvs.into_iter().map(|gv| {
        let client = client.clone();
        async move {
            let g = kube::discovery::pinned_group(&client, &gv).await;
            (gv, g)
        }
    });
    futures::stream::iter(jobs).buffered(16).collect().await
}

/// A built-in group with kinds the sidebar lists by name: discovery can't do without it.
/// Add-on groups stay optional, even listed ones: projectcalico.org is an APIService that
/// can be down like metrics.k8s.io.
fn required(group: &str) -> bool {
    is_builtin_group(group) && KNOWN.iter().any(|(_, g, _, _)| *g == group)
}

/// Each kind of each group once, at its most stable version (v1 before v1beta1 before
/// v1alpha2).
fn most_stable(resources: Vec<(ApiResource, ApiCapabilities)>) -> Vec<(ApiResource, ApiCapabilities)> {
    let mut by_kind: HashMap<(String, String), (ApiResource, ApiCapabilities)> = HashMap::new();
    for (ar, caps) in resources {
        let priority = |ar: &ApiResource| kube::core::Version::parse(&ar.version).priority();
        match by_kind.entry((ar.group.clone(), ar.kind.clone())) {
            std::collections::hash_map::Entry::Occupied(mut e) if priority(&ar) > priority(&e.get().0) => {
                e.insert((ar, caps));
            }
            std::collections::hash_map::Entry::Occupied(_) => {}
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert((ar, caps));
            }
        }
    }
    by_kind.into_values().collect()
}

pub(super) fn listable(caps: &ApiCapabilities) -> bool {
    caps.supports_operation(verbs::LIST) && caps.supports_operation(verbs::WATCH)
}

pub(crate) fn is_builtin_group(group: &str) -> bool {
    !group.contains('.') || group.ends_with(".k8s.io")
}

/// What the sidebar leaves out (`HIDDEN_SECTION`): events.k8s.io's copy of the core Events,
/// metrics (not stored objects), API priority and fairness, and the old `Endpoints`,
/// `ComponentStatus` and `Binding`.
pub(super) fn is_noise(ar: &ApiResource) -> bool {
    matches!(ar.group.as_str(), "events.k8s.io" | "metrics.k8s.io" | "flowcontrol.apiserver.k8s.io")
        || matches!(ar.kind.as_str(), "ComponentStatus" | "Binding" | "Endpoints")
}

pub async fn namespaces(client: Client) -> Result<Vec<String>> {
    run(async move {
        let api: Api<Namespace> = Api::all(client);
        let list = api.list_metadata(&ListParams::default()).await?;
        let mut names: Vec<String> = list.items.into_iter().filter_map(|n| n.metadata.name).collect();
        names.sort();
        Ok(names)
    })
    .await
}

/// Resolve what a human or agent types (`po`, `pods`, `Deployment`,
/// `certificates.cert-manager.io`) to a discovered kind.
pub fn find_kind<'a>(kinds: &'a [Kind], query: &str) -> Option<&'a Kind> {
    let q = query.trim().to_lowercase();
    let q = match q.as_str() {
        "po" => "pods",
        "deploy" => "deployments",
        "svc" => "services",
        "cm" => "configmaps",
        "ns" => "namespaces",
        "no" => "nodes",
        "ing" => "ingresses",
        "sts" => "statefulsets",
        "ds" => "daemonsets",
        "rs" => "replicasets",
        "pvc" => "persistentvolumeclaims",
        "pv" => "persistentvolumes",
        "sa" => "serviceaccounts",
        "cj" => "cronjobs",
        "hpa" => "horizontalpodautoscalers",
        "pdb" => "poddisruptionbudgets",
        "ev" => "events",
        "crd" | "crds" => "customresourcedefinitions",
        other => other,
    };
    let (name, group) = match q.split_once('.') {
        Some((n, g)) => (n, Some(g)),
        None => (q, None),
    };
    let matches = |k: &&Kind| {
        let names_match =
            k.ar.plural.to_lowercase() == name || k.ar.kind.to_lowercase() == name || k.label.to_lowercase() == name;
        names_match && group.is_none_or(|g| k.ar.group.to_lowercase() == g)
    };
    // Prefer core and well-known kinds when a name is ambiguous (`svc` is the Service, not
    // Knative's): sidebar order alone puts a custom resource first when its section is earlier.
    kinds.iter().filter(|k| !is_custom(k)).find(matches).or_else(|| kinds.iter().find(matches))
}

/// The discovered kind a manifest's object names with its `apiVersion` and `kind`.
pub fn manifest_kind<'a>(kinds: &'a [Kind], api_version: &str, kind: &str) -> Option<&'a Kind> {
    kinds.iter().find(|k| k.ar.api_version == api_version && k.ar.kind == kind)
}

/// Whether a manifest's document is a `CustomResourceDefinition`.
pub fn is_crd(doc: &serde_json::Value) -> bool {
    doc["kind"] == "CustomResourceDefinition"
        && doc["apiVersion"].as_str().is_some_and(|v| v.starts_with("apiextensions.k8s.io/"))
}

/// The kinds a `CustomResourceDefinition` document defines, one per served version, as
/// discovery lists them once the API server serves them. Empty for any other document.
pub fn crd_kinds(doc: &serde_json::Value) -> Vec<Kind> {
    if !is_crd(doc) {
        return Vec::new();
    }
    let spec = &doc["spec"];
    let text = |path: &str| spec.pointer(path).and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    let (Some(group), Some(kind), Some(plural)) = (text("/group"), text("/names/kind"), text("/names/plural")) else {
        return Vec::new();
    };
    let namespaced = text("/scope") != Some("Cluster");
    spec["versions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| v["served"].as_bool() != Some(false))
        .filter_map(|v| v["name"].as_str().filter(|s| !s.is_empty()))
        .map(|version| {
            let ar = ApiResource {
                group: group.into(),
                version: version.into(),
                api_version: format!("{group}/{version}"),
                kind: kind.into(),
                plural: plural.into(),
            };
            kind_of(ar, namespaced)
        })
        .collect()
}

/// The listable kinds of one group version, asked of the API server now: one request, where
/// a full discovery asks for every group.
pub async fn discover_version(client: Client, group: String, version: String) -> Result<Vec<Kind>> {
    run(async move {
        let gv = kube::core::GroupVersion::gv(&group, &version);
        let g = kube::discovery::pinned_group(&client, &gv).await?;
        Ok(kinds_of(g.recommended_resources()))
    })
    .await
}

/// `kinds` with those of `group`/`version` replaced by `fresh` (what that group version
/// serves now), in sidebar order.
pub fn with_version(kinds: &[Kind], group: &str, version: &str, fresh: Vec<Kind>) -> Vec<Kind> {
    let mut out: Vec<Kind> =
        kinds.iter().filter(|k| k.ar.group != group || k.ar.version != version).cloned().chain(fresh).collect();
    sort_for_sidebar(&mut out);
    out
}

#[cfg(test)]
mod required_tests {
    #[test]
    fn core_and_listed_groups_are_required() {
        use super::required;
        assert!(required(""));
        assert!(required("apps"));
        assert!(required("batch"));
        assert!(required("networking.k8s.io"));
        assert!(!required("metrics.k8s.io"));
        assert!(!required("custom.metrics.k8s.io"));
        assert!(!required("cert-manager.io"));
        assert!(!required("projectcalico.org"));
    }
}

#[cfg(test)]
mod aggregated_tests {
    use super::{SkippedGroup, from_aggregated, group_of_name, kinds_of};

    /// A group whose APIService is down comes back stale, with the kinds last known or none:
    /// it is named among the skipped, once, and the groups that answered are all there.
    #[test]
    fn a_stale_group_is_named_not_dropped() {
        let list: kube::core::discovery::v2::APIGroupDiscoveryList = serde_json::from_value(serde_json::json!({
            "items": [
                {"metadata": {"name": "apps"}, "versions": [{"version": "v1", "freshness": "Current", "resources": [
                    {"resource": "deployments", "responseKind": {"kind": "Deployment"}, "scope": "Namespaced",
                     "verbs": ["get", "list", "watch"]}]}]},
                {"metadata": {"name": "metrics.k8s.io"}, "versions": [{"version": "v1beta1", "freshness": "Stale"}]},
                {"metadata": {"name": "argoproj.io"}, "versions": [
                    {"version": "v1alpha1", "freshness": "Stale", "resources": [
                        {"resource": "applications", "responseKind": {"kind": "Application"}, "scope": "Namespaced",
                         "verbs": ["get", "list", "watch"]}]},
                    {"version": "v1beta1", "freshness": "Stale"}]},
            ]
        }))
        .unwrap();
        let (resources, skipped) = from_aggregated(list.items);
        let kinds = kinds_of(resources);
        assert!(kinds.iter().any(|k| k.is("apps", "Deployment")));
        assert!(kinds.iter().any(|k| k.is("argoproj.io", "Application")), "the kinds last known are kept");
        let groups: Vec<&str> = skipped.iter().map(|s: &SkippedGroup| s.group.as_str()).collect();
        assert_eq!(groups, ["metrics.k8s.io", "argoproj.io"]);
        assert!(skipped[0].error.contains("metrics.k8s.io/v1beta1"), "{}", skipped[0].error);
    }

    /// By group: a group is skipped only when none of its versions answered, once, with the
    /// version that failed.
    #[test]
    fn a_group_is_skipped_only_when_no_version_answered() {
        use std::collections::HashSet;

        use kube::core::GroupVersion;

        use super::skipped_by_group;
        let failed = vec![
            (GroupVersion::gv("argoproj.io", "v1alpha1"), "503"),
            (GroupVersion::gv("metrics.k8s.io", "v1beta1"), "503"),
            (GroupVersion::gv("metrics.k8s.io", "v1beta2"), "503"),
        ];
        // argoproj.io answered at another version.
        let answered: HashSet<&str> = ["", "argoproj.io"].into();
        let skipped = skipped_by_group(&failed, &answered);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].group, "metrics.k8s.io");
        assert_eq!(skipped[0].error, "metrics.k8s.io/v1beta1: 503");
        assert!(skipped_by_group(&failed[..1], &HashSet::new())[0].error.starts_with("argoproj.io/v1alpha1"));
    }

    #[test]
    fn a_saved_name_gives_its_group() {
        assert_eq!(group_of_name("pods"), "");
        assert_eq!(group_of_name("deployments.apps"), "apps");
        assert_eq!(group_of_name("applications.argoproj.io"), "argoproj.io");
    }
}

#[cfg(test)]
mod version_tests {
    use kube::discovery::{ApiCapabilities, ApiResource, Scope};

    fn res(group: &str, version: &str, kind: &str) -> (ApiResource, ApiCapabilities) {
        let ar = ApiResource {
            group: group.into(),
            version: version.into(),
            api_version: format!("{group}/{version}"),
            kind: kind.into(),
            plural: format!("{}s", kind.to_lowercase()),
        };
        (ar, ApiCapabilities { scope: Scope::Namespaced, subresources: vec![], operations: vec![] })
    }

    #[test]
    fn keeps_kinds_of_every_version_at_the_most_stable() {
        let gw = "gateway.networking.k8s.io";
        let mut kept: Vec<String> = super::most_stable(vec![
            res(gw, "v1", "HTTPRoute"),
            res(gw, "v1beta1", "HTTPRoute"),
            res(gw, "v1alpha2", "HTTPRoute"),
            res(gw, "v1alpha2", "TCPRoute"),
            res("monitoring.coreos.com", "v1alpha1", "AlertmanagerConfig"),
            res("monitoring.coreos.com", "v1beta1", "AlertmanagerConfig"),
            res("other.example.com", "v1alpha1", "HTTPRoute"),
        ])
        .into_iter()
        .map(|(ar, _)| ar.api_version + "/" + &ar.kind)
        .collect();
        kept.sort();
        assert_eq!(
            kept,
            [
                "gateway.networking.k8s.io/v1/HTTPRoute",
                "gateway.networking.k8s.io/v1alpha2/TCPRoute",
                "monitoring.coreos.com/v1beta1/AlertmanagerConfig",
                "other.example.com/v1alpha1/HTTPRoute",
            ]
        );
    }
}

#[cfg(test)]
mod section_tests {
    #[test]
    fn maps_crd_groups() {
        use super::crd_section;
        assert_eq!(crd_section("external-secrets.io"), "Configuration");
        assert_eq!(crd_section("generators.external-secrets.io"), "Configuration");
        assert_eq!(crd_section("cilium.io"), "Networking");
        assert_eq!(crd_section("security.istio.io"), "Access Control");
        assert_eq!(crd_section("networking.istio.io"), "Networking");
        assert_eq!(crd_section("externaldns.k8s.io"), "Networking");
        assert_eq!(crd_section("kustomize.toolkit.fluxcd.io"), "Delivery");
        assert_eq!(crd_section("postgresql.cnpg.io"), "Data");
        assert_eq!(crd_section("barmancloud.cnpg.io"), "Storage");
        assert_eq!(crd_section("widgets.example.com"), super::CUSTOM_SECTION);
        assert_eq!(crd_section("iam.services.k8s.aws"), "Cloud");
    }

    /// A kind found by discovery, here or on a context that isn't connected, gets the
    /// section and label the sidebar gives it.
    #[test]
    fn a_discovered_kind_is_labelled_as_in_the_sidebar() {
        use super::{Kind, is_custom, kind_of};
        let found = |group: &str, kind: &str| {
            let k = Kind::fake(group, kind);
            kind_of(k.ar, k.namespaced)
        };
        let shown = |k: &Kind| (k.section, k.label.clone(), is_custom(k));
        assert_eq!(shown(&found("", "Namespace")), ("Cluster", "Namespaces".into(), false));
        assert_eq!(shown(&found("apps", "Deployment")), ("Workloads", "Deployments".into(), false));
        assert_eq!(
            shown(&found("gateway.networking.k8s.io", "HTTPRoute")),
            ("Networking", "HTTP Routes".into(), false)
        );
        // Built-in plumbing, then custom resources by their group.
        let csr = found("certificates.k8s.io", "CertificateSigningRequest");
        assert_eq!(shown(&csr), ("Administration", "CertificateSigningRequest".into(), false));
        assert_eq!(shown(&found("cert-manager.io", "Certificate")), ("Configuration", "Certificate".into(), true));
        assert_eq!(shown(&found("widgets.example.com", "Widget")), (super::CUSTOM_SECTION, "Widget".into(), true));
    }
}

#[cfg(test)]
mod noise_tests {
    use kube::discovery::{ApiCapabilities, ApiResource, Scope};
    use proptest::prelude::*;

    use super::{Kind, find_kind, in_sidebar, is_custom, is_noise, kinds_of, listable, manifest_kind};

    /// A type as discovery reports it at v1; `listable` when it may be listed and watched.
    fn res(group: &str, kind: &str, plural: &str, namespaced: bool, listable: bool) -> (ApiResource, ApiCapabilities) {
        let ar = ApiResource {
            group: group.into(),
            version: "v1".into(),
            api_version: if group.is_empty() { "v1".into() } else { format!("{group}/v1") },
            kind: kind.into(),
            plural: plural.into(),
        };
        let verbs: &[&str] = if listable { &["create", "get", "list", "watch"] } else { &["create"] };
        let scope = if namespaced { Scope::Namespaced } else { Scope::Cluster };
        (
            ar,
            ApiCapabilities {
                scope,
                subresources: vec![],
                operations: verbs.iter().map(ToString::to_string).collect(),
            },
        )
    }

    /// What a lookup found, to compare: its group and kind.
    fn id(k: &Kind) -> (&str, &str) {
        (k.ar.group.as_str(), k.ar.kind.as_str())
    }

    /// kwok's release manifest holds a `FlowSchema` and a `PriorityLevelConfiguration`: Apply
    /// and the agents find them, like the core `Endpoints` and events.k8s.io's Events, and the
    /// sidebar lists none of them.
    #[test]
    fn the_sidebar_leaves_out_types_apply_and_agents_still_find() {
        let flow = "flowcontrol.apiserver.k8s.io";
        let kinds = kinds_of(vec![
            res(flow, "FlowSchema", "flowschemas", false, true),
            res(flow, "PriorityLevelConfiguration", "prioritylevelconfigurations", false, true),
            res("events.k8s.io", "Event", "events", true, true),
            res("", "Endpoints", "endpoints", true, true),
            res("", "Event", "events", true, true),
            res("", "Pod", "pods", true, true),
            res("", "Binding", "bindings", true, false),
        ]);
        let order: Vec<(&str, bool)> = kinds.iter().map(|k| (k.ar.kind.as_str(), in_sidebar(k))).collect();
        assert_eq!(
            order,
            [
                ("Event", true),
                ("Pod", true),
                ("Endpoints", false),
                ("Event", false),
                ("FlowSchema", false),
                ("PriorityLevelConfiguration", false),
            ]
        );
        let schema = manifest_kind(&kinds, "flowcontrol.apiserver.k8s.io/v1", "FlowSchema").expect("FlowSchema");
        assert!(!schema.namespaced);
        assert!(manifest_kind(&kinds, "flowcontrol.apiserver.k8s.io/v1", "PriorityLevelConfiguration").is_some());
        assert!(manifest_kind(&kinds, "v1", "Endpoints").is_some());
        assert!(manifest_kind(&kinds, "events.k8s.io/v1", "Event").is_some());
        // The names agents and links use, like kubectl's.
        assert_eq!(find_kind(&kinds, "flowschemas").map(|k| k.ar.group.as_str()), Some(flow));
        assert_eq!(find_kind(&kinds, "endpoints").map(|k| k.ar.kind.as_str()), Some("Endpoints"));
        assert_eq!(find_kind(&kinds, "events.events.k8s.io").map(|k| k.ar.group.as_str()), Some("events.k8s.io"));
        // A name both groups serve is the kind the sidebar shows.
        assert_eq!(find_kind(&kinds, "events").map(|k| k.ar.group.as_str()), Some(""));
    }

    /// Types a cluster may serve, each kind once per group as discovery keeps them, in any
    /// order: noise and not, built-in and custom, listable or not.
    fn served() -> impl Strategy<Value = Vec<(ApiResource, ApiCapabilities)>> {
        let groups: &'static [&str] = &[
            "",
            "apps",
            "networking.k8s.io",
            "events.k8s.io",
            "metrics.k8s.io",
            "flowcontrol.apiserver.k8s.io",
            "cert-manager.io",
            "example.com",
        ];
        let kinds: &'static [(&str, &str)] = &[
            ("Pod", "pods"),
            ("Event", "events"),
            ("Ingress", "ingresses"),
            ("Endpoints", "endpoints"),
            ("Binding", "bindings"),
            ("FlowSchema", "flowschemas"),
            ("PriorityLevelConfiguration", "prioritylevelconfigurations"),
            ("Certificate", "certificates"),
            ("Widget", "widgets"),
        ];
        let key = (prop::sample::select(groups), prop::sample::select(kinds));
        prop::collection::btree_map(key, (any::<bool>(), any::<bool>()), 0..24)
            .prop_map(|served| {
                served
                    .into_iter()
                    .map(|((group, (kind, plural)), (namespaced, listable))| {
                        res(group, kind, plural, namespaced, listable)
                    })
                    .collect::<Vec<_>>()
            })
            .prop_shuffle()
    }

    proptest! {
        /// Whatever a cluster serves, every type that can be listed resolves from a manifest
        /// (`apiVersion` and `kind`) and, in a named group, from its kubectl name. The sidebar
        /// lists what it did before discovery kept the noise, ahead of the noise, and a name
        /// typed finds a type it lists before one it leaves out.
        #[test]
        fn every_listable_type_resolves_and_the_sidebar_keeps_out_the_noise(served in served()) {
            let kinds = kinds_of(served.clone());
            let wanted: Vec<&ApiResource> = served.iter().filter(|(_, caps)| listable(caps)).map(|(ar, _)| ar).collect();
            prop_assert_eq!(kinds.len(), wanted.len());
            for ar in wanted {
                let expected = Some((ar.group.as_str(), ar.kind.as_str()));
                prop_assert_eq!(manifest_kind(&kinds, &ar.api_version, &ar.kind).map(id), expected);
                if !ar.group.is_empty() {
                    prop_assert_eq!(find_kind(&kinds, &format!("{}.{}", ar.plural, ar.group)).map(id), expected);
                }
            }
            for k in &kinds {
                prop_assert_eq!(in_sidebar(k), !is_noise(&k.ar), "{}", k.key());
            }
            prop_assert!(kinds.windows(2).all(|w| in_sidebar(&w[0]) || !in_sidebar(&w[1])), "noise among shown types");
            for k in kinds.iter().filter(|k| in_sidebar(k) && !is_custom(k)) {
                let found = find_kind(&kinds, &k.ar.plural);
                prop_assert!(found.is_some_and(in_sidebar), "{} found {:?}", k.ar.plural, found.map(Kind::key));
            }
        }
    }
}

#[cfg(test)]
mod crd_tests {
    use serde_json::json;

    use super::{Kind, crd_kinds, is_crd, with_version};

    fn crd(scope: &str, versions: serde_json::Value) -> serde_json::Value {
        json!({
            "apiVersion": "apiextensions.k8s.io/v1",
            "kind": "CustomResourceDefinition",
            "metadata": {"name": "stages.kwok.x-k8s.io"},
            "spec": {"group": "kwok.x-k8s.io", "scope": scope, "names": {"kind": "Stage", "plural": "stages"}, "versions": versions},
        })
    }

    /// A CRD in a manifest gives the types its objects in the same manifest are planned as:
    /// one per served version, with the CRD's plural and scope.
    #[test]
    fn a_crd_defines_a_type_per_served_version() {
        let versions =
            json!([{"name": "v1alpha1", "served": true}, {"name": "v1alpha2", "served": false}, {"name": "v1"}]);
        let kinds = crd_kinds(&crd("Cluster", versions.clone()));
        let found: Vec<(&str, &str, &str, bool)> = kinds
            .iter()
            .map(|k| (k.ar.api_version.as_str(), k.ar.kind.as_str(), k.ar.plural.as_str(), k.namespaced))
            .collect();
        assert_eq!(
            found,
            [("kwok.x-k8s.io/v1alpha1", "Stage", "stages", false), ("kwok.x-k8s.io/v1", "Stage", "stages", false)]
        );
        assert!(crd_kinds(&crd("Namespaced", versions.clone())).iter().all(|k| k.namespaced));
        // Not a CRD, or one without names: nothing.
        let mut other = crd("Namespaced", versions.clone());
        other["apiVersion"] = json!("example.com/v1");
        assert!(!is_crd(&other) && crd_kinds(&other).is_empty());
        let mut nameless = crd("Namespaced", versions);
        nameless["spec"]["names"] = json!({});
        assert!(crd_kinds(&nameless).is_empty());
        assert!(crd_kinds(&json!({"apiVersion": "v1", "kind": "ConfigMap"})).is_empty());
    }

    /// What one group version serves now replaces what discovery had of it, and only of it.
    #[test]
    fn a_group_version_found_again_replaces_its_kinds_only() {
        let mut beta = Kind::fake("example.com", "Widget");
        beta.ar.version = "v1beta1".into();
        beta.ar.api_version = "example.com/v1beta1".into();
        let cached = vec![Kind::fake("", "Pod"), Kind::fake("example.com", "Widget"), beta];
        let fresh = vec![Kind::fake("example.com", "Gadget")];
        let kinds = with_version(&cached, "example.com", "v1", fresh);
        let mut found: Vec<String> = kinds.iter().map(|k| format!("{} {}", k.ar.api_version, k.ar.kind)).collect();
        found.sort();
        assert_eq!(found, ["example.com/v1 Gadget", "example.com/v1beta1 Widget", "v1 Pod"]);
    }
}
