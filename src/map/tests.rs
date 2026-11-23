use serde_json::{Value, json};

use super::*;

fn kind(group: &str, k: &str) -> Kind {
    Kind::fake(group, k)
}

/// The relations as (label, ["Kind name (note)"]), for short assertions.
fn shown(sections: &[Section]) -> Vec<(&'static str, Vec<String>)> {
    sections
        .iter()
        .map(|s| {
            let items = s
                .items
                .iter()
                .map(|i| match &i.note {
                    Some(n) => format!("{} {} ({n})", i.kind, i.name),
                    None => format!("{} {}", i.kind, i.name),
                })
                .collect();
            (s.rel.label(), items)
        })
        .collect()
}

fn names(sections: &[Section], rel: Rel) -> Vec<String> {
    shown(sections).into_iter().find(|(l, _)| *l == rel.label()).map(|(_, i)| i).unwrap_or_default()
}

fn meta(ns: &str, name: &str) -> Value {
    json!({ "namespace": ns, "name": name, "uid": format!("uid-{name}") })
}

fn owned_by(ns: &str, name: &str, owner_uid: &str, labels: Value) -> Value {
    json!({ "metadata": { "namespace": ns, "name": name, "labels": labels,
        "ownerReferences": [{ "apiVersion": "postgresql.cnpg.io/v1", "kind": "Cluster", "name": "pg", "uid": owner_uid }] } })
}

/// Every way a pod spec names a Secret is a use: env, envFrom, a volume (projected too) and
/// an image pull Secret; `optional` ones too. What a pod needs to start leaves out the
/// optional ones and the pull Secrets, as before.
#[test]
fn a_pod_spec_uses_secrets_and_config_maps_every_way() {
    let spec = json!({
        "serviceAccountName": "runner",
        "imagePullSecrets": [{ "name": "registry" }],
        "volumes": [
            { "secret": { "secretName": "certs" } },
            { "projected": { "sources": [{ "configMap": { "name": "ca" } }] } },
            { "persistentVolumeClaim": { "claimName": "data" } },
        ],
        "initContainers": [{ "envFrom": [{ "secretRef": { "name": "maybe", "optional": true } }] }],
        "containers": [{
            "env": [
                { "valueFrom": { "secretKeyRef": { "name": "db", "key": "password" } } },
                { "valueFrom": { "configMapKeyRef": { "name": "settings", "key": "mode" } } },
            ],
            "envFrom": [{ "configMapRef": { "name": "env" } }],
        }],
    });
    let all: Vec<String> =
        refs::spec_refs(Some("shop"), &spec, true).iter().map(|t| format!("{} {}", t.of.kind(), t.name)).collect();
    for want in [
        "Secret registry",
        "Secret certs",
        "Secret maybe",
        "Secret db",
        "ConfigMap ca",
        "ConfigMap settings",
        "ConfigMap env",
        "PersistentVolumeClaim data",
        "ServiceAccount runner",
    ] {
        assert!(all.iter().any(|t| t == want), "{want} missing from {all:?}");
    }
    let pod = json!({ "metadata": { "namespace": "shop" }, "spec": spec });
    let needed: Vec<String> = refs::pod_refs(&pod).iter().map(|t| t.name.clone()).collect();
    assert!(!needed.contains(&"maybe".to_string()) && !needed.contains(&"registry".to_string()));
    assert!(!needed.contains(&"runner".to_string()));
    assert!(needed.contains(&"db".to_string()) && needed.contains(&"certs".to_string()));
}

/// A Secret's Map: its owner (an ExternalSecret, said once though it also names the Secret),
/// the workloads and pods that read it, the ServiceAccount and the Ingress (TLS) that name it,
/// and the Certificate that issued it.
#[test]
fn a_secret_s_map() {
    let secret = json!({ "metadata": {
        "namespace": "shop", "name": "web-tls", "uid": "s1",
        "annotations": { "cert-manager.io/certificate-name": "web" },
        "ownerReferences": [{ "apiVersion": "external-secrets.io/v1beta1", "kind": "ExternalSecret", "name": "web-tls", "uid": "e1" }],
    } });
    let mut related = Related::default();
    related.referenced_by.insert("deployments".into(), vec!["web".into()]);
    related.referenced_by.insert("pods".into(), vec!["web-1".into()]);
    let lists = Lists::default()
        .with(
            "",
            "ServiceAccount",
            vec![
                json!({ "metadata": meta("shop", "puller"), "imagePullSecrets": [{ "name": "web-tls" }] }),
                json!({ "metadata": meta("shop", "other"), "secrets": [{ "name": "else" }] }),
            ],
        )
        .with(
            "networking.k8s.io",
            "Ingress",
            vec![
                json!({ "metadata": meta("shop", "web"), "spec": { "tls": [{ "secretName": "web-tls" }] } }),
                json!({ "metadata": meta("shop", "plain"), "spec": { "rules": [] } }),
            ],
        )
        .with(ES, "ExternalSecret", vec![json!({ "metadata": meta("shop", "web-tls"), "spec": {} })]);
    let map = sections(&kind("", "Secret"), &secret, &related, Some(&lists));
    assert_eq!(names(&map, Rel::OwnedBy), ["ExternalSecret web-tls"]);
    assert_eq!(names(&map, Rel::UsedBy), ["Deployment web", "Ingress web (TLS)", "Pod web-1", "ServiceAccount puller"]);
    assert_eq!(names(&map, Rel::PointedToBy), ["Certificate web (issued it)"]);
    // Every object once.
    let all: Vec<&Item> = map.iter().flat_map(|s| &s.items).collect();
    assert_eq!(all.iter().filter(|i| i.kind == "ExternalSecret").count(), 1);
    // It reads in its namespace only: its ServiceAccounts and Ingresses, and the operator's
    // kinds that name Secrets when the cluster serves them.
    let served = [kind(ES, "ExternalSecret"), kind("cert-manager.io", "Certificate")];
    let read: Vec<String> = wants(&kind("", "Secret"), &secret, &served).iter().map(|w| w.ar.kind.clone()).collect();
    assert_eq!(read, ["ServiceAccount", "Ingress", "Certificate", "ExternalSecret"]);
}

/// A ConfigMap's Map: what reads it (from the overview's scan, the same rule as `spec_refs`).
#[test]
fn a_config_map_s_map() {
    let cm = json!({ "metadata": { "namespace": "shop", "name": "settings", "uid": "c1" } });
    let mut related = Related::default();
    related.referenced_by.insert("cronjobs".into(), vec!["report".into()]);
    related.referenced_by.insert("statefulsets".into(), vec!["db".into()]);
    let map = sections(&kind("", "ConfigMap"), &cm, &related, None);
    assert_eq!(shown(&map), [("Used by", vec!["CronJob report".to_string(), "StatefulSet db".to_string()])]);
    assert!(wants(&kind("", "ConfigMap"), &cm, &[]).is_empty(), "nothing more to read");
}

/// A custom resource nothing else is known about shows its owners, and what it owns among
/// what was read; with neither, nothing (the tab says so).
#[test]
fn a_custom_resource_with_only_owners() {
    let widget = kind("example.com", "Widget");
    let obj = json!({ "metadata": { "namespace": "shop", "name": "w", "uid": "w1",
        "ownerReferences": [{ "apiVersion": "example.com/v1", "kind": "Gadget", "name": "g", "uid": "g1" }] } });
    let lists = Lists::default().with(
        "",
        "ConfigMap",
        vec![
            json!({ "metadata": { "namespace": "shop", "name": "w-config", "ownerReferences": [{ "uid": "w1" }] } }),
            json!({ "metadata": { "namespace": "shop", "name": "unrelated" } }),
        ],
    );
    let map = sections(&widget, &obj, &Related::default(), Some(&lists));
    assert_eq!(
        shown(&map),
        [("Owned by", vec!["Gadget g".to_string()]), ("Owns", vec!["ConfigMap w-config".to_string()]),]
    );
    assert_eq!(map[0].items[0].link_kind(), "gadget.example.com");
    let alone = json!({ "metadata": { "namespace": "shop", "name": "w", "uid": "w1" } });
    assert!(sections(&widget, &alone, &Related::default(), Some(&Lists::default())).is_empty());
    // Cluster-scoped: nothing is read.
    let cluster_wide = json!({ "metadata": { "name": "w", "uid": "w1" } });
    assert!(wants(&widget, &cluster_wide, &[]).is_empty());
}

/// A CloudNativePG Cluster: the pods, PVCs, Services and Secrets it owns, a Secret labelled
/// with its name, and the Backups, ScheduledBackup and Pooler naming it in `spec.cluster`.
#[test]
fn a_cnpg_cluster_s_map() {
    let cluster = kind(CNPG, "Cluster");
    let obj = json!({ "metadata": { "namespace": "db", "name": "pg", "uid": "u1" }, "spec": { "instances": 2 } });
    let label = json!({ "cnpg.io/cluster": "pg" });
    let named =
        |name: &str, cluster: &str| json!({ "metadata": meta("db", name), "spec": { "cluster": { "name": cluster } } });
    let lists = Lists::default()
        .with(
            "",
            "Pod",
            vec![
                owned_by("db", "pg-1", "u1", label.clone()),
                owned_by("db", "pg-2", "u1", label.clone()),
                owned_by("db", "other-1", "u2", json!({ "cnpg.io/cluster": "other" })),
            ],
        )
        .with("", "PersistentVolumeClaim", vec![owned_by("db", "pg-1", "u1", label.clone())])
        .with(
            "",
            "Service",
            vec![owned_by("db", "pg-rw", "u1", label.clone()), owned_by("db", "pg-ro", "u1", label.clone())],
        )
        .with(
            "",
            "Secret",
            vec![
                owned_by("db", "pg-app", "u1", label.clone()),
                json!({ "metadata": { "namespace": "db", "name": "pg-ca", "labels": label } }),
            ],
        )
        .with(CNPG, "Backup", vec![named("pg-20261123", "pg"), named("other-20261123", "other")])
        .with(CNPG, "ScheduledBackup", vec![named("pg-daily", "pg")])
        .with(CNPG, "Pooler", vec![named("pg-pooler-rw", "pg")]);
    let map = sections(&cluster, &obj, &Related::default(), Some(&lists));
    assert_eq!(
        names(&map, Rel::Owns),
        ["PersistentVolumeClaim pg-1", "Pod pg-1", "Pod pg-2", "Secret pg-app", "Service pg-ro", "Service pg-rw",]
    );
    assert_eq!(names(&map, Rel::Labelled), ["Secret pg-ca"]);
    assert_eq!(
        names(&map, Rel::PointedToBy),
        ["Backup pg-20261123", "Pooler pg-pooler-rw", "ScheduledBackup pg-daily"]
    );
    // What it reads: what operators create (metadata alone) and CNPG's other kinds, served
    // ones only, in its namespace.
    let served = [cluster.clone(), kind(CNPG, "Backup"), kind(CNPG, "ScheduledBackup"), kind(CNPG, "Pooler")];
    let read = wants(&cluster, &obj, &served);
    assert!(read.iter().any(|w| w.ar.kind == "Pod" && w.meta));
    assert!(read.iter().any(|w| w.ar.kind == "Secret" && w.meta), "a Secret's data is never read");
    for k in ["Backup", "ScheduledBackup", "Pooler"] {
        assert!(read.iter().any(|w| w.ar.kind == k && !w.meta), "{k}");
    }
    assert!(!read.iter().any(|w| w.ar.kind == "Cluster"));
    // A Backup's own Map points to its Cluster, without reading anything.
    let backup = named("pg-20261123", "pg");
    let map = sections(&kind(CNPG, "Backup"), &backup, &Related::default(), None);
    assert_eq!(names(&map, Rel::PointsTo), ["Cluster pg"]);
}

/// The generic rule: a `cluster` / `…Ref` object or a `cluster` name, at any depth; a
/// `secretName` only names a Secret; a reference saying another kind doesn't count.
#[test]
fn an_operator_s_spec_names_an_object() {
    assert!(names_in_spec(&json!({ "cluster": { "name": "pg" } }), "Cluster", "pg"));
    assert!(names_in_spec(&json!({ "clusterName": "pg" }), "Kafka", "pg"));
    assert!(names_in_spec(&json!({ "a": [{ "b": { "clusterRef": { "name": "pg" } } }] }), "Cluster", "pg"));
    assert!(!names_in_spec(&json!({ "targetRef": { "kind": "Deployment", "name": "pg" } }), "Cluster", "pg"));
    assert!(names_in_spec(&json!({ "targetRef": { "kind": "Cluster", "name": "pg" } }), "Cluster", "pg"));
    assert!(names_in_spec(&json!({ "secretName": "pg" }), "Secret", "pg"));
    assert!(!names_in_spec(&json!({ "secretName": "pg" }), "Cluster", "pg"));
    assert!(!names_in_spec(&json!({ "secretRef": { "name": "pg" } }), "Cluster", "pg"));
    assert!(!names_in_spec(&json!({ "name": "pg", "image": "pg" }), "Cluster", "pg"));
}

/// An Ingress sends to its Services, each with the pods behind it, and uses its TLS Secret.
#[test]
fn an_ingress_s_map() {
    let ing = json!({ "metadata": { "namespace": "shop", "name": "web", "uid": "i1" }, "spec": {
        "ingressClassName": "nginx",
        "tls": [{ "secretName": "web-tls" }],
        "rules": [{ "http": { "paths": [{ "backend": { "service": { "name": "web", "port": { "number": 80 } } } }] } }],
    } });
    let pod = json!({ "metadata": meta("shop", "web-1"), "status": { "phase": "Running" } });
    let lists = Lists::default().with_backend("web", vec![pod]);
    let map = sections(&kind("networking.k8s.io", "Ingress"), &ing, &Related::default(), Some(&lists));
    assert_eq!(names(&map, Rel::SendsTo), ["Service web"]);
    let web = &map.iter().find(|s| s.rel == Rel::SendsTo).unwrap().items[0];
    assert_eq!(
        web.under.iter().map(|p| (p.name.as_str(), p.note.as_deref())).collect::<Vec<_>>(),
        [("web-1", Some("Running"))]
    );
    assert_eq!(names(&map, Rel::Uses), ["IngressClass nginx", "Secret web-tls (TLS)"]);
}

/// A Deployment's Map adds to its tree the PDB and NetworkPolicy selecting its pods, its HPA,
/// and the Ingress sending to its Service; not those selecting other pods.
#[test]
fn a_deployment_s_map_beside_its_tree() {
    let deploy = json!({ "metadata": { "namespace": "shop", "name": "web", "uid": "d1" }, "spec": {
        "selector": { "matchLabels": { "app": "web" } },
        "template": { "metadata": { "labels": { "app": "web" } }, "spec": { "containers": [] } },
    } });
    let mut related = Related::default();
    related.services = vec![
        json!({ "metadata": meta("shop", "web"), "spec": { "selector": { "app": "web" } } }),
        json!({ "metadata": meta("shop", "db"), "spec": { "selector": { "app": "db" } } }),
    ];
    let ingress = |name: &str, svc: &str| json!({ "metadata": meta("shop", name), "spec": { "defaultBackend": { "service": { "name": svc } } } });
    let lists = Lists::default()
        .with("policy", "PodDisruptionBudget", vec![
            json!({ "metadata": meta("shop", "web"), "spec": { "selector": { "matchLabels": { "app": "web" } } } }),
            json!({ "metadata": meta("shop", "db"), "spec": { "selector": { "matchLabels": { "app": "db" } } } }),
        ])
        .with("networking.k8s.io", "NetworkPolicy", vec![json!({ "metadata": meta("shop", "all"), "spec": { "podSelector": {} } })])
        .with("autoscaling", "HorizontalPodAutoscaler", vec![
            json!({ "metadata": meta("shop", "web"), "spec": { "scaleTargetRef": { "kind": "Deployment", "name": "web" } } }),
            json!({ "metadata": meta("shop", "api"), "spec": { "scaleTargetRef": { "kind": "Deployment", "name": "api" } } }),
        ])
        .with("networking.k8s.io", "Ingress", vec![ingress("public", "web"), ingress("db", "db")]);
    let map = sections(&kind("apps", "Deployment"), &deploy, &related, Some(&lists));
    assert_eq!(names(&map, Rel::SelectedBy), ["NetworkPolicy all", "PodDisruptionBudget web"]);
    assert_eq!(names(&map, Rel::ScaledBy), ["HorizontalPodAutoscaler web"]);
    assert_eq!(names(&map, Rel::ReceivesFrom), ["Ingress public (to Service web)"]);
    // The tree shows what its pods use: not repeated here.
    assert!(names(&map, Rel::Uses).is_empty());
}

/// A node's pods are capped, the rest counted; the page held them already (field selector).
#[test]
fn long_relations_are_capped() {
    let node = json!({ "metadata": { "name": "n1", "uid": "n1" } });
    let mut related = Related::default();
    related.pods = (0..SHOWN_AT_MOST + 7).map(|i| json!({ "metadata": meta("shop", &format!("p{i:03}")) })).collect();
    let map = sections(&kind("", "Node"), &node, &related, None);
    assert_eq!(map.len(), 1);
    assert_eq!((map[0].rel, map[0].items.len(), map[0].more), (Rel::Runs, SHOWN_AT_MOST, 7));
    assert!(wants(&kind("", "Node"), &node, &[]).is_empty(), "a cluster-scoped object reads nothing more");
}

/// A Service's Map: the pods it selects (held by the page), and the Ingresses and HTTPRoutes
/// sending to it.
#[test]
fn a_service_s_map() {
    let svc = json!({ "metadata": { "namespace": "shop", "name": "web", "uid": "s1" }, "spec": { "selector": { "app": "web" } } });
    let mut related = Related::default();
    related.pods = vec![json!({ "metadata": meta("shop", "web-1"), "status": { "phase": "Running" } })];
    let route =
        json!({ "metadata": meta("shop", "web"), "spec": { "rules": [{ "backendRefs": [{ "name": "web" }] }] } });
    let lists = Lists::default().with(refs::GATEWAY, "HTTPRoute", vec![route]);
    let map = sections(&kind("", "Service"), &svc, &related, Some(&lists));
    assert_eq!(names(&map, Rel::Selects), ["Pod web-1 (Running)"]);
    assert_eq!(names(&map, Rel::ReceivesFrom), ["HTTPRoute web (to Service web)"]);
}

/// What a list keeps of an object: what the rules read (names, labels, owners, spec, a
/// ServiceAccount's Secrets), not its status or managedFields.
#[test]
fn a_listed_object_keeps_what_the_rules_read() {
    let sa = json!({
        "metadata": { "namespace": "shop", "name": "puller", "uid": "a1", "labels": null,
            "ownerReferences": [{ "uid": "o1" }], "managedFields": [{ "manager": "kubectl" }] },
        "imagePullSecrets": [{ "name": "registry" }],
        "secrets": [{ "name": "token" }],
        "status": { "anything": true },
    });
    let kept = serde_json::to_value(serde_json::from_value::<Lean>(sa).unwrap()).unwrap();
    assert_eq!(kept.pointer("/imagePullSecrets/0/name"), Some(&json!("registry")));
    assert_eq!(kept.pointer("/secrets/0/name"), Some(&json!("token")));
    assert_eq!(kept.pointer("/metadata/ownerReferences/0/uid"), Some(&json!("o1")));
    assert!(kept.pointer("/metadata/managedFields").is_none() && kept.get("status").is_none());
    let lists = Lists::default().with("", "ServiceAccount", vec![kept]);
    assert_eq!(
        names(
            &sections(
                &kind("", "Secret"),
                &json!({ "metadata": { "namespace": "shop", "name": "registry" } }),
                &Related::default(),
                Some(&lists)
            ),
            Rel::UsedBy
        ),
        ["ServiceAccount puller"]
    );
}
