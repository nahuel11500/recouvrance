//! Usage from metrics-server and the kubelet; quantities and allocation.

#[allow(unused_imports)]
use super::*;

/// Pod usage from metrics-server: (namespace, name) -> (millicores, bytes).
pub type PodUsage = HashMap<(String, String), (f64, f64)>;

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

/// Poll metrics-server every 15s. Keeps retrying (backing off) while metrics
/// aren't available; polls nothing while `pause` is set.
pub fn watch_pod_metrics(
    client: Client,
    namespace: Option<String>,
    pause: Pause,
) -> (TaskGuard, mpsc::UnboundedReceiver<PodUsage>) {
    let (tx, rx) = mpsc::unbounded();
    let guard = spawn_guarded(async move {
        let path = match &namespace {
            Some(ns) => format!("/apis/metrics.k8s.io/v1beta1/namespaces/{ns}/pods"),
            None => "/apis/metrics.k8s.io/v1beta1/pods".to_string(),
        };
        let mut failures = 0;
        loop {
            wait_unpaused(&pause).await;
            let Ok(req) = http::Request::get(&path).body(Vec::new()) else { return };
            let Ok(list) = client.request::<serde_json::Value>(req).await else {
                if tx.is_closed() {
                    return;
                }
                retry_wait(&mut failures).await;
                continue;
            };
            failures = 0;
            let mut usage = PodUsage::new();
            for item in list["items"].as_array().into_iter().flatten() {
                let ns = item.pointer("/metadata/namespace").and_then(|v| v.as_str()).unwrap_or_default();
                let name = item.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default();
                let (mut cpu, mut mem) = (0.0, 0.0);
                for c in item["containers"].as_array().into_iter().flatten() {
                    cpu += c.pointer("/usage/cpu").and_then(|v| v.as_str()).map(parse_cpu).unwrap_or(0.0);
                    mem += c.pointer("/usage/memory").and_then(|v| v.as_str()).map(parse_bytes).unwrap_or(0.0);
                }
                usage.insert((ns.to_string(), name.to_string()), (cpu, mem));
            }
            if tx.unbounded_send(usage).is_err() {
                return;
            }
            tokio::time::sleep(crate::settings::secs(crate::settings::get().metrics_refresh)).await;
        }
    });
    (guard, rx)
}

pub use crate::quantity::{parse_bytes, parse_cpu};

pub fn format_cpu(millicores: f64) -> String {
    if millicores >= 1000.0 { format!("{:.2}", millicores / 1000.0) } else { format!("{:.0}m", millicores) }
}

/// CPU for prose and labels: "250m", "1.5 cores", "188 cores".
pub fn format_cores(millicores: f64) -> String {
    match millicores {
        m if m < 1000.0 => format!("{m:.0}m"),
        m if m < 10_000.0 => {
            format!("{} cores", format!("{:.2}", m / 1000.0).trim_end_matches('0').trim_end_matches('.'))
        }
        m => format!("{:.0} cores", m / 1000.0),
    }
}

pub fn format_bytes(bytes: f64) -> String {
    let mi = bytes / (1024.0 * 1024.0);
    if mi >= 1024.0 { format!("{:.2}Gi", mi / 1024.0) } else { format!("{:.0}Mi", mi) }
}

/// Per-container usage of pods in a namespace: pod -> container -> (millicores, bytes).
pub type ContainerUsage = HashMap<String, HashMap<String, (f64, f64)>>;

pub async fn container_usage(
    client: Client,
    namespace: String,
    label_selector: Option<String>,
) -> Result<ContainerUsage> {
    run(async move {
        let mut path = format!("/apis/metrics.k8s.io/v1beta1/namespaces/{namespace}/pods");
        if let Some(l) = &label_selector {
            path.push_str(&format!("?labelSelector={}", url_escape(l)));
        }
        let list: serde_json::Value = client.request(http::Request::get(&path).body(Vec::new())?).await?;
        let mut out = ContainerUsage::new();
        for item in list["items"].as_array().into_iter().flatten() {
            let pod = item.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let containers = out.entry(pod).or_default();
            for c in item["containers"].as_array().into_iter().flatten() {
                let name = c["name"].as_str().unwrap_or_default().to_string();
                let cpu = c.pointer("/usage/cpu").and_then(|v| v.as_str()).map(parse_cpu).unwrap_or(0.0);
                let mem = c.pointer("/usage/memory").and_then(|v| v.as_str()).map(parse_bytes).unwrap_or(0.0);
                containers.insert(name, (cpu, mem));
            }
        }
        Ok(out)
    })
    .await
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

/// Poll node metrics and allocatable every 15s, retrying (backing off) on errors;
/// nothing while `pause` is set.
/// A pod's CPU (millicores) and memory (bytes) requests and limits, summed over its
/// containers. No limit when a container has none: the pod can then use what the node has.
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
    spec: LeanPodSpec,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct LeanPodMeta {
    name: String,
    namespace: String,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct LeanPodSpec {
    containers: Vec<LeanContainer>,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct LeanContainer {
    resources: LeanResources,
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct LeanResources {
    requests: HashMap<String, String>,
    limits: HashMap<String, String>,
}

fn pod_resources(containers: &[LeanContainer]) -> PodResources {
    let sum = |key: &str, of: fn(&LeanResources) -> &HashMap<String, String>, parse: fn(&str) -> f64, all: bool| {
        let vals: Vec<Option<f64>> = containers.iter().map(|c| of(&c.resources).get(key).map(|q| parse(q))).collect();
        if vals.iter().all(Option::is_none) || all && vals.iter().any(Option::is_none) {
            return None;
        }
        Some(vals.into_iter().flatten().sum())
    };
    PodResources {
        cpu: (sum("cpu", |r| &r.requests, parse_cpu, false), sum("cpu", |r| &r.limits, parse_cpu, true)),
        mem: (sum("memory", |r| &r.requests, parse_bytes, false), sum("memory", |r| &r.limits, parse_bytes, true)),
    }
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
            let map: PodResourcesMap = pods
                .into_iter()
                .map(|p| ((p.metadata.namespace, p.metadata.name), pod_resources(&p.spec.containers)))
                .collect();
            if tx.unbounded_send(map).is_err() {
                return;
            }
            tokio::time::sleep((started.elapsed() * 10).max(std::time::Duration::from_secs(60))).await;
        }
    });
    (guard, rx)
}

pub fn watch_node_metrics(client: Client, pause: Pause) -> (TaskGuard, mpsc::UnboundedReceiver<NodeUsage>) {
    let (tx, rx) = mpsc::unbounded();
    let guard = spawn_guarded(async move {
        let nodes = ar_of::<k8s_openapi::api::core::v1::Node>();
        let mut failures = 0;
        loop {
            wait_unpaused(&pause).await;
            let Ok(req) = http::Request::get("/apis/metrics.k8s.io/v1beta1/nodes").body(Vec::new()) else { return };
            let (Ok(metrics), Ok(list)) = (
                client.request::<serde_json::Value>(req).await,
                lists::list_lean::<NodeAllocatable>(client.clone(), nodes.clone(), None).await,
            ) else {
                if tx.is_closed() {
                    return;
                }
                retry_wait(&mut failures).await;
                continue;
            };
            failures = 0;
            let mut out = NodeUsage::new();
            for n in list {
                let name = n.metadata.name.unwrap_or_default();
                let alloc = n.status.allocatable;
                let cpu = alloc.get("cpu").map(|q| parse_cpu(q)).unwrap_or(0.0);
                let mem = alloc.get("memory").map(|q| parse_bytes(q)).unwrap_or(0.0);
                out.insert(name, ((0.0, 0.0), (cpu, mem)));
            }
            for item in metrics["items"].as_array().into_iter().flatten() {
                let name = item.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default();
                let cpu = item.pointer("/usage/cpu").and_then(|v| v.as_str()).map(parse_cpu).unwrap_or(0.0);
                let mem = item.pointer("/usage/memory").and_then(|v| v.as_str()).map(parse_bytes).unwrap_or(0.0);
                if let Some(e) = out.get_mut(name) {
                    e.0 = (cpu, mem);
                }
            }
            if tx.unbounded_send(out).is_err() {
                return;
            }
            tokio::time::sleep(crate::settings::secs(crate::settings::get().metrics_refresh)).await;
        }
    });
    (guard, rx)
}

/// Per-container usage in every namespace: ((namespace, pod), container -> (millicores, bytes)).
pub async fn all_container_usage(client: Client) -> Result<Vec<((String, String), HashMap<String, (f64, f64)>)>> {
    run(async move {
        let list: serde_json::Value =
            client.request(http::Request::get("/apis/metrics.k8s.io/v1beta1/pods").body(Vec::new())?).await?;
        let mut out = Vec::new();
        for item in list["items"].as_array().into_iter().flatten() {
            let ns = item.pointer("/metadata/namespace").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let pod = item.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let mut c = HashMap::new();
            for cont in item["containers"].as_array().into_iter().flatten() {
                let cpu = cont.pointer("/usage/cpu").and_then(|v| v.as_str()).map(parse_cpu).unwrap_or(0.0);
                let mem = cont.pointer("/usage/memory").and_then(|v| v.as_str()).map(parse_bytes).unwrap_or(0.0);
                c.insert(cont["name"].as_str().unwrap_or_default().to_string(), (cpu, mem));
            }
            out.push(((ns, pod), c));
        }
        Ok(out)
    })
    .await
}

/// The kubelet's stats summary for a node (filesystem and per-pod ephemeral storage);
/// needs `nodes/proxy`.
pub async fn node_stats(client: Client, node: String) -> Option<serde_json::Value> {
    get_json(client, format!("/api/v1/nodes/{node}/proxy/stats/summary")).await
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

/// Sum of pod requests and limits for one resource, the way the scheduler counts:
/// per pod, the larger of its containers' sum and any init container, plus overhead.
/// Finished pods are left out.
pub fn pods_allocated(pods: &[serde_json::Value], resource: &str) -> (f64, f64) {
    let parse = |q: &str| match resource {
        "cpu" => parse_cpu(q),
        "memory" | "ephemeral-storage" => parse_bytes(q),
        _ => q.parse::<f64>().unwrap_or_else(|_| parse_bytes(q)),
    };
    let get = |c: &serde_json::Value, which: &str| {
        c.pointer(&format!("/resources/{which}/{}", resource.replace('/', "~1")))
            .and_then(|v| v.as_str())
            .map(parse)
            .unwrap_or(0.0)
    };
    let (mut req, mut lim) = (0.0, 0.0);
    for p in pods {
        if matches!(p.pointer("/status/phase").and_then(|v| v.as_str()), Some("Succeeded" | "Failed")) {
            continue;
        }
        let containers = p.pointer("/spec/containers").and_then(|c| c.as_array()).map(Vec::as_slice).unwrap_or(&[]);
        let inits = p.pointer("/spec/initContainers").and_then(|c| c.as_array()).map(Vec::as_slice).unwrap_or(&[]);
        let sidecar = |c: &serde_json::Value| c["restartPolicy"].as_str() == Some("Always");
        let pod = |which: &str| {
            pod_amount(
                containers.iter().map(|c| get(c, which)).sum(),
                inits.iter().map(|c| (sidecar(c), get(c, which))),
            )
        };
        let overhead = p
            .pointer(&format!("/spec/overhead/{}", resource.replace('/', "~1")))
            .and_then(|v| v.as_str())
            .map(parse)
            .unwrap_or(0.0);
        req += pod("requests") + overhead;
        lim += pod("limits") + overhead;
    }
    (req, lim)
}

/// One pod's request or limit as the scheduler counts it, from its containers' sum and its
/// init containers in order as (is a sidecar, amount): sidecars (init containers that keep
/// running) add to the sum, and a regular init container runs alongside the sidecars
/// started before it. Overhead is not included.
pub fn pod_amount(containers: f64, inits: impl IntoIterator<Item = (bool, f64)>) -> f64 {
    let (mut sidecars, mut peak) = (0.0, 0.0f64);
    for (sidecar, amount) in inits {
        if sidecar {
            sidecars += amount;
        } else {
            peak = peak.max(sidecars + amount);
        }
    }
    (containers + sidecars).max(peak)
}

#[cfg(test)]
mod quantity_tests {
    use super::{parse_bytes, parse_cpu};

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

    fn container(requests: &[(&str, &str)], limits: &[(&str, &str)]) -> LeanContainer {
        let map = |kv: &[(&str, &str)]| kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        LeanContainer { resources: LeanResources { requests: map(requests), limits: map(limits) } }
    }

    #[test]
    fn sums_requests_and_limits() {
        let r = pod_resources(&[
            container(&[("cpu", "250m"), ("memory", "256Mi")], &[("cpu", "1"), ("memory", "512Mi")]),
            container(&[("cpu", "250m")], &[("cpu", "500m")]),
        ]);
        assert_eq!(r.cpu, (Some(500.0), Some(1500.0)));
        // A container without a memory limit: the pod has none.
        assert_eq!(r.mem, (Some(256.0 * 1024.0 * 1024.0), None));
        assert_eq!(pod_resources(&[container(&[], &[])]), PodResources::default());
    }

    #[test]
    fn pod_amount_counts_like_the_scheduler() {
        assert_eq!(pod_amount(500.0, []), 500.0);
        // Sidecars run with the containers.
        assert_eq!(pod_amount(500.0, [(true, 100.0)]), 600.0);
        // An init container runs alone, or next to the sidecars started before it.
        assert_eq!(pod_amount(500.0, [(false, 2000.0)]), 2000.0);
        assert_eq!(pod_amount(500.0, [(true, 100.0), (false, 2000.0)]), 2100.0);
        assert_eq!(pod_amount(500.0, [(false, 2000.0), (true, 100.0)]), 2000.0);
    }
}
