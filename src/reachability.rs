//! "Why doesn't it answer?": the path a request takes to a Service, an Ingress or an
//! HTTPRoute, checked step by step on what the cluster says (class, address, backends, the
//! pods a Service selects, their ports, ready endpoints, certificates), naming the first step
//! that breaks and the `kubectl` command to see it. The Reachability tab and the
//! `check_reachability` tool share it.
//!
//! Reading and checking are apart: [`gather`] reads what the checks need, in the object's own
//! cluster; [`check`] works on what was read, so each step is tested on objects made up here.
//! NetworkPolicies are left out: predicting what they block is not this check's job.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::{Pod, Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::IngressClass;
use kube::Client;
use kube::discovery::ApiResource;
use serde_json::{Value, json};

use crate::details::{kubectl, shell_word};
use crate::k8s::{self, Kind, ObjectRef};

/// The kinds whose path is checked.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Service,
    Ingress,
    HttpRoute,
}

impl Target {
    /// The check for this kind, if it has one.
    pub fn of(kind: &Kind) -> Option<Self> {
        if kind.is("", "Service") {
            Some(Self::Service)
        } else if kind.is("networking.k8s.io", "Ingress") {
            Some(Self::Ingress)
        } else if kind.is("gateway.networking.k8s.io", "HTTPRoute") {
            Some(Self::HttpRoute)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Ok,
    /// Requests get through, but something may bite (some pods left out, a certificate
    /// expiring soon).
    Warn,
    /// Requests stop here.
    Broken,
    /// Not checked: a read failed or isn't allowed, or the step goes somewhere this doesn't follow.
    Unknown,
}

/// An object a step names, to open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    /// The kind as kubectl names it: `pods`, `secrets`.
    pub kind: &'static str,
    pub namespace: Option<String>,
    pub name: String,
}

/// A line under a step (a pod that is not ready, and why), with the object it is about.
#[derive(Clone, Debug)]
pub struct Item {
    pub text: String,
    pub link: Option<Link>,
}

/// One step of the path.
#[derive(Clone, Debug)]
pub struct Step {
    /// What the step is about: "Ingress", "Service web", "TLS".
    pub section: String,
    pub title: String,
    pub state: State,
    pub detail: String,
    /// The `kubectl` command that shows what the step read.
    pub command: Option<String>,
    pub items: Vec<Item>,
}

impl Step {
    fn new(section: &str, title: impl Into<String>, state: State, detail: impl Into<String>) -> Self {
        Self {
            section: section.to_string(),
            title: title.into(),
            state,
            detail: detail.into(),
            command: None,
            items: Vec::new(),
        }
    }

    fn with_command(mut self, command: String) -> Self {
        self.command = Some(command);
        self
    }

    fn with_items(mut self, items: Vec<Item>) -> Self {
        self.items = items;
        self
    }
}

/// The verdict: the first step that breaks, else whether every step could be checked.
pub fn verdict(steps: &[Step]) -> (State, String) {
    if let Some(s) = steps.iter().find(|s| s.state == State::Broken) {
        return (State::Broken, format!("Breaks at {} › {}: {}", s.section, s.title, s.detail));
    }
    let count = |state: State| steps.iter().filter(|s| s.state == state).count();
    match (count(State::Unknown), count(State::Warn)) {
        (0, 0) => (State::Ok, "Every step checks out.".into()),
        (0, warn) => (State::Warn, format!("No step breaks; {warn} warning(s).")),
        (unknown, _) => (State::Unknown, format!("No step breaks, but {unknown} could not be checked.")),
    }
}

/// The steps as text, for agents: the verdict, then each step by section, with the command
/// to see the ones that are not fine.
pub fn to_text(steps: &[Step]) -> String {
    let mut out = verdict(steps).1;
    out.push('\n');
    let mut section = "";
    for s in steps {
        if s.section != section {
            section = s.section.as_str();
            out.push_str(&format!("\n{section}\n"));
        }
        let state = match s.state {
            State::Ok => "ok",
            State::Warn => "WARN",
            State::Broken => "BROKEN",
            State::Unknown => "not checked",
        };
        out.push_str(&format!("  [{state}] {}: {}\n", s.title, s.detail));
        for i in &s.items {
            out.push_str(&format!("      - {}\n", i.text));
        }
        if let Some(c) = s.command.as_ref().filter(|_| s.state != State::Ok) {
            out.push_str(&format!("      $ {c}\n"));
        }
    }
    out
}

// MARK: Reading

/// A read the checks are about to make: a `list` or a `get` of a kind, in a namespace (none for
/// a cluster-scoped kind), by name.
pub struct Read<'a> {
    pub list: bool,
    pub ar: &'a ApiResource,
    pub namespace: Option<&'a str>,
    pub name: Option<&'a str>,
}

/// Whether a read may be made: an agent's rules, decided before it is made.
pub type Allowed = Arc<dyn Fn(&Read<'_>) -> bool + Send + Sync>;

/// Why a read was not made.
pub const NOT_ALLOWED: &str = "your rules do not allow reading it";

/// Reads the objects of one cluster (the checked object's).
#[derive(Clone)]
pub struct Reader {
    client: Client,
    /// The agents' rules; None in the app, where the user's own credentials decide.
    allowed: Option<Allowed>,
}

impl Reader {
    pub fn new(client: Client, allowed: Option<Allowed>) -> Self {
        Self { client, allowed }
    }

    fn may(&self, read: &Read<'_>) -> bool {
        self.allowed.as_ref().is_none_or(|f| f(read))
    }

    /// One object by name: None when it doesn't exist.
    async fn get(&self, ar: ApiResource, namespace: Option<String>, name: String) -> Fetched<Option<Value>> {
        if !self.may(&Read { list: false, ar: &ar, namespace: namespace.as_deref(), name: Some(name.as_str()) }) {
            return Err(NOT_ALLOWED.into());
        }
        let kind = Kind { ar, namespaced: namespace.is_some(), section: "", label: String::new() };
        match k8s::get_object(self.client.clone(), ObjectRef { kind, name, namespace, uid: String::new() }).await {
            Ok(v) => Ok(Some(v)),
            Err(e) if k8s::is_not_found(&e) => Ok(None),
            Err(e) => Err(k8s::short_error(&e)),
        }
    }

    /// The objects of a kind, by label selector. Those the rules hide by name are left out.
    async fn list(&self, ar: ApiResource, namespace: Option<String>, selector: Option<String>) -> Fetched<Vec<Value>> {
        if !self.may(&Read { list: true, ar: &ar, namespace: namespace.as_deref(), name: None }) {
            return Err(NOT_ALLOWED.into());
        }
        let mut items = k8s::list_values(self.client.clone(), ar.clone(), namespace.clone(), selector)
            .await
            .map_err(|e| k8s::short_error(&e))?;
        if self.allowed.is_some() {
            items.retain(|v| {
                let name = s(v, "/metadata/name");
                self.may(&Read { list: true, ar: &ar, namespace: namespace.as_deref(), name: Some(name) })
            });
        }
        Ok(items)
    }
}

/// A read: what came back, or why nothing did.
pub type Fetched<T> = Result<T, String>;

/// An object of the checked cluster: (namespace, name).
pub type Key = (String, String);

/// What the checks read, all in the checked object's cluster.
#[derive(Default)]
pub struct World {
    /// The `IngressClass` objects (for an Ingress).
    pub classes: Option<Fetched<Vec<Value>>>,
    pub gateways: BTreeMap<Key, Fetched<Option<Value>>>,
    pub services: BTreeMap<Key, Fetched<Option<Value>>>,
    /// TLS Secrets, with only their certificate kept.
    pub secrets: BTreeMap<Key, Fetched<Option<Value>>>,
    /// By Service: the pods its selector matches (none read without a selector).
    pub pods: BTreeMap<Key, Fetched<Vec<Value>>>,
    /// By Service: its EndpointSlices.
    pub slices: BTreeMap<Key, Fetched<Vec<Value>>>,
}

/// Read what the checks of `obj` need, through `reader`: what the object names (class,
/// Gateways, Services, TLS Secrets), then behind each Service its pods and EndpointSlices.
/// A read that fails is kept as such: it makes its step "not checked", not the whole check.
pub async fn gather(reader: Reader, target: Target, obj: Value) -> World {
    let ns = s(&obj, "/metadata/namespace");
    let mut world = World::default();
    let mut secrets: Vec<Key> = Vec::new();
    match target {
        Target::Service => {
            let key = (ns.to_string(), s(&obj, "/metadata/name").to_string());
            world.services.insert(key, Ok(Some(obj.clone())));
        }
        Target::Ingress => {
            world.classes = Some(reader.list(k8s::ar_of::<IngressClass>(), None, None).await);
            for t in arr(&obj, "/spec/tls") {
                if let Some(name) = t["secretName"].as_str().filter(|n| !n.is_empty()) {
                    secrets.push((ns.to_string(), name.to_string()));
                }
            }
        }
        Target::HttpRoute => {
            let ar = gateway_ar(&obj);
            for (_, key) in route_gateways(&obj) {
                if !world.gateways.contains_key(&key) {
                    let read = reader.get(ar.clone(), Some(key.0.clone()), key.1.clone()).await;
                    world.gateways.insert(key, read);
                }
            }
            secrets = route_secrets(&obj, &world.gateways);
        }
    }
    let mut services: Vec<Key> = Vec::new();
    for b in backends(target, &obj) {
        if let Backend::Service { key, .. } = b
            && !services.contains(&key)
            && !world.services.contains_key(&key)
        {
            services.push(key);
        }
    }
    secrets.sort();
    secrets.dedup();
    let svc_ar = k8s::ar_of::<Service>();
    let secret_ar = k8s::ar_of::<Secret>();
    let (read_services, read_secrets) = futures::join!(
        futures::future::join_all(services.iter().map(|k| reader.get(svc_ar.clone(), Some(k.0.clone()), k.1.clone()))),
        futures::future::join_all(secrets.iter().map(|k| reader.get(
            secret_ar.clone(),
            Some(k.0.clone()),
            k.1.clone()
        )))
    );
    world.services.extend(services.into_iter().zip(read_services));
    // Only the certificate is kept: the private key is never needed.
    world.secrets.extend(secrets.into_iter().zip(read_secrets).map(|(k, r)| (k, r.map(|o| o.map(certificate_only)))));
    let behind: Vec<(Key, Option<String>)> = world
        .services
        .iter()
        .filter_map(|(key, read)| {
            let svc = read.as_ref().ok()?.as_ref()?;
            if s(svc, "/spec/type") == "ExternalName" {
                return None;
            }
            // A Service without a selector has no pods to list (its endpoints are written by hand).
            let selector = k8s::pod_selection(&service_kind(), svc).list_selector().map(Option::unwrap_or_default);
            Some((key.clone(), selector))
        })
        .collect();
    let reader = &reader;
    let reads = futures::future::join_all(behind.iter().map(|(key, selector)| async move {
        let pods = futures::future::OptionFuture::from(
            selector.as_ref().map(|sel| reader.list(k8s::ar_of::<Pod>(), Some(key.0.clone()), Some(sel.clone()))),
        );
        let slices = reader.list(
            k8s::ar_of::<EndpointSlice>(),
            Some(key.0.clone()),
            Some(format!("kubernetes.io/service-name={}", key.1)),
        );
        futures::join!(pods, slices)
    }))
    .await;
    for ((key, _), (pods, slices)) in behind.into_iter().zip(reads) {
        if let Some(pods) = pods {
            world.pods.insert(key.clone(), pods);
        }
        world.slices.insert(key, slices);
    }
    world
}

fn service_kind() -> Kind {
    Kind { ar: k8s::ar_of::<Service>(), namespaced: true, section: "", label: String::new() }
}

/// A TLS Secret reduced to its certificate.
fn certificate_only(secret: Value) -> Value {
    json!({
        "metadata": {"name": secret.pointer("/metadata/name"), "namespace": secret.pointer("/metadata/namespace")},
        "type": secret.get("type"),
        "data": {"tls.crt": secret.pointer("/data/tls.crt")},
    })
}

/// Gateways in the API version of the route that names them.
fn gateway_ar(route: &Value) -> ApiResource {
    const GROUP: &str = "gateway.networking.k8s.io";
    let api_version = route["apiVersion"]
        .as_str()
        .filter(|v| v.strip_prefix(GROUP).is_some_and(|rest| rest.starts_with('/')))
        .unwrap_or("gateway.networking.k8s.io/v1");
    ApiResource {
        group: GROUP.into(),
        version: api_version.rsplit('/').next().unwrap_or("v1").into(),
        api_version: api_version.into(),
        kind: "Gateway".into(),
        plural: "gateways".into(),
    }
}

// MARK: Checks

/// Check the path to `obj` on what was read (`world`); `now` dates the certificates.
pub fn check(context: &str, target: Target, obj: &Value, world: &World, now: jiff::Timestamp) -> Vec<Step> {
    let mut steps = Vec::new();
    match target {
        Target::Service => {
            let key = (s(obj, "/metadata/namespace").to_string(), s(obj, "/metadata/name").to_string());
            service_steps(context, &key, obj, None, world, &mut steps);
        }
        Target::Ingress => {
            ingress_steps(context, obj, world, &mut steps);
            for (key, ports) in backend_steps(context, target, obj, world, &mut steps) {
                if let Some(Ok(Some(svc))) = world.services.get(&key) {
                    service_steps(context, &key, svc, Some(ports.as_slice()), world, &mut steps);
                }
            }
            ingress_tls(context, obj, world, now, &mut steps);
        }
        Target::HttpRoute => {
            route_steps(context, obj, world, &mut steps);
            for (key, ports) in backend_steps(context, target, obj, world, &mut steps) {
                if let Some(Ok(Some(svc))) = world.services.get(&key) {
                    service_steps(context, &key, svc, Some(ports.as_slice()), world, &mut steps);
                }
            }
            route_tls(context, obj, world, now, &mut steps);
        }
    }
    steps
}

fn s<'a>(v: &'a Value, path: &str) -> &'a str {
    v.pointer(path).and_then(Value::as_str).unwrap_or("")
}

fn arr<'a>(v: &'a Value, path: &str) -> &'a [Value] {
    v.pointer(path).and_then(Value::as_array).map_or(&[][..], Vec::as_slice)
}

fn find_condition<'a>(conditions: &'a [Value], ty: &str) -> Option<&'a Value> {
    conditions.iter().find(|c| c["type"].as_str() == Some(ty))
}

/// A condition's message, else its reason.
fn condition_text(c: &Value) -> &str {
    c["message"].as_str().filter(|m| !m.is_empty()).or(c["reason"].as_str()).unwrap_or("")
}

/// The addresses in an Ingress's status: IPs or host names.
fn lb_addresses(ing: &Value) -> Vec<String> {
    arr(ing, "/status/loadBalancer/ingress")
        .iter()
        .filter_map(|a| a["ip"].as_str().or(a["hostname"].as_str()))
        .map(String::from)
        .collect()
}

/// A Service port as people name it: `http:80`, or `80`.
fn port_label(p: &Value) -> String {
    let port = p["port"].as_i64().unwrap_or(0);
    match p["name"].as_str().filter(|n| !n.is_empty()) {
        Some(n) => format!("{n}:{port}"),
        None => port.to_string(),
    }
}

/// Where a Service port sends requests on the pods: its `targetPort`, else the same number.
fn target_label(p: &Value) -> String {
    match &p["targetPort"] {
        Value::String(name) => name.clone(),
        t => t.as_i64().or(p["port"].as_i64()).unwrap_or(0).to_string(),
    }
}

fn pod_link(ns: &str, name: &str) -> Option<Link> {
    Some(Link { kind: "pods", namespace: Some(ns.to_string()), name: name.to_string() })
}

/// How a backend names the Service port it sends to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PortRef {
    Number(i64),
    Name(String),
    Missing,
}

impl PortRef {
    fn label(&self) -> String {
        match self {
            Self::Number(n) => n.to_string(),
            Self::Name(n) => n.clone(),
            Self::Missing => "?".into(),
        }
    }

    /// The port of the Service it names.
    fn find<'a>(&self, svc: &'a Value) -> Option<&'a Value> {
        arr(svc, "/spec/ports").iter().find(|p| match self {
            Self::Number(n) => p["port"].as_i64() == Some(*n),
            Self::Name(n) => p["name"].as_str() == Some(n.as_str()),
            Self::Missing => false,
        })
    }
}

/// Where a rule sends requests.
#[derive(Clone, Debug, PartialEq)]
enum Backend {
    Service {
        from: String,
        key: Key,
        port: PortRef,
    },
    /// Not a Service (an Ingress `resource` backend, another kind in a route): not followed.
    Other {
        from: String,
        what: String,
    },
}

/// The backends of an Ingress (its default backend, then each rule's paths) or of a route.
fn backends(target: Target, obj: &Value) -> Vec<Backend> {
    let ns = s(obj, "/metadata/namespace");
    let mut out = Vec::new();
    match target {
        Target::Service => {}
        Target::Ingress => {
            let mut add = |from: String, b: &Value| {
                if let Some(svc) = b.get("service") {
                    let port = match (svc.pointer("/port/number").and_then(Value::as_i64), svc.pointer("/port/name")) {
                        (Some(n), _) => PortRef::Number(n),
                        (None, Some(Value::String(n))) => PortRef::Name(n.clone()),
                        _ => PortRef::Missing,
                    };
                    out.push(Backend::Service { from, key: (ns.to_string(), s(svc, "/name").to_string()), port });
                } else if let Some(r) = b.get("resource") {
                    out.push(Backend::Other { from, what: format!("{} {}", s(r, "/kind"), s(r, "/name")) });
                }
            };
            if let Some(b) = obj.pointer("/spec/defaultBackend") {
                add("the default backend".into(), b);
            }
            for r in arr(obj, "/spec/rules") {
                let host = r["host"].as_str().unwrap_or("*");
                for p in arr(r, "/http/paths") {
                    add(format!("{host}{}", p["path"].as_str().unwrap_or("/")), &p["backend"]);
                }
            }
        }
        Target::HttpRoute => {
            for (i, r) in arr(obj, "/spec/rules").iter().enumerate() {
                for b in arr(r, "/backendRefs") {
                    let from = format!("rule {}", i + 1);
                    let kind = b["kind"].as_str().unwrap_or("Service");
                    if b["group"].as_str().unwrap_or("").is_empty() && kind == "Service" {
                        let key = (b["namespace"].as_str().unwrap_or(ns).to_string(), s(b, "/name").to_string());
                        let port = b["port"].as_i64().map_or(PortRef::Missing, PortRef::Number);
                        out.push(Backend::Service { from, key, port });
                    } else {
                        out.push(Backend::Other { from, what: format!("{kind} {}", s(b, "/name")) });
                    }
                }
            }
        }
    }
    out
}

/// One step for each Service and port the rules send to (rules sending to the same one share
/// it). Returns the Services found with the ports used, to follow them.
fn backend_steps(
    context: &str,
    target: Target,
    obj: &Value,
    world: &World,
    steps: &mut Vec<Step>,
) -> Vec<(Key, Vec<Value>)> {
    const SECTION: &str = "Backends";
    let all = backends(target, obj);
    if all.is_empty() && target == Target::Ingress {
        steps.push(Step::new(
            SECTION,
            "Rules",
            State::Broken,
            "No rules and no default backend: it sends requests nowhere.",
        ));
    }
    let mut groups: Vec<(Key, PortRef, Vec<String>)> = Vec::new();
    for b in all {
        match b {
            Backend::Service { from, key, port } => match groups.iter_mut().find(|g| g.0 == key && g.1 == port) {
                Some(g) => g.2.push(from),
                None => groups.push((key, port, vec![from])),
            },
            Backend::Other { from, what } => {
                steps.push(Step::new(SECTION, from, State::Unknown, format!("{what}: not a Service, not followed.")));
            }
        }
    }
    let mut behind: Vec<(Key, Vec<Value>)> = Vec::new();
    for (key, port, from) in groups {
        let (ns, name) = (key.0.as_str(), key.1.as_str());
        let title = format!("{name}:{}", port.label());
        let command = kubectl(context, Some(ns), &format!("get service {} -o wide", shell_word(name)));
        let mut used = None;
        let (state, detail) = match world.services.get(&key) {
            None => (State::Unknown, format!("Service {name} was not read.")),
            Some(Err(e)) => (State::Unknown, format!("Could not read Service {name}: {e}")),
            Some(Ok(None)) => (State::Broken, format!("No Service {name} in {ns}.")),
            Some(Ok(Some(svc))) if s(svc, "/spec/type") == "ExternalName" => (
                State::Ok,
                format!("Service {name} is an ExternalName: requests go to {}.", s(svc, "/spec/externalName")),
            ),
            Some(Ok(Some(svc))) => match port.find(svc) {
                Some(p) => {
                    used = Some(p.clone());
                    (State::Ok, format!("Service port {} → target port {}.", port_label(p), target_label(p)))
                }
                None if port == PortRef::Missing => (State::Broken, format!("No port given for Service {name}.")),
                None => {
                    let ports: Vec<String> = arr(svc, "/spec/ports").iter().map(port_label).collect();
                    let has = if ports.is_empty() { "none".to_string() } else { ports.join(", ") };
                    (State::Broken, format!("Service {name} has no port {} (its ports: {has}).", port.label()))
                }
            },
        };
        steps.push(
            Step::new(SECTION, title, state, format!("{detail} From {}.", from.join(", "))).with_command(command),
        );
        if let Some(p) = used {
            match behind.iter_mut().find(|b| b.0 == key) {
                Some(b) => {
                    if !b.1.contains(&p) {
                        b.1.push(p);
                    }
                }
                None => behind.push((key, vec![p])),
            }
        }
    }
    behind
}

/// The steps behind a Service: the pods its selector matches, the target port on them, and
/// its ready endpoints. `only` limits them to the ports a rule sends to.
fn service_steps(context: &str, key: &Key, svc: &Value, only: Option<&[Value]>, world: &World, steps: &mut Vec<Step>) {
    let (ns, name) = (key.0.as_str(), key.1.as_str());
    let section = format!("Service {name}");
    if s(svc, "/spec/type") == "ExternalName" {
        let detail = format!(
            "An ExternalName: the cluster DNS answers with {}; nothing in the cluster behind it to check.",
            s(svc, "/spec/externalName")
        );
        steps.push(Step::new(&section, "External name", State::Ok, detail));
        return;
    }
    let ports: Vec<&Value> = match only {
        Some(p) => p.iter().collect(),
        None => arr(svc, "/spec/ports").iter().collect(),
    };
    let pods: Option<&[Value]> = if let Some(selector) = k8s::pod_selection(&service_kind(), svc).list_selector() {
        let sel = selector.unwrap_or_default();
        let command = kubectl(context, Some(ns), &format!("get pods -l {} -o wide", shell_word(&sel)));
        let step = match world.pods.get(key) {
            None => Step::new(&section, "Selector", State::Unknown, "Its pods were not read."),
            Some(Err(e)) => Step::new(&section, "Selector", State::Unknown, format!("Could not list its pods: {e}")),
            Some(Ok(pods)) if pods.is_empty() => {
                Step::new(&section, "Selects no pods", State::Broken, format!("No pod in {ns} has the labels {sel}."))
            }
            Some(Ok(pods)) => Step::new(&section, "Selector", State::Ok, format!("{} pod(s) match {sel}.", pods.len())),
        };
        steps.push(step.with_command(command));
        world.pods.get(key).and_then(|r| r.as_ref().ok()).map(Vec::as_slice)
    } else {
        let detail = "No selector: its endpoints are not picked from pods but written by hand or by a controller.";
        steps.push(Step::new(&section, "Selector", State::Ok, detail));
        None
    };
    if let Some(pods) = pods.filter(|p| !p.is_empty()) {
        for p in &ports {
            steps.push(target_port_step(&section, ns, p, pods));
        }
    }
    steps.push(endpoints_step(context, &section, key, &ports, pods, world.slices.get(key)));
}

/// The ports a pod's containers declare.
fn container_ports(pod: &Value) -> impl Iterator<Item = &Value> {
    arr(pod, "/spec/containers").iter().flat_map(|c| arr(c, "/ports"))
}

/// A pod's declared ports, as `8080/http, 9090`.
fn declared(pod: &Value) -> String {
    let ports: Vec<String> = container_ports(pod)
        .map(|p| match p["name"].as_str() {
            Some(n) => format!("{}/{n}", p["containerPort"]),
            None => p["containerPort"].to_string(),
        })
        .collect();
    if ports.is_empty() { "none".into() } else { ports.join(", ") }
}

/// Whether the Service port's `targetPort` exists on its pods. A named one must be declared
/// by a container, or the pod gets no traffic on that port; a number need not be (declaring
/// ports is optional), but one no container declares while they declare others is suspect.
fn target_port_step(section: &str, ns: &str, port: &Value, pods: &[Value]) -> Step {
    let title = format!("Target port of {}", port_label(port));
    if let Value::String(name) = &port["targetPort"] {
        let missing: Vec<&Value> = pods
            .iter()
            .filter(|pod| !container_ports(pod).any(|p| p["name"].as_str() == Some(name.as_str())))
            .collect();
        if missing.is_empty() {
            return Step::new(section, title, State::Ok, format!("Every pod declares a port named {name}."));
        }
        if missing.len() == pods.len() {
            let detail =
                format!("No container of its pods has a port named {name} (they declare: {}).", declared(&pods[0]));
            return Step::new(section, title, State::Broken, detail);
        }
        let items = missing
            .iter()
            .map(|pod| {
                let pod_name = s(pod, "/metadata/name");
                Item { text: format!("{pod_name}: declares {}", declared(pod)), link: pod_link(ns, pod_name) }
            })
            .collect();
        let detail =
            format!("{} of {} pods have no port named {name}: they get no traffic on it.", missing.len(), pods.len());
        return Step::new(section, title, State::Warn, detail).with_items(items);
    }
    let n = port["targetPort"].as_i64().or(port["port"].as_i64()).unwrap_or(0);
    let any_declared = pods.iter().any(|p| container_ports(p).next().is_some());
    if !any_declared {
        let detail = format!("Port {n}; the containers declare no ports, which Kubernetes doesn't require.");
        return Step::new(section, title, State::Ok, detail);
    }
    if pods.iter().any(|pod| container_ports(pod).any(|p| p["containerPort"].as_i64() == Some(n))) {
        return Step::new(section, title, State::Ok, format!("Port {n}, declared by its containers."));
    }
    let detail = format!(
        "No container declares port {n} (they declare: {}). Requests still go to {n}: check the app listens there.",
        declared(&pods[0])
    );
    Step::new(section, title, State::Warn, detail)
}

/// The ready addresses of the EndpointSlices that carry the Service port named `port` ("" for
/// one without a name), or of every slice when the Service has no ports. A readiness the
/// slice doesn't give counts as ready, as the API says.
fn ready_addresses<'a>(slices: &'a [Value], port: Option<&str>) -> BTreeSet<&'a str> {
    slices
        .iter()
        .filter(|sl| port.is_none_or(|p| arr(sl, "/ports").iter().any(|x| x["name"].as_str().unwrap_or("") == p)))
        .flat_map(|sl| arr(sl, "/endpoints"))
        .filter(|e| e.pointer("/conditions/ready").and_then(Value::as_bool) != Some(false))
        .filter_map(|e| arr(e, "/addresses").first().and_then(Value::as_str))
        .collect()
}

/// The pods behind a Service that are not ready, and why: those its selector matches, and
/// those its EndpointSlices list as not ready (a Service without a selector has only these).
fn not_ready_items(ns: &str, pods: Option<&[Value]>, slices: &[Value]) -> Vec<Item> {
    let mut out: Vec<(String, String)> = pods
        .unwrap_or_default()
        .iter()
        .filter_map(|p| Some((s(p, "/metadata/name").to_string(), not_ready_reason(p)?)))
        .collect();
    let in_slices = slices
        .iter()
        .flat_map(|sl| arr(sl, "/endpoints"))
        .filter(|e| e.pointer("/conditions/ready").and_then(Value::as_bool) == Some(false))
        .filter(|e| s(e, "/targetRef/kind") == "Pod");
    for e in in_slices {
        let name = s(e, "/targetRef/name");
        if !out.iter().any(|o| o.0 == name) {
            let terminating = e.pointer("/conditions/terminating").and_then(Value::as_bool) == Some(true);
            out.push((name.to_string(), if terminating { "terminating" } else { "not ready" }.to_string()));
        }
    }
    const MAX: usize = 20;
    let more = out.len().saturating_sub(MAX);
    let mut items: Vec<Item> = out
        .into_iter()
        .take(MAX)
        .map(|(name, why)| Item { text: format!("{name}: {why}"), link: pod_link(ns, &name) })
        .collect();
    if more > 0 {
        items.push(Item { text: format!("and {more} more"), link: None });
    }
    items
}

/// Why a pod is not ready, in words; None when it is.
pub fn not_ready_reason(pod: &Value) -> Option<String> {
    let conditions = arr(pod, "/status/conditions");
    if find_condition(conditions, "Ready").is_some_and(|c| c["status"] == "True") {
        return None;
    }
    if pod.pointer("/metadata/deletionTimestamp").is_some() {
        return Some("terminating".into());
    }
    let phase = s(pod, "/status/phase");
    if phase == "Succeeded" || phase == "Failed" {
        return Some(format!("finished ({phase})"));
    }
    if phase == "Pending" {
        if s(pod, "/spec/nodeName").is_empty() {
            let why = find_condition(conditions, "PodScheduled").map(condition_text).filter(|m| !m.is_empty());
            return Some(
                why.map_or_else(|| "pending, not scheduled".into(), |m| format!("pending, not scheduled: {m}")),
            );
        }
        return Some(format!("pending ({})", crate::overview::pod_status(pod)));
    }
    let specs = arr(pod, "/spec/containers");
    let containers: Vec<String> = arr(pod, "/status/containerStatuses")
        .iter()
        .filter(|c| c["ready"].as_bool() != Some(true))
        .map(|c| {
            let name = s(c, "/name");
            let spec = specs.iter().find(|x| x["name"].as_str() == Some(name));
            let has = |probe: &str| spec.is_some_and(|x| x.get(probe).is_some());
            let why = if let Some(r) = c.pointer("/state/waiting/reason").and_then(Value::as_str) {
                r.to_string()
            } else if c.pointer("/state/terminated").is_some() {
                "terminated".into()
            } else if c["started"].as_bool() == Some(false) && has("startupProbe") {
                "startup probe not passed yet".into()
            } else if has("readinessProbe") {
                "readiness probe failing".into()
            } else {
                "not ready".into()
            };
            format!("{name}: {why}")
        })
        .collect();
    if !containers.is_empty() {
        return Some(containers.join("; "));
    }
    // Its containers are ready: a readiness gate holds it back.
    let gates: Vec<&str> = arr(pod, "/spec/readinessGates")
        .iter()
        .filter_map(|g| g["conditionType"].as_str())
        .filter(|t| find_condition(conditions, t).is_none_or(|c| c["status"] != "True"))
        .collect();
    if !gates.is_empty() {
        return Some(format!("readiness gate {} not met", gates.join(", ")));
    }
    let why = find_condition(conditions, "Ready").map(condition_text).filter(|m| !m.is_empty());
    Some(why.map_or_else(|| "not ready".into(), String::from))
}

/// A Service's ready endpoints, for each port checked; the pods not ready, and why.
fn endpoints_step(
    context: &str,
    section: &str,
    key: &Key,
    ports: &[&Value],
    pods: Option<&[Value]>,
    slices: Option<&Fetched<Vec<Value>>>,
) -> Step {
    let (ns, name) = (key.0.as_str(), key.1.as_str());
    let command =
        kubectl(context, Some(ns), &format!("get endpointslices -l kubernetes.io/service-name={}", shell_word(name)));
    let slices = match slices {
        Some(Ok(slices)) => slices,
        Some(Err(e)) => {
            let detail = format!("Could not list its EndpointSlices: {e}");
            return Step::new(section, "Endpoints", State::Unknown, detail).with_command(command);
        }
        None => {
            return Step::new(section, "Endpoints", State::Unknown, "Its EndpointSlices were not read.")
                .with_command(command);
        }
    };
    let counts: Vec<(String, usize)> = if ports.is_empty() {
        vec![(String::new(), ready_addresses(slices, None).len())]
    } else {
        ports
            .iter()
            .map(|p| (port_label(p), ready_addresses(slices, Some(p["name"].as_str().unwrap_or(""))).len()))
            .collect()
    };
    let not_ready = not_ready_items(ns, pods, slices);
    let empty: Vec<&str> = counts.iter().filter(|c| c.1 == 0).map(|c| c.0.as_str()).collect();
    let (state, detail) = if empty.is_empty() {
        let ready = match counts.as_slice() {
            [(_, n)] => format!("{n} ready"),
            _ => counts.iter().map(|(p, n)| format!("{p}: {n} ready")).collect::<Vec<_>>().join(", "),
        };
        if not_ready.is_empty() {
            (State::Ok, format!("{ready}."))
        } else {
            (State::Warn, format!("{ready}; not ready:"))
        }
    } else {
        let on = if counts.len() > 1 { format!(" on {}", empty.join(", ")) } else { String::new() };
        let why = if not_ready.is_empty() { "" } else { " Not ready:" };
        (State::Broken, format!("No ready endpoint{on}: requests have nowhere to go.{why}"))
    };
    Step::new(section, "Endpoints", state, detail).with_command(command).with_items(not_ready)
}

const NO_CLASS: &str = "No class, and no IngressClass is the default: no controller is asked to serve this Ingress.";

/// The Ingress itself: its class, which names the controller that serves it, and the address
/// that controller gave it.
fn ingress_steps(context: &str, ing: &Value, world: &World, steps: &mut Vec<Step>) {
    const SECTION: &str = "Ingress";
    let (ns, name) = (s(ing, "/metadata/namespace"), s(ing, "/metadata/name"));
    let addresses = lb_addresses(ing);
    let in_spec = ing.pointer("/spec/ingressClassName").and_then(Value::as_str);
    let annotated = ing.pointer("/metadata/annotations/kubernetes.io~1ingress.class").and_then(Value::as_str);
    // With an address, some controller took the Ingress, whatever its class says.
    let unserved = if addresses.is_empty() { State::Broken } else { State::Warn };
    let mut controller = None;
    let class_step = match &world.classes {
        None => None,
        Some(Err(e)) => {
            Some(Step::new(SECTION, "Class", State::Unknown, format!("Could not list IngressClasses: {e}")))
        }
        Some(Ok(classes)) => {
            let named = |n: &str| classes.iter().find(|c| s(c, "/metadata/name") == n);
            let default = classes.iter().find(|c| {
                c.pointer("/metadata/annotations/ingressclass.kubernetes.io~1is-default-class").and_then(Value::as_str)
                    == Some("true")
            });
            Some(match (in_spec, annotated) {
                (Some(n), _) | (None, Some(n)) if named(n).is_some() => {
                    controller = named(n).map(|c| s(c, "/spec/controller"));
                    let detail = format!("Served by the controller {}.", controller.unwrap_or_default());
                    Step::new(SECTION, format!("Class {n}"), State::Ok, detail)
                }
                (Some(n), _) => {
                    let detail = format!("No IngressClass named {n}: no controller is asked to serve this Ingress.");
                    Step::new(SECTION, format!("Class {n}"), unserved, detail)
                }
                (None, Some(n)) => {
                    let detail = format!(
                        "Named by the old kubernetes.io/ingress.class annotation, with no IngressClass of that name: \
                         only a controller started for class {n} serves it."
                    );
                    Step::new(SECTION, format!("Class {n}"), State::Warn, detail)
                }
                (None, None) => match default {
                    Some(c) => {
                        controller = Some(s(c, "/spec/controller"));
                        let detail = format!("Served by the controller {}.", s(c, "/spec/controller"));
                        Step::new(SECTION, format!("Class {} (the default)", s(c, "/metadata/name")), State::Ok, detail)
                    }
                    None => Step::new(SECTION, "Class", unserved, NO_CLASS),
                },
            })
        }
    };
    if let Some(step) = class_step {
        steps.push(step.with_command(kubectl(context, None, "get ingressclass")));
    }
    let describe = kubectl(context, Some(ns), &format!("describe ingress {}", shell_word(name)));
    if addresses.is_empty() {
        let by = controller.map(|c| format!(" ({c})")).unwrap_or_default();
        let detail = format!(
            "No address in its status: the controller{by} has not taken it, or doesn't report addresses. \
             Check that the controller runs, and the Ingress's events."
        );
        steps.push(Step::new(SECTION, "Address", State::Broken, detail).with_command(describe));
    } else {
        steps.push(Step::new(SECTION, "Address", State::Ok, addresses.join(", ")).with_command(describe));
    }
}

/// The certificate of each TLS entry of an Ingress.
fn ingress_tls(context: &str, ing: &Value, world: &World, now: jiff::Timestamp, steps: &mut Vec<Step>) {
    let ns = s(ing, "/metadata/namespace");
    for t in arr(ing, "/spec/tls") {
        let hosts: Vec<&str> = arr(t, "/hosts").iter().filter_map(Value::as_str).collect();
        if let Some(name) = t["secretName"].as_str().filter(|n| !n.is_empty()) {
            steps.push(secret_step(context, &(ns.to_string(), name.to_string()), &hosts, world, now));
        } else {
            let detail = format!(
                "No secretName: the controller serves its default certificate for {}, which browsers usually reject.",
                if hosts.is_empty() { "these hosts".to_string() } else { hosts.join(", ") }
            );
            steps.push(Step::new("TLS", "Default certificate", State::Warn, detail));
        }
    }
}

/// A TLS Secret: it exists, and its certificate is valid now and covers `hosts`.
fn secret_step(context: &str, key: &Key, hosts: &[&str], world: &World, now: jiff::Timestamp) -> Step {
    let (ns, name) = (key.0.as_str(), key.1.as_str());
    let command = format!(
        "{} | base64 -d | openssl x509 -noout -subject -ext subjectAltName -dates",
        kubectl(context, Some(ns), &format!("get secret {} -o jsonpath='{{.data.tls\\.crt}}'", shell_word(name)))
    );
    let read = world.secrets.get(key);
    let (state, detail) = match read {
        None => (State::Unknown, "Not read.".to_string()),
        Some(Err(e)) => (State::Unknown, format!("Could not read it: {e}")),
        Some(Ok(None)) => (
            State::Broken,
            format!(
                "No Secret {name} in {ns}: the controller serves its default certificate. If cert-manager makes \
                 it, see why its Certificate is not ready."
            ),
        ),
        Some(Ok(Some(secret))) => cert_verdict(secret, hosts, now),
    };
    let items = match read {
        Some(Ok(Some(_))) => vec![Item {
            text: format!("Secret {name}"),
            link: Some(Link { kind: "secrets", namespace: Some(ns.to_string()), name: name.to_string() }),
        }],
        _ => Vec::new(),
    };
    Step::new("TLS", format!("Secret {name}"), state, detail).with_command(command).with_items(items)
}

/// What a TLS Secret's certificate says for `hosts`: valid now, and naming each of them.
fn cert_verdict(secret: &Value, hosts: &[&str], now: jiff::Timestamp) -> (State, String) {
    let Some(cert) = crate::certs::of_tls_secret(secret) else {
        return (State::Broken, "No certificate in its tls.crt.".into());
    };
    let date = |t: Option<jiff::Timestamp>| t.map_or_else(String::new, |t| t.strftime("%Y-%m-%d").to_string());
    if cert.not_yet_valid(now) {
        return (State::Broken, format!("Not valid until {}: clients reject it.", date(cert.not_before)));
    }
    if let Some(end) = cert.not_after
        && end < now
    {
        let ago = match crate::certs::days_ago(end, now) {
            0 => "today".to_string(),
            1 => "1 day ago".to_string(),
            n => format!("{n} days ago"),
        };
        return (State::Broken, format!("Expired {ago} ({}): clients reject it.", date(Some(end))));
    }
    let sans: Vec<String> = cert.sans.iter().map(|n| n.to_lowercase()).collect();
    let names: Vec<&str> = sans.iter().map(String::as_str).collect();
    let uncovered: Vec<&str> =
        hosts.iter().copied().filter(|h| !crate::forwards::tls_covers(&names, &h.to_lowercase())).collect();
    if !uncovered.is_empty() {
        let has = if names.is_empty() { "no host name".to_string() } else { names.join(", ") };
        return (State::Broken, format!("It does not cover {}: it names {has}.", uncovered.join(", ")));
    }
    let until = date(cert.not_after);
    let days = cert.not_after.map_or(i64::MAX, |t| (t.as_second() - now.as_second()).div_euclid(86400));
    if days < 14 {
        return (State::Warn, format!("Expires in {days} day(s), on {until}."));
    }
    let covers = if hosts.is_empty() { String::new() } else { format!("; covers {}", hosts.join(", ")) };
    (State::Ok, format!("Valid until {until}{covers}."))
}

/// A route's `parentRefs` that are Gateways, with where each Gateway is.
fn route_gateways(route: &Value) -> Vec<(&Value, Key)> {
    let ns = s(route, "/metadata/namespace");
    arr(route, "/spec/parentRefs")
        .iter()
        .filter(|p| is_gateway(p))
        .map(|p| (p, (p["namespace"].as_str().unwrap_or(ns).to_string(), s(p, "/name").to_string())))
        .collect()
}

fn is_gateway(parent: &Value) -> bool {
    parent["group"].as_str().unwrap_or("gateway.networking.k8s.io") == "gateway.networking.k8s.io"
        && parent["kind"].as_str().unwrap_or("Gateway") == "Gateway"
}

/// The listeners of a Gateway that a `parentRef` attaches to (by `sectionName` and `port`).
fn attached_listeners<'a>(gateway: &'a Value, parent: &Value) -> Vec<&'a Value> {
    let (section, port) = (parent["sectionName"].as_str(), parent["port"].as_i64());
    arr(gateway, "/spec/listeners")
        .iter()
        .filter(|l| section.is_none_or(|n| l["name"].as_str() == Some(n)))
        .filter(|l| port.is_none_or(|p| l["port"].as_i64() == Some(p)))
        .collect()
}

/// The Secrets of a listener that ends TLS (HTTPS): its `certificateRefs`.
fn listener_secrets(gateway: &Value, listener: &Value) -> Vec<Key> {
    if listener["protocol"].as_str() != Some("HTTPS") {
        return Vec::new();
    }
    let ns = s(gateway, "/metadata/namespace");
    arr(listener, "/tls/certificateRefs")
        .iter()
        .filter(|r| r["group"].as_str().unwrap_or("").is_empty() && r["kind"].as_str().unwrap_or("Secret") == "Secret")
        .map(|r| (r["namespace"].as_str().unwrap_or(ns).to_string(), s(r, "/name").to_string()))
        .collect()
}

/// The TLS Secrets of the HTTPS listeners a route attaches to.
fn route_secrets(route: &Value, gateways: &BTreeMap<Key, Fetched<Option<Value>>>) -> Vec<Key> {
    let mut out = Vec::new();
    for (parent, key) in route_gateways(route) {
        if let Some(Ok(Some(gw))) = gateways.get(&key) {
            for l in attached_listeners(gw, parent) {
                out.extend(listener_secrets(gw, l));
            }
        }
    }
    out
}

/// Whether a listener's `hostname` lets `host` through: any without one, else the same name
/// or one under its `*.` wildcard.
fn listener_admits(listener: Option<&str>, host: &str) -> bool {
    listener.is_none_or(|l| {
        l == host || l.strip_prefix('*').is_some_and(|suffix| host.len() > suffix.len() && host.ends_with(suffix))
    })
}

/// The route's Gateways: each one there, programmed and with an address, and the route
/// accepted on it with its references resolved.
fn route_steps(context: &str, route: &Value, world: &World, steps: &mut Vec<Step>) {
    const SECTION: &str = "Route";
    let (ns, name) = (s(route, "/metadata/namespace"), s(route, "/metadata/name"));
    let parents = arr(route, "/spec/parentRefs");
    if parents.is_empty() {
        let detail = "No parentRefs: it is attached to no Gateway, so none serves it.";
        steps.push(Step::new(SECTION, "Gateway", State::Broken, detail));
    }
    let route_cmd = kubectl(context, Some(ns), &format!("describe httproute {}", shell_word(name)));
    for parent in parents {
        if !is_gateway(parent) {
            let what = format!("{} {}", parent["kind"].as_str().unwrap_or("Gateway"), s(parent, "/name"));
            steps.push(Step::new(SECTION, what, State::Unknown, "Not a Gateway: not followed."));
            continue;
        }
        let key = (parent["namespace"].as_str().unwrap_or(ns).to_string(), s(parent, "/name").to_string());
        let (gw_ns, gw_name) = (key.0.as_str(), key.1.as_str());
        let mut title = if gw_ns == ns { format!("Gateway {gw_name}") } else { format!("Gateway {gw_ns}/{gw_name}") };
        if let Some(section) = parent["sectionName"].as_str() {
            title.push_str(&format!(", listener {section}"));
        }
        let gw_cmd = kubectl(context, Some(gw_ns), &format!("describe gateway {}", shell_word(gw_name)));
        let gw = match world.gateways.get(&key) {
            None => {
                steps.push(Step::new(SECTION, title, State::Unknown, "Not read."));
                continue;
            }
            Some(Err(e)) => {
                steps.push(Step::new(SECTION, title, State::Unknown, format!("Could not read it: {e}")));
                continue;
            }
            Some(Ok(None)) => {
                let detail = format!("No Gateway {gw_name} in {gw_ns}.");
                steps.push(Step::new(SECTION, title, State::Broken, detail).with_command(gw_cmd));
                continue;
            }
            Some(Ok(Some(gw))) => gw,
        };
        let programmed = find_condition(arr(gw, "/status/conditions"), "Programmed");
        let addresses: Vec<&str> = arr(gw, "/status/addresses").iter().filter_map(|a| a["value"].as_str()).collect();
        let (state, detail) = match programmed {
            Some(c) if c["status"] == "False" => (State::Broken, format!("Not programmed: {}", condition_text(c))),
            _ if addresses.is_empty() => (
                State::Broken,
                "No address in its status: its controller has not set it up. Check that the controller of its \
                 GatewayClass runs."
                    .to_string(),
            ),
            _ => (State::Ok, format!("Address {}.", addresses.join(", "))),
        };
        steps.push(Step::new(SECTION, title.clone(), state, detail).with_command(gw_cmd));
        let status = arr(route, "/status/parents")
            .iter()
            .find(|st| crate::overview::same_parent(parent, &st["parentRef"], Some(ns)));
        let (state, detail) = match status {
            None => (State::Warn, "No controller has reported on the route for this Gateway yet.".to_string()),
            Some(st) => {
                let conditions = arr(st, "/conditions");
                let failed = |ty: &str| find_condition(conditions, ty).filter(|c| c["status"] == "False");
                if let Some(c) = failed("Accepted") {
                    (State::Broken, format!("Not accepted: {}", condition_text(c)))
                } else if let Some(c) = failed("ResolvedRefs") {
                    (State::Broken, format!("References not resolved: {}", condition_text(c)))
                } else {
                    (State::Ok, "Accepted, its references resolved.".to_string())
                }
            }
        };
        steps.push(Step::new(SECTION, format!("Accepted by {title}"), state, detail).with_command(route_cmd.clone()));
    }
}

/// The certificates of the HTTPS listeners a route attaches to, for the route's host names.
fn route_tls(context: &str, route: &Value, world: &World, now: jiff::Timestamp, steps: &mut Vec<Step>) {
    let hostnames: Vec<&str> = arr(route, "/spec/hostnames").iter().filter_map(Value::as_str).collect();
    let mut seen: Vec<(Key, Vec<&str>)> = Vec::new();
    for (parent, key) in route_gateways(route) {
        let Some(Ok(Some(gw))) = world.gateways.get(&key) else { continue };
        for l in attached_listeners(gw, parent) {
            let listener = l["hostname"].as_str();
            let hosts: Vec<&str> = if hostnames.is_empty() {
                listener.into_iter().collect()
            } else {
                hostnames.iter().copied().filter(|h| listener_admits(listener, h)).collect()
            };
            for secret in listener_secrets(gw, l) {
                if seen.iter().any(|(k, h)| *k == secret && *h == hosts) {
                    continue;
                }
                steps.push(secret_step(context, &secret, &hosts, world, now));
                seen.push((secret, hosts.clone()));
            }
        }
    }
}

// MARK: From this machine

/// A request to an Ingress host from this machine: where the name resolves, and what answered.
#[derive(Clone, Debug)]
pub struct Probe {
    pub url: String,
    /// The addresses the host resolves to here, or why it doesn't.
    pub resolved: Result<Vec<std::net::IpAddr>, String>,
    /// Whether one of them is the Ingress's address; None when that can't be said.
    pub matches: Option<bool>,
    /// The HTTP status and how long it took, or what failed (TLS, refused, timed out).
    pub answer: Result<(u16, Duration), String>,
}

impl Probe {
    /// Where the host resolves, in words.
    pub fn dns_text(&self) -> (State, String) {
        match &self.resolved {
            Err(e) => (State::Broken, format!("Does not resolve here: {e}")),
            Ok(ips) => {
                let ips = ips.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                match self.matches {
                    Some(true) => (State::Ok, format!("Resolves to {ips}, the Ingress's address.")),
                    Some(false) => (
                        State::Warn,
                        format!(
                            "Resolves to {ips}, not the Ingress's address (a CDN or a load balancer in front, or \
                             another DNS view, can explain it)."
                        ),
                    ),
                    None => (State::Ok, format!("Resolves to {ips}.")),
                }
            }
        }
    }

    /// What answered, in words: a 5xx breaks, a 4xx may be the app's own answer.
    pub fn answer_text(&self) -> (State, String) {
        match &self.answer {
            Err(e) => (State::Broken, e.clone()),
            Ok((code, took)) => {
                let state = match code {
                    500.. => State::Broken,
                    400.. => State::Warn,
                    _ => State::Ok,
                };
                (state, format!("HTTP {code} in {} ms.", took.as_millis()))
            }
        }
    }
}

/// The pages of an Ingress to try from this machine: each plain host of its rules (not a
/// wildcard), over https when a TLS entry covers it. (host, URL) pairs.
pub fn probe_urls(ing: &Value) -> Vec<(String, String)> {
    let tls: Vec<Vec<&str>> =
        arr(ing, "/spec/tls").iter().map(|t| arr(t, "/hosts").iter().filter_map(Value::as_str).collect()).collect();
    let mut out: Vec<(String, String)> = Vec::new();
    for host in arr(ing, "/spec/rules").iter().filter_map(|r| r["host"].as_str()) {
        let https = tls.iter().any(|hosts| crate::forwards::tls_covers(hosts, host));
        if let Some(url) = crate::forwards::host_url(host, "/", https)
            && !out.iter().any(|(_, u)| *u == url)
        {
            out.push((host.to_string(), url));
        }
    }
    out
}

/// Resolve each host of an Ingress here and send it a GET (redirects not followed).
pub async fn probe_all(ing: Value) -> Vec<Probe> {
    let addresses = lb_addresses(&ing);
    futures::future::join_all(probe_urls(&ing).into_iter().map(|(host, url)| {
        let addresses = addresses.clone();
        async move {
            let done = k8s::run(probe(host, url.clone(), addresses)).await;
            done.unwrap_or_else(|e| Probe {
                url,
                resolved: Err(format!("{e:#}")),
                matches: None,
                answer: Err(format!("{e:#}")),
            })
        }
    }))
    .await
}

async fn probe(host: String, url: String, addresses: Vec<String>) -> anyhow::Result<Probe> {
    let resolved = resolve(&host).await;
    // The Ingress's address may be a name too (a cloud load balancer's).
    let mut expected = Vec::new();
    for a in &addresses {
        match a.parse::<std::net::IpAddr>() {
            Ok(ip) => expected.push(ip),
            Err(_) => expected.extend(resolve(a).await.unwrap_or_default()),
        }
    }
    let matches = match &resolved {
        Ok(ips) if !expected.is_empty() => Some(ips.iter().any(|ip| expected.contains(ip))),
        _ => None,
    };
    let answer = request(&url).await;
    Ok(Probe { url, resolved, matches, answer })
}

async fn resolve(host: &str) -> Result<Vec<std::net::IpAddr>, String> {
    let lookup = tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host((host, 443)))
        .await
        .map_err(|_| "no answer after 5 s".to_string())?;
    let mut ips: Vec<std::net::IpAddr> = lookup.map_err(|e| e.to_string())?.map(|a| a.ip()).collect();
    ips.sort();
    ips.dedup();
    Ok(ips)
}

/// GET `url`: the status and how long it took. The body is not read.
async fn request(url: &str) -> Result<(u16, Duration), String> {
    use bytes::Bytes;
    use http_body_util::Full;
    let fetch = async {
        let https =
            hyper_rustls::HttpsConnectorBuilder::new().with_native_roots()?.https_or_http().enable_http1().build();
        let client: hyper_util::client::legacy::Client<_, Full<Bytes>> =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);
        let req = http::Request::get(url).header("user-agent", "recouvrance").body(Full::new(Bytes::new()))?;
        let started = std::time::Instant::now();
        let resp = client.request(req).await?;
        anyhow::Ok((resp.status().as_u16(), started.elapsed()))
    };
    match tokio::time::timeout(Duration::from_secs(10), fetch).await {
        Err(_) => Err("no answer after 10 s".into()),
        Ok(answer) => answer.map_err(|e| format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const CTX: &str = "kind-test";
    // Self-signed, SAN test.example and 10.0.0.1, valid ten years.
    const PEM: &str = include_str!("../assets/test-cert.pem");

    fn key(name: &str) -> Key {
        ("web".into(), name.into())
    }

    fn service(ports: Value) -> Value {
        json!({"metadata": {"name": "web", "namespace": "web"}, "spec": {"selector": {"app": "web"}, "ports": ports}})
    }

    fn http_port() -> Value {
        json!([{"name": "http", "port": 80, "targetPort": "http"}])
    }

    /// A running pod of `web`, its container declaring `ports`, ready or failing its probe.
    fn pod(name: &str, ready: bool, ports: Value) -> Value {
        let status = if ready { "True" } else { "False" };
        json!({
            "metadata": {"name": name, "namespace": "web", "labels": {"app": "web"}},
            "spec": {"nodeName": "n1", "containers": [
                {"name": "app", "ports": ports, "readinessProbe": {"httpGet": {"port": 8080}}}]},
            "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": status}],
                "containerStatuses": [{"name": "app", "ready": ready, "started": true, "state": {"running": {}}}]},
        })
    }

    fn http_on_8080() -> Value {
        json!([{"name": "http", "containerPort": 8080}])
    }

    /// An EndpointSlice of `web` carrying port `http`: (address, pod, ready).
    fn slice(endpoints: &[(&str, &str, bool)]) -> Value {
        let endpoints: Vec<Value> = endpoints
            .iter()
            .map(|(ip, pod, ready)| {
                json!({"addresses": [ip], "conditions": {"ready": ready}, "targetRef": {"kind": "Pod", "name": pod}})
            })
            .collect();
        json!({"ports": [{"name": "http", "port": 8080}], "endpoints": endpoints})
    }

    fn world(svc: &Value, pods: Vec<Value>, slices: Vec<Value>) -> World {
        let mut w = World::default();
        w.services.insert(key("web"), Ok(Some(svc.clone())));
        w.pods.insert(key("web"), Ok(pods));
        w.slices.insert(key("web"), Ok(slices));
        w
    }

    fn healthy_service() -> (Value, World) {
        let svc = service(http_port());
        let w = world(&svc, vec![pod("web-1", true, http_on_8080())], vec![slice(&[("10.0.0.5", "web-1", true)])]);
        (svc, w)
    }

    fn first_break(steps: &[Step]) -> Option<&Step> {
        steps.iter().find(|s| s.state == State::Broken)
    }

    fn tls_secret() -> Value {
        use base64::Engine as _;
        json!({"data": {"tls.crt": base64::engine::general_purpose::STANDARD.encode(PEM)}})
    }

    /// A day into the test certificate's validity.
    fn valid_now() -> jiff::Timestamp {
        crate::certs::parse_pem(PEM.as_bytes())[0].not_before.unwrap() + jiff::SignedDuration::from_hours(24)
    }

    fn ingress(port: Value) -> Value {
        json!({
            "metadata": {"name": "site", "namespace": "web"},
            "spec": {
                "ingressClassName": "nginx",
                "tls": [{"hosts": ["test.example"], "secretName": "site-tls"}],
                "rules": [{"host": "test.example", "http": {"paths": [
                    {"path": "/", "pathType": "Prefix", "backend": {"service": {"name": "web", "port": port}}}]}}],
            },
            "status": {"loadBalancer": {"ingress": [{"ip": "203.0.113.7"}]}},
        })
    }

    /// Everything an Ingress to the healthy `web` Service needs.
    fn ingress_world() -> World {
        let (_, mut w) = healthy_service();
        let nginx = json!({"metadata": {"name": "nginx"}, "spec": {"controller": "k8s.io/ingress-nginx"}});
        w.classes = Some(Ok(vec![nginx]));
        w.secrets.insert(key("site-tls"), Ok(Some(tls_secret())));
        w
    }

    #[test]
    fn a_healthy_service_checks_out() {
        let (svc, w) = healthy_service();
        let steps = check(CTX, Target::Service, &svc, &w, jiff::Timestamp::now());
        assert!(steps.iter().all(|s| s.state == State::Ok), "{steps:#?}");
        assert_eq!(verdict(&steps), (State::Ok, "Every step checks out.".to_string()));
    }

    #[test]
    fn a_selector_that_matches_no_pods_breaks() {
        let svc = service(http_port());
        let steps = check(CTX, Target::Service, &svc, &world(&svc, vec![], vec![]), jiff::Timestamp::now());
        let broken = first_break(&steps).unwrap();
        assert_eq!(broken.title, "Selects no pods");
        assert_eq!(broken.command.as_deref(), Some("kubectl --context kind-test -n web get pods -l app=web -o wide"));
    }

    #[test]
    fn a_named_target_port_must_be_declared_by_the_pods() {
        let svc = service(http_port());
        let named_web = json!([{"name": "web", "containerPort": 8080}]);
        let w = world(&svc, vec![pod("web-1", true, named_web.clone())], vec![]);
        let steps = check(CTX, Target::Service, &svc, &w, jiff::Timestamp::now());
        let broken = first_break(&steps).unwrap();
        assert_eq!(broken.title, "Target port of http:80");
        assert_eq!(broken.detail, "No container of its pods has a port named http (they declare: 8080/web).");
        // Some pods only: those get no traffic on it, which is said, not a break.
        let w = world(
            &svc,
            vec![pod("web-1", true, http_on_8080()), pod("web-2", true, named_web)],
            vec![slice(&[("10.0.0.5", "web-1", true)])],
        );
        let steps = check(CTX, Target::Service, &svc, &w, jiff::Timestamp::now());
        let target = steps.iter().find(|s| s.title == "Target port of http:80").unwrap();
        assert_eq!(target.state, State::Warn);
        assert_eq!(target.items[0].link.as_ref().unwrap().name, "web-2");
        assert!(first_break(&steps).is_none());
    }

    #[test]
    fn a_numbered_target_port_need_not_be_declared() {
        let svc = service(json!([{"port": 80, "targetPort": 8080}]));
        let slices = vec![json!({"ports": [{"name": "", "port": 8080}], "endpoints": [{"addresses": ["10.0.0.5"]}]})];
        let w = world(&svc, vec![pod("web-1", true, json!([]))], slices.clone());
        let steps = check(CTX, Target::Service, &svc, &w, jiff::Timestamp::now());
        assert!(steps.iter().all(|s| s.state == State::Ok), "{steps:#?}");
        // Declaring others but not this one is suspect.
        let w = world(&svc, vec![pod("web-1", true, json!([{"containerPort": 9090}]))], slices);
        let steps = check(CTX, Target::Service, &svc, &w, jiff::Timestamp::now());
        assert_eq!(steps.iter().find(|s| s.title == "Target port of 80").unwrap().state, State::Warn);
    }

    #[test]
    fn no_ready_endpoint_breaks_and_says_why_each_pod_is_not_ready() {
        let svc = service(http_port());
        let w = world(&svc, vec![pod("web-1", false, http_on_8080())], vec![slice(&[("10.0.0.5", "web-1", false)])]);
        let steps = check(CTX, Target::Service, &svc, &w, jiff::Timestamp::now());
        let broken = first_break(&steps).unwrap();
        assert_eq!(broken.title, "Endpoints");
        // Once, though both the pod and its slice say it is not ready.
        assert_eq!(broken.items.len(), 1);
        assert_eq!(broken.items[0].text, "web-1: app: readiness probe failing");
        let link = broken.items[0].link.as_ref().unwrap();
        assert_eq!((link.kind, link.namespace.as_deref(), link.name.as_str()), ("pods", Some("web"), "web-1"));
        // One ready and one not: requests get through, the other pod is named.
        let w = world(
            &svc,
            vec![pod("web-1", true, http_on_8080()), pod("web-2", false, http_on_8080())],
            vec![slice(&[("10.0.0.5", "web-1", true), ("10.0.0.6", "web-2", false)])],
        );
        let steps = check(CTX, Target::Service, &svc, &w, jiff::Timestamp::now());
        let endpoints = steps.iter().find(|s| s.title == "Endpoints").unwrap();
        assert_eq!(endpoints.state, State::Warn);
        assert!(endpoints.detail.starts_with("1 ready"), "{}", endpoints.detail);
    }

    #[test]
    fn endpoints_count_per_port() {
        // The slice has no port `metrics`: its named port did not resolve on the pod.
        let svc = service(json!([{"name": "http", "port": 80, "targetPort": "http"},
            {"name": "metrics", "port": 9090, "targetPort": 9090}]));
        let w = world(&svc, vec![pod("web-1", true, http_on_8080())], vec![slice(&[("10.0.0.5", "web-1", true)])]);
        let steps = check(CTX, Target::Service, &svc, &w, jiff::Timestamp::now());
        let endpoints = steps.iter().find(|s| s.title == "Endpoints").unwrap();
        assert_eq!(endpoints.state, State::Broken);
        assert!(endpoints.detail.contains("on metrics:9090"), "{}", endpoints.detail);
    }

    #[test]
    fn reads_that_fail_are_not_checked_rather_than_broken() {
        let svc = service(http_port());
        let mut w = world(&svc, vec![], vec![]);
        w.pods.insert(key("web"), Err("forbidden".into()));
        w.slices.insert(key("web"), Err(NOT_ALLOWED.into()));
        let steps = check(CTX, Target::Service, &svc, &w, jiff::Timestamp::now());
        assert!(first_break(&steps).is_none(), "{steps:#?}");
        assert_eq!(steps.iter().filter(|s| s.state == State::Unknown).count(), 2);
        assert_eq!(verdict(&steps).0, State::Unknown);
        assert!(to_text(&steps).contains(NOT_ALLOWED));
    }

    #[test]
    fn a_service_without_selector_goes_by_its_slices() {
        let svc = json!({"metadata": {"name": "web", "namespace": "web"}, "spec": {"ports": [{"port": 5432}]}});
        let mut w = World::default();
        let manual = json!({"ports": [{"port": 5432}], "endpoints": [{"addresses": ["192.0.2.1"]}]});
        w.slices.insert(key("web"), Ok(vec![manual]));
        let steps = check(CTX, Target::Service, &svc, &w, jiff::Timestamp::now());
        assert!(steps.iter().all(|s| s.state == State::Ok), "{steps:#?}");
        w.slices.insert(key("web"), Ok(vec![]));
        let steps = check(CTX, Target::Service, &svc, &w, jiff::Timestamp::now());
        assert_eq!(first_break(&steps).unwrap().title, "Endpoints");
    }

    #[test]
    fn an_external_name_has_nothing_behind_it() {
        let svc = json!({"metadata": {"name": "web", "namespace": "web"},
            "spec": {"type": "ExternalName", "externalName": "db.example.com"}});
        let steps = check(CTX, Target::Service, &svc, &World::default(), jiff::Timestamp::now());
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].state, State::Ok);
    }

    #[test]
    fn a_healthy_ingress_checks_out() {
        let ing = ingress(json!({"name": "http"}));
        let steps = check(CTX, Target::Ingress, &ing, &ingress_world(), valid_now());
        assert!(steps.iter().all(|s| s.state == State::Ok), "{steps:#?}");
        let sections: Vec<&str> = steps.iter().map(|s| s.section.as_str()).collect();
        assert_eq!(sections, ["Ingress", "Ingress", "Backends", "Service web", "Service web", "Service web", "TLS"]);
    }

    #[test]
    fn a_backend_port_the_service_lacks_breaks() {
        let ing = ingress(json!({"number": 81}));
        let steps = check(CTX, Target::Ingress, &ing, &ingress_world(), valid_now());
        let broken = first_break(&steps).unwrap();
        assert_eq!((broken.section.as_str(), broken.title.as_str()), ("Backends", "web:81"));
        assert!(broken.detail.contains("has no port 81 (its ports: http:80)"), "{}", broken.detail);
        // Its pods are not checked for a port no rule reaches.
        assert!(!steps.iter().any(|s| s.section == "Service web"));
        // By number or by name, the same port.
        let steps = check(CTX, Target::Ingress, &ingress(json!({"number": 80})), &ingress_world(), valid_now());
        assert!(first_break(&steps).is_none(), "{steps:#?}");
    }

    #[test]
    fn a_missing_backend_service_breaks() {
        let mut w = ingress_world();
        w.services.insert(key("web"), Ok(None));
        let steps = check(CTX, Target::Ingress, &ingress(json!({"number": 80})), &w, valid_now());
        assert_eq!(first_break(&steps).unwrap().detail, "No Service web in web. From test.example/.");
    }

    #[test]
    fn an_ingress_class_without_controller_breaks_unless_one_took_it() {
        let mut w = ingress_world();
        w.classes = Some(Ok(vec![]));
        let mut ing = ingress(json!({"number": 80}));
        let steps = check(CTX, Target::Ingress, &ing, &w, valid_now());
        // It has an address: some controller took it after all.
        assert_eq!(steps[0].state, State::Warn);
        ing["status"] = json!({});
        let steps = check(CTX, Target::Ingress, &ing, &w, valid_now());
        assert_eq!(first_break(&steps).unwrap().title, "Class nginx");
        assert_eq!(steps[1].state, State::Broken);
        // No class at all: the default one serves it.
        ing["spec"].as_object_mut().unwrap().remove("ingressClassName");
        let default = json!({"metadata": {"name": "traefik",
            "annotations": {"ingressclass.kubernetes.io/is-default-class": "true"}},
            "spec": {"controller": "traefik.io/ingress-controller"}});
        w.classes = Some(Ok(vec![default]));
        let steps = check(CTX, Target::Ingress, &ing, &w, valid_now());
        assert_eq!((steps[0].title.as_str(), steps[0].state), ("Class traefik (the default)", State::Ok));
        let broken = first_break(&steps).unwrap();
        assert_eq!(broken.title, "Address");
        assert!(broken.detail.contains("traefik.io/ingress-controller"), "{}", broken.detail);
    }

    #[test]
    fn a_missing_tls_secret_breaks() {
        let mut w = ingress_world();
        w.secrets.insert(key("site-tls"), Ok(None));
        let steps = check(CTX, Target::Ingress, &ingress(json!({"number": 80})), &w, valid_now());
        let broken = first_break(&steps).unwrap();
        assert_eq!((broken.section.as_str(), broken.title.as_str()), ("TLS", "Secret site-tls"));
        assert!(broken.detail.starts_with("No Secret site-tls in web"));
        assert!(broken.items.is_empty());
    }

    #[test]
    fn certificates_must_be_valid_now_and_cover_the_hosts() {
        let cert = crate::certs::parse_pem(PEM.as_bytes()).remove(0);
        let (start, end) = (cert.not_before.unwrap(), cert.not_after.unwrap());
        let hours = jiff::SignedDuration::from_hours;
        fn of_test_cert(hosts: &[&str], now: jiff::Timestamp) -> (State, String) {
            cert_verdict(&tls_secret(), hosts, now)
        }
        assert_eq!(of_test_cert(&["test.example"], start + hours(24)).0, State::Ok);
        // Expired: the days that went by, rounded down (3 hours is today, not "-1 days").
        let (state, detail) = of_test_cert(&["test.example"], end + hours(3));
        assert_eq!(state, State::Broken);
        assert!(detail.starts_with("Expired today"), "{detail}");
        assert!(of_test_cert(&["test.example"], end + hours(24 * 3 + 1)).1.starts_with("Expired 3 days ago"));
        assert_eq!(of_test_cert(&["test.example"], start - hours(1)).0, State::Broken);
        // Expiring soon is a warning.
        assert_eq!(of_test_cert(&["test.example"], end - hours(24 * 5)).0, State::Warn);
        let (state, detail) = of_test_cert(&["test.example", "other.example"], start + hours(24));
        assert_eq!(state, State::Broken);
        assert_eq!(detail, "It does not cover other.example: it names test.example, 10.0.0.1.");
        // Names compare without case.
        assert_eq!(of_test_cert(&["Test.Example"], start + hours(24)).0, State::Ok);
        assert_eq!(cert_verdict(&json!({"data": {}}), &[], start).0, State::Broken);
    }

    #[test]
    fn the_expired_tls_secret_of_an_ingress_breaks_it() {
        let end = crate::certs::parse_pem(PEM.as_bytes())[0].not_after.unwrap();
        let now = end + jiff::SignedDuration::from_hours(48);
        let steps = check(CTX, Target::Ingress, &ingress(json!({"number": 80})), &ingress_world(), now);
        let broken = first_break(&steps).unwrap();
        assert_eq!(broken.title, "Secret site-tls");
        assert!(broken.detail.starts_with("Expired 2 days ago"), "{}", broken.detail);
        assert!(broken.command.as_deref().unwrap().contains("jsonpath='{.data.tls\\.crt}' | base64 -d"));
    }

    #[test]
    fn an_ingress_without_backends_sends_nowhere() {
        let ing = json!({"metadata": {"name": "site", "namespace": "web"}, "spec": {"ingressClassName": "nginx"},
            "status": {"loadBalancer": {"ingress": [{"hostname": "lb.example"}]}}});
        let steps = check(CTX, Target::Ingress, &ing, &ingress_world(), valid_now());
        assert_eq!(first_break(&steps).unwrap().title, "Rules");
    }

    fn route() -> Value {
        json!({
            "apiVersion": "gateway.networking.k8s.io/v1",
            "metadata": {"name": "site", "namespace": "web"},
            "spec": {"parentRefs": [{"name": "gw", "namespace": "infra", "sectionName": "https"}],
                "hostnames": ["test.example"],
                "rules": [{"backendRefs": [{"name": "web", "port": 80}]}]},
            "status": {"parents": [{"parentRef": {"group": "gateway.networking.k8s.io", "kind": "Gateway",
                "namespace": "infra", "name": "gw", "sectionName": "https"},
                "conditions": [{"type": "Accepted", "status": "True"}, {"type": "ResolvedRefs", "status": "True"}]}]},
        })
    }

    fn gateway() -> Value {
        json!({
            "metadata": {"name": "gw", "namespace": "infra"},
            "spec": {"listeners": [
                {"name": "http", "protocol": "HTTP", "port": 80},
                {"name": "https", "protocol": "HTTPS", "port": 443, "hostname": "*.example",
                    "tls": {"certificateRefs": [{"name": "wildcard"}]}}]},
            "status": {"addresses": [{"value": "198.51.100.4"}],
                "conditions": [{"type": "Programmed", "status": "True"}]},
        })
    }

    fn route_world() -> World {
        let (_, mut w) = healthy_service();
        w.gateways.insert(("infra".into(), "gw".into()), Ok(Some(gateway())));
        w.secrets.insert(("infra".into(), "wildcard".into()), Ok(Some(tls_secret())));
        w
    }

    #[test]
    fn a_healthy_route_checks_out() {
        let steps = check(CTX, Target::HttpRoute, &route(), &route_world(), valid_now());
        assert!(steps.iter().all(|s| s.state == State::Ok), "{steps:#?}");
        assert_eq!(steps.last().unwrap().title, "Secret wildcard");
        // The listener's Secret is read in the Gateway's namespace.
        assert_eq!(route_secrets(&route(), &route_world().gateways), [("infra".to_string(), "wildcard".to_string())]);
        assert_eq!(gateway_ar(&route()).api_version, "gateway.networking.k8s.io/v1");
    }

    #[test]
    fn a_route_breaks_at_its_gateway_or_its_status() {
        let mut w = route_world();
        w.gateways.insert(("infra".into(), "gw".into()), Ok(None));
        let steps = check(CTX, Target::HttpRoute, &route(), &w, valid_now());
        assert_eq!(first_break(&steps).unwrap().detail, "No Gateway gw in infra.");
        let mut r = route();
        r["status"]["parents"][0]["conditions"][0] =
            json!({"type": "Accepted", "status": "False", "reason": "NotAllowedByListeners", "message": ""});
        let steps = check(CTX, Target::HttpRoute, &r, &route_world(), valid_now());
        let broken = first_break(&steps).unwrap();
        assert_eq!(broken.title, "Accepted by Gateway infra/gw, listener https");
        assert_eq!(broken.detail, "Not accepted: NotAllowedByListeners");
        // No controller has said anything yet: a warning, not a break.
        let mut r = route();
        r["status"] = json!({});
        let steps = check(CTX, Target::HttpRoute, &r, &route_world(), valid_now());
        assert!(first_break(&steps).is_none());
        assert_eq!(verdict(&steps).0, State::Warn);
    }

    #[test]
    fn listener_host_names() {
        assert!(listener_admits(None, "a.example"));
        assert!(listener_admits(Some("*.example"), "a.example"));
        assert!(!listener_admits(Some("*.example"), "example"));
        assert!(!listener_admits(Some("a.example"), "b.example"));
    }

    #[test]
    fn why_pods_are_not_ready() {
        let unscheduled = json!({"status": {"phase": "Pending", "conditions": [{"type": "PodScheduled",
            "status": "False", "message": "0/3 nodes are available: 3 Insufficient cpu."}]}});
        assert_eq!(
            not_ready_reason(&unscheduled).unwrap(),
            "pending, not scheduled: 0/3 nodes are available: 3 Insufficient cpu."
        );
        let pulling = json!({"spec": {"nodeName": "n1"}, "status": {"phase": "Pending", "containerStatuses": [
            {"name": "app", "ready": false, "state": {"waiting": {"reason": "ImagePullBackOff"}}}]}});
        assert_eq!(not_ready_reason(&pulling).unwrap(), "pending (ImagePullBackOff)");
        let deleted =
            json!({"metadata": {"deletionTimestamp": "2026-01-01T00:00:00Z"}, "status": {"phase": "Running"}});
        assert_eq!(not_ready_reason(&deleted).unwrap(), "terminating");
        let crashing = json!({"status": {"phase": "Running", "containerStatuses": [{"name": "app", "ready": false,
            "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}});
        assert_eq!(not_ready_reason(&crashing).unwrap(), "app: CrashLoopBackOff");
        let gated = json!({"spec": {"readinessGates": [{"conditionType": "example.com/lb"}]},
            "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "False"}],
                "containerStatuses": [{"name": "app", "ready": true}]}});
        assert_eq!(not_ready_reason(&gated).unwrap(), "readiness gate example.com/lb not met");
        assert_eq!(not_ready_reason(&pod("web-1", true, json!([]))), None);
    }

    #[test]
    fn ingress_hosts_to_try_from_this_machine() {
        let ing = json!({"spec": {"tls": [{"hosts": ["test.example"]}], "rules": [
            {"host": "test.example"}, {"host": "*.wild.example"}, {"host": "plain.example"}, {"host": "test.example"}]}});
        let expected = [
            ("test.example".to_string(), "https://test.example/".to_string()),
            ("plain.example".to_string(), "http://plain.example/".to_string()),
        ];
        assert_eq!(probe_urls(&ing), expected);
    }

    #[test]
    fn what_a_request_from_this_machine_met() {
        let probe = |matches, answer| Probe {
            url: "https://test.example/".into(),
            resolved: Ok(vec!["203.0.113.7".parse().unwrap()]),
            matches,
            answer,
        };
        let took = Duration::from_millis(120);
        assert_eq!(probe(Some(true), Ok((200, took))).answer_text(), (State::Ok, "HTTP 200 in 120 ms.".to_string()));
        assert_eq!(probe(None, Ok((404, took))).answer_text().0, State::Warn);
        assert_eq!(probe(None, Ok((503, took))).answer_text().0, State::Broken);
        assert_eq!(probe(Some(false), Ok((200, took))).dns_text().0, State::Warn);
        assert_eq!(probe(Some(true), Ok((200, took))).dns_text().0, State::Ok);
        let failed = Probe { resolved: Err("no such host".into()), ..probe(None, Err("refused".into())) };
        assert_eq!(failed.dns_text().0, State::Broken);
        assert_eq!(failed.answer_text(), (State::Broken, "refused".to_string()));
    }

    #[test]
    fn agents_read_the_steps_with_commands_for_the_broken_ones() {
        let svc = service(http_port());
        let steps = check(CTX, Target::Service, &svc, &world(&svc, vec![], vec![]), jiff::Timestamp::now());
        let text = to_text(&steps);
        assert!(text.starts_with("Breaks at Service web › Selects no pods: No pod in web has the labels app=web."));
        assert!(text.contains("[BROKEN] Selects no pods"));
        assert!(text.contains("$ kubectl --context kind-test -n web get pods -l app=web -o wide"));
    }

    fn state() -> impl Strategy<Value = State> {
        prop_oneof![Just(State::Ok), Just(State::Warn), Just(State::Broken), Just(State::Unknown)]
    }

    proptest! {
        /// The verdict names the first step that breaks, whatever comes before or after it;
        /// with none broken, it never says the path breaks.
        #[test]
        fn the_verdict_is_the_first_break(states in proptest::collection::vec(state(), 0..12)) {
            let steps: Vec<Step> = states
                .iter()
                .enumerate()
                .map(|(i, st)| Step::new("S", format!("step {i}"), *st, format!("detail {i}")))
                .collect();
            let (state, text) = verdict(&steps);
            match states.iter().position(|s| *s == State::Broken) {
                Some(i) => {
                    prop_assert_eq!(state, State::Broken);
                    prop_assert_eq!(text, format!("Breaks at S › step {i}: detail {i}"));
                }
                None => {
                    prop_assert_ne!(state, State::Broken);
                    prop_assert_eq!(state == State::Ok, states.iter().all(|s| *s == State::Ok));
                }
            }
        }
    }
}
