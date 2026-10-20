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

/// A Kubernetes quantity, split as (number, power of ten, binary multiplier): a
/// decimal number with an optional exponent (`1e3`, `5E-2`), then an SI suffix
/// (`n u m k M G T P E`) or a binary one (`Ki Mi Gi Ti Pi Ei`). None when the
/// number does not parse; an unknown suffix counts as none.
fn split_quantity(q: &str) -> Option<(f64, i32, f64)> {
    let q = q.trim();
    let end = q
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || c == '.' || (i == 0 && (c == '+' || c == '-'))))
        .map_or(q.len(), |(i, _)| i);
    let (mut num, mut suffix) = q.split_at(end);
    // `e`/`E` and a signed integer is a decimal exponent; a lone `E` is exa.
    if let Some(exp) = suffix.strip_prefix(['e', 'E']) {
        let digits = exp.strip_prefix(['+', '-']).unwrap_or(exp);
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            (num, suffix) = (q, "");
        }
    }
    let n: f64 = num.parse().ok()?;
    Some(match suffix {
        "n" => (n, -9, 1.0),
        "u" => (n, -6, 1.0),
        "m" => (n, -3, 1.0),
        "k" | "K" => (n, 3, 1.0),
        "M" => (n, 6, 1.0),
        "G" => (n, 9, 1.0),
        "T" => (n, 12, 1.0),
        "P" => (n, 15, 1.0),
        "E" => (n, 18, 1.0),
        "Ki" => (n, 0, 1024f64),
        "Mi" => (n, 0, 1024f64.powi(2)),
        "Gi" => (n, 0, 1024f64.powi(3)),
        "Ti" => (n, 0, 1024f64.powi(4)),
        "Pi" => (n, 0, 1024f64.powi(5)),
        "Ei" => (n, 0, 1024f64.powi(6)),
        _ => (n, 0, 1.0),
    })
}

/// The quantity times 10^`shift` (dividing for negative powers keeps "250m" exact).
fn quantity_scaled(q: &str, shift: i32) -> Option<f64> {
    let (n, exp, binary) = split_quantity(q)?;
    let exp = exp + shift;
    let n = n * binary;
    Some(if exp >= 0 { n * 10f64.powi(exp) } else { n / 10f64.powi(-exp) })
}

/// CPU quantity to millicores: "250m", "1", "123456n", "12u", "2k", "1e3".
pub fn parse_cpu(q: &str) -> f64 {
    quantity_scaled(q, 3).unwrap_or(0.0)
}

/// Memory quantity to bytes: "128Mi", "1Gi", "500M", "12345Ki", "1000", "1e9", "1Pi".
pub fn parse_bytes(q: &str) -> f64 {
    quantity_scaled(q, 0).unwrap_or(0.0)
}

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

/// Poll node metrics and allocatable every 15s, retrying (backing off) on errors;
/// nothing while `pause` is set.
pub fn watch_node_metrics(client: Client, pause: Pause) -> (TaskGuard, mpsc::UnboundedReceiver<NodeUsage>) {
    let (tx, rx) = mpsc::unbounded();
    let guard = spawn_guarded(async move {
        let nodes: Api<k8s_openapi::api::core::v1::Node> = Api::all(client.clone());
        let mut failures = 0;
        loop {
            wait_unpaused(&pause).await;
            let Ok(req) = http::Request::get("/apis/metrics.k8s.io/v1beta1/nodes").body(Vec::new()) else { return };
            let (Ok(metrics), Ok(list)) =
                (client.request::<serde_json::Value>(req).await, nodes.list(&ListParams::default()).await)
            else {
                if tx.is_closed() {
                    return;
                }
                retry_wait(&mut failures).await;
                continue;
            };
            failures = 0;
            let mut out = NodeUsage::new();
            for n in list.items {
                let name = n.metadata.name.clone().unwrap_or_default();
                let alloc = n.status.and_then(|s| s.allocatable).unwrap_or_default();
                let cpu = alloc.get("cpu").map(|q| parse_cpu(&q.0)).unwrap_or(0.0);
                let mem = alloc.get("memory").map(|q| parse_bytes(&q.0)).unwrap_or(0.0);
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
        // Sidecars (init containers that keep running) add to the sum; a regular init
        // container runs alongside the sidecars started before it.
        let sidecar = |c: &serde_json::Value| c["restartPolicy"].as_str() == Some("Always");
        let sum = |which: &str| {
            containers.iter().chain(inits.iter().filter(|c| sidecar(c))).map(|c| get(c, which)).sum::<f64>()
        };
        let init = |which: &str| {
            let (mut before, mut peak) = (0.0, 0.0f64);
            for c in inits {
                if sidecar(c) {
                    before += get(c, which);
                } else {
                    peak = peak.max(before + get(c, which));
                }
            }
            peak
        };
        let overhead = p
            .pointer(&format!("/spec/overhead/{}", resource.replace('/', "~1")))
            .and_then(|v| v.as_str())
            .map(parse)
            .unwrap_or(0.0);
        req += sum("requests").max(init("requests")) + overhead;
        lim += sum("limits").max(init("limits")) + overhead;
    }
    (req, lim)
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
