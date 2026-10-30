use proptest::prelude::*;
use serde_json::{Value, json};

use super::*;

fn kind(group: &str, version: &str, k: &str, plural: &str) -> Kind {
    let api_version = if group.is_empty() { version.to_string() } else { format!("{group}/{version}") };
    Kind {
        ar: kube::discovery::ApiResource {
            group: group.into(),
            version: version.into(),
            api_version,
            kind: k.into(),
            plural: plural.into(),
        },
        namespaced: true,
        section: "",
        label: k.into(),
    }
}

fn lean<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).unwrap()
}

fn forbidden() -> anyhow::Error {
    kube::Error::Api(kube::core::Status::failure("forbidden", "Forbidden").with_code(403).boxed()).into()
}

fn release(manifest: &str) -> crate::helm::Release {
    let payload = crate::helm::tests::payload("web", "shop", 3);
    let secret = crate::helm::tests::helm_secret(("sh.helm.release.v1.web.v3", "shop", "web"), &payload, false);
    let mut r = crate::helm::decode(&secret).unwrap();
    r.manifest = manifest.into();
    r
}

#[test]
fn minor_versions() {
    assert_eq!(minor_of("v1.30.4-eks-1a2b"), Some(30));
    assert_eq!(minor_of("1.31"), Some(31));
    assert_eq!(minor_of("v1.29.0+k3s1"), Some(29));
    assert_eq!(minor_of("1.33+"), Some(33));
    assert_eq!(minor_of("v2.1.0"), None);
    assert_eq!(minor_of("v1."), None);
    assert_eq!(minor_of(""), None);
}

#[test]
fn the_skew_policy() {
    // Three minor versions behind from 1.25 on, two before.
    assert_eq!(oldest_node_version(31), 28);
    assert_eq!(oldest_node_version(28), 25);
    assert_eq!(oldest_node_version(27), 25);
    assert_eq!(oldest_node_version(26), 24);
    assert_eq!(oldest_node_version(24), 22);
}

proptest! {
    /// Any 1.x version string gives its minor version, whatever follows it.
    #[test]
    fn any_version_gives_its_minor(minor in 0..200u32, patch in 0..50u32, suffix in "[-+][a-z0-9.]{0,12}", v in any::<bool>()) {
        let text = format!("{}1.{minor}.{patch}{suffix}", if v { "v" } else { "" });
        prop_assert_eq!(minor_of(&text), Some(minor));
    }

    /// A node version is allowed exactly when it is at most its lag behind the target.
    #[test]
    fn oldest_allowed_is_the_policy(target in 16..200u32, minor in 0..200u32) {
        prop_assume!(minor <= target);
        let lag = if minor >= 25 { 3 } else { 2 };
        prop_assert_eq!(minor >= oldest_node_version(target), target - minor <= lag);
    }
}

#[test]
fn objects_written_in_a_removed_version_are_found() {
    let hpa = kind("autoscaling", "v2", "HorizontalPodAutoscaler", "horizontalpodautoscalers");
    let removals: Vec<&Removal> = removed::between(25, 26).collect();
    let items: Vec<ObjectLite> = lean(json!([
        {"metadata": {"name": "by-helm", "namespace": "a", "managedFields": [
            {"manager": "helm", "apiVersion": "autoscaling/v2beta2"},
            {"manager": "kube-controller-manager", "apiVersion": "autoscaling/v2"}]}},
        {"metadata": {"name": "applied", "namespace": "a", "annotations": {
            "kubectl.kubernetes.io/last-applied-configuration":
                "{\"apiVersion\":\"autoscaling/v2beta2\",\"kind\":\"HorizontalPodAutoscaler\",\"metadata\":{\"name\":\"applied\"}}"}}},
        {"metadata": {"name": "current", "namespace": "a", "managedFields": [
            {"manager": "kubectl", "apiVersion": "autoscaling/v2"}]}},
        {"metadata": {"name": "nothing", "namespace": "a"}},
    ]));
    let found = object_uses(&hpa, &removals, &items);
    let names: Vec<(&str, &str)> = found.iter().map(|(r, o)| (r.api_version, o.name.as_str())).collect();
    assert_eq!(names, [("autoscaling/v2beta2", "by-helm"), ("autoscaling/v2beta2", "applied")]);
    assert_eq!(found[0].1.note, "written by helm in autoscaling/v2beta2");
    assert_eq!(found[0].1.kind, "horizontalpodautoscalers.autoscaling");
    assert_eq!(found[0].1.namespace.as_deref(), Some("a"));
    assert!(found[1].1.note.contains("kubectl apply"));
}

#[test]
fn the_kind_is_listed_at_a_version_still_served() {
    let kinds = [
        kind("extensions", "v1beta1", "Ingress", "ingresses"),
        kind("networking.k8s.io", "v1", "Ingress", "ingresses"),
        kind("example.com", "v1", "Ingress", "ingresses"),
        kind("", "v1", "Event", "events"),
    ];
    let ingress = removed::find("networking.k8s.io/v1beta1", "Ingress").unwrap();
    assert_eq!(served_kind(&kinds, ingress).map(|k| k.ar.api_version.as_str()), Some("networking.k8s.io/v1"));
    let event = removed::find("events.k8s.io/v1beta1", "Event").unwrap();
    assert_eq!(served_kind(&kinds, event).map(|k| k.ar.api_version.as_str()), Some("v1"));
    // A kind of another group with the same name is not it.
    let only_custom = [kind("example.com", "v1", "Ingress", "ingresses")];
    assert!(served_kind(&only_custom, ingress).is_none());
    let review = removed::find("authentication.k8s.io/v1beta1", "TokenReview").unwrap();
    assert!(served_kind(&kinds, review).is_none());
}

#[test]
fn helm_manifests_naming_removed_versions() {
    let manifest = "---\n# Source: web/templates/ingress.yaml\napiVersion: extensions/v1beta1\nkind: Ingress\nmetadata:\n  name: web\n---\n# Source: web/templates/role.yaml\napiVersion: \"rbac.authorization.k8s.io/v1beta1\"\nkind: ClusterRole\nmetadata:\n  name: web-reader\n---\napiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n---\napiVersion: flowcontrol.apiserver.k8s.io/v1beta3\nkind: FlowSchema\nmetadata:\n  name: web\n---\napiVersion: batch/v1beta1\nkind: CronJob\nmetadata:\n  name: nightly\n  namespace: jobs\n";
    let r = release(manifest);
    let found = manifest_uses(&r, 31);
    let got: Vec<(&str, &str, Option<&str>, &str)> =
        found.iter().map(|(r, o)| (r.kind, o.kind.as_str(), o.namespace.as_deref(), o.name.as_str())).collect();
    assert_eq!(
        got,
        [
            ("Ingress", "ingresses.networking.k8s.io", Some("shop"), "web"),
            ("ClusterRole", "clusterroles.rbac.authorization.k8s.io", None, "web-reader"),
            ("CronJob", "cronjobs.batch", Some("jobs"), "nightly"),
        ]
    );
    let helm = found[0].1.helm.clone().unwrap();
    assert_eq!(helm.secret_name(), "sh.helm.release.v1.web.v3");
    assert_eq!(found[0].1.note, "in Helm release shop/web (revision 3)");
    // 1.32 removes flowcontrol v1beta3 too.
    assert!(manifest_uses(&r, 32).iter().any(|(r, _)| r.kind == "FlowSchema"));
}

#[test]
fn metric_lines() {
    let mut m = ApiMetrics::default();
    for line in [
        "# HELP apiserver_requested_deprecated_apis [STABLE] Gauge of deprecated APIs that have been requested",
        "# TYPE apiserver_requested_deprecated_apis gauge",
        r#"apiserver_requested_deprecated_apis{group="flowcontrol.apiserver.k8s.io",removed_release="1.32",resource="flowschemas",subresource="",version="v1beta3"} 1"#,
        r#"apiserver_requested_deprecated_apis{group="x.io",removed_release="",resource="things",subresource="status",version="v1"} 0"#,
        r#"apiserver_requested_deprecated_apis_total{group="a"} 3"#,
        r#"kubernetes_feature_enabled{name="OldThing",stage="DEPRECATED"} 0"#,
        r#"kubernetes_feature_enabled{name="New\"Thing",stage="ALPHA"} 1"#,
        r#"kubernetes_feature_enabled{name="Done",stage=""} 1"#,
    ] {
        m.add(line);
    }
    assert_eq!(
        m.deprecated,
        [DeprecatedCall {
            group: "flowcontrol.apiserver.k8s.io".into(),
            version: "v1beta3".into(),
            resource: "flowschemas".into(),
            subresource: String::new(),
            removed_release: "1.32".into(),
        }]
    );
    let gates: Vec<(&str, &str, bool)> =
        m.gates.iter().map(|g| (g.name.as_str(), g.stage.as_str(), g.enabled)).collect();
    assert_eq!(gates, [("OldThing", "DEPRECATED", false), ("New\"Thing", "ALPHA", true), ("Done", "", true)]);
    let findings = gate_findings(33, &m.gates);
    assert_eq!(findings.len(), 2);
    assert_eq!(findings[0].details, ["OldThing (off)"]);
    assert_eq!(findings[1].details, ["New\"Thing (on)"]);
    assert!(findings[1].action.contains("1.33"));
}

fn answers() -> Answers {
    Answers {
        current: 31,
        target: 32,
        live: Ok((Vec::new(), Vec::new())),
        helm: Ok((Vec::new(), 0)),
        metrics: Ok(ApiMetrics::default()),
        nodes: Ok(Vec::new()),
        pods: Ok(Vec::new()),
        pdbs: Ok(Vec::new()),
        crds: Ok(Vec::new()),
    }
}

#[test]
fn a_part_refused_keeps_the_others() {
    let nodes: Vec<NodeLite> =
        lean(json!([{"metadata": {"name": "old"}, "status": {"nodeInfo": {"kubeletVersion": "v1.27.3"}}}]));
    let report = assemble(Answers { nodes: Ok(nodes), pods: Err(forbidden()), crds: Err(forbidden()), ..answers() });
    assert_eq!(
        report.skipped,
        [
            "Pods: forbidden (kube-proxy versions and pods under several budgets not checked)",
            "CRDs: forbidden (add-on versions not checked)"
        ]
    );
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].title, "1 node(s) on kubelet 1.27, 5 minor versions behind 1.32");
    // Nothing found and nothing refused: an all-clear.
    let clear = assemble(answers());
    assert!(clear.findings.is_empty() && clear.skipped.is_empty());
    assert!(to_text("c", &clear, |_| true).contains("Nothing to do found.\n"));
    let refused = assemble(Answers { metrics: Err(forbidden()), ..answers() });
    assert!(to_text("c", &refused, |_| true).contains("Nothing to do found in what could be read."));
}

#[test]
fn one_finding_per_removed_api_with_every_source() {
    let fs = removed::find("flowcontrol.apiserver.k8s.io/v1beta3", "FlowSchema").unwrap();
    let ingress = removed::find("extensions/v1beta1", "Ingress").unwrap();
    let live = (fs, Object::new("flowschemas.flowcontrol.apiserver.k8s.io", None, "mine", "written by x"));
    let mut charted = Object::new("ingresses.networking.k8s.io", Some("shop"), "web", "in Helm release shop/web");
    charted.helm = Some(HelmRef { namespace: "shop".into(), release: "web".into(), revision: 3 });
    let metrics = ApiMetrics {
        deprecated: vec![
            DeprecatedCall {
                group: "flowcontrol.apiserver.k8s.io".into(),
                version: "v1beta3".into(),
                resource: "flowschemas".into(),
                removed_release: "1.32".into(),
                ..Default::default()
            },
            // Not removed by the target: not a finding.
            DeprecatedCall {
                group: "a.k8s.io".into(),
                version: "v1beta1".into(),
                resource: "widgets".into(),
                removed_release: "1.36".into(),
                ..Default::default()
            },
            // Not in the table: its own finding.
            DeprecatedCall {
                group: "b.k8s.io".into(),
                version: "v1beta1".into(),
                resource: "gadgets".into(),
                removed_release: "1.32".into(),
                ..Default::default()
            },
        ],
        gates: Vec::new(),
    };
    let report = assemble(Answers {
        live: Ok((vec![live], Vec::new())),
        helm: Ok((vec![(ingress, charted)], 2)),
        metrics: Ok(metrics),
        ..answers()
    });
    let titles: Vec<&str> = report.findings.iter().map(|f| f.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "FlowSchema flowcontrol.apiserver.k8s.io/v1beta3: removed in 1.32",
            "Ingress extensions/v1beta1: removed in 1.22 already",
            "gadgets b.k8s.io/v1beta1: removed in 1.32",
        ]
    );
    let flow = &report.findings[0];
    assert_eq!(flow.severity, Severity::Critical);
    assert_eq!(
        flow.action,
        "Migrate FlowSchema flowcontrol.apiserver.k8s.io/v1beta3 → flowcontrol.apiserver.k8s.io/v1"
    );
    assert_eq!(flow.objects.len(), 1);
    assert!(flow.details[0].contains("flowschemas in flowcontrol.apiserver.k8s.io/v1beta3"));
    assert!(report.findings[1].action.contains("helm-mapkubeapis") && report.findings[1].details.is_empty());
    assert!(!flow.action.contains("Helm"));
    assert_eq!(report.skipped, ["Helm: 2 release(s) could not be read (their manifests not checked)"]);
}

#[test]
fn skew_of_kubelets_and_kube_proxies() {
    let nodes: Vec<NodeLite> = lean(json!([
        {"metadata": {"name": "a"}, "status": {"nodeInfo": {"kubeletVersion": "v1.28.1"}}},
        {"metadata": {"name": "b"}, "status": {"nodeInfo": {"kubeletVersion": "v1.27.9"}}},
        {"metadata": {"name": "c"}, "status": {"nodeInfo": {"kubeletVersion": "v1.33.0"}}},
        {"metadata": {"name": "d"}, "status": {"nodeInfo": {"kubeletVersion": "v1.31.0"}}},
    ]));
    let found = kubelet_skew(31, &nodes);
    let titles: Vec<(Severity, &str)> = found.iter().map(|f| (f.severity, f.title.as_str())).collect();
    assert_eq!(
        titles,
        [
            (Severity::Critical, "1 node(s) on kubelet 1.27, 4 minor versions behind 1.31"),
            (Severity::Warning, "1 node(s) on kubelet 1.33, newer than 1.31"),
        ]
    );
    assert_eq!(found[0].objects[0].name, "b");
    assert!(found[0].action.contains("kubelet 1.28 or newer"));

    assert_eq!(kube_proxy_minor("registry.k8s.io/kube-proxy:v1.30.2"), Some(30));
    assert_eq!(
        kube_proxy_minor("602401143452.dkr.ecr.eu-west-1.amazonaws.com/eks/kube-proxy:v1.29.0-minimal-eksbuild.1"),
        Some(29)
    );
    assert_eq!(kube_proxy_minor("gke.gcr.io/kube-proxy-amd64:v1.29.1-gke.1589000"), Some(29));
    assert_eq!(kube_proxy_minor("localhost:5000/kube-proxy:v1.30.0"), Some(30));
    assert_eq!(kube_proxy_minor("registry.k8s.io/kube-proxy@sha256:abc"), None);
    assert_eq!(kube_proxy_minor("cilium/cilium:v1.16.0"), None);

    let pods: Vec<PodLite> = lean(json!([
        {"metadata": {"name": "kube-proxy-a", "namespace": "kube-system"},
         "spec": {"nodeName": "a", "containers": [{"image": "registry.k8s.io/kube-proxy:v1.27.1"}]}},
        {"metadata": {"name": "kube-proxy-d", "namespace": "kube-system"},
         "spec": {"nodeName": "d", "containers": [{"image": "registry.k8s.io/kube-proxy:v1.31.1"}]}},
        // Not kube-system's: not the cluster's kube-proxy.
        {"metadata": {"name": "kube-proxy-x", "namespace": "lab"},
         "spec": {"nodeName": "a", "containers": [{"image": "registry.k8s.io/kube-proxy:v1.20.1"}]}},
    ]));
    let found = kube_proxy_skew(31, &pods, Some(nodes.as_slice()));
    let titles: Vec<&str> = found.iter().map(|f| f.title.as_str()).collect();
    assert_eq!(titles, ["kube-proxy 1.27: 4 minor versions behind 1.31"]);
    assert_eq!(found[0].objects[0].name, "kube-proxy-a");
    // Four minor versions from the kubelet next to it.
    let nodes: Vec<NodeLite> =
        lean(json!([{"metadata": {"name": "d"}, "status": {"nodeInfo": {"kubeletVersion": "v1.27.0"}}}]));
    let found = kube_proxy_skew(31, &pods, Some(nodes.as_slice()));
    assert!(
        found.iter().any(|f| f.title.starts_with("kube-proxy more than three") && f.objects[0].name == "kube-proxy-d")
    );

    assert!(control_plane_hop(30, 31).is_none());
    let hop = control_plane_hop(30, 32).unwrap();
    assert_eq!(hop.action, "The control plane upgrades one minor version at a time: 1.30 → 1.31 → 1.32");
}

fn pdb(name: &str, selector: Value, mut spec: Value, status: Value) -> Value {
    spec["selector"] = selector;
    json!({"metadata": {"name": name, "namespace": "a"}, "spec": spec, "status": status})
}

fn pod(name: &str, labels: Value, phase: &str) -> Value {
    json!({"metadata": {"name": name, "namespace": "a", "labels": labels}, "spec": {"nodeName": "n1"},
        "status": {"phase": phase}})
}

#[test]
fn drains_that_would_block() {
    let pdbs = [
        // One pod, none may go.
        pdb(
            "single",
            json!({"matchLabels": {"app": "db"}}),
            json!({"minAvailable": 1}),
            json!({"disruptionsAllowed": 0, "expectedPods": 1, "currentHealthy": 1}),
        ),
        // Three pods, one unhealthy: none may go now.
        pdb(
            "tight",
            json!({"matchLabels": {"app": "web"}}),
            json!({"maxUnavailable": 1}),
            json!({"disruptionsAllowed": 0, "expectedPods": 3, "currentHealthy": 2}),
        ),
        // Every pod of the namespace (policy/v1): overlaps the others.
        pdb("all", json!({}), json!({"maxUnavailable": "50%"}), json!({"disruptionsAllowed": 2, "expectedPods": 5})),
        // No selector: selects nothing.
        pdb("none", Value::Null, json!({"minAvailable": 1}), json!({"disruptionsAllowed": 0, "expectedPods": 0})),
    ];
    let pods: Vec<PodLite> = lean(json!([
        pod("db-0", json!({"app": "db"}), "Running"),
        pod("web-1", json!({"app": "web"}), "Running"),
        pod("web-2", json!({"app": "web"}), "Pending"),
        pod("job-1", json!({"app": "web"}), "Succeeded"),
        pod("other", json!({"app": "other"}), "Running"),
    ]));
    let found = drain_findings(&pdbs, Some(pods.as_slice()));
    let titles: Vec<&str> = found.iter().map(|f| f.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "A PodDisruptionBudget guards a single pod: draining its node waits forever",
            "A PodDisruptionBudget allows no disruption (2 of 3 pods healthy)",
            "1 pod(s) selected by 2 PodDisruptionBudgets",
            "2 pod(s) selected by 2 PodDisruptionBudgets",
        ]
    );
    let names = |f: &Finding| f.objects.iter().map(|o| format!("{} {}", o.kind, o.name)).collect::<Vec<_>>();
    assert_eq!(names(&found[0]), ["poddisruptionbudgets.policy single", "pods db-0"]);
    assert_eq!(found[0].objects[0].note, "minAvailable 1");
    // The finished pod is left out: its eviction ignores budgets.
    assert_eq!(names(&found[1]), ["poddisruptionbudgets.policy tight", "pods web-1", "pods web-2"]);
    assert_eq!(
        names(&found[2]),
        ["poddisruptionbudgets.policy all", "poddisruptionbudgets.policy single", "pods db-0"]
    );
    assert_eq!(
        names(&found[3]),
        ["poddisruptionbudgets.policy all", "poddisruptionbudgets.policy tight", "pods web-1", "pods web-2"]
    );
    // Pods unknown: the budgets still.
    assert_eq!(drain_findings(&pdbs, None).len(), 2);
}

#[test]
fn deprecated_crd_versions() {
    let crds: Vec<CrdLite> = lean(json!([
        {"metadata": {"name": "certificates.cert-manager.io"}, "spec": {"group": "cert-manager.io",
            "names": {"kind": "Certificate"}, "versions": [
                {"name": "v1alpha2", "served": true, "deprecated": true, "deprecationWarning": "v1alpha2 is going away"},
                {"name": "v1beta1", "served": true, "deprecated": true},
                {"name": "v1", "served": true, "storage": true},
                {"name": "v1alpha1", "served": false, "deprecated": true}]},
         "status": {"storedVersions": ["v1alpha2", "v1"]}},
        {"metadata": {"name": "widgets.example.com"}, "spec": {"group": "example.com",
            "names": {"kind": "Widget"}, "versions": [{"name": "v1", "served": true}]}},
    ]));
    let found = crd_findings(&crds);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].title, "Deprecated custom resource versions served: v1alpha2, v1beta1");
    assert_eq!(found[0].action, "Move manifests and clients to cert-manager.io/v1");
    let note = &found[0].objects[0].note;
    assert!(
        note.starts_with("Certificate · v1alpha2 is going away · objects may still be stored in v1alpha2"),
        "{note}"
    );
}

#[test]
fn the_text_leaves_out_what_an_agent_may_not_see() {
    let report = Report {
        current: 31,
        target: 32,
        findings: vec![
            Finding {
                severity: Severity::Critical,
                area: Area::RemovedApis,
                title: "FlowSchema flowcontrol.apiserver.k8s.io/v1beta3: removed in 1.32".into(),
                action: "Migrate".into(),
                details: vec!["Clients asked".into()],
                objects: vec![Object::new("flowschemas", None, "secret-one", "")],
            },
            Finding {
                severity: Severity::Warning,
                area: Area::Drains,
                title: "A PodDisruptionBudget guards a single pod".into(),
                action: "Run 2".into(),
                details: Vec::new(),
                objects: vec![Object::new("pods", Some("hidden"), "db-0", "")],
            },
            Finding {
                severity: Severity::Warning,
                area: Area::Drains,
                title: "A PodDisruptionBudget allows no disruption".into(),
                action: "Fix".into(),
                details: Vec::new(),
                objects: vec![
                    Object::new("pods", Some("hidden"), "x", ""),
                    Object::new("pods", Some("ok"), "y", "on n1"),
                ],
            },
        ],
        skipped: vec!["CRDs: forbidden (add-on versions not checked)".into()],
    };
    let text = to_text("prod", &report, |o| o.namespace.as_deref() == Some("ok"));
    assert!(text.starts_with("Upgrade check of prod: Kubernetes 1.31 → 1.32\n"), "{text}");
    // Kept for what the metrics say, without its object.
    assert!(text.contains("CRITICAL · Removed APIs · FlowSchema") && !text.contains("secret-one"), "{text}");
    assert!(!text.contains("guards a single pod") && !text.contains("db-0"), "{text}");
    assert!(text.contains("  - pods ok/y: on n1\n  (1 more you may not see)"), "{text}");
    assert!(text.contains("1 finding(s) about objects you may not see are left out."), "{text}");
    assert!(text.contains("Not checked: CRDs: forbidden"), "{text}");
}
