//! The API versions Kubernetes releases stopped serving, from the deprecation guide
//! (<https://kubernetes.io/docs/reference/using-api/deprecation-guide/>): one entry per
//! API version and kind, each tested against the guide below.

/// An API version of a kind that a release no longer serves.
#[derive(Debug, PartialEq, Eq)]
pub struct Removal {
    /// The minor release (1.x) that stopped serving it.
    pub removed_in: u32,
    pub api_version: &'static str,
    pub kind: &'static str,
    /// The resource's plural, as the API server's metrics name it.
    pub plural: &'static str,
    pub namespaced: bool,
    /// The API version to use instead, as the guide says; None when nothing replaces it.
    pub replacement: Option<&'static str>,
    /// What changes on the way, when something notable does.
    pub note: &'static str,
}

const fn rm(
    removed_in: u32,
    api_version: &'static str,
    kind: &'static str,
    plural: &'static str,
    namespaced: bool,
    replacement: Option<&'static str>,
    note: &'static str,
) -> Removal {
    Removal { removed_in, api_version, kind, plural, namespaced, replacement, note }
}

const FLOW_V1: Option<&str> = Some("flowcontrol.apiserver.k8s.io/v1");
const HPA_NOTE: &str = "targetAverageUtilization becomes target.averageUtilization with target.type: Utilization";
const INGRESS_NOTE: &str = "spec.backend is spec.defaultBackend, serviceName and servicePort are service.name and \
                            service.port, and pathType is required";
const SAR_NOTE: &str = "spec.group is renamed spec.groups";
const WEBHOOK_NOTE: &str = "failurePolicy defaults to Fail, matchPolicy to Equivalent and timeoutSeconds to 10s; \
                            sideEffects (None or NoneOnDryRun) and admissionReviewVersions are required";
const SELECTOR_NOTE: &str = "spec.selector is required and immutable";
const ROLLING_NOTE: &str = "spec.selector is required and immutable, and updateStrategy defaults to RollingUpdate";

/// Every removal in the guide, newest release first.
pub static REMOVALS: &[Removal] = &[
    // v1.32
    rm(32, "flowcontrol.apiserver.k8s.io/v1beta3", "FlowSchema", "flowschemas", false, FLOW_V1, ""),
    rm(
        32,
        "flowcontrol.apiserver.k8s.io/v1beta3",
        "PriorityLevelConfiguration",
        "prioritylevelconfigurations",
        false,
        FLOW_V1,
        "spec.limited.nominalConcurrencyShares defaults to 30 only when unset: an explicit 0 stays 0",
    ),
    // v1.29
    rm(29, "flowcontrol.apiserver.k8s.io/v1beta2", "FlowSchema", "flowschemas", false, FLOW_V1, ""),
    rm(
        29,
        "flowcontrol.apiserver.k8s.io/v1beta2",
        "PriorityLevelConfiguration",
        "prioritylevelconfigurations",
        false,
        FLOW_V1,
        "spec.limited.assuredConcurrencyShares is renamed nominalConcurrencyShares",
    ),
    // v1.27
    rm(27, "storage.k8s.io/v1beta1", "CSIStorageCapacity", "csistoragecapacities", true, Some("storage.k8s.io/v1"), ""),
    // v1.26
    rm(
        26,
        "flowcontrol.apiserver.k8s.io/v1beta1",
        "FlowSchema",
        "flowschemas",
        false,
        Some("flowcontrol.apiserver.k8s.io/v1beta2"),
        "",
    ),
    rm(
        26,
        "flowcontrol.apiserver.k8s.io/v1beta1",
        "PriorityLevelConfiguration",
        "prioritylevelconfigurations",
        false,
        Some("flowcontrol.apiserver.k8s.io/v1beta2"),
        "",
    ),
    rm(
        26,
        "autoscaling/v2beta2",
        "HorizontalPodAutoscaler",
        "horizontalpodautoscalers",
        true,
        Some("autoscaling/v2"),
        HPA_NOTE,
    ),
    // v1.25
    rm(25, "batch/v1beta1", "CronJob", "cronjobs", true, Some("batch/v1"), ""),
    rm(
        25,
        "discovery.k8s.io/v1beta1",
        "EndpointSlice",
        "endpointslices",
        true,
        Some("discovery.k8s.io/v1"),
        "each endpoint's nodeName and zone replace topology, which becomes the read-only deprecatedTopology",
    ),
    rm(
        25,
        "events.k8s.io/v1beta1",
        "Event",
        "events",
        true,
        Some("events.k8s.io/v1"),
        "involvedObject is renamed regarding, type is Normal or Warning, and action, reason, reportingController \
         and reportingInstance are required",
    ),
    rm(
        25,
        "autoscaling/v2beta1",
        "HorizontalPodAutoscaler",
        "horizontalpodautoscalers",
        true,
        Some("autoscaling/v2"),
        HPA_NOTE,
    ),
    rm(
        25,
        "policy/v1beta1",
        "PodDisruptionBudget",
        "poddisruptionbudgets",
        true,
        Some("policy/v1"),
        "an empty spec.selector ({}) selects every pod of the namespace in policy/v1, none in policy/v1beta1",
    ),
    rm(
        25,
        "policy/v1beta1",
        "PodSecurityPolicy",
        "podsecuritypolicies",
        false,
        None,
        "nothing replaces it: move to Pod Security Admission or an admission webhook",
    ),
    rm(25, "node.k8s.io/v1beta1", "RuntimeClass", "runtimeclasses", false, Some("node.k8s.io/v1"), ""),
    // v1.22
    rm(
        22,
        "admissionregistration.k8s.io/v1beta1",
        "MutatingWebhookConfiguration",
        "mutatingwebhookconfigurations",
        false,
        Some("admissionregistration.k8s.io/v1"),
        WEBHOOK_NOTE,
    ),
    rm(
        22,
        "admissionregistration.k8s.io/v1beta1",
        "ValidatingWebhookConfiguration",
        "validatingwebhookconfigurations",
        false,
        Some("admissionregistration.k8s.io/v1"),
        WEBHOOK_NOTE,
    ),
    rm(
        22,
        "apiextensions.k8s.io/v1beta1",
        "CustomResourceDefinition",
        "customresourcedefinitions",
        false,
        Some("apiextensions.k8s.io/v1"),
        "spec.scope must be set and each version needs a structural schema; version, validation, subresources \
         and additionalPrinterColumns move under spec.versions",
    ),
    rm(22, "apiregistration.k8s.io/v1beta1", "APIService", "apiservices", false, Some("apiregistration.k8s.io/v1"), ""),
    rm(22, "authentication.k8s.io/v1beta1", "TokenReview", "tokenreviews", false, Some("authentication.k8s.io/v1"), ""),
    rm(
        22,
        "authorization.k8s.io/v1beta1",
        "LocalSubjectAccessReview",
        "localsubjectaccessreviews",
        true,
        Some("authorization.k8s.io/v1"),
        SAR_NOTE,
    ),
    rm(
        22,
        "authorization.k8s.io/v1beta1",
        "SelfSubjectAccessReview",
        "selfsubjectaccessreviews",
        false,
        Some("authorization.k8s.io/v1"),
        SAR_NOTE,
    ),
    rm(
        22,
        "authorization.k8s.io/v1beta1",
        "SubjectAccessReview",
        "subjectaccessreviews",
        false,
        Some("authorization.k8s.io/v1"),
        SAR_NOTE,
    ),
    rm(
        22,
        "authorization.k8s.io/v1beta1",
        "SelfSubjectRulesReview",
        "selfsubjectrulesreviews",
        false,
        Some("authorization.k8s.io/v1"),
        SAR_NOTE,
    ),
    rm(
        22,
        "certificates.k8s.io/v1beta1",
        "CertificateSigningRequest",
        "certificatesigningrequests",
        false,
        Some("certificates.k8s.io/v1"),
        "spec.signerName and spec.usages are required",
    ),
    rm(22, "coordination.k8s.io/v1beta1", "Lease", "leases", true, Some("coordination.k8s.io/v1"), ""),
    rm(22, "extensions/v1beta1", "Ingress", "ingresses", true, Some("networking.k8s.io/v1"), INGRESS_NOTE),
    rm(22, "networking.k8s.io/v1beta1", "Ingress", "ingresses", true, Some("networking.k8s.io/v1"), INGRESS_NOTE),
    rm(22, "networking.k8s.io/v1beta1", "IngressClass", "ingressclasses", false, Some("networking.k8s.io/v1"), ""),
    rm(
        22,
        "rbac.authorization.k8s.io/v1beta1",
        "ClusterRole",
        "clusterroles",
        false,
        Some("rbac.authorization.k8s.io/v1"),
        "",
    ),
    rm(
        22,
        "rbac.authorization.k8s.io/v1beta1",
        "ClusterRoleBinding",
        "clusterrolebindings",
        false,
        Some("rbac.authorization.k8s.io/v1"),
        "",
    ),
    rm(22, "rbac.authorization.k8s.io/v1beta1", "Role", "roles", true, Some("rbac.authorization.k8s.io/v1"), ""),
    rm(
        22,
        "rbac.authorization.k8s.io/v1beta1",
        "RoleBinding",
        "rolebindings",
        true,
        Some("rbac.authorization.k8s.io/v1"),
        "",
    ),
    rm(22, "scheduling.k8s.io/v1beta1", "PriorityClass", "priorityclasses", false, Some("scheduling.k8s.io/v1"), ""),
    rm(22, "storage.k8s.io/v1beta1", "CSIDriver", "csidrivers", false, Some("storage.k8s.io/v1"), ""),
    rm(22, "storage.k8s.io/v1beta1", "CSINode", "csinodes", false, Some("storage.k8s.io/v1"), ""),
    rm(22, "storage.k8s.io/v1beta1", "StorageClass", "storageclasses", false, Some("storage.k8s.io/v1"), ""),
    rm(22, "storage.k8s.io/v1beta1", "VolumeAttachment", "volumeattachments", false, Some("storage.k8s.io/v1"), ""),
    // v1.16
    rm(16, "extensions/v1beta1", "NetworkPolicy", "networkpolicies", true, Some("networking.k8s.io/v1"), ""),
    rm(16, "extensions/v1beta1", "DaemonSet", "daemonsets", true, Some("apps/v1"), ROLLING_NOTE),
    rm(16, "apps/v1beta2", "DaemonSet", "daemonsets", true, Some("apps/v1"), ROLLING_NOTE),
    rm(
        16,
        "extensions/v1beta1",
        "Deployment",
        "deployments",
        true,
        Some("apps/v1"),
        "spec.selector is required and immutable, spec.rollbackTo is gone, and the defaults of \
         progressDeadlineSeconds, revisionHistoryLimit, maxSurge and maxUnavailable change",
    ),
    rm(
        16,
        "apps/v1beta1",
        "Deployment",
        "deployments",
        true,
        Some("apps/v1"),
        "spec.selector is required and immutable, spec.rollbackTo is gone, and revisionHistoryLimit defaults to 10",
    ),
    rm(16, "apps/v1beta2", "Deployment", "deployments", true, Some("apps/v1"), SELECTOR_NOTE),
    rm(16, "apps/v1beta1", "StatefulSet", "statefulsets", true, Some("apps/v1"), ROLLING_NOTE),
    rm(16, "apps/v1beta2", "StatefulSet", "statefulsets", true, Some("apps/v1"), SELECTOR_NOTE),
    rm(16, "extensions/v1beta1", "ReplicaSet", "replicasets", true, Some("apps/v1"), SELECTOR_NOTE),
    rm(16, "apps/v1beta1", "ReplicaSet", "replicasets", true, Some("apps/v1"), SELECTOR_NOTE),
    rm(16, "apps/v1beta2", "ReplicaSet", "replicasets", true, Some("apps/v1"), SELECTOR_NOTE),
    rm(
        16,
        "extensions/v1beta1",
        "PodSecurityPolicy",
        "podsecuritypolicies",
        false,
        Some("policy/v1beta1"),
        "policy/v1beta1 is removed in 1.25 too",
    ),
];

impl Removal {
    pub fn group(&self) -> &'static str {
        self.api_version.split_once('/').map_or("", |(g, _)| g)
    }

    pub fn version(&self) -> &'static str {
        self.api_version.rsplit('/').next().unwrap_or(self.api_version)
    }

    /// What to move to when the cluster runs `target`: the guide's replacement, or its own
    /// replacement when `target` removes that too (flowcontrol v1beta1 → v1beta2 → v1).
    pub fn replacement_at(&self, target: u32) -> Option<&'static str> {
        let mut next = self.replacement?;
        // Each step is an entry of the table: no more steps than entries.
        for _ in 0..REMOVALS.len() {
            match find(next, self.kind) {
                Some(r) if r.removed_in <= target => next = r.replacement?,
                _ => return Some(next),
            }
        }
        Some(next)
    }

    /// What to do, for a cluster going to `target`: "Migrate HorizontalPodAutoscaler
    /// autoscaling/v2beta2 → autoscaling/v2 (…)".
    pub fn advice(&self, target: u32) -> String {
        let step = match self.replacement_at(target) {
            Some(to) => format!("Migrate {} {} → {to}", self.kind, self.api_version),
            None => format!("Stop using {} {}", self.kind, self.api_version),
        };
        if self.note.is_empty() { step } else { format!("{step}: {}", self.note) }
    }

    /// The kubectl-style name of the kind as the cluster serves it after the upgrade, for
    /// links: `horizontalpodautoscalers.autoscaling`. Events open as the core kind (the
    /// sidebar has no events.k8s.io).
    pub fn kubectl_kind(&self) -> String {
        if self.kind == "Event" {
            return "events".into();
        }
        let api_version = self.replacement.unwrap_or(self.api_version);
        match api_version.split_once('/') {
            Some((group, _)) => format!("{}.{group}", self.plural),
            None => self.plural.to_string(),
        }
    }
}

/// The removal of `kind` in `api_version`, if a release removed it.
pub fn find(api_version: &str, kind: &str) -> Option<&'static Removal> {
    REMOVALS.iter().find(|r| r.api_version == api_version && r.kind == kind)
}

/// The removal of a resource as the API server's metrics name it (group, version, plural).
pub fn find_resource(group: &str, version: &str, plural: &str) -> Option<&'static Removal> {
    REMOVALS.iter().find(|r| r.group() == group && r.version() == version && r.plural == plural)
}

/// Whether some release up to `target` removed an API version (of any kind).
pub fn removes_version(api_version: &str, target: u32) -> bool {
    REMOVALS.iter().any(|r| r.api_version == api_version && r.removed_in <= target)
}

/// The removals of the releases after `current`, up to `target`.
pub fn between(current: u32, target: u32) -> impl Iterator<Item = &'static Removal> {
    REMOVALS.iter().filter(move |r| r.removed_in > current && r.removed_in <= target)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A release of the guide: (release, the API versions it stopped serving, the kinds, the
    /// version the guide says to migrate to).
    type GuideRelease = (u32, &'static [&'static str], &'static [&'static str], Option<&'static str>);

    /// The guide, release by release.
    const GUIDE: &[GuideRelease] = &[
        (
            32,
            &["flowcontrol.apiserver.k8s.io/v1beta3"],
            &["FlowSchema", "PriorityLevelConfiguration"],
            Some("flowcontrol.apiserver.k8s.io/v1"),
        ),
        (
            29,
            &["flowcontrol.apiserver.k8s.io/v1beta2"],
            &["FlowSchema", "PriorityLevelConfiguration"],
            Some("flowcontrol.apiserver.k8s.io/v1"),
        ),
        (27, &["storage.k8s.io/v1beta1"], &["CSIStorageCapacity"], Some("storage.k8s.io/v1")),
        (
            26,
            &["flowcontrol.apiserver.k8s.io/v1beta1"],
            &["FlowSchema", "PriorityLevelConfiguration"],
            Some("flowcontrol.apiserver.k8s.io/v1beta2"),
        ),
        (26, &["autoscaling/v2beta2"], &["HorizontalPodAutoscaler"], Some("autoscaling/v2")),
        (25, &["batch/v1beta1"], &["CronJob"], Some("batch/v1")),
        (25, &["discovery.k8s.io/v1beta1"], &["EndpointSlice"], Some("discovery.k8s.io/v1")),
        (25, &["events.k8s.io/v1beta1"], &["Event"], Some("events.k8s.io/v1")),
        (25, &["autoscaling/v2beta1"], &["HorizontalPodAutoscaler"], Some("autoscaling/v2")),
        (25, &["policy/v1beta1"], &["PodDisruptionBudget"], Some("policy/v1")),
        (25, &["policy/v1beta1"], &["PodSecurityPolicy"], None),
        (25, &["node.k8s.io/v1beta1"], &["RuntimeClass"], Some("node.k8s.io/v1")),
        (
            22,
            &["admissionregistration.k8s.io/v1beta1"],
            &["MutatingWebhookConfiguration", "ValidatingWebhookConfiguration"],
            Some("admissionregistration.k8s.io/v1"),
        ),
        (22, &["apiextensions.k8s.io/v1beta1"], &["CustomResourceDefinition"], Some("apiextensions.k8s.io/v1")),
        (22, &["apiregistration.k8s.io/v1beta1"], &["APIService"], Some("apiregistration.k8s.io/v1")),
        (22, &["authentication.k8s.io/v1beta1"], &["TokenReview"], Some("authentication.k8s.io/v1")),
        (
            22,
            &["authorization.k8s.io/v1beta1"],
            &["LocalSubjectAccessReview", "SelfSubjectAccessReview", "SubjectAccessReview", "SelfSubjectRulesReview"],
            Some("authorization.k8s.io/v1"),
        ),
        (22, &["certificates.k8s.io/v1beta1"], &["CertificateSigningRequest"], Some("certificates.k8s.io/v1")),
        (22, &["coordination.k8s.io/v1beta1"], &["Lease"], Some("coordination.k8s.io/v1")),
        (22, &["extensions/v1beta1", "networking.k8s.io/v1beta1"], &["Ingress"], Some("networking.k8s.io/v1")),
        (22, &["networking.k8s.io/v1beta1"], &["IngressClass"], Some("networking.k8s.io/v1")),
        (
            22,
            &["rbac.authorization.k8s.io/v1beta1"],
            &["ClusterRole", "ClusterRoleBinding", "Role", "RoleBinding"],
            Some("rbac.authorization.k8s.io/v1"),
        ),
        (22, &["scheduling.k8s.io/v1beta1"], &["PriorityClass"], Some("scheduling.k8s.io/v1")),
        (
            22,
            &["storage.k8s.io/v1beta1"],
            &["CSIDriver", "CSINode", "StorageClass", "VolumeAttachment"],
            Some("storage.k8s.io/v1"),
        ),
        (16, &["extensions/v1beta1"], &["NetworkPolicy"], Some("networking.k8s.io/v1")),
        (16, &["extensions/v1beta1", "apps/v1beta2"], &["DaemonSet"], Some("apps/v1")),
        (16, &["extensions/v1beta1", "apps/v1beta1", "apps/v1beta2"], &["Deployment"], Some("apps/v1")),
        (16, &["apps/v1beta1", "apps/v1beta2"], &["StatefulSet"], Some("apps/v1")),
        (16, &["extensions/v1beta1", "apps/v1beta1", "apps/v1beta2"], &["ReplicaSet"], Some("apps/v1")),
        (16, &["extensions/v1beta1"], &["PodSecurityPolicy"], Some("policy/v1beta1")),
    ];

    /// Each (release, API version, kind, replacement) of the guide, once.
    fn guide_entries() -> Vec<(u32, &'static str, &'static str, Option<&'static str>)> {
        GUIDE
            .iter()
            .flat_map(|(release, versions, kinds, to)| {
                versions.iter().flat_map(move |v| kinds.iter().map(move |k| (*release, *v, *k, *to)))
            })
            .collect()
    }

    /// Every removal of the guide is in the table, with its release and replacement, and the
    /// table has nothing the guide doesn't.
    #[test]
    fn the_table_is_the_guide() {
        let guide = guide_entries();
        for &(release, api_version, kind, to) in &guide {
            let r = find(api_version, kind).unwrap_or_else(|| panic!("{kind} {api_version} is missing"));
            assert_eq!((r.removed_in, r.replacement), (release, to), "{kind} {api_version}");
        }
        assert_eq!(REMOVALS.len(), guide.len(), "the table has entries the guide doesn't");
        for r in REMOVALS {
            let n = REMOVALS.iter().filter(|o| o.api_version == r.api_version && o.kind == r.kind).count();
            assert_eq!(n, 1, "{} {} twice", r.kind, r.api_version);
        }
    }

    /// Each entry is found the ways the check looks it up, and its advice leads somewhere
    /// the target still serves.
    #[test]
    fn each_entry_is_found_and_advised() {
        for r in REMOVALS {
            let what = format!("{} {}", r.kind, r.api_version);
            assert!(!r.group().is_empty() && r.version().starts_with('v'), "{what}");
            assert_eq!(format!("{}/{}", r.group(), r.version()), r.api_version, "{what}");
            assert_eq!(find_resource(r.group(), r.version(), r.plural), Some(r), "{what}");
            assert_eq!(r.plural, r.plural.to_lowercase(), "{what}");
            assert!(r.plural.starts_with(&r.kind.to_lowercase()[..r.kind.len() - 1]), "{what}: plural {}", r.plural);
            assert!(removes_version(r.api_version, r.removed_in), "{what}");
            assert_eq!(between(r.removed_in - 1, r.removed_in).filter(|o| *o == r).count(), 1, "{what}");
            assert_eq!(between(r.removed_in, 99).filter(|o| *o == r).count(), 0, "{what}");
            for target in r.removed_in..40 {
                let advice = r.advice(target);
                assert!(advice.contains(r.kind) && advice.contains(r.api_version), "{what}: {advice}");
                match r.replacement_at(target) {
                    Some(to) => {
                        assert_ne!(to, r.api_version, "{what}");
                        assert!(find(to, r.kind).is_none_or(|n| n.removed_in > target), "{what} → {to} at 1.{target}");
                        assert!(advice.contains(&format!("→ {to}")), "{what}: {advice}");
                    }
                    None => assert!(advice.starts_with("Stop using"), "{what}: {advice}"),
                }
            }
            let link = r.kubectl_kind();
            assert!(link.starts_with(r.plural), "{what}: {link}");
        }
    }

    #[test]
    fn replacements_follow_later_removals() {
        let fs = find("flowcontrol.apiserver.k8s.io/v1beta1", "FlowSchema").unwrap();
        assert_eq!(fs.replacement_at(26), Some("flowcontrol.apiserver.k8s.io/v1beta2"));
        assert_eq!(fs.replacement_at(29), Some("flowcontrol.apiserver.k8s.io/v1"));
        let psp = find("extensions/v1beta1", "PodSecurityPolicy").unwrap();
        assert_eq!(psp.replacement_at(16), Some("policy/v1beta1"));
        assert_eq!(psp.replacement_at(25), None);
        assert!(psp.advice(25).starts_with("Stop using PodSecurityPolicy extensions/v1beta1"));
        let hpa = find("autoscaling/v2beta2", "HorizontalPodAutoscaler").unwrap();
        assert!(hpa.advice(26).starts_with("Migrate HorizontalPodAutoscaler autoscaling/v2beta2 → autoscaling/v2: "));
        assert_eq!(hpa.kubectl_kind(), "horizontalpodautoscalers.autoscaling");
        assert_eq!(find("extensions/v1beta1", "Ingress").unwrap().kubectl_kind(), "ingresses.networking.k8s.io");
        assert_eq!(find("events.k8s.io/v1beta1", "Event").unwrap().kubectl_kind(), "events");
    }
}
