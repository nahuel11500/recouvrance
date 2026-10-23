//! API discovery: the resource types a cluster serves and where they go in the sidebar.

#[allow(unused_imports)]
use super::*;

#[derive(Clone, Debug)]
pub struct Kind {
    pub ar: ApiResource,
    pub namespaced: bool,
    pub section: &'static str,
    pub label: String,
}

impl Kind {
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
        .map(|(_, s)| *s)
        .unwrap_or(CUSTOM_SECTION)
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

pub async fn discover(client: Client) -> Result<Vec<Kind>> {
    run(async move {
        let resources = match Discovery::new(client.clone()).run_aggregated().await {
            // Every version, not only the preferred one: Gateway API prefers v1, but TCPRoute
            // is only in v1alpha2.
            Ok(d) => d.groups().flat_map(|g| g.resources_by_stability()).collect(),
            Err(_) => discover_by_group(&client).await?,
        };
        let mut known: Vec<Kind> = Vec::new();
        let mut custom: Vec<Kind> = Vec::new();
        for (ar, caps) in resources {
            if !listable(&caps) || ar.plural.contains('/') {
                continue;
            }
            let namespaced = caps.scope == Scope::Namespaced;
            match KNOWN.iter().find(|(_, g, k, _)| *g == ar.group && *k == ar.kind) {
                Some((section, _, _, label)) => known.push(Kind { ar, namespaced, section, label: label.to_string() }),
                // Old events.k8s.io duplicates and metrics are noise in a sidebar.
                None if is_noise(&ar) => {}
                // Remaining built-in APIs are cluster plumbing, not custom resources
                // (some CRD groups end in .k8s.io too: those are mapped above).
                None if is_builtin_group(&ar.group) && crd_section(&ar.group) == CUSTOM_SECTION => {
                    known.push(Kind { label: ar.kind.clone(), ar, namespaced, section: "Administration" })
                }
                None => custom.push(Kind { label: ar.kind.clone(), section: crd_section(&ar.group), ar, namespaced }),
            }
        }
        known.extend(custom);
        sort_for_sidebar(&mut known);
        Ok(known)
    })
    .await
}

/// Discovery for clusters without aggregated discovery (before 1.26, or turned off): every
/// version of each group, side by side, each kind kept at its most stable version. A group
/// that does not answer (an APIService that is down, like metrics.k8s.io) is left out, as
/// kubectl does, instead of failing them all; one with built-in kinds the sidebar lists
/// (Pods, Deployments…) is asked again, then fails discovery rather than leaving them out
/// without a word.
async fn discover_by_group(client: &Client) -> Result<Vec<(ApiResource, ApiCapabilities)>> {
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
    let (retry, skipped): (Vec<_>, Vec<_>) = failed.into_iter().partition(|(gv, _)| required(&gv.group));
    let skipped = skipped.into_iter().next().map(|(_, e)| e);
    for (gv, g) in pinned_groups(client, retry.into_iter().map(|(gv, _)| gv).collect()).await {
        match g {
            Ok(g) => resources.extend(g.recommended_resources()),
            Err(e) => return Err(anyhow::Error::from(e).context(format!("discovery of {} failed", gv.api_version()))),
        }
    }
    match skipped {
        Some(e) if resources.is_empty() => Err(e.into()),
        _ => Ok(most_stable(resources)),
    }
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
}
