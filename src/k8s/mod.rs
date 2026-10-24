//! Kubernetes backend. Everything here is read-only: it only issues GET, LIST
//! and WATCH requests (plus exec/port-forward, which the user starts explicitly).
//!
//! All network work runs on a dedicated tokio runtime; the UI awaits results
//! through `JoinHandle`s or channels, which work from any executor.

use std::future::Future;
use std::sync::LazyLock;

use anyhow::{Result, anyhow};
use kube::Client;
use kube::api::{Api, DynamicObject, ListParams};
use kube::discovery::ApiResource;

mod access;
mod config;
mod discovery;
mod exec;
#[cfg(feature = "dev")]
#[cfg(feature = "dev")]
pub(crate) mod experiments;
mod forward;
pub(crate) mod gate;
pub(crate) mod lists;
pub(crate) mod meter;
mod metrics;
pub(crate) mod names;
mod pods;
pub(crate) mod podspec;
mod sources;
mod table;
mod writes;

pub(crate) use access::*;
pub(crate) use config::*;
pub(crate) use discovery::*;
pub(crate) use exec::*;
pub(crate) use forward::*;
pub(crate) use metrics::*;
pub(crate) use pods::*;
pub(crate) use sources::*;
pub(crate) use table::*;
pub(crate) use writes::*;

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .thread_name("kube-io")
        .build()
        .expect("tokio runtime")
});

/// Run a future on the tokio runtime and await it from any executor.
pub async fn run<F, T>(f: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    RUNTIME.spawn(f).await.map_err(|e| anyhow!("task failed: {e}"))?
}

/// Like `run`, but dropping the future stops the work (a Stop button, a closed view).
pub async fn run_abortable<F, T>(f: F) -> Result<T>
where
    F: Future<Output = Result<T>> + Send + 'static,
    T: Send + 'static,
{
    let task = RUNTIME.spawn(f);
    let _guard = TaskGuard(task.abort_handle());
    task.await.map_err(|e| anyhow!("task failed: {e}"))?
}

/// Aborts the tokio task when dropped, so closing a view stops its stream.
pub struct TaskGuard(tokio::task::AbortHandle);

impl Drop for TaskGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub fn spawn_guarded<F>(f: F) -> TaskGuard
where
    F: Future<Output = ()> + Send + 'static,
{
    TaskGuard(RUNTIME.spawn(f).abort_handle())
}

/// Run work to completion even if nobody waits for it (cleanup).
pub fn spawn_detached<F>(f: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    RUNTIME.spawn(f);
}

#[derive(Clone)]
pub struct ObjectRef {
    pub kind: Kind,
    pub name: String,
    pub namespace: Option<String>,
    pub uid: String,
}

pub async fn get_object(client: Client, obj: ObjectRef) -> Result<serde_json::Value> {
    get_object_opts(client, obj, false).await
}

/// The object, with `metadata.managedFields` kept when asked.
pub async fn get_object_opts(client: Client, obj: ObjectRef, keep_managed: bool) -> Result<serde_json::Value> {
    run(async move {
        let api: Api<DynamicObject> = match &obj.namespace {
            Some(ns) => Api::namespaced_with(client, ns, &obj.kind.ar),
            None => Api::all_with(client, &obj.kind.ar),
        };
        let mut value = serde_json::to_value(api.get(&obj.name).await?)?;
        if !keep_managed && let Some(meta) = value.get_mut("metadata").and_then(|m| m.as_object_mut()) {
            meta.remove("managedFields");
        }
        Ok(value)
    })
    .await
}

/// YAML for display, with Secret values masked unless revealed.
pub fn to_yaml(value: &serde_json::Value, is_secret: bool, reveal_secrets: bool) -> Result<String> {
    if !is_secret || reveal_secrets {
        return Ok(crate::yaml::to_string(value)?);
    }
    let mut value = value.clone();
    mask_secret_values(&mut value);
    Ok(crate::yaml::to_string(&value)?)
}

const MASK: &str = "••••••";

/// `kubectl apply` keeps a full copy of the object, values included, in this annotation.
const LAST_APPLIED: &str = "kubectl.kubernetes.io/last-applied-configuration";

/// Hide a Secret's values (keys stay visible): `data`, `stringData`, and the copy
/// `kubectl apply` keeps in an annotation.
pub fn mask_secret_values(value: &mut serde_json::Value) {
    for field in ["data", "stringData"] {
        if let Some(data) = value.get_mut(field).and_then(|d| d.as_object_mut()) {
            for v in data.values_mut() {
                *v = serde_json::Value::String(MASK.into());
            }
        }
    }
    if let Some(v) = value.pointer_mut(&format!("/metadata/annotations/{}", LAST_APPLIED.replace('/', "~1"))) {
        *v = serde_json::Value::String(MASK.into());
    }
}

/// kubectl-style age: 45s, 12m, 5h, 3d, 2y.
/// An error as a person reads it: the API server's own message (not kube's debug dump of
/// the whole Status), otherwise the error chain.
pub fn short_error(e: &anyhow::Error) -> String {
    for cause in e.chain() {
        if let Some(kube::Error::Api(status)) = cause.downcast_ref::<kube::Error>() {
            return status.message.clone();
        }
    }
    let text = format!("{e:#}");
    // A timeout from the connect or request deadline, said plainly instead of
    // "ServiceError: client error (Connect): client error (Connect): deadline has elapsed".
    if text.contains("deadline has elapsed") || text.contains("request timed out") {
        let step = if text.contains("Connect") { "connecting to the API server" } else { "waiting for the API server" };
        return format!("The cluster did not answer (timed out {step})");
    }
    text
}

/// The HTTP status of an API error (403 forbidden, 404 not found…), if it is one.
pub fn api_status(e: &anyhow::Error) -> Option<u16> {
    e.chain().find_map(|c| match c.downcast_ref::<kube::Error>() {
        Some(kube::Error::Api(status)) => Some(status.code),
        _ => None,
    })
}

pub fn age(t: jiff::Timestamp) -> String {
    let secs = (jiff::Timestamp::now().as_second() - t.as_second()).max(0);
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 3600 * 2 => format!("{}m", s / 60),
        s if s < 86400 * 2 => format!("{}h", s / 3600),
        s if s < 86400 * 365 * 2 => format!("{}d", s / 86400),
        s => format!("{}y", s / (86400 * 365)),
    }
}

/// List objects as JSON, optionally filtered by label selector.
pub async fn list_values(
    client: Client,
    ar: ApiResource,
    namespace: Option<String>,
    label_selector: Option<String>,
) -> Result<Vec<serde_json::Value>> {
    list_values_fields(client, ar, namespace, label_selector, None).await
}

pub async fn list_values_fields(
    client: Client,
    ar: ApiResource,
    namespace: Option<String>,
    label_selector: Option<String>,
    field_selector: Option<String>,
) -> Result<Vec<serde_json::Value>> {
    run(async move {
        let api: Api<DynamicObject> = match &namespace {
            Some(ns) => Api::namespaced_with(client, ns, &ar),
            None => Api::all_with(client, &ar),
        };
        let mut lp = ListParams::default();
        if let Some(l) = &label_selector {
            lp = lp.labels(l);
        }
        if let Some(f) = &field_selector {
            lp = lp.fields(f);
        }
        let list = api.list(&lp).await?;
        Ok(list.items.into_iter().filter_map(|o| serde_json::to_value(o).ok()).collect())
    })
    .await
}

/// Whether `read` is an older version of the object than `shown`, by resourceVersion.
/// The API calls it opaque, but every server in use numbers it as an increasing integer;
/// anything else is never called older.
pub fn older_version(read: &serde_json::Value, shown: &serde_json::Value) -> bool {
    let version = |v: &serde_json::Value| v.pointer("/metadata/resourceVersion")?.as_str()?.parse::<u64>().ok();
    let same = |p: &str| read.pointer(p) == shown.pointer(p);
    same("/metadata/uid") && matches!((version(read), version(shown)), (Some(a), Some(b)) if a < b)
}

pub fn ar_of<K: kube::Resource<DynamicType = ()>>() -> ApiResource {
    ApiResource::erase::<K>(&())
}

fn url_escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Which pods an object selects, as a list request would ask for them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    /// None at all (no selector, an empty Service selector, an operator we can't read).
    Nothing,
    /// Every pod of the namespace.
    Everything,
    /// The pods matching this label selector.
    Labels(String),
}

impl Selection {
    /// The label selector to list with: `Some(None)` lists every pod, `None` lists none.
    pub fn list_selector(&self) -> Option<Option<String>> {
        match self {
            Self::Nothing => None,
            Self::Everything => Some(None),
            Self::Labels(s) => Some(Some(s.clone())),
        }
    }
}

/// A `LabelSelector` (`matchLabels` and `matchExpressions`) in the API server's selector syntax.
/// A missing or null selector selects nothing; an empty one everything when `empty_is_all`.
/// An operator this doesn't know selects nothing, rather than more than it should.
pub fn label_selection(selector: Option<&serde_json::Value>, empty_is_all: bool) -> Selection {
    let Some(selector) = selector.filter(|s| !s.is_null()) else { return Selection::Nothing };
    let mut parts: Vec<String> = selector
        .get("matchLabels")
        .and_then(|l| l.as_object())
        .into_iter()
        .flatten()
        .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
        .collect();
    for e in selector.get("matchExpressions").and_then(|e| e.as_array()).into_iter().flatten() {
        let key = e["key"].as_str().unwrap_or_default();
        let values: Vec<&str> =
            e["values"].as_array().map(|v| v.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        parts.push(match e["operator"].as_str().unwrap_or_default() {
            "In" => format!("{key} in ({})", values.join(",")),
            "NotIn" => format!("{key} notin ({})", values.join(",")),
            "Exists" => key.to_string(),
            "DoesNotExist" => format!("!{key}"),
            _ => return Selection::Nothing,
        });
    }
    match parts.is_empty() {
        true if empty_is_all => Selection::Everything,
        true => Selection::Nothing,
        false => Selection::Labels(parts.join(",")),
    }
}

/// The pods an object stands for, the way Kubernetes picks them, in one place for the pages
/// and the agents' tools:
/// - a Service: its `spec.selector` labels; none when empty;
/// - a PodDisruptionBudget: none without a selector, all with `{}` (policy/v1);
/// - a NetworkPolicy: its `podSelector`, `{}` being every pod;
/// - a workload (Deployment, StatefulSet, DaemonSet, ReplicaSet, Job): its `spec.selector`.
pub fn pod_selection(kind: &Kind, obj: &serde_json::Value) -> Selection {
    if kind.is("", "Service") {
        let labels = obj.pointer("/spec/selector").cloned().map(|m| serde_json::json!({ "matchLabels": m }));
        return label_selection(labels.as_ref(), false);
    }
    if kind.is("policy", "PodDisruptionBudget") {
        return label_selection(obj.pointer("/spec/selector"), true);
    }
    if kind.is("networking.k8s.io", "NetworkPolicy") {
        return label_selection(obj.pointer("/spec/podSelector"), true);
    }
    label_selection(obj.pointer("/spec/selector"), false)
}

/// Whether a label selector (matchLabels + matchExpressions) selects these labels.
/// An empty selector selects everything.
pub fn selector_matches(selector: &serde_json::Value, labels: &serde_json::Map<String, serde_json::Value>) -> bool {
    let get = |k: &str| labels.get(k).and_then(|v| v.as_str());
    let ml = selector
        .get("matchLabels")
        .and_then(|l| l.as_object())
        .into_iter()
        .flatten()
        .all(|(k, v)| get(k) == v.as_str());
    let me = selector.get("matchExpressions").and_then(|e| e.as_array()).into_iter().flatten().all(|e| {
        let key = e["key"].as_str().unwrap_or_default();
        let values: Vec<&str> =
            e["values"].as_array().map(|v| v.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        match e["operator"].as_str().unwrap_or_default() {
            "In" => get(key).is_some_and(|v| values.contains(&v)),
            "NotIn" => get(key).is_none_or(|v| !values.contains(&v)),
            "Exists" => get(key).is_some(),
            "DoesNotExist" => get(key).is_none(),
            _ => false,
        }
    });
    ml && me
}

/// GET a JSON path on the API server (None on any error, e.g. no permission).
pub async fn get_json(client: Client, path: String) -> Option<serde_json::Value> {
    run(async move { Ok(client.request::<serde_json::Value>(http::Request::get(path).body(Vec::new())?).await?) })
        .await
        .ok()
}

#[cfg(test)]
mod selector_tests {
    #[test]
    fn selectors() {
        let sel = serde_json::json!({"matchLabels": {"app": "web"}, "matchExpressions": [{"key": "tier", "operator": "In", "values": ["a", "b"]}, {"key": "canary", "operator": "DoesNotExist"}]});
        assert_eq!(
            super::label_selection(Some(&sel), false),
            super::Selection::Labels("app=web,tier in (a,b),!canary".into())
        );
        let labels = |v: serde_json::Value| v.as_object().unwrap().clone();
        assert!(super::selector_matches(&sel, &labels(serde_json::json!({"app": "web", "tier": "a"}))));
        assert!(!super::selector_matches(&sel, &labels(serde_json::json!({"app": "web", "tier": "c"}))));
        assert!(!super::selector_matches(&sel, &labels(serde_json::json!({"app": "web", "tier": "a", "canary": "1"}))));
        assert!(super::selector_matches(&serde_json::json!({}), &labels(serde_json::json!({"x": "y"}))));
    }

    /// The pods of each kind, as Kubernetes picks them; an operator we don't know selects
    /// none rather than more.
    #[test]
    fn which_pods_an_object_selects() {
        use super::{Kind, Selection, pod_selection};
        use serde_json::json;
        let kind = |group: &str, k: &str| Kind {
            ar: kube::discovery::ApiResource {
                group: group.into(),
                version: "v1".into(),
                api_version: "v1".into(),
                kind: k.into(),
                plural: format!("{}s", k.to_lowercase()),
            },
            namespaced: true,
            section: "",
            label: String::new(),
        };
        let pdb = kind("policy", "PodDisruptionBudget");
        assert_eq!(pod_selection(&pdb, &json!({"spec": {"minAvailable": 1}})), Selection::Nothing);
        assert_eq!(pod_selection(&pdb, &json!({"spec": {"selector": null}})), Selection::Nothing);
        assert_eq!(pod_selection(&pdb, &json!({"spec": {"selector": {}}})), Selection::Everything);
        let only_expressions =
            json!({"spec": {"selector": {"matchExpressions": [{"key": "app", "operator": "Exists"}]}}});
        assert_eq!(pod_selection(&pdb, &only_expressions), Selection::Labels("app".into()));
        let svc = kind("", "Service");
        assert_eq!(
            pod_selection(&svc, &json!({"spec": {"selector": {"app": "web"}}})),
            Selection::Labels("app=web".into())
        );
        assert_eq!(pod_selection(&svc, &json!({"spec": {"selector": {}}})), Selection::Nothing);
        assert_eq!(pod_selection(&svc, &json!({"spec": {}})), Selection::Nothing);
        let netpol = kind("networking.k8s.io", "NetworkPolicy");
        assert_eq!(pod_selection(&netpol, &json!({"spec": {"podSelector": {}}})), Selection::Everything);
        let deploy = kind("apps", "Deployment");
        let odd = json!({"spec": {"selector": {"matchLabels": {"app": "web"}, "matchExpressions": [{"key": "x", "operator": "Gt", "values": ["1"]}]}}});
        assert_eq!(pod_selection(&deploy, &odd), Selection::Nothing);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_an_abortable_run_stops_it() {
        use futures::FutureExt as _;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let mut work = Box::pin(run_abortable(async move {
            let _tx = tx;
            std::future::pending::<()>().await;
            Ok(())
        }));
        assert!(work.as_mut().now_or_never().is_none());
        drop(work);
        // The aborted task drops what it held.
        assert!(futures::executor::block_on(rx).is_err());
    }

    #[test]
    fn timeouts_are_said_plainly() {
        let connect = anyhow!("ServiceError: client error (Connect): client error (Connect): deadline has elapsed");
        assert_eq!(short_error(&connect), "The cluster did not answer (timed out connecting to the API server)");
        let request = anyhow!("ServiceError: request timed out");
        assert_eq!(short_error(&request), "The cluster did not answer (timed out waiting for the API server)");
        assert_eq!(short_error(&anyhow!("other")), "other");
    }

    #[test]
    fn secrets_are_masked_everywhere() {
        let applied = r#"{"apiVersion":"v1","kind":"Secret","data":{"config":"ZXhhbXBsZQ=="}}"#;
        let secret = serde_json::json!({
            "kind": "Secret",
            "metadata": {"name": "db", "annotations": {LAST_APPLIED: applied, "team": "a"}},
            "data": {"config": "ZXhhbXBsZQ=="},
            "stringData": {"note": "example"},
        });
        let masked = to_yaml(&secret, true, false).unwrap();
        assert!(!masked.contains("ZXhhbXBsZQ") && !masked.contains("example"), "{masked}");
        assert!(masked.contains("config:") && masked.contains("note:") && masked.contains("team: a"));
        assert!(to_yaml(&secret, true, true).unwrap().contains("example"));
        assert!(to_yaml(&secret, false, false).unwrap().contains("example"));
    }

    #[test]
    fn quantities() {
        assert_eq!(parse_cpu("250m"), 250.0);
        assert_eq!(parse_cpu("2"), 2000.0);
        assert_eq!(format_cores(250.0), "250m");
        assert_eq!(format_cores(1500.0), "1.5 cores");
        assert_eq!(format_cores(2000.0), "2 cores");
        assert_eq!(format_cores(187_600.0), "188 cores");
        assert_eq!(parse_cpu("1500000n"), 1.5);
        assert_eq!(parse_bytes("1Ki"), 1024.0);
        assert_eq!(parse_bytes("128Mi"), 128.0 * 1024.0 * 1024.0);
        assert_eq!(parse_bytes("1G"), 1e9);
        assert_eq!(format_cpu(1500.0), "1.50");
        assert_eq!(format_cpu(42.4), "42m");
        assert_eq!(format_bytes(256.0 * 1024.0 * 1024.0), "256Mi");
    }

    #[test]
    fn kind_aliases() {
        let ar = |group: &str, kind: &str, plural: &str| ApiResource {
            group: group.into(),
            version: "v1".into(),
            api_version: if group.is_empty() { "v1".into() } else { format!("{group}/v1") },
            kind: kind.into(),
            plural: plural.into(),
        };
        let kinds = vec![
            Kind { ar: ar("", "Pod", "pods"), namespaced: true, section: "Workloads", label: "Pods".into() },
            Kind {
                ar: ar("apps", "Deployment", "deployments"),
                namespaced: true,
                section: "Workloads",
                label: "Deployments".into(),
            },
            Kind {
                ar: ar("cert-manager.io", "Certificate", "certificates"),
                namespaced: true,
                section: CUSTOM_SECTION,
                label: "Certificate".into(),
            },
        ];
        assert_eq!(find_kind(&kinds, "po").unwrap().ar.kind, "Pod");
        assert_eq!(find_kind(&kinds, "deploy").unwrap().ar.kind, "Deployment");
        assert_eq!(find_kind(&kinds, "Deployment").unwrap().ar.kind, "Deployment");
        assert_eq!(find_kind(&kinds, "certificates.cert-manager.io").unwrap().ar.kind, "Certificate");
        assert!(find_kind(&kinds, "secrets").is_none());

        // Custom resources named like built-ins, in earlier sidebar sections.
        let mut kinds = vec![
            Kind {
                ar: ar("", "Service", "services"),
                namespaced: true,
                section: "Networking",
                label: "Services".into(),
            },
            Kind {
                ar: ar("rbac.authorization.k8s.io", "Role", "roles"),
                namespaced: true,
                section: "Access Control",
                label: "Roles".into(),
            },
            Kind {
                ar: ar("serving.knative.dev", "Service", "services"),
                namespaced: true,
                section: crd_section("serving.knative.dev"),
                label: "Service".into(),
            },
            Kind {
                ar: ar("iam.services.k8s.aws", "Role", "roles"),
                namespaced: true,
                section: crd_section("iam.services.k8s.aws"),
                label: "Role".into(),
            },
        ];
        sort_for_sidebar(&mut kinds);
        assert_eq!(kinds[0].ar.group, "serving.knative.dev");
        assert_eq!(find_kind(&kinds, "svc").unwrap().ar.group, "");
        assert_eq!(find_kind(&kinds, "Service").unwrap().ar.group, "");
        assert_eq!(find_kind(&kinds, "roles").unwrap().ar.group, "rbac.authorization.k8s.io");
        assert_eq!(find_kind(&kinds, "services.serving.knative.dev").unwrap().ar.group, "serving.knative.dev");
        assert_eq!(find_kind(&kinds, "roles.iam.services.k8s.aws").unwrap().ar.group, "iam.services.k8s.aws");
    }
}
