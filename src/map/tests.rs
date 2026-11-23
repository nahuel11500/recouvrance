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
    shown(sections).into_iter().find(|(l, _)| *l == rel.label()).map_or_default(|(_, i)| i)
}

/// The Map of `obj`, from its page's `related` (as `Known` takes it) and the tab's `lists`.
fn map_of(k: &Kind, obj: &Value, related: &Related, lists: &Lists) -> Vec<Section> {
    sections(k, obj, &Known::of(k, related), lists, &[])
}

fn none() -> Lists {
    Lists::default()
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
/// optional ones and the pull Secrets, as before. The volume the API server adds to every pod
/// (`kube-api-access-*`, with `kube-root-ca.crt`) isn't a use, but a pod still needs it.
#[test]
fn a_pod_spec_uses_secrets_and_config_maps_every_way() {
    let spec = json!({
        "serviceAccountName": "runner",
        "imagePullSecrets": [{ "name": "registry" }],
        "volumes": [
            { "name": "certs", "secret": { "secretName": "certs" } },
            { "name": "ca", "projected": { "sources": [{ "configMap": { "name": "ca" } }] } },
            { "name": "data", "persistentVolumeClaim": { "claimName": "data" } },
            { "name": "kube-api-access-x2x9z", "projected": { "sources": [
                { "serviceAccountToken": { "path": "token" } },
                { "configMap": { "name": "kube-root-ca.crt" } },
            ] } },
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
    assert!(!all.iter().any(|t| t == "ConfigMap kube-root-ca.crt"), "{all:?}");
    let pod = json!({ "metadata": { "namespace": "shop" }, "spec": spec });
    let needed: Vec<String> = refs::pod_refs(&pod).iter().map(|t| t.name.clone()).collect();
    assert!(!needed.contains(&"maybe".to_string()) && !needed.contains(&"registry".to_string()));
    assert!(!needed.contains(&"runner".to_string()));
    assert!(needed.contains(&"db".to_string()) && needed.contains(&"certs".to_string()));
    assert!(needed.contains(&"kube-root-ca.crt".to_string()));
}

/// A Secret's Map: its owner (an `ExternalSecret`, said once though it also names the Secret),
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
    let lists = none()
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
    let map = map_of(&kind("", "Secret"), &secret, &related, &lists);
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

/// A ConfigMap's Map: what reads it (from the overview's scan, the same rule as `spec_refs`),
/// and why that scan is not the whole story when part of it failed.
#[test]
fn a_config_map_s_map() {
    let cm = json!({ "metadata": { "namespace": "shop", "name": "settings", "uid": "c1" } });
    let mut related = Related::default();
    related.referenced_by.insert("cronjobs".into(), vec!["report".into()]);
    related.referenced_by.insert("statefulsets".into(), vec!["db".into()]);
    let map = map_of(&kind("", "ConfigMap"), &cm, &related, &none());
    assert_eq!(shown(&map), [("Used by", vec!["CronJob report".to_string(), "StatefulSet db".to_string()])]);
    assert!(wants(&kind("", "ConfigMap"), &cm, &[]).is_empty(), "nothing more to read");
    related.errors.insert("referenced_by", "daemonsets forbidden".into());
    assert_eq!(Known::of(&kind("", "ConfigMap"), &related).errors, ["what uses it: daemonsets forbidden"]);
}

/// A custom resource nothing else is known about shows its owners, and what it owns among
/// what was read; with neither, nothing.
#[test]
fn a_custom_resource_with_only_owners() {
    let widget = kind("example.com", "Widget");
    let obj = json!({ "metadata": { "namespace": "shop", "name": "w", "uid": "w1",
        "ownerReferences": [{ "apiVersion": "example.com/v1", "kind": "Gadget", "name": "g", "uid": "g1" }] } });
    let lists = none().with(
        "",
        "ConfigMap",
        vec![
            json!({ "metadata": { "namespace": "shop", "name": "w-config", "ownerReferences": [{ "uid": "w1" }] } }),
            json!({ "metadata": { "namespace": "shop", "name": "unrelated" } }),
        ],
    );
    let map = map_of(&widget, &obj, &Related::default(), &lists);
    assert_eq!(
        shown(&map),
        [("Owned by", vec!["Gadget g".to_string()]), ("Owns", vec!["ConfigMap w-config".to_string()]),]
    );
    assert_eq!(map[0].items[0].link_kind(), "gadget.example.com");
    let alone = json!({ "metadata": { "namespace": "shop", "name": "w", "uid": "w1" } });
    assert!(map_of(&widget, &alone, &Related::default(), &none()).is_empty());
    // Cluster-scoped: nothing is read.
    let cluster_wide = json!({ "metadata": { "name": "w", "uid": "w1" } });
    assert!(wants(&widget, &cluster_wide, &[]).is_empty());
    // A custom resource has rules (owners, labels, its operator's kinds); a Gateway, a
    // StorageClass or a ClusterRole only owners.
    assert!(!only_owners(&widget));
    for (g, k) in
        [(refs::GATEWAY, "Gateway"), ("storage.k8s.io", "StorageClass"), ("rbac.authorization.k8s.io", "ClusterRole")]
    {
        assert!(only_owners(&kind(g, k)), "{k}");
    }
    for (g, k) in [("", "Secret"), ("apps", "Deployment"), (refs::GATEWAY, "HTTPRoute"), ("", "Node")] {
        assert!(!only_owners(&kind(g, k)), "{k}");
    }
}

/// A mirror pod's owner is its Node: cluster-scoped, linked without the pod's namespace.
#[test]
fn a_cluster_scoped_owner_has_no_namespace() {
    let pod = json!({ "metadata": { "namespace": "kube-system", "name": "etcd-n1", "uid": "p1",
        "ownerReferences": [{ "apiVersion": "v1", "kind": "Node", "name": "n1", "uid": "n1" }] } });
    let map = map_of(&kind("", "Pod"), &pod, &Related { netpols: Some(Vec::new()), ..Default::default() }, &none());
    assert_eq!(map[0].items[0].namespace, None);
    let mut served = kind("example.com", "Fleet");
    served.namespaced = false;
    let w = json!({ "metadata": { "namespace": "shop", "name": "w", "uid": "w1",
        "ownerReferences": [{ "apiVersion": "example.com/v1", "kind": "Fleet", "name": "f", "uid": "f1" }] } });
    let map = sections(&kind("example.com", "Widget"), &w, &Known::default(), &none(), &[served]);
    assert_eq!(map[0].items[0].namespace, None);
}

/// A `CloudNativePG` Cluster: the pods, PVCs, Services and Secrets it owns, a Secret labelled
/// with its name, and the Backups, `ScheduledBackup` and Pooler naming it in `spec.cluster`.
#[test]
fn a_cnpg_cluster_s_map() {
    let cluster = kind(CNPG, "Cluster");
    let obj = json!({ "metadata": { "namespace": "db", "name": "pg", "uid": "u1" }, "spec": { "instances": 2 } });
    let label = json!({ "cnpg.io/cluster": "pg" });
    let named =
        |name: &str, cluster: &str| json!({ "metadata": meta("db", name), "spec": { "cluster": { "name": cluster } } });
    let lists = none()
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
    let map = map_of(&cluster, &obj, &Related::default(), &lists);
    assert_eq!(
        names(&map, Rel::Owns),
        ["PersistentVolumeClaim pg-1", "Pod pg-1", "Pod pg-2", "Secret pg-app", "Service pg-ro", "Service pg-rw",]
    );
    assert_eq!(names(&map, Rel::Labelled), ["Secret pg-ca"]);
    assert_eq!(
        names(&map, Rel::PointedToBy),
        ["Backup pg-20261123", "Pooler pg-pooler-rw", "ScheduledBackup pg-daily"]
    );
    // Labels are the weakest: last.
    assert_eq!(map.last().map(|s| s.rel), Some(Rel::Labelled));
    // What it reads: what operators create (metadata alone) and CNPG's other kinds, served
    // ones only (with their specs when few), in its namespace.
    let served = [cluster.clone(), kind(CNPG, "Backup"), kind(CNPG, "ScheduledBackup"), kind(CNPG, "Pooler")];
    let read = wants(&cluster, &obj, &served);
    assert!(read.iter().any(|w| w.ar.kind == "Pod" && w.read == Read::Meta));
    assert!(read.iter().any(|w| w.ar.kind == "Secret" && w.read == Read::Meta), "a Secret's data is never read");
    for k in ["Backup", "ScheduledBackup", "Pooler"] {
        assert!(read.iter().any(|w| w.ar.kind == k && w.read == Read::SpecsIfFew), "{k}");
    }
    assert!(!read.iter().any(|w| w.ar.kind == "Cluster"));
    // A Backup's own Map points to its Cluster, without reading anything.
    let backup = named("pg-20261123", "pg");
    let map = map_of(&kind(CNPG, "Backup"), &backup, &Related::default(), &none());
    assert_eq!(names(&map, Rel::PointsTo), ["Cluster pg"]);
}

/// A `ScheduledBackup` named like its Cluster is not what the Cluster's Backups, Pooler and
/// Database name: the field says a Cluster.
#[test]
fn a_same_name_of_another_kind_is_not_named() {
    let sb = kind(CNPG, "ScheduledBackup");
    let obj = json!({ "metadata": { "namespace": "db", "name": "pg", "uid": "sb1" }, "spec": { "cluster": { "name": "pg" } } });
    let named = |name: &str| json!({ "metadata": meta("db", name), "spec": { "cluster": { "name": "pg" } } });
    let lists = none().with(CNPG, "Backup", vec![named("pg-1")]).with(CNPG, "Pooler", vec![named("pg-rw")]).with(
        CNPG,
        "Database",
        vec![named("app")],
    );
    let map = map_of(&sb, &obj, &Related::default(), &lists);
    assert!(names(&map, Rel::PointedToBy).is_empty(), "{map:?}");
    assert_eq!(names(&map, Rel::PointsTo), ["Cluster pg"]);
}

/// The generic rule: a `cluster` field or a `…Ref` names a kind ending as the field says; a
/// `kind` in the reference wins.
#[test]
fn an_operator_s_spec_names_an_object() {
    assert!(names_in_spec(&json!({ "cluster": { "name": "pg" } }), "Cluster", "pg"));
    assert!(names_in_spec(&json!({ "clusterName": "pg" }), "KafkaCluster", "pg"));
    assert!(!names_in_spec(&json!({ "clusterName": "pg" }), "Kafka", "pg"));
    assert!(!names_in_spec(&json!({ "cluster": { "name": "pg" } }), "ScheduledBackup", "pg"));
    assert!(names_in_spec(&json!({ "a": [{ "b": { "clusterRef": { "name": "pg" } } }] }), "PostgresCluster", "pg"));
    assert!(names_in_spec(&json!({ "issuerRef": { "name": "ca" } }), "Issuer", "ca"));
    assert!(!names_in_spec(&json!({ "issuerRef": { "name": "ca" } }), "Certificate", "ca"));
    assert!(names_in_spec(&json!({ "storeRef": { "name": "vault" } }), "SecretStore", "vault"));
    assert!(names_in_spec(&json!({ "issuerRef": { "kind": "ClusterIssuer", "name": "ca" } }), "ClusterIssuer", "ca"));
    assert!(!names_in_spec(&json!({ "issuerRef": { "kind": "ClusterIssuer", "name": "ca" } }), "Issuer", "ca"));
    assert!(!names_in_spec(&json!({ "targetRef": { "kind": "Deployment", "name": "pg" } }), "Cluster", "pg"));
    assert!(names_in_spec(&json!({ "targetRef": { "kind": "Cluster", "name": "pg" } }), "Cluster", "pg"));
    assert!(!names_in_spec(&json!({ "secretName": "pg" }), "Cluster", "pg"));
    assert!(!names_in_spec(&json!({ "name": "pg", "image": "pg" }), "Cluster", "pg"));
}

/// Objects labelled with an instance's name: by its operator's label, or the generic
/// `app.kubernetes.io/instance` unless Helm manages them (a release's name); `part-of` isn't.
#[test]
fn objects_labelled_with_an_instance_s_name() {
    let kafka = kind("kafka.strimzi.io", "Kafka");
    let obj = json!({ "metadata": { "namespace": "mq", "name": "main", "uid": "k1" } });
    let labelled =
        |name: &str, labels: Value| json!({ "metadata": { "namespace": "mq", "name": name, "labels": labels } });
    let lists = none().with(
        "",
        "ConfigMap",
        vec![
            labelled("strimzi", json!({ "strimzi.io/cluster": "main" })),
            labelled("operator-made", json!({ "app.kubernetes.io/instance": "main" })),
            labelled(
                "helm-made",
                json!({ "app.kubernetes.io/instance": "main", "app.kubernetes.io/managed-by": "Helm" }),
            ),
            labelled("part", json!({ "app.kubernetes.io/part-of": "main" })),
            labelled("other", json!({ "strimzi.io/cluster": "other" })),
        ],
    );
    let map = map_of(&kafka, &obj, &Related::default(), &lists);
    assert_eq!(names(&map, Rel::Labelled), ["ConfigMap operator-made", "ConfigMap strimzi"]);
    // `cnpg.io/cluster` points at a CNPG Cluster, not at another kind named alike.
    let cm = vec![labelled("x", json!({ "cnpg.io/cluster": "main" }))];
    assert!(map_of(&kafka, &obj, &Related::default(), &none().with("", "ConfigMap", cm)).is_empty());
}

/// An Ingress sends to its Services, each with the pods behind it (counted past the cap), and
/// uses its TLS Secret and class.
#[test]
fn an_ingress_s_map() {
    let ing = json!({ "metadata": { "namespace": "shop", "name": "web", "uid": "i1" }, "spec": {
        "ingressClassName": "nginx",
        "tls": [{ "secretName": "web-tls" }],
        "rules": [{ "http": { "paths": [{ "backend": { "service": { "name": "web", "port": { "number": 80 } } } }] } }],
    } });
    let pods: Vec<Value> = (0..SHOWN_AT_MOST + 3)
        .map(|i| json!({ "metadata": meta("shop", &format!("web-{i:02}")), "status": { "phase": "Running" } }))
        .collect();
    let lists = none().with_backend("web", pods);
    let map = map_of(&kind("networking.k8s.io", "Ingress"), &ing, &Related::default(), &lists);
    assert_eq!(names(&map, Rel::SendsTo), ["Service web"]);
    let web = &map.iter().find(|s| s.rel == Rel::SendsTo).unwrap().items[0];
    assert_eq!((web.under.len(), web.more_under), (SHOWN_AT_MOST, 3));
    assert_eq!((web.under[0].name.as_str(), web.under[0].note.as_deref()), ("web-00", Some("Running")));
    assert_eq!(names(&map, Rel::Uses), ["IngressClass nginx", "Secret web-tls (TLS)"]);
}

/// An `HTTPRoute`'s own Map: its Services (one in another namespace, without pods read), and
/// its parent Gateways.
#[test]
fn an_http_route_s_map() {
    let route = json!({ "metadata": { "namespace": "shop", "name": "web", "uid": "r1" }, "spec": {
        "parentRefs": [{ "name": "public", "namespace": "infra" }, { "name": "inner" }],
        "rules": [{ "backendRefs": [{ "name": "web" }, { "name": "auth", "namespace": "sso" }] }],
    } });
    let lists = none().with_backend("web", vec![json!({ "metadata": meta("shop", "web-1") })]);
    let map = map_of(&kind(refs::GATEWAY, "HTTPRoute"), &route, &Related::default(), &lists);
    let sends = &map.iter().find(|s| s.rel == Rel::SendsTo).unwrap().items;
    let found: Vec<(Option<&str>, &str, usize)> =
        sends.iter().map(|i| (i.namespace.as_deref(), i.name.as_str(), i.under.len())).collect();
    assert_eq!(found, [(Some("sso"), "auth", 0), (Some("shop"), "web", 1)]);
    let gateways: Vec<(Option<&str>, &str)> = map
        .iter()
        .find(|s| s.rel == Rel::Uses)
        .unwrap()
        .items
        .iter()
        .map(|i| (i.namespace.as_deref(), i.name.as_str()))
        .collect();
    assert_eq!(gateways, [(Some("shop"), "inner"), (Some("infra"), "public")]);
}

/// A Deployment's Map adds to its tree the PDB and NetworkPolicy selecting its pods, its HPA,
/// and the Ingress sending to its Service; not those selecting other pods.
#[test]
fn a_deployment_s_map_beside_its_tree() {
    let deploy = json!({ "metadata": { "namespace": "shop", "name": "web", "uid": "d1" }, "spec": {
        "selector": { "matchLabels": { "app": "web" } },
        "template": { "metadata": { "labels": { "app": "web" } }, "spec": { "containers": [] } },
    } });
    let related = Related {
        services: vec![
            json!({ "metadata": meta("shop", "web"), "spec": { "selector": { "app": "web" } } }),
            json!({ "metadata": meta("shop", "db"), "spec": { "selector": { "app": "db" } } }),
        ],
        ..Default::default()
    };
    let ingress = |name: &str, svc: &str| json!({ "metadata": meta("shop", name), "spec": { "defaultBackend": { "service": { "name": svc } } } });
    let lists = none()
        .with("policy", "PodDisruptionBudget", vec![
            json!({ "metadata": meta("shop", "web"), "spec": { "selector": { "matchLabels": { "app": "web" } } } }),
            json!({ "metadata": meta("shop", "db"), "spec": { "selector": { "matchLabels": { "app": "db" } } } }),
        ])
        .with("networking.k8s.io", "NetworkPolicy", vec![
            json!({ "metadata": meta("shop", "all"), "spec": { "podSelector": {} } }),
        ])
        .with("autoscaling", "HorizontalPodAutoscaler", vec![
            json!({ "metadata": meta("shop", "web"), "spec": { "scaleTargetRef": { "kind": "Deployment", "name": "web" } } }),
            json!({ "metadata": meta("shop", "api"), "spec": { "scaleTargetRef": { "kind": "Deployment", "name": "api" } } }),
        ])
        .with("networking.k8s.io", "Ingress", vec![ingress("public", "web"), ingress("db", "db")]);
    let map = map_of(&kind("apps", "Deployment"), &deploy, &related, &lists);
    assert_eq!(names(&map, Rel::SelectedBy), ["NetworkPolicy all", "PodDisruptionBudget web"]);
    assert_eq!(names(&map, Rel::ScaledBy), ["HorizontalPodAutoscaler web"]);
    assert_eq!(names(&map, Rel::ReceivesFrom), ["Ingress public (to Service web)"]);
    // The tree shows what its pods use: not repeated here.
    assert!(names(&map, Rel::Uses).is_empty());
}

/// A pod's NetworkPolicies are those its page read; when they could not be read, the Map
/// says so rather than showing none.
#[test]
fn a_pod_s_network_policies() {
    let pod = json!({ "metadata": { "namespace": "shop", "name": "web-1", "uid": "p1", "labels": { "app": "web" } } });
    let netpol = |name: &str, app: &str| json!({ "metadata": meta("shop", name), "spec": { "podSelector": { "matchLabels": { "app": app } } } });
    let related = Related { netpols: Some(vec![netpol("web-in", "web"), netpol("db-in", "db")]), ..Default::default() };
    let map = map_of(&kind("", "Pod"), &pod, &related, &none());
    assert_eq!(names(&map, Rel::SelectedBy), ["NetworkPolicy web-in"]);
    let unread = Known::of(&kind("", "Pod"), &Related::default());
    assert_eq!(unread.errors, ["NetworkPolicies: could not be read"]);
}

/// A node's pods are capped, the rest counted; the page held them already (field selector).
#[test]
fn long_relations_are_capped() {
    let node = json!({ "metadata": { "name": "n1", "uid": "n1" } });
    let related = Related {
        pods: (0..SHOWN_AT_MOST + 7).map(|i| json!({ "metadata": meta("shop", &format!("p{i:03}")) })).collect(),
        ..Default::default()
    };
    let map = map_of(&kind("", "Node"), &node, &related, &none());
    assert_eq!(map.len(), 1);
    assert_eq!((map[0].rel, map[0].items.len(), map[0].more), (Rel::Runs, SHOWN_AT_MOST, 7));
    assert!(wants(&kind("", "Node"), &node, &[]).is_empty(), "a cluster-scoped object reads nothing more");
}

/// A Service's Map: the pods it selects (held by the page), and the Ingresses and `HTTPRoutes`
/// sending to it.
#[test]
fn a_service_s_map() {
    let svc = json!({ "metadata": { "namespace": "shop", "name": "web", "uid": "s1" }, "spec": { "selector": { "app": "web" } } });
    let related = Related {
        pods: vec![json!({ "metadata": meta("shop", "web-1"), "status": { "phase": "Running" } })],
        ..Default::default()
    };
    let route =
        json!({ "metadata": meta("shop", "web"), "spec": { "rules": [{ "backendRefs": [{ "name": "web" }] }] } });
    let lists = none().with(refs::GATEWAY, "HTTPRoute", vec![route]);
    let map = map_of(&kind("", "Service"), &svc, &related, &lists);
    assert_eq!(names(&map, Rel::Selects), ["Pod web-1 (Running)"]);
    assert_eq!(names(&map, Rel::ReceivesFrom), ["HTTPRoute web (to Service web)"]);
}

/// A ServiceAccount's Map: its Secrets, the pods running as it (held by the page) and the
/// workloads whose template names it.
#[test]
fn a_service_account_s_map() {
    let sa = json!({ "metadata": { "namespace": "shop", "name": "runner", "uid": "a1" },
        "secrets": [{ "name": "runner-token" }], "imagePullSecrets": [{ "name": "registry" }] });
    let related = Related { pods: vec![json!({ "metadata": meta("shop", "web-1") })], ..Default::default() };
    let workload = |name: &str, account: &str| json!({ "metadata": meta("shop", name), "spec": { "template": { "spec": { "serviceAccountName": account } } } });
    let cron = json!({ "metadata": meta("shop", "report"),
        "spec": { "jobTemplate": { "spec": { "template": { "spec": { "serviceAccount": "runner" } } } } } });
    let lists = none().with("apps", "Deployment", vec![workload("web", "runner"), workload("api", "default")]).with(
        "batch",
        "CronJob",
        vec![cron],
    );
    let map = map_of(&kind("", "ServiceAccount"), &sa, &related, &lists);
    assert_eq!(names(&map, Rel::Uses), ["Secret registry", "Secret runner-token"]);
    assert_eq!(names(&map, Rel::UsedBy), ["CronJob report", "Deployment web", "Pod web-1"]);
}

/// A PVC uses its volume and class (cluster-scoped) and is used by its pods; a PV is used by
/// its claim, in the claim's namespace.
#[test]
fn a_claim_and_its_volume() {
    let pvc = json!({ "metadata": { "namespace": "db", "name": "data", "uid": "c1" },
        "spec": { "volumeName": "pv-1", "storageClassName": "fast" } });
    let related = Related { pods: vec![json!({ "metadata": meta("db", "pg-1") })], ..Default::default() };
    let map = map_of(&kind("", "PersistentVolumeClaim"), &pvc, &related, &none());
    let uses: Vec<(&str, Option<&str>)> =
        map[0].items.iter().map(|i| (i.kind.as_str(), i.namespace.as_deref())).collect();
    assert_eq!(map[0].rel, Rel::Uses);
    assert_eq!(uses, [("PersistentVolume", None), ("StorageClass", None)]);
    assert_eq!(names(&map, Rel::UsedBy), ["Pod pg-1"]);
    let pv = json!({ "metadata": { "name": "pv-1", "uid": "v1" },
        "spec": { "claimRef": { "namespace": "db", "name": "data" }, "storageClassName": "fast" } });
    let map = map_of(&kind("", "PersistentVolume"), &pv, &Related::default(), &none());
    let claim = &map.iter().find(|s| s.rel == Rel::UsedBy).unwrap().items[0];
    assert_eq!(
        (claim.kind.as_str(), claim.namespace.as_deref(), claim.name.as_str()),
        ("PersistentVolumeClaim", Some("db"), "data")
    );
    assert_eq!(names(&map, Rel::Uses), ["StorageClass fast"]);
}

/// A binding uses its role (a ClusterRole without a namespace) and binds its ServiceAccounts,
/// each in its own namespace; users and groups aren't objects.
#[test]
fn a_binding_s_map() {
    let rb = json!({ "metadata": { "namespace": "shop", "name": "edit", "uid": "b1" },
        "roleRef": { "kind": "Role", "name": "editor" },
        "subjects": [{ "kind": "ServiceAccount", "name": "ci" }, { "kind": "ServiceAccount", "name": "argo", "namespace": "argocd" },
            { "kind": "User", "name": "alice" }] });
    let map = map_of(&kind("rbac.authorization.k8s.io", "RoleBinding"), &rb, &Related::default(), &none());
    let role = &map.iter().find(|s| s.rel == Rel::Uses).unwrap().items[0];
    assert_eq!((role.kind.as_str(), role.namespace.as_deref()), ("Role", Some("shop")));
    let bound: Vec<(Option<&str>, &str)> = map
        .iter()
        .find(|s| s.rel == Rel::Binds)
        .unwrap()
        .items
        .iter()
        .map(|i| (i.namespace.as_deref(), i.name.as_str()))
        .collect();
    assert_eq!(bound, [(Some("argocd"), "argo"), (Some("shop"), "ci")]);
    let crb = json!({ "metadata": { "name": "admins", "uid": "b2" }, "roleRef": { "kind": "ClusterRole", "name": "admin" },
        "subjects": [{ "kind": "ServiceAccount", "name": "ops", "namespace": "ops" }] });
    let map = map_of(&kind("rbac.authorization.k8s.io", "ClusterRoleBinding"), &crb, &Related::default(), &none());
    let role = &map.iter().find(|s| s.rel == Rel::Uses).unwrap().items[0];
    assert_eq!((role.kind.as_str(), role.namespace.as_deref()), ("ClusterRole", None));
    assert_eq!(names(&map, Rel::Binds), ["ServiceAccount ops"]);
}

/// A CronJob's Map: what its pods use, and the Jobs it owns (held by the page).
#[test]
fn a_cron_job_s_map() {
    let cron = json!({ "metadata": { "namespace": "shop", "name": "report", "uid": "c1" },
        "spec": { "jobTemplate": { "spec": { "template": { "spec": {
            "containers": [{ "envFrom": [{ "secretRef": { "name": "db" } }] }] } } } } } });
    let related = Related { extra: vec![json!({ "metadata": meta("shop", "report-123") })], ..Default::default() };
    let map = map_of(&kind("batch", "CronJob"), &cron, &related, &none());
    assert_eq!(names(&map, Rel::Owns), ["Job report-123"]);
    assert_eq!(names(&map, Rel::Uses), ["Secret db"]);
}

/// An HPA scales its target; a VPA too, from its `targetRef`.
#[test]
fn autoscalers_scale_their_target() {
    let hpa = json!({ "metadata": { "namespace": "shop", "name": "web", "uid": "h1" },
        "spec": { "scaleTargetRef": { "apiVersion": "apps/v1", "kind": "Deployment", "name": "web" } } });
    let map = map_of(&kind("autoscaling", "HorizontalPodAutoscaler"), &hpa, &Related::default(), &none());
    assert_eq!(names(&map, Rel::Scales), ["Deployment web"]);
    assert_eq!(map[0].items[0].link_kind(), "deployment.apps");
    let vpa = json!({ "metadata": { "namespace": "shop", "name": "db", "uid": "v1" },
        "spec": { "targetRef": { "apiVersion": "apps/v1", "kind": "StatefulSet", "name": "db" } } });
    let map = map_of(&kind("autoscaling.k8s.io", "VerticalPodAutoscaler"), &vpa, &Related::default(), &none());
    assert_eq!(names(&map, Rel::Scales), ["StatefulSet db"]);
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
    let lists = none().with("", "ServiceAccount", vec![kept]);
    let secret = json!({ "metadata": { "namespace": "shop", "name": "registry" } });
    assert_eq!(
        names(&map_of(&kind("", "Secret"), &secret, &Related::default(), &lists), Rel::UsedBy),
        ["ServiceAccount puller"]
    );
}

/// An API server that refuses Ingresses and doesn't serve HTTPRoutes, for `load`.
#[derive(Clone)]
struct Refusing;

impl tower::Service<http::Request<kube::client::Body>> for Refusing {
    type Response = http::Response<http_body_util::Full<bytes::Bytes>>;
    type Error = std::convert::Infallible;
    type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<kube::client::Body>) -> Self::Future {
        let code = if req.uri().path().ends_with("/ingresses") { 403 } else { 404 };
        let reason = if code == 403 { "Forbidden" } else { "NotFound" };
        let body = json!({ "kind": "Status", "apiVersion": "v1", "status": "Failure", "code": code, "reason": reason });
        let response = http::Response::builder().status(code).body(body.to_string().into_bytes().into()).unwrap();
        std::future::ready(Ok(response))
    }
}

/// A list refused is said; a kind not served (404) is skipped without a word.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_list_is_said_a_missing_kind_skipped() {
    let client = Client::new(Refusing, "default");
    let svc = json!({ "metadata": { "namespace": "shop", "name": "web", "uid": "s1" } });
    let lists = load(client, kind("", "Service"), svc, vec![kind(refs::GATEWAY, "HTTPRoute")]).await;
    assert_eq!(lists.errors, ["ingresses: forbidden"]);
    assert!(lists.read.is_empty(), "{:?}", lists.read);
}
