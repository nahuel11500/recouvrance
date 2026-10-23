//! Usage history from the cluster's Prometheus, read through the API server's
//! service proxy (so it works wherever `kubectl` does, with no port-forward).

use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use k8s_openapi::api::core::v1::Service;
use kube::{Api, Client, api::ListParams};

use crate::k8s::run;

/// A lookup: the target found (or not), and when.
type Lookup = (Option<String>, Instant);

/// `namespace/service:port`, found once per cluster (keyed by context).
static FOUND: LazyLock<Mutex<HashMap<String, Lookup>>> = LazyLock::new(Default::default);

/// "No Prometheus" is asked again after this long: one may have been installed since.
const RETRY_MISSING: Duration = Duration::from_secs(300);

/// A cached lookup, unless it found nothing and is old enough to try again.
fn cached(entry: Option<&Lookup>, now: Instant) -> Option<Option<String>> {
    match entry? {
        (None, at) if now.duration_since(*at) >= RETRY_MISSING => None,
        (found, _) => Some(found.clone()),
    }
}

/// Service names that usually serve the Prometheus query API, best first.
const CANDIDATES: [&str; 10] = [
    "kube-prometheus-stack-prometheus",
    "prometheus-operated",
    "prometheus-k8s",
    "prometheus-server",
    "prometheus",
    "vmselect",
    "thanos-query",
    "victoria-metrics-single-server",
    "vmsingle",
    "victoria-metrics",
];

async fn find(client: Client) -> Result<Option<String>> {
    let api: Api<Service> = Api::all(client);
    let list = api.list_metadata(&ListParams::default()).await?;
    let mut best: Option<(usize, String)> = None;
    for s in list.items {
        let (Some(name), Some(ns)) = (s.metadata.name.clone(), s.metadata.namespace.clone()) else { continue };
        let Some(rank) = CANDIDATES.iter().position(|c| name == *c || name.ends_with(&format!("-{c}"))) else {
            continue;
        };
        if best.as_ref().is_none_or(|(r, _)| rank < *r) {
            best = Some((rank, format!("{ns}/{name}")));
        }
    }
    let Some((_, target)) = best else { return Ok(None) };
    // Port: 9090 for Prometheus, 8481/8428 for VictoriaMetrics, 9090 for Thanos query too.
    let (ns, name) = target.split_once('/').unwrap_or_default();
    let svc = Api::<Service>::namespaced(api_client(&api), ns).get(name).await?;
    let ports: Vec<i32> = svc.spec.and_then(|s| s.ports).unwrap_or_default().iter().map(|p| p.port).collect();
    let port = [9090, 8481, 8428, 10902, 80]
        .into_iter()
        .find(|p| ports.contains(p))
        .or(ports.first().copied())
        .unwrap_or(9090);
    // VictoriaMetrics cluster: vmselect serves the Prometheus API under /select/<tenant>/prometheus.
    let prefix = if name.contains("vmselect") { "/select/0/prometheus" } else { "" };
    Ok(Some(format!("{ns}/{name}:{port}{prefix}")))
}

fn api_client(api: &Api<Service>) -> Client {
    api.clone().into_client()
}

/// The Prometheus service for this cluster, if any (cached; `override_target`
/// from settings wins).
pub async fn target(context: &str, client: Client) -> Result<Option<String>> {
    if !crate::settings::metrics_on(context) {
        return Ok(None);
    }
    if let Some(t) = crate::settings::get().prometheus.get(context).filter(|t| !t.is_empty()) {
        return Ok(Some(t.clone()));
    }
    if let Some(found) = cached(FOUND.lock().unwrap().get(context), Instant::now()) {
        return Ok(found);
    }
    let found = run(find(client)).await?;
    FOUND.lock().unwrap().insert(context.to_string(), (found.clone(), Instant::now()));
    Ok(found)
}

/// One series from a range query: (unix seconds, value).
pub type Series = Vec<(i64, f64)>;

/// Points of several series added up by time, in time order.
fn sum_series(all: impl IntoIterator<Item = (i64, f64)>) -> Series {
    let mut by_time: BTreeMap<i64, f64> = BTreeMap::new();
    for (t, val) in all {
        *by_time.entry(t).or_default() += val;
    }
    by_time.into_iter().collect()
}

/// `query_range` over the last `minutes`, summed into one series.
pub async fn range(client: Client, target: String, promql: String, minutes: i64, step_secs: i64) -> Result<Series> {
    let all = range_by(client, target, promql, minutes, step_secs, "").await?;
    Ok(sum_series(all.into_values().flatten()))
}

/// `query_range` keeping one series per value of `label` (summed when several share it).
pub async fn range_by(
    client: Client,
    target: String,
    promql: String,
    minutes: i64,
    step_secs: i64,
    label: &str,
) -> Result<HashMap<String, Series>> {
    let label = label.to_string();
    run(async move {
        let end = jiff::Timestamp::now().as_second();
        let start = end - minutes * 60;
        let mut q = form_urlencoded(&[
            ("query", &promql),
            ("start", &start.to_string()),
            ("end", &end.to_string()),
            ("step", &step_secs.to_string()),
        ]);
        q.insert(0, '?');
        let text = match proxy_path(&target)? {
            Some(path) => {
                let req = http::Request::get(format!("{path}/api/v1/query_range{q}")).body(Vec::new())?;
                client.request_text(req).await?
            }
            None => {
                crate::assistant::get_url(format!("{}/api/v1/query_range{q}", target.trim_end_matches('/'))).await?
            }
        };
        let v: serde_json::Value = serde_json::from_str(&text)?;
        if v["status"] != "success" {
            bail!("prometheus: {}", v["error"].as_str().unwrap_or("query failed"));
        }
        let mut points: HashMap<String, Vec<(i64, f64)>> = HashMap::new();
        for r in v["data"]["result"].as_array().into_iter().flatten() {
            let key = r["metric"][label.as_str()].as_str().unwrap_or_default().to_string();
            let series = points.entry(key).or_default();
            for p in r["values"].as_array().into_iter().flatten() {
                let t = p[0].as_f64().unwrap_or(0.0) as i64;
                let val: f64 = p[1].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                series.push((t, val));
            }
        }
        Ok(points.into_iter().map(|(k, p)| (k, sum_series(p))).collect())
    })
    .await
}

/// CPU (millicores) and memory (bytes) of each container of a pod over the last `minutes`.
pub async fn container_usage(
    client: Client,
    target: String,
    ns: &str,
    pod: &str,
    minutes: i64,
) -> Result<HashMap<String, (Series, Series)>> {
    let sel = format!(r#"namespace="{ns}",pod="{pod}",container!="",container!="POD""#);
    let step = (minutes * 60 / 120).max(15);
    let cpu = range_by(
        client.clone(),
        target.clone(),
        format!("sum by (container) (rate(container_cpu_usage_seconds_total{{{sel}}}[{}s]))", step.max(300)),
        minutes,
        step,
        "container",
    );
    let mem = range_by(
        client,
        target,
        format!("sum by (container) (container_memory_working_set_bytes{{{sel}}})"),
        minutes,
        step,
        "container",
    );
    let (cpu, mem) = futures::join!(cpu, mem);
    let (cpu, mut mem) = (cpu?, mem?);
    let mut out = HashMap::new();
    for (name, c) in cpu {
        let m = mem.remove(&name).unwrap_or_default();
        out.insert(name, (c.into_iter().map(|(t, v)| (t, v * 1000.0)).collect(), m));
    }
    for (name, m) in mem {
        out.insert(name, (Vec::new(), m));
    }
    Ok(out)
}

/// The API server proxy path for `namespace/service:port[/path]` (VictoriaMetrics
/// vmselect needs `/select/0/prometheus`); None for a direct `http(s)://` URL.
pub fn proxy_path(target: &str) -> Result<Option<String>> {
    if target.starts_with("http://") || target.starts_with("https://") {
        return Ok(None);
    }
    let (ns, rest) = target.split_once('/').context("target is namespace/service:port[/path] or an http(s) URL")?;
    let (svc, prefix) = match rest.split_once('/') {
        Some((svc, p)) => (svc, format!("/{}", p.trim_end_matches('/'))),
        None => (rest, String::new()),
    };
    Ok(Some(format!("/api/v1/namespaces/{ns}/services/{svc}/proxy{prefix}")))
}

fn form_urlencoded(pairs: &[(&str, &str)]) -> String {
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    pairs.iter().map(|(k, v)| format!("{}={}", enc(k), enc(v))).collect::<Vec<_>>().join("&")
}

/// Pods a workload's usage charts cover at most: each name goes in the query URL.
pub const MAX_CHART_PODS: usize = 300;

/// A regex matching exactly these pod names, escaped for a PromQL string (`pod=~"…"`).
pub fn pods_regex(pods: &[String]) -> String {
    pods.iter().map(|p| regex::escape(p).replace('\\', "\\\\")).collect::<Vec<_>>().join("|")
}

/// CPU (cores) and memory (bytes) of a set of pods over the last `minutes`.
pub async fn pod_usage(
    client: Client,
    target: String,
    ns: &str,
    pods: &[String],
    minutes: i64,
) -> Result<(Series, Series)> {
    let sel = format!(r#"namespace="{ns}",pod=~"{}",container!="",container!="POD""#, pods_regex(pods));
    let step = (minutes * 60 / 120).max(15);
    let cpu = range(
        client.clone(),
        target.clone(),
        format!("sum(rate(container_cpu_usage_seconds_total{{{sel}}}[{}s]))", step.max(300)),
        minutes,
        step,
    );
    let mem = range(client, target, format!("sum(container_memory_working_set_bytes{{{sel}}})"), minutes, step);
    let (cpu, mem) = futures::join!(cpu, mem);
    Ok((cpu?, mem?))
}

/// CPU (cores) and memory (bytes) used by every container in the cluster.
pub async fn cluster_usage(client: Client, target: String, minutes: i64) -> Result<(Series, Series)> {
    let sel = r#"container!="",container!="POD""#;
    let step = (minutes * 60 / 120).max(15);
    let cpu = range(
        client.clone(),
        target.clone(),
        format!("sum(rate(container_cpu_usage_seconds_total{{{sel}}}[{}s]))", step.max(300)),
        minutes,
        step,
    );
    let mem = range(client, target, format!("sum(container_memory_working_set_bytes{{{sel}}})"), minutes, step);
    let (cpu, mem) = futures::join!(cpu, mem);
    Ok((cpu?, mem?))
}

/// CPU (cores) and memory (bytes) used by all containers on a node.
pub async fn node_usage(client: Client, target: String, node: &str, minutes: i64) -> Result<(Series, Series)> {
    let sel = format!(r#"node="{node}",container!="",container!="POD""#);
    let step = (minutes * 60 / 120).max(15);
    let cpu = range(
        client.clone(),
        target.clone(),
        format!("sum(rate(container_cpu_usage_seconds_total{{{sel}}}[{}s]))", step.max(300)),
        minutes,
        step,
    );
    let mem = range(client, target, format!("sum(container_memory_working_set_bytes{{{sel}}})"), minutes, step);
    let (cpu, mem) = futures::join!(cpu, mem);
    Ok((cpu?, mem?))
}

/// GPU use from NVIDIA's DCGM exporter: average utilization (%), memory used and total (bytes),
/// and how many GPUs were seen.
#[derive(Clone, Debug, Default)]
pub struct GpuUsage {
    pub util: Series,
    pub mem: Series,
    pub mem_total: Option<f64>,
    pub gpus: usize,
}

/// What GPU metrics are about: some pods, a node, or the whole cluster.
pub enum GpuScope<'a> {
    Pods { ns: &'a str, pods: &'a [String] },
    Node(&'a str),
    Cluster,
}

/// DCGM selectors for a scope. The pod using a GPU is `exported_namespace`/`exported_pod` when
/// Prometheus scrapes the exporter as a target (the usual ServiceMonitor), or `namespace`/`pod`
/// with honorLabels; nodes are `hostname` (or `Hostname`, `node`, `kubernetes_node`).
pub fn gpu_selectors(scope: &GpuScope) -> Vec<String> {
    match scope {
        GpuScope::Pods { ns, pods } => {
            let re = pods_regex(pods);
            // Without honorLabels, namespace/pod name the exporter itself, never a workload.
            vec![
                format!(r#"exported_namespace="{ns}",exported_pod=~"{re}""#),
                format!(r#"namespace="{ns}",pod=~"{re}",container!="nvidia-dcgm-exporter""#),
            ]
        }
        GpuScope::Node(node) => vec![
            format!(r#"hostname="{node}""#),
            format!(r#"Hostname="{node}""#),
            format!(r#"node="{node}""#),
            format!(r#"kubernetes_node="{node}""#),
        ],
        GpuScope::Cluster => vec![String::new()],
    }
}

/// `agg(metric{sel1}) or agg(metric{sel2})…`: the first label scheme that has data wins.
fn gpu_query(agg: &str, metric: &str, selectors: &[String]) -> String {
    selectors.iter().map(|sel| format!("{agg}({metric}{{{sel}}})")).collect::<Vec<_>>().join(" or ")
}

/// GPU history for a scope; None when there is no GPU data (no GPUs, or no DCGM exporter).
pub async fn gpu_usage(client: Client, target: String, scope: GpuScope<'_>, minutes: i64) -> Option<GpuUsage> {
    let sel = gpu_selectors(&scope);
    let step = (minutes * 60 / 120).max(15);
    let util = range(client.clone(), target.clone(), gpu_query("avg", "DCGM_FI_DEV_GPU_UTIL", &sel), minutes, step);
    let used = range(client.clone(), target.clone(), gpu_query("sum", "DCGM_FI_DEV_FB_USED", &sel), minutes, step);
    let free = range(client.clone(), target.clone(), gpu_query("sum", "DCGM_FI_DEV_FB_FREE", &sel), minutes, step);
    let count = range(client, target, gpu_query("count", "DCGM_FI_DEV_GPU_UTIL", &sel), minutes, step);
    let (util, used, free, count) = futures::join!(util, used, free, count);
    let util = util.ok()?;
    if util.is_empty() {
        return None;
    }
    // DCGM reports framebuffer memory in MiB.
    let mib = |s: Series| s.into_iter().map(|(t, v)| (t, v * 1024.0 * 1024.0)).collect::<Series>();
    let mem = mib(used.unwrap_or_default());
    let mem_total = match (mem.last(), free.ok().and_then(|f| f.last().copied())) {
        (Some((_, u)), Some((_, f))) => Some(u + f * 1024.0 * 1024.0),
        _ => None,
    };
    let gpus = count.ok().and_then(|c| c.last().map(|(_, n)| *n as usize)).unwrap_or(0);
    Some(GpuUsage { util, mem, mem_total, gpus })
}

#[cfg(test)]
mod gpu_tests {
    #[test]
    fn queries() {
        use super::{GpuScope, gpu_query, gpu_selectors};
        let pods = ["trainer".to_string()];
        let sel = gpu_selectors(&GpuScope::Pods { ns: "ml", pods: &pods });
        assert_eq!(
            gpu_query("avg", "DCGM_FI_DEV_GPU_UTIL", &sel),
            r#"avg(DCGM_FI_DEV_GPU_UTIL{exported_namespace="ml",exported_pod=~"trainer"}) or avg(DCGM_FI_DEV_GPU_UTIL{namespace="ml",pod=~"trainer",container!="nvidia-dcgm-exporter"})"#
        );
        assert_eq!(
            gpu_query("sum", "DCGM_FI_DEV_FB_USED", &gpu_selectors(&GpuScope::Cluster)),
            "sum(DCGM_FI_DEV_FB_USED{})"
        );
    }
}

#[cfg(test)]
mod target_tests {
    #[test]
    fn targets() {
        use super::proxy_path;
        assert_eq!(
            proxy_path("monitoring/prom:9090").unwrap().unwrap(),
            "/api/v1/namespaces/monitoring/services/prom:9090/proxy"
        );
        assert_eq!(
            proxy_path("vm/vmselect:8481/select/0/prometheus/").unwrap().unwrap(),
            "/api/v1/namespaces/vm/services/vmselect:8481/proxy/select/0/prometheus"
        );
        assert!(proxy_path("https://vm.example/select/0/prometheus").unwrap().is_none());
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    #[test]
    fn sums_series_by_time() {
        let summed = super::sum_series([(30, 1.0), (10, 2.0), (30, 0.5), (20, 4.0), (10, 1.0)]);
        assert_eq!(summed, [(10, 3.0), (20, 4.0), (30, 1.5)]);
        assert!(super::sum_series([]).is_empty());
    }

    #[test]
    fn missing_prometheus_is_retried() {
        let start = Instant::now();
        let later = start + super::RETRY_MISSING + Duration::from_secs(1);
        let found = (Some("monitoring/prom:9090".to_string()), start);
        let missing = (None, start);
        assert_eq!(super::cached(None, start), None, "never looked up");
        assert_eq!(super::cached(Some(&found), later), Some(Some("monitoring/prom:9090".into())), "found: kept");
        assert_eq!(super::cached(Some(&missing), start), Some(None), "missing: cached for a while");
        assert_eq!(super::cached(Some(&missing), later), None, "missing: asked again later");
    }

    #[test]
    fn encodes_queries() {
        assert_eq!(super::form_urlencoded(&[("query", r#"sum(x{a="b"})"#)]), "query=sum%28x%7Ba%3D%22b%22%7D%29");
        assert_eq!(super::pods_regex(&["a.b".into(), "c".into()]), r"a\\.b|c");
    }
}
