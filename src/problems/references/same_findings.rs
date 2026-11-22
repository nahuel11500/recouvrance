//! The traffic checks as they read whole objects before #842, against the lean reads that
//! replaced them: for every object, the same problems with the same reasons. A check changed
//! on purpose changes in `whole` too, or drops its property here.

use proptest::prelude::*;
use serde_json::{Value, json};

use super::*;

/// The checks as they were, on whole objects (`serde_json::Value`).
mod whole {
    use super::*;

    fn items(v: &Value) -> impl Iterator<Item = &Value> {
        v.as_array().into_iter().flatten()
    }

    fn created(obj: &Value) -> Option<jiff::Timestamp> {
        text(obj, "/metadata/creationTimestamp")?.parse().ok()
    }

    /// Not being deleted, and older than `SETTLE` (an unknown age counts as old).
    pub fn settled(obj: &Value) -> bool {
        obj.pointer("/metadata/deletionTimestamp").is_none_or(Value::is_null) && since(created(obj), SETTLE)
    }

    /// The pods that may serve: not those that ran to completion.
    fn live(pods: &[Value]) -> Vec<&Value> {
        pods.iter().filter(|p| !matches!(text(p, "/status/phase"), Some("Succeeded" | "Failed"))).collect()
    }

    fn pod_ready(pod: &Value) -> bool {
        let deleting = pod.pointer("/metadata/deletionTimestamp").is_some_and(|t| !t.is_null());
        !deleting && k8s::pod_ready(pod)
    }

    pub fn judge_service(selected: &[Value], scaled: Option<&str>) -> Option<(Severity, String)> {
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

    /// `workloads`: (kind, object), in any namespace.
    pub fn scaled_to_zero(service: &Value, workloads: &[(&str, Value)]) -> Option<String> {
        let ns = text(service, "/metadata/namespace");
        let selector = service.pointer("/spec/selector")?;
        let selector = json!({ "matchLabels": selector });
        workloads.iter().find_map(|(kind, w)| {
            let labels = w.pointer("/spec/template/metadata/labels")?.as_object()?;
            let zero = w.pointer("/spec/replicas").and_then(Value::as_i64) == Some(0);
            (zero && text(w, "/metadata/namespace") == ns && k8s::selector_matches(&selector, labels))
                .then(|| format!("{kind} {}", text(w, "/metadata/name").unwrap_or_default()))
        })
    }

    fn ingress_secrets(ing: &Value) -> Vec<Target> {
        let ns = text(ing, "/metadata/namespace").map(String::from);
        let names: BTreeSet<&str> =
            items(&ing["spec"]["tls"]).filter_map(|t| t["secretName"].as_str().filter(|n| !n.is_empty())).collect();
        names.into_iter().map(|name| Target { of: Of::Secret, namespace: ns.clone(), name: name.to_string() }).collect()
    }

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

    pub fn judge_ingress(
        ing: &Value,
        services: &Listed<&Value>,
        secret: impl Fn(&Target) -> Option<bool>,
    ) -> Vec<String> {
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

    pub fn judge_route(route: &Value, services: &Listed<&Value>) -> Vec<String> {
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

    pub fn key(obj: &Value) -> (String, String) {
        let part = |p: &str| text(obj, p).unwrap_or_default().to_string();
        (part("/metadata/namespace"), part("/metadata/name"))
    }

    pub fn services_by_name<'a>(scope: Option<&str>, services: &'a [Value]) -> Listed<&'a Value> {
        Listed::new(
            scope,
            services.iter().filter_map(|s| {
                Some((text(s, "/metadata/namespace")?.to_string(), text(s, "/metadata/name")?.to_string(), s))
            }),
        )
    }

    pub fn problem(kind: &Kind, obj: &Value, severity: Severity, reason: String) -> Option<Problem> {
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
}

/// An object as its list is read now.
fn lean<T: DeserializeOwned>(obj: &Value) -> T {
    serde_json::from_value(obj.clone()).unwrap()
}

/// A field as an API server may send it: absent (None), `null`, or one of `values`.
fn field(values: &[Value]) -> impl Strategy<Value = Option<Value>> + use<> {
    prop_oneof![Just(None::<Value>), Just(Some(Value::Null)), prop::sample::select(values.to_vec()).prop_map(Some)]
}

/// A list field: absent, `null`, or a few of `item`.
fn list<S: Strategy<Value = Value> + 'static>(item: S) -> impl Strategy<Value = Option<Value>> {
    prop_oneof![
        Just(None::<Value>),
        Just(Some(Value::Null)),
        prop::collection::vec(item, 0..3).prop_map(|items| Some(Value::Array(items))),
    ]
}

/// `obj` with the fields that are there.
fn with(mut obj: Value, fields: impl IntoIterator<Item = (&'static str, Option<Value>)>) -> Value {
    for (key, value) in fields {
        if let Some(value) = value {
            obj[key] = value;
        }
    }
    obj
}

/// Metadata with what the checks read (or not), and what they don't read.
fn meta() -> impl Strategy<Value = Value> {
    let now = jiff::Timestamp::now().to_string();
    (
        field(&[json!("app"), json!("other"), json!("")]),
        field(&[json!("web"), json!("api"), json!("")]),
        field(&[json!("2026-01-01T00:00:00Z"), json!(now), json!(""), json!("yesterday")]),
        field(&[json!("2026-01-01T00:00:00Z")]),
    )
        .prop_map(|(namespace, name, created, deleted)| {
            let noise = json!({"uid": "u", "resourceVersion": "9", "labels": {"app": "web"},
                "annotations": {"kubectl.kubernetes.io/last-applied-configuration": "{\"spec\":{}}"},
                "managedFields": [{"manager": "kubectl", "operation": "Update", "fieldsType": "FieldsV1",
                    "fieldsV1": {"f:metadata": {"f:labels": {".": {}, "f:app": {}}}, "f:spec": {}}}]});
            with(
                noise,
                [
                    ("namespace", namespace),
                    ("name", name),
                    ("creationTimestamp", created),
                    ("deletionTimestamp", deleted),
                ],
            )
        })
}

fn service() -> impl Strategy<Value = Value> {
    let port = (field(&[json!("http"), json!("grpc"), json!("")]), field(&[json!(80), json!(8080)])).prop_map(
        |(name, port)| with(json!({"protocol": "TCP", "targetPort": "http"}), [("name", name), ("port", port)]),
    );
    let selector = field(&[json!({}), json!({"app": "web"}), json!({"app": "api", "tier": "front"})]);
    (meta(), field(&[json!("ClusterIP"), json!("ExternalName"), json!("")]), selector, list(port)).prop_map(
        |(metadata, kind, selector, ports)| {
            let spec =
                with(json!({"clusterIP": "10.96.0.1"}), [("type", kind), ("selector", selector), ("ports", ports)]);
            json!({"metadata": metadata, "spec": spec, "status": {"loadBalancer": {}}})
        },
    )
}

fn ingress() -> impl Strategy<Value = Value> {
    let port = prop_oneof![
        Just(None::<Value>),
        Just(Some(Value::Null)),
        field(&[json!(80), json!(8080)]).prop_map(|n| Some(with(json!({}), [("number", n)]))),
        field(&[json!("http"), json!("grpc"), json!("")]).prop_map(|n| Some(with(json!({}), [("name", n)]))),
    ];
    let service = (field(&[json!("web"), json!("api"), json!("gone"), json!("")]), port)
        .prop_map(|(name, port)| with(json!({}), [("name", name), ("port", port)]));
    let backend = prop_oneof![
        Just(json!({"resource": {"kind": "Bucket", "name": "b"}})),
        prop_oneof![Just(None::<Value>), Just(Some(Value::Null)), service.prop_map(Some)]
            .prop_map(|service| with(json!({}), [("service", service)])),
    ];
    let path = backend.clone().prop_map(|b| json!({"path": "/", "pathType": "Prefix", "backend": b}));
    let rule = list(path).prop_map(|paths| {
        let http = paths.map(|paths| with(json!({}), [("paths", Some(paths))]));
        with(json!({"host": "a.example.com"}), [("http", http)])
    });
    let tls = field(&[json!("a-tls"), json!("b-tls"), json!("x-tls"), json!("")])
        .prop_map(|name| with(json!({"hosts": ["a.example.com"]}), [("secretName", name)]));
    let default = prop_oneof![Just(None::<Value>), Just(Some(Value::Null)), backend.prop_map(Some)];
    (meta(), default, list(rule), list(tls)).prop_map(|(metadata, default, rules, tls)| {
        let spec =
            with(json!({"ingressClassName": "nginx"}), [("defaultBackend", default), ("rules", rules), ("tls", tls)]);
        json!({"metadata": metadata, "spec": spec, "status": {"loadBalancer": {"ingress": [{"ip": "192.0.2.1"}]}}})
    })
}

fn route() -> impl Strategy<Value = Value> {
    let backend = (
        field(&[json!(""), json!("example.com")]),
        field(&[json!("Service"), json!("Bucket"), json!("")]),
        field(&[json!("web"), json!("api"), json!("gone"), json!("")]),
        field(&[json!("app"), json!("other"), json!("")]),
    )
        .prop_map(|(group, kind, name, namespace)| {
            let fields = [("group", group), ("kind", kind), ("name", name), ("namespace", namespace)];
            with(json!({"port": 80, "weight": 1}), fields)
        });
    let rule =
        list(backend).prop_map(|refs| with(json!({"matches": [{"path": {"value": "/"}}]}), [("backendRefs", refs)]));
    (meta(), list(rule)).prop_map(|(metadata, rules)| {
        let spec = with(json!({"parentRefs": [{"name": "gateway"}]}), [("rules", rules)]);
        json!({"metadata": metadata, "spec": spec, "status": {"parents": []}})
    })
}

fn pod() -> impl Strategy<Value = Value> {
    let condition = (
        field(&[json!("Ready"), json!("ContainersReady"), json!("PodScheduled")]),
        field(&[json!("True"), json!("False"), json!("Unknown")]),
    )
        .prop_map(|(kind, status)| with(json!({"lastProbeTime": null}), [("type", kind), ("status", status)]));
    let phase = field(&[json!("Running"), json!("Pending"), json!("Succeeded"), json!("Failed"), json!("")]);
    (meta(), phase, list(condition)).prop_map(|(metadata, phase, conditions)| {
        let status = with(json!({"podIP": "10.0.0.1"}), [("phase", phase), ("conditions", conditions)]);
        json!({"metadata": metadata, "spec": {"containers": [{"name": "main", "image": "busybox"}]}, "status": status})
    })
}

fn workload() -> impl Strategy<Value = Value> {
    let labels =
        field(&[json!({}), json!({"app": "web"}), json!({"app": "web", "tier": "front"}), json!({"app": "api"})]);
    (meta(), field(&[json!(0), json!(1), json!(2)]), labels).prop_map(|(metadata, replicas, labels)| {
        let template = json!({"metadata": with(json!({"creationTimestamp": null}), [("labels", labels)]),
            "spec": {"containers": [{"name": "main", "image": "busybox"}]}});
        let spec =
            with(json!({"selector": {"matchLabels": {"app": "web"}}, "template": template}), [("replicas", replicas)]);
        json!({"metadata": metadata, "spec": spec, "status": {"observedGeneration": 1}})
    })
}

/// Of a problem, what comes from its object.
fn named(p: Option<Problem>) -> Option<(Option<String>, String, Option<jiff::Timestamp>)> {
    p.map(|p| (p.namespace, p.name, p.age))
}

proptest! {
    /// Its name, namespace and age, and whether it is old enough to judge, are read the same.
    #[test]
    fn an_object_is_read_the_same(metadata in meta()) {
        let obj = json!({"metadata": metadata});
        let read: Service = lean(&obj);
        prop_assert_eq!(read.metadata.settled(), whole::settled(&obj));
        prop_assert_eq!(read.metadata.key(), whole::key(&obj));
        let kind = Kind::fake("", "Service");
        let new = problem(&kind, &read.metadata, Severity::Warning, "x".into());
        prop_assert_eq!(named(new), named(whole::problem(&kind, &obj, Severity::Warning, "x".into())));
    }

    /// An Ingress: the same reasons, whatever its backends and TLS Secrets and the Services
    /// listed, in one namespace or all.
    #[test]
    fn an_ingress_is_judged_the_same(
        ing in ingress(),
        services in prop::collection::vec(service(), 0..4),
        scope in prop::option::of(Just("app")),
    ) {
        // One TLS Secret exists, one doesn't, one could not be read.
        let secret = |t: &Target| (t.name != "x-tls").then_some(t.name == "a-tls");
        let old = whole::judge_ingress(&ing, &whole::services_by_name(scope, &services), secret);
        let read: Vec<Service> = services.iter().map(lean).collect();
        prop_assert_eq!(judge_ingress(&lean(&ing), &services_by_name(scope, &read), secret), old);
        prop_assert_eq!(lean::<Ingress>(&ing).metadata.settled(), whole::settled(&ing));
    }

    /// An HTTPRoute: the same missing Services.
    #[test]
    fn an_http_route_is_judged_the_same(
        route in route(),
        services in prop::collection::vec(service(), 0..4),
        scope in prop::option::of(Just("app")),
    ) {
        let old = whole::judge_route(&route, &whole::services_by_name(scope, &services));
        let read: Vec<Service> = services.iter().map(lean).collect();
        prop_assert_eq!(judge_route(&lean(&route), &services_by_name(scope, &read)), old);
    }

    /// The pods a Service selects: the same verdict, its workload scaled to 0 or not.
    #[test]
    fn a_service_s_pods_are_judged_the_same(
        pods in prop::collection::vec(pod(), 0..4),
        scaled in prop::option::of(Just("Deployment web")),
    ) {
        let read: Vec<SelectedPod> = pods.iter().map(lean).collect();
        prop_assert_eq!(judge_service(&read, scaled), whole::judge_service(&pods, scaled));
    }

    /// The workload scaled to 0 that a Service selects: the same one, among workloads of any
    /// namespace, replicas and labels.
    #[test]
    fn a_service_s_workload_is_found_the_same(
        svc in service(),
        workloads in prop::collection::vec((prop::sample::select(vec!["Deployment", "StatefulSet"]), workload()), 0..4),
    ) {
        // Only a Service that selects pods is looked into.
        if k8s::pod_selection(&Kind::fake("", "Service"), &svc) == Selection::Nothing {
            return Ok(());
        }
        let old = whole::scaled_to_zero(&svc, &workloads);
        let scaled: Vec<Scaled> = workloads.iter().filter_map(|(kind, w)| lean::<Workload>(w).scaled(*kind)).collect();
        prop_assert_eq!(scaled_to_zero(&lean(&svc), &scaled), old);
    }
}
