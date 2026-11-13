//! Usage from metrics-server and the kubelet; quantities and allocation.

use std::collections::HashMap;

use anyhow::Result;
use futures::channel::mpsc;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Client;
use serde::Deserialize;

use super::cadence::{Cadence, Freshness, Reading, date_header, reading};
use super::{Pause, TaskGuard, ar_of, get_json, lists, podspec, run, spawn_guarded, url_escape};

/// Pod usage from metrics-server: (namespace, name) -> (millicores, bytes).
pub type PodUsage = HashMap<(String, String), (f64, f64)>;

/// Per-container usage of pods: (namespace, pod) -> container -> (millicores, bytes). The
/// same pod name can be in several namespaces.
pub type PodContainerUsage = HashMap<(String, String), HashMap<String, (f64, f64)>>;

/// A metrics.k8s.io pod list, by (namespace, pod) then container. A quantity that does not
/// read counts 0.
pub fn parse_pod_metrics(list: &serde_json::Value) -> PodContainerUsage {
    fn text<'a>(v: &'a serde_json::Value, p: &str) -> &'a str {
        v.pointer(p).and_then(serde_json::Value::as_str).unwrap_or_default()
    }
    let mut out = PodContainerUsage::new();
    for item in list["items"].as_array().into_iter().flatten() {
        let key = (text(item, "/metadata/namespace").to_string(), text(item, "/metadata/name").to_string());
        let containers = out.entry(key).or_default();
        for c in item["containers"].as_array().into_iter().flatten() {
            let usage = (parse_cpu(text(c, "/usage/cpu")), parse_bytes(text(c, "/usage/memory")));
            containers.insert(text(c, "/name").to_string(), usage);
        }
    }
    out
}

/// A pod's usage: its containers' added up.
fn pod_total(containers: &HashMap<String, (f64, f64)>) -> (f64, f64) {
    containers.values().fold((0.0, 0.0), |(cpu, mem), (c, m)| (cpu + c, mem + m))
}

/// The metrics.k8s.io pod list of a namespace (None: every namespace), with the pods a
/// label selector picks.
fn pod_metrics_path(namespace: Option<&str>, label_selector: Option<&str>) -> String {
    let mut path = match namespace {
        Some(ns) => format!("/apis/metrics.k8s.io/v1beta1/namespaces/{ns}/pods"),
        None => "/apis/metrics.k8s.io/v1beta1/pods".to_string(),
    };
    if let Some(l) = label_selector {
        path.push_str("?labelSelector=");
        path.push_str(&url_escape(l));
    }
    path
}

/// Per-container usage of the pods of a namespace (None: every namespace) a label selector
/// picks, with when it was measured.
pub async fn pod_usage(
    client: Client,
    namespace: Option<String>,
    label_selector: Option<String>,
) -> Result<(PodContainerUsage, Reading)> {
    run(async move {
        let path = pod_metrics_path(namespace.as_deref(), label_selector.as_deref());
        let (list, read) = get_metrics(&client, &path).await?;
        Ok((parse_pod_metrics(&list), read))
    })
    .await
}

/// Why metrics-server could not be read, for a page or a tool to say: "no metrics-server"
/// (no metrics API), "metrics-server unavailable" (its APIService answers 503: not running
/// or not ready), "forbidden" or "failed: …".
pub fn metrics_error(e: &anyhow::Error) -> String {
    match super::api_status(e) {
        Some(404) => "no metrics-server".into(),
        Some(503) => "metrics-server unavailable".into(),
        _ => lists::load_error(e),
    }
}

/// The same for an agent, as an error: what is missing or down, not "404 page not found"
/// or "service unavailable". `what` is what could not be read ("node metrics").
pub fn explain_metrics_error(e: anyhow::Error, what: &str) -> anyhow::Error {
    match super::api_status(&e) {
        Some(404) => anyhow::anyhow!("no {what}: metrics-server is not installed in this cluster"),
        Some(503) => anyhow::anyhow!(
            "no {what}: metrics-server is unavailable (the metrics.k8s.io APIService answers 503: not running or not ready)"
        ),
        _ => e,
    }
}

/// After a failed poll: wait (doubling up to 5 minutes), then try again.
async fn retry_wait(failures: &mut u32) {
    *failures = failures.saturating_add(1);
    let secs = 15u64.saturating_mul(1 << (*failures - 1).min(5)).min(300);
    tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
}

/// While the list showing these metrics is hidden, poll nothing.
async fn wait_unpaused(pause: &Pause) {
    while pause.is_paused() {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// GET a metrics.k8s.io list, with the API server's clock when it answered (its `Date`
/// header), and the times its items were measured.
async fn get_metrics(client: &Client, path: &str) -> Result<(serde_json::Value, Reading)> {
    let res = client.send(http::Request::get(path).body(kube::client::Body::empty())?).await?;
    let date = res.headers().get(http::header::DATE).and_then(|v| v.to_str().ok()).and_then(date_header);
    let status = res.status();
    let body = res.into_body().collect_bytes().await?;
    if !status.is_success() {
        // The same error `Client::request` gives, so callers still see a 404 as one.
        let text = String::from_utf8_lossy(&body);
        let refusal = serde_json::from_slice::<kube::core::Status>(&body)
            .unwrap_or_else(|_| kube::core::Status::failure(&text, "Failed to parse error data"))
            .with_code(status.as_u16());
        return Err(kube::Error::Api(refusal.boxed()).into());
    }
    let list: serde_json::Value = serde_json::from_slice(&body)?;
    let read = reading(list["items"].as_array().map_or_default(Vec::as_slice), date);
    Ok((list, read))
}

/// Poll metrics-server when its next sample is due ([`Cadence`]), never more often than
/// `metrics_refresh`. Keeps retrying (backing off) while metrics aren't available; polls
/// nothing while `pause` is set.
pub fn watch_pod_metrics(
    client: Client,
    namespace: Option<String>,
    pause: Pause,
) -> (TaskGuard, mpsc::UnboundedReceiver<(PodUsage, Freshness)>) {
    let (tx, rx) = mpsc::unbounded();
    let guard = spawn_guarded(async move {
        let path = pod_metrics_path(namespace.as_deref(), None);
        let mut failures = 0;
        let mut cadence = Cadence::default();
        loop {
            wait_unpaused(&pause).await;
            let Ok((list, read)) = get_metrics(&client, &path).await else {
                if tx.is_closed() {
                    return;
                }
                retry_wait(&mut failures).await;
                continue;
            };
            failures = 0;
            let usage: PodUsage = parse_pod_metrics(&list).into_iter().map(|(k, c)| (k, pod_total(&c))).collect();
            let wait = cadence.next(&read, crate::settings::secs(crate::settings::get().metrics_refresh));
            if tx.unbounded_send((usage, Freshness::new(&read, cadence.every()))).is_err() {
                return;
            }
            tokio::time::sleep(wait).await;
        }
    });
    (guard, rx)
}

pub use crate::quantity::{parse_bytes, parse_cpu};

pub fn format_cpu(millicores: f64) -> String {
    if millicores >= 1000.0 { format!("{:.2}", millicores / 1000.0) } else { format!("{millicores:.0}m") }
}

/// CPU for prose and labels: "250m", "1.5 cores", "188 cores".
pub fn format_cores(millicores: f64) -> String {
    match millicores {
        m if m < 1000.0 => format!("{m:.0}m"),
        m if m < 10_000.0 => {
            crate::plural::count(format!("{:.2}", m / 1000.0).trim_end_matches('0').trim_end_matches('.'), "core")
        }
        m => format!("{:.0} cores", m / 1000.0),
    }
}

pub fn format_bytes(bytes: f64) -> String {
    let mi = bytes / (1024.0 * 1024.0);
    if mi >= 1024.0 { format!("{:.2}Gi", mi / 1024.0) } else { format!("{mi:.0}Mi") }
}

/// A figure in at most three significant digits: "8.64", "11.2", "111", "1023".
fn three_digits(x: f64) -> String {
    match x {
        x if x < 9.995 => format!("{x:.2}"),
        x if x < 99.95 => format!("{x:.1}"),
        x => format!("{x:.0}"),
    }
}

/// CPU short enough for a narrow column: "250m", "1.50", "12.9", "128".
pub fn short_cpu(millicores: f64) -> String {
    let m = millicores.max(0.0);
    if m < 999.5 { format!("{m:.0}m") } else { three_digits(m / 1000.0) }
}

/// Memory short enough for a narrow column: "512Mi", "8.64Gi", "11.2Gi", "111Gi", "1.50Ti".
pub fn short_bytes(bytes: f64) -> String {
    const UNITS: [&str; 4] = ["Mi", "Gi", "Ti", "Pi"];
    let mut v = bytes.max(0.0) / (1024.0 * 1024.0);
    if v < 1023.5 {
        return format!("{v:.0}Mi");
    }
    let mut unit = 0;
    while v >= 1023.5 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{}{}", three_digits(v), UNITS[unit])
}

/// Per-container usage of pods in a namespace: pod -> container -> (millicores, bytes).
pub type ContainerUsage = HashMap<String, HashMap<String, (f64, f64)>>;

/// Per-container usage of the pods of a namespace a label selector picks, by pod, with when
/// it was measured.
pub async fn container_usage_read(
    client: Client,
    namespace: String,
    label_selector: Option<String>,
) -> Result<(ContainerUsage, Reading)> {
    let (usage, read) = pod_usage(client, Some(namespace), label_selector).await?;
    Ok((usage.into_iter().map(|((_, pod), c)| (pod, c)).collect(), read))
}

/// Node usage and allocatable: name -> ((millicores, bytes) used, (millicores, bytes) allocatable).
pub type NodeUsage = HashMap<String, ((f64, f64), (f64, f64))>;

/// A node's name and allocatable resources: all the metrics poll needs (a full Node also
/// carries managed fields and the image list, read and dropped every poll otherwise).
#[derive(Deserialize, Default)]
#[serde(default)]
struct NodeAllocatable {
    metadata: ObjectMeta,
    status: NodeAllocatableStatus,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NodeAllocatableStatus {
    allocatable: HashMap<String, String>,
}

/// A pod's CPU (millicores) and memory (bytes) requests and limits, summed over its
/// containers and sidecars, plus overhead; pod-level resources win when set. No limit when a
/// container has none: the pod can then use what the node has.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PodResources {
    pub cpu: (Option<f64>, Option<f64>),
    pub mem: (Option<f64>, Option<f64>),
}

/// (namespace, pod) -> its requests and limits.
pub type PodResourcesMap = HashMap<(String, String), PodResources>;

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct LeanPod {
    metadata: LeanPodMeta,
    spec: podspec::LeanSpec,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct LeanPodMeta {
    name: String,
    namespace: String,
}

/// Set against usage: what the running pod is held to (regular init containers are done).
fn pod_resources(spec: &podspec::LeanSpec) -> PodResources {
    let of = |r: &str| {
        let e = spec.effective(r, podspec::Count::Running);
        (e.request(), e.limit())
    };
    PodResources { cpu: of("cpu"), mem: of("memory") }
}

/// The pods' requests and limits, for the usage bars of the pods list: one light list
/// (only those fields are parsed), again every minute or ten times as long as it took,
/// for pods that came since. Nothing while `pause` is set.
pub fn watch_pod_resources(
    client: Client,
    namespace: Option<String>,
    pause: Pause,
) -> (TaskGuard, mpsc::UnboundedReceiver<PodResourcesMap>) {
    let (tx, rx) = mpsc::unbounded();
    let guard = spawn_guarded(async move {
        let mut failures = 0;
        loop {
            wait_unpaused(&pause).await;
            let started = std::time::Instant::now();
            let list = lists::list_lean_cached::<LeanPod>(client.clone(), ar_of::<Pod>(), namespace.clone()).await;
            let Ok(pods) = list else {
                if tx.is_closed() {
                    return;
                }
                retry_wait(&mut failures).await;
                continue;
            };
            failures = 0;
            let map: PodResourcesMap =
                pods.into_iter().map(|p| ((p.metadata.namespace, p.metadata.name), pod_resources(&p.spec))).collect();
            if tx.unbounded_send(map).is_err() {
                return;
            }
            tokio::time::sleep((started.elapsed() * 10).max(std::time::Duration::from_secs(60))).await;
        }
    });
    (guard, rx)
}

/// Node usage and allocatable now, from metrics-server and the nodes.
pub async fn node_usage(client: Client) -> Result<NodeUsage> {
    Ok(run(node_usage_now(client)).await?.0)
}

async fn node_usage_now(client: Client) -> Result<(NodeUsage, Reading)> {
    let ((metrics, read), list) = futures::try_join!(
        get_metrics(&client, "/apis/metrics.k8s.io/v1beta1/nodes"),
        lists::list_lean::<NodeAllocatable>(client.clone(), ar_of::<k8s_openapi::api::core::v1::Node>(), None),
    )?;
    let mut out = NodeUsage::new();
    for n in list {
        let name = n.metadata.name.unwrap_or_default();
        let alloc = n.status.allocatable;
        let cpu = alloc.get("cpu").map_or(0.0, |q| parse_cpu(q));
        let mem = alloc.get("memory").map_or(0.0, |q| parse_bytes(q));
        out.insert(name, ((0.0, 0.0), (cpu, mem)));
    }
    for item in metrics["items"].as_array().into_iter().flatten() {
        let name = item.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default();
        let cpu = item.pointer("/usage/cpu").and_then(|v| v.as_str()).map_or(0.0, parse_cpu);
        let mem = item.pointer("/usage/memory").and_then(|v| v.as_str()).map_or(0.0, parse_bytes);
        if let Some(e) = out.get_mut(name) {
            e.0 = (cpu, mem);
        }
    }
    Ok((out, read))
}

/// Poll node metrics and allocatable when the next sample is due (as pods do), retrying
/// (backing off) on errors; nothing while `pause` is set.
pub fn watch_node_metrics(
    client: Client,
    pause: Pause,
) -> (TaskGuard, mpsc::UnboundedReceiver<(NodeUsage, Freshness)>) {
    let (tx, rx) = mpsc::unbounded();
    let guard = spawn_guarded(async move {
        let mut failures = 0;
        let mut cadence = Cadence::default();
        loop {
            wait_unpaused(&pause).await;
            let Ok((out, read)) = node_usage_now(client.clone()).await else {
                if tx.is_closed() {
                    return;
                }
                retry_wait(&mut failures).await;
                continue;
            };
            failures = 0;
            let wait = cadence.next(&read, crate::settings::secs(crate::settings::get().metrics_refresh));
            if tx.unbounded_send((out, Freshness::new(&read, cadence.every()))).is_err() {
                return;
            }
            tokio::time::sleep(wait).await;
        }
    });
    (guard, rx)
}

/// How long a page waits for a kubelet's stats: a busy one can take tens of seconds, and
/// one that never answers would hold the page's other sections for minutes.
const NODE_STATS_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The kubelet's stats summary for a node (filesystem and per-pod ephemeral storage);
/// needs `nodes/proxy`. None when it doesn't answer within `NODE_STATS_WAIT`.
pub async fn node_stats(client: Client, node: String) -> Option<serde_json::Value> {
    let read = get_json(client, format!("/api/v1/nodes/{node}/proxy/stats/summary"));
    run(async move { Ok(tokio::time::timeout(NODE_STATS_WAIT, read).await.ok().flatten()) }).await.ok().flatten()
}

/// DRA ResourceSlices published for a node (resource.k8s.io v1, then v1beta1/v1beta2).
pub async fn resource_slices(client: Client, node: String) -> Vec<serde_json::Value> {
    for v in ["v1", "v1beta2", "v1beta1"] {
        let path = format!("/apis/resource.k8s.io/{v}/resourceslices?fieldSelector=spec.nodeName%3D{node}");
        if let Some(list) = get_json(client.clone(), path).await {
            return list["items"].as_array().cloned().unwrap_or_default();
        }
    }
    Vec::new()
}

/// Sum of pod requests and limits for one resource, the way the scheduler counts them
/// ([`podspec::effective`]); a container without a limit counts 0. Finished pods are left out.
pub fn pods_allocated(pods: &[serde_json::Value], resource: &str) -> (f64, f64) {
    let (mut req, mut lim) = (0.0, 0.0);
    for p in pods {
        if matches!(p.pointer("/status/phase").and_then(|v| v.as_str()), Some("Succeeded" | "Failed")) {
            continue;
        }
        let e = podspec::of_value(p, resource, podspec::Count::Scheduled);
        req += e.request;
        lim += e.limit;
    }
    (req, lim)
}

#[cfg(test)]
mod pod_metrics_tests {
    use super::{metrics_error, parse_pod_metrics, pod_metrics_path, pod_total};

    #[test]
    fn says_why_metrics_server_could_not_be_read() {
        let refused = |code: u16| -> anyhow::Error {
            kube::Error::Api(kube::core::Status::failure("refused", "Refused").with_code(code).boxed()).into()
        };
        assert_eq!(metrics_error(&refused(404)), "no metrics-server");
        // Its APIService down (metrics-server scaled to 0, not ready).
        assert_eq!(metrics_error(&refused(503)), "metrics-server unavailable");
        assert_eq!(metrics_error(&refused(403)), "forbidden");
    }

    #[test]
    fn reads_every_container_of_every_pod() {
        let list = serde_json::json!({"items": [
            {"metadata": {"name": "web-1", "namespace": "shop"}, "containers": [
                {"name": "app", "usage": {"cpu": "250000000n", "memory": "512Ki"}},
                {"name": "proxy", "usage": {"cpu": "1500u", "memory": "2Mi"}},
            ]},
            {"metadata": {"name": "db-0", "namespace": "data"}, "containers": [
                {"name": "pg", "usage": {"cpu": "1", "memory": "1Gi"}},
                // A quantity that does not read counts 0.
                {"name": "exporter", "usage": {"cpu": "?"}},
            ]},
            // The same pod name in another namespace stays apart.
            {"metadata": {"name": "web-1", "namespace": "staging"}, "containers": [
                {"name": "app", "usage": {"cpu": "10m", "memory": "1Mi"}},
            ]},
        ]});
        let u = parse_pod_metrics(&list);
        let pod = |ns: &str, name: &str| &u[&(ns.to_string(), name.to_string())];
        assert_eq!(pod("shop", "web-1")["app"], (250.0, 512.0 * 1024.0));
        assert_eq!(pod("shop", "web-1")["proxy"], (1.5, 2.0 * 1024.0 * 1024.0));
        assert_eq!(pod("data", "db-0")["pg"], (1000.0, 1024f64.powi(3)));
        assert_eq!(pod("data", "db-0")["exporter"], (0.0, 0.0));
        assert_eq!(pod("staging", "web-1")["app"].0, 10.0);
        assert_eq!(pod_total(pod("shop", "web-1")), (251.5, 512.0 * 1024.0 + 2.0 * 1024.0 * 1024.0));
        assert!(parse_pod_metrics(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn paths() {
        assert_eq!(pod_metrics_path(None, None), "/apis/metrics.k8s.io/v1beta1/pods");
        assert_eq!(pod_metrics_path(Some("shop"), None), "/apis/metrics.k8s.io/v1beta1/namespaces/shop/pods");
        assert!(
            pod_metrics_path(Some("shop"), Some("app=web"))
                .starts_with("/apis/metrics.k8s.io/v1beta1/namespaces/shop/pods?labelSelector=app")
        );
    }
}

#[cfg(test)]
mod quantity_tests {
    use proptest::prelude::*;

    use super::{parse_bytes, parse_cpu, short_bytes, short_cpu};

    #[test]
    fn short_figures() {
        let gi = 1024f64.powi(3);
        assert_eq!(short_bytes(512.0 * 1024.0 * 1024.0), "512Mi");
        assert_eq!(short_bytes(8.64 * gi), "8.64Gi");
        assert_eq!(short_bytes(11.17 * gi), "11.2Gi");
        assert_eq!(short_bytes(111.17 * gi), "111Gi");
        assert_eq!(short_bytes(1500.0 * gi), "1.46Ti");
        assert_eq!(short_bytes(1023.9 * 1024.0 * 1024.0), "1.00Gi");
        assert_eq!(short_cpu(6.0), "6m");
        assert_eq!(short_cpu(999.7), "1.00");
        assert_eq!(short_cpu(12_890.0), "12.9");
        assert_eq!(short_cpu(128_000.0), "128");
    }

    proptest! {
        /// Every figure fits the usage column: at most six characters, for any usage up
        /// to an exabyte and ten thousand cores.
        #[test]
        fn short_figures_fit(bytes in 0.0f64..1e18, millicores in 0.0f64..1e7) {
            prop_assert!(short_bytes(bytes).len() <= 6, "{}", short_bytes(bytes));
            prop_assert!(short_cpu(millicores).len() <= 6, "{}", short_cpu(millicores));
        }
    }

    #[test]
    fn cpu() {
        assert_eq!(parse_cpu("250m"), 250.0);
        assert_eq!(parse_cpu("1"), 1000.0);
        assert_eq!(parse_cpu("0.5"), 500.0);
        assert_eq!(parse_cpu("1500000n"), 1.5);
        assert_eq!(parse_cpu("12u"), 0.012);
        assert_eq!(parse_cpu("2k"), 2_000_000.0);
        assert_eq!(parse_cpu("1e3"), 1_000_000.0);
        assert_eq!(parse_cpu("5E-1"), 500.0);
        assert_eq!(parse_cpu(""), 0.0);
        assert_eq!(parse_cpu("abc"), 0.0);
    }

    #[test]
    fn bytes() {
        assert_eq!(parse_bytes("1000"), 1000.0);
        assert_eq!(parse_bytes("1e3"), 1000.0);
        assert_eq!(parse_bytes("1E3"), 1000.0);
        assert_eq!(parse_bytes("1E"), 1e18);
        assert_eq!(parse_bytes("2P"), 2e15);
        assert_eq!(parse_bytes("1Pi"), 1024f64.powi(5));
        assert_eq!(parse_bytes("1Ei"), 1024f64.powi(6));
        assert_eq!(parse_bytes("12345Ki"), 12345.0 * 1024.0);
        assert_eq!(parse_bytes("1.5Gi"), 1.5 * 1024f64.powi(3));
        assert_eq!(parse_bytes("500M"), 5e8);
        assert_eq!(parse_bytes("1k"), 1000.0);
        assert_eq!(parse_bytes("1500m"), 1.5);
    }
}

#[cfg(test)]
mod resources_tests {
    use super::*;

    fn spec(v: serde_json::Value) -> podspec::LeanSpec {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn sums_requests_and_limits() {
        let r = pod_resources(&spec(serde_json::json!({"containers": [
            {"resources": {"requests": {"cpu": "250m", "memory": "256Mi"}, "limits": {"cpu": "1", "memory": "512Mi"}}},
            {"resources": {"requests": {"cpu": "250m"}, "limits": {"cpu": "500m"}}},
        ]})));
        assert_eq!(r.cpu, (Some(500.0), Some(1500.0)));
        // A container without a memory limit: the pod has none.
        assert_eq!(r.mem, (Some(256.0 * 1024.0 * 1024.0), None));
        assert_eq!(pod_resources(&spec(serde_json::json!({"containers": [{}]}))), PodResources::default());
    }

    #[test]
    fn counts_sidecars_and_overhead() {
        let s = spec(serde_json::json!({
            "initContainers": [
                {"restartPolicy": "Always", "resources": {"requests": {"cpu": "50m"}, "limits": {"cpu": "150m"}}},
                // A regular init container has finished by the time usage is read.
                {"resources": {"requests": {"cpu": "1"}, "limits": {"cpu": "2"}}},
            ],
            "containers": [{"resources": {"requests": {"cpu": "100m"}, "limits": {"cpu": "200m"}}}],
            "overhead": {"cpu": "10m"},
        }));
        assert_eq!(pod_resources(&s).cpu, (Some(160.0), Some(360.0)));
    }

    #[test]
    fn pods_allocated_counts_like_the_scheduler() {
        let pod = serde_json::json!({"spec": {
            "initContainers": [{"resources": {"requests": {"cpu": "2"}}}],
            "containers": [{"resources": {"requests": {"cpu": "500m"}}}],
        }});
        let done = serde_json::json!({"status": {"phase": "Succeeded"}, "spec": {"containers": [{"resources": {"requests": {"cpu": "1"}}}]}});
        assert_eq!(pods_allocated(&[pod, done], "cpu"), (2000.0, 0.0));
    }
}
