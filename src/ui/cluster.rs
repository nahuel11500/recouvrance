//! Cluster overview: capacity, usage, node health and top namespaces, per cluster.

use std::collections::HashMap;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use k8s_openapi::api::core::v1 as core;
use kube::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::details::OpenObject;
use crate::k8s;

#[derive(Default)]
struct Summary {
    context: String,
    /// None when `/version` failed.
    version: Option<String>,
    /// Whether nodes, pods and node metrics could be read: the numbers from them are
    /// shown as unavailable otherwise, not as 0.
    nodes_known: bool,
    pods_known: bool,
    usage_known: bool,
    nodes: usize,
    nodes_ready: usize,
    kubelet_versions: Vec<(String, usize)>,
    pods_running: usize,
    pods_total: usize,
    namespaces: Option<usize>,
    /// (allocatable, requested, used) in millicores / bytes.
    cpu: (f64, f64, f64),
    mem: (f64, f64, f64),
    /// Node name and the problems it reports.
    node_issues: Vec<(String, Vec<String>)>,
    /// namespace -> (cpu used, mem used)
    top_ns: Vec<(String, f64, f64)>,
    /// CPU (millicores) and memory over the chosen range, from Prometheus.
    history: Option<(crate::prom::Series, crate::prom::Series)>,
    /// The range the charts cover, in minutes: the one asked for when they were read, which
    /// the setting may have moved from since.
    chart_minutes: i64,
    /// GPUs: (allocatable, requested) over every GPU resource (nvidia.com/gpu, amd.com/gpu…).
    gpus: (f64, f64),
    /// GPU utilization and memory, from the DCGM exporter.
    gpu: Option<crate::prom::GpuUsage>,
    /// The parts that could not be read, e.g. "Nodes: forbidden".
    errors: Vec<String>,
    /// Set when nothing could be read: the cluster is out of reach.
    error: Option<String>,
}

pub struct ClusterOverview {
    summaries: Option<Vec<Summary>>,
    _task: Task<()>,
}

impl EventEmitter<OpenObject> for ClusterOverview {}

impl ClusterOverview {
    pub fn new(clusters: Vec<(String, Client)>, cx: &mut Context<Self>) -> Self {
        let task = cx.spawn(async move |this, cx| {
            loop {
                let exec = cx.background_executor().clone();
                if !crate::app::wait_visible(&this, cx, &exec).await {
                    break;
                }
                let started = std::time::Instant::now();
                // The numbers as soon as the API servers answer; the charts when Prometheus
                // does (on a big cluster it takes several times longer).
                let (this2, mut cx2) = (this.clone(), cx.clone());
                let numbers = async {
                    let mut summaries =
                        futures::future::join_all(clusters.iter().map(|(ctx, c)| load(ctx.clone(), c.clone()))).await;
                    this2.update(&mut cx2, |this, cx| {
                        // Keep the charts shown until the new ones come.
                        for (new, old) in summaries.iter_mut().zip(this.summaries.iter_mut().flatten()) {
                            if new.context == old.context {
                                new.history = old.history.take();
                                new.gpu = old.gpu.take();
                                new.chart_minutes = old.chart_minutes;
                            }
                        }
                        this.summaries = Some(summaries);
                        cx.notify();
                    })
                };
                let charts = futures::future::join_all(clusters.iter().map(|(ctx, c)| usage(ctx, c.clone())));
                let (shown, charts) = futures::join!(numbers, charts);
                let took = started.elapsed();
                if shown.is_err()
                    || this
                        .update(cx, |this, cx| {
                            for (s, (history, gpu, minutes)) in this.summaries.iter_mut().flatten().zip(charts) {
                                s.history = history;
                                s.gpu = gpu;
                                s.chart_minutes = minutes;
                            }
                            cx.notify();
                        })
                        .is_err()
                {
                    break;
                }
                // Big clusters take long to list; don't hammer their API servers.
                let wait = (took * 2).max(crate::settings::secs(crate::settings::get().overview_refresh));
                cx.background_executor().timer(wait).await;
            }
        });
        Self { summaries: None, _task: task }
    }
}

/// GPUs in a resource list: every extended resource whose name ends in `/gpu`.
fn gpu_count(resources: &HashMap<String, String>) -> f64 {
    resources.iter().filter(|(k, _)| k.ends_with("/gpu")).filter_map(|(_, v)| v.parse::<f64>().ok()).sum()
}

fn st<'a>(v: &'a Value, p: &str) -> &'a str {
    v.pointer(p).and_then(Value::as_str).unwrap_or_default()
}

// Only the fields the overview reads: big clusters' pods and nodes are never held as JSON.

#[derive(Deserialize, Default)]
#[serde(default)]
struct Named {
    name: String,
    namespace: String,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct NodeLite {
    metadata: Named,
    spec: NodeSpecLite,
    status: NodeStatusLite,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct NodeSpecLite {
    unschedulable: bool,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct NodeStatusLite {
    conditions: Vec<CondLite>,
    node_info: NodeInfoLite,
    allocatable: HashMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct NodeInfoLite {
    kubelet_version: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CondLite {
    #[serde(rename = "type")]
    type_: String,
    status: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PodLite {
    spec: k8s::podspec::LeanSpec,
    status: PodStatusLite,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct PodStatusLite {
    phase: String,
}

/// metrics.k8s.io lists: usage per node, or per pod container.
#[derive(Deserialize, Default)]
#[serde(default)]
struct MetricsList {
    items: Vec<MetricsItem>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct MetricsItem {
    metadata: Named,
    usage: HashMap<String, String>,
    containers: Vec<MetricsContainer>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct MetricsContainer {
    usage: HashMap<String, String>,
}

fn quantity<'a>(m: &'a HashMap<String, String>, key: &str) -> &'a str {
    m.get(key).map(String::as_str).unwrap_or_default()
}

/// Nodes into the summary: readiness, problems, kubelet versions and allocatable.
fn add_nodes(s: &mut Summary, nodes: &[NodeLite]) {
    s.nodes = nodes.len();
    let mut versions: HashMap<String, usize> = HashMap::new();
    for n in nodes {
        let conds = &n.status.conditions;
        let ready = conds.iter().any(|c| c.type_ == "Ready" && c.status == "True");
        if ready {
            s.nodes_ready += 1;
        }
        let mut issues: Vec<String> =
            // The conditions whose True is bad (pressure, node-problem-detector): a custom
            // condition that reads True when healthy is no issue.
            conds.iter().filter(|c| crate::status::true_is_bad(&c.type_) && c.status == "True").map(|c| c.type_.clone()).collect();
        if !ready {
            issues.insert(0, "NotReady".into());
        }
        if n.spec.unschedulable {
            issues.push("Cordoned".into());
        }
        if !issues.is_empty() {
            s.node_issues.push((n.metadata.name.clone(), issues));
        }
        *versions.entry(n.status.node_info.kubelet_version.clone()).or_default() += 1;
        s.cpu.0 += k8s::parse_cpu(quantity(&n.status.allocatable, "cpu"));
        s.mem.0 += k8s::parse_bytes(quantity(&n.status.allocatable, "memory"));
        s.gpus.0 += gpu_count(&n.status.allocatable);
    }
    let mut versions: Vec<(String, usize)> = versions.into_iter().collect();
    versions.sort();
    s.kubelet_versions = versions;
}

/// Pods into the summary: how many run, and what running or pending ones request.
fn add_pods(s: &mut Summary, pods: &[PodLite]) {
    s.pods_total = pods.len();
    for p in pods {
        let phase = p.status.phase.as_str();
        if phase == "Running" {
            s.pods_running += 1;
        }
        if phase == "Running" || phase == "Pending" {
            let spec = &p.spec;
            // As the scheduler counts: sidecars and init containers too, plus overhead.
            let scheduled = |r: &str| spec.effective(r, k8s::podspec::Count::Scheduled);
            s.cpu.1 += scheduled("cpu").request;
            s.mem.1 += scheduled("memory").request;
            s.gpus.1 +=
                spec.resource_names().iter().filter(|r| r.ends_with("/gpu")).map(|r| scheduled(r).limit).sum::<f64>();
        }
    }
}

/// A metrics.k8s.io list, or None when metrics are off or metrics-server didn't answer
/// (missing, down or refused): usage is then unknown, not zero.
async fn metrics(on: bool, client: Client, path: &'static str) -> Option<MetricsList> {
    if !on {
        return None;
    }
    k8s::run(async move { Ok(client.request(http::Request::get(path).body(Vec::new())?).await?) }).await.ok()
}

/// The cluster overview's load, for the dev smoke run's timings.
#[cfg(feature = "dev")]
pub async fn debug_load(context: String, client: Client) -> usize {
    let t = std::time::Instant::now();
    let numbers = async {
        let s = load(context.clone(), client.clone()).await;
        println!("cluster overview numbers: {:?}", t.elapsed());
        s
    };
    let charts = async {
        usage(&context, client.clone()).await;
        println!("cluster overview charts: {:?}", t.elapsed());
    };
    futures::join!(numbers, charts).0.pods_running
}

/// CPU and memory over the chosen range, and GPUs, from Prometheus (when there is one).
/// With the range it covers, in minutes.
type Usage = (Option<(crate::prom::Series, crate::prom::Series)>, Option<crate::prom::GpuUsage>, i64);

async fn usage(context: &str, client: Client) -> Usage {
    let minutes = crate::settings::get().usage_range;
    let Ok(Some(target)) = crate::prom::target(context, client.clone()).await else { return (None, None, minutes) };
    let (history, gpu) = futures::join!(
        crate::prom::cluster_usage(client.clone(), target.clone(), minutes),
        crate::prom::gpu_usage(client, target, crate::prom::GpuScope::Cluster, minutes),
    );
    (history.ok().map(|(cpu, mem)| (cpu.into_iter().map(|(t, v)| (t, v * 1000.0)).collect(), mem)), gpu, minutes)
}

/// Everything but the charts, all at once: one request after the other, a big cluster took
/// over 30 s.
async fn load(context: String, client: Client) -> Summary {
    let on = crate::settings::metrics_on(&context);
    let mut s = Summary { context, ..Default::default() };
    let c = client.clone();
    let version = k8s::run(async move {
        let v: Value = c.request(http::Request::get("/version").body(Vec::new())?).await?;
        Ok(st(&v, "/gitVersion").to_string())
    });
    let (version, nodes, pods, namespaces, node_metrics, pod_metrics) = futures::join!(
        version,
        k8s::lists::list_lean::<NodeLite>(client.clone(), k8s::ar_of::<core::Node>(), None),
        k8s::lists::list_lean_cached::<PodLite>(client.clone(), k8s::ar_of::<core::Pod>(), None),
        k8s::namespaces(client.clone()),
        metrics(on, client.clone(), "/apis/metrics.k8s.io/v1beta1/nodes"),
        metrics(on, client.clone(), "/apis/metrics.k8s.io/v1beta1/pods"),
    );
    // Each part on its own: a user who can't list nodes still sees the pods.
    let failed = |what: &str, e: &anyhow::Error| match k8s::api_status(e) {
        Some(403) => format!("{what}: forbidden"),
        _ => format!("{what}: {}", k8s::short_error(e)),
    };
    match version {
        Ok(v) => s.version = Some(v),
        Err(e) => s.errors.push(failed("Version", &e)),
    }
    match nodes {
        Ok(nodes) => {
            add_nodes(&mut s, &nodes);
            s.nodes_known = true;
        }
        Err(e) => s.errors.push(failed("Nodes", &e)),
    }
    match pods {
        Ok(pods) => {
            add_pods(&mut s, &pods);
            s.pods_known = true;
        }
        Err(e) => s.errors.push(failed("Pods", &e)),
    }
    match namespaces {
        Ok(n) => s.namespaces = Some(n.len()),
        Err(e) => s.errors.push(failed("Namespaces", &e)),
    }

    // Usage: nodes for the totals, pods for the per-namespace ranking.
    if let Some(node_metrics) = node_metrics {
        for n in node_metrics.items {
            s.cpu.2 += k8s::parse_cpu(quantity(&n.usage, "cpu"));
            s.mem.2 += k8s::parse_bytes(quantity(&n.usage, "memory"));
        }
        s.usage_known = true;
    }
    if let Some(pod_metrics) = pod_metrics {
        let mut by_ns: HashMap<String, (f64, f64)> = HashMap::new();
        for p in pod_metrics.items {
            let e = by_ns.entry(p.metadata.namespace).or_default();
            for c in &p.containers {
                e.0 += k8s::parse_cpu(quantity(&c.usage, "cpu"));
                e.1 += k8s::parse_bytes(quantity(&c.usage, "memory"));
            }
        }
        let mut top: Vec<(String, f64, f64)> = by_ns.into_iter().map(|(ns, (c, m))| (ns, c, m)).collect();
        top.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        top.truncate(10);
        s.top_ns = top;
    }
    // Nothing answered: the cluster is out of reach, one line says why.
    if s.version.is_none() && !s.nodes_known && !s.pods_known {
        s.error = s.errors.first().cloned();
    }
    s
}

impl ClusterOverview {
    fn tile(label: &str, value: String, sub: String, cx: &App) -> Div {
        v_flex()
            .w(px(180.))
            .p_3()
            .gap_0p5()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(label.to_string()))
            .child(div().text_2xl().font_weight(FontWeight::SEMIBOLD).child(value))
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(sub))
    }

    /// A tile for a number that could not be read.
    fn unavailable(label: &str, cx: &App) -> Div {
        Self::tile(label, "—".into(), "unavailable".into(), cx)
    }

    /// One bar with usage (solid) and requests (outlined marker) against allocatable. A
    /// part that could not be read (None) is left off and said to be unavailable.
    fn capacity(
        label: &str,
        icon: Lucide,
        (alloc, req, used): (f64, Option<f64>, Option<f64>),
        fmt: fn(f64) -> String,
        color: Hsla,
        cx: &App,
    ) -> Div {
        let t = cx.theme();
        let pct = |v: f64| if alloc > 0.0 { (v / alloc).clamp(0.0, 1.0) } else { 0.0 };
        let req_color = if req.is_some_and(|r| r > alloc * 0.9) { t.warning } else { t.foreground };
        let part = |word: &str, v: Option<f64>| match v {
            Some(v) => format!("{word} {} ({:.0}%)", fmt(v), pct(v) * 100.0),
            None => format!("{word} unavailable"),
        };
        v_flex()
            .flex_1()
            .min_w(px(320.))
            .gap_1()
            .child(
                h_flex().gap_1().text_sm().child(Icon::new(icon).size_4().text_color(color)).child(label.to_string()),
            )
            .child(
                div()
                    .relative()
                    .h(px(14.))
                    .w_full()
                    .rounded_md()
                    .bg(t.secondary)
                    .when_some(used, |d, used| {
                        d.child(
                            div()
                                .absolute()
                                .left_0()
                                .top_0()
                                .h_full()
                                .rounded_md()
                                .bg(color)
                                .w(relative(pct(used) as f32)),
                        )
                    })
                    .when_some(req, |d, req| {
                        d.child(
                            div()
                                .absolute()
                                .top(px(-3.))
                                .h(px(20.))
                                .w(px(2.))
                                .bg(req_color)
                                .left(relative(pct(req) as f32)),
                        )
                    }),
            )
            .child(
                h_flex()
                    .gap_4()
                    .text_xs()
                    .text_color(t.muted_foreground)
                    .child(part("Used", used))
                    .child(part("Requested", req))
                    .child(format!("Allocatable {}", fmt(alloc))),
            )
    }
}

impl Render for ClusterOverview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let Some(summaries) = &self.summaries else {
            return div()
                .p_4()
                .text_color(theme.muted_foreground)
                .child("Loading cluster overview…")
                .into_any_element();
        };
        let sections = summaries.iter().enumerate().map(|(si, s)| {
            if let Some(e) = &s.error {
                return v_flex()
                    .gap_1()
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(s.context.clone()))
                    .child(div().text_sm().text_color(theme.danger).child(e.clone()))
                    .into_any_element();
            }
            let versions = s.kubelet_versions.iter().map(|(v, n)| format!("{v} ×{n}")).collect::<Vec<_>>().join(", ");
            let max_mem = s.top_ns.iter().map(|t| t.2).fold(1.0, f64::max);
            let bar = |(alloc, req, used): (f64, f64, f64)| {
                (alloc, s.pods_known.then_some(req), s.usage_known.then_some(used))
            };
            v_flex()
                .gap_4()
                .child(
                    h_flex()
                        .gap_2()
                        .items_baseline()
                        .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(s.context.clone()))
                        .when_some(s.version.as_ref(), |d, version| {
                            d.child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("Kubernetes {version}")),
                            )
                        }),
                )
                .children(s.errors.iter().map(|e| div().text_sm().text_color(theme.danger).child(e.clone())))
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_3()
                        .child(if s.nodes_known {
                            Self::tile(
                                "Nodes",
                                format!("{}/{}", s.nodes_ready, s.nodes),
                                if s.nodes_ready == s.nodes {
                                    "all ready".into()
                                } else {
                                    format!("{} not ready", s.nodes - s.nodes_ready)
                                },
                                cx,
                            )
                        } else {
                            Self::unavailable("Nodes", cx)
                        })
                        .child(if s.pods_known {
                            Self::tile("Pods", s.pods_running.to_string(), format!("running of {}", s.pods_total), cx)
                        } else {
                            Self::unavailable("Pods", cx)
                        })
                        .child(match s.namespaces {
                            Some(n) => Self::tile("Namespaces", n.to_string(), String::new(), cx),
                            None => Self::unavailable("Namespaces", cx),
                        })
                        .child(if s.nodes_known {
                            Self::tile("Kubelet", s.kubelet_versions.len().to_string(), versions, cx)
                        } else {
                            Self::unavailable("Kubelet", cx)
                        }),
                )
                // Allocatable comes from the nodes: without them there is nothing to measure against.
                .when(s.nodes_known, |d| {
                    d.child(
                        h_flex()
                            .flex_wrap()
                            .gap_6()
                            .child(Self::capacity("CPU", Lucide::Cpu, bar(s.cpu), k8s::format_cores, theme.blue, cx))
                            .child(Self::capacity(
                                "Memory",
                                Lucide::MemoryStick,
                                bar(s.mem),
                                k8s::format_bytes,
                                theme.success,
                                cx,
                            )),
                    )
                })
                .when_some(s.history.as_ref(), |d, (cpu, mem)| {
                    use crate::overview::{ChartSpec, usage_chart};
                    let minutes = s.chart_minutes;
                    let (known_req, known_alloc) = (s.pods_known, s.nodes_known);
                    let spec = |title, req, lim, color, fmt, id| ChartSpec {
                        title,
                        req: known_req.then_some(req),
                        req_word: "requested",
                        lim: known_alloc.then_some(lim),
                        lim_word: "allocatable",
                        pin_lim: false,
                        color,
                        fmt,
                        id,
                        minutes,
                    };
                    let avg = |v: &crate::prom::Series| {
                        if v.is_empty() { 0.0 } else { v.iter().map(|p| p.1).sum::<f64>() / v.len() as f64 }
                    };
                    let ratio = |used: f64, req: f64| if req > 0.0 { used / req * 100.0 } else { 0.0 };
                    let (cpu_eff, mem_eff) = (ratio(avg(cpu), s.cpu.1), ratio(avg(mem), s.mem.1));
                    let tone = |p: f64| if p < 30.0 { theme.warning } else { theme.muted_foreground };
                    // Requests come from the pods: without them the share is unknown.
                    d.when(known_req, |d| {
                        d.child(
                            h_flex()
                                .gap_1()
                                .text_sm()
                                .text_color(theme.muted_foreground)
                                .child("Over this range, workloads used on average")
                                .child(
                                    div()
                                        .text_color(tone(cpu_eff))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(format!("{cpu_eff:.0}% of the CPU")),
                                )
                                .child("and")
                                .child(
                                    div()
                                        .text_color(tone(mem_eff))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(format!("{mem_eff:.0}% of the memory")),
                                )
                                .child("they request."),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_3()
                            .child(usage_chart(
                                cpu,
                                spec(
                                    "CPU used",
                                    s.cpu.1,
                                    s.cpu.0,
                                    theme.blue,
                                    k8s::format_cores as fn(f64) -> String,
                                    "cl-cpu",
                                ),
                                cx,
                            ))
                            .child(usage_chart(
                                mem,
                                spec(
                                    "Memory used",
                                    s.mem.1,
                                    s.mem.0,
                                    theme.success,
                                    k8s::format_bytes as fn(f64) -> String,
                                    "cl-mem",
                                ),
                                cx,
                            )),
                    )
                })
                .when(s.gpus.0 > 0.0 || s.gpu.is_some(), |d| {
                    use crate::overview::{ChartSpec, usage_chart};
                    let minutes = s.chart_minutes;
                    let pct = |v: f64| format!("{v:.0}%");
                    let mut row = h_flex().gap_3();
                    if let Some(g) = &s.gpu {
                        row = row
                            .child(usage_chart(
                                &g.util,
                                ChartSpec {
                                    title: "GPU utilization (average)",
                                    req: None,
                                    req_word: "",
                                    lim: Some(100.0),
                                    lim_word: "max",
                                    pin_lim: true,
                                    color: theme.warning,
                                    fmt: pct,
                                    id: "cl-gpu",
                                    minutes,
                                },
                                cx,
                            ))
                            .child(usage_chart(
                                &g.mem,
                                ChartSpec {
                                    title: "GPU memory used",
                                    req: None,
                                    req_word: "",
                                    lim: g.mem_total,
                                    lim_word: "total",
                                    pin_lim: true,
                                    color: theme.danger,
                                    fmt: k8s::format_bytes,
                                    id: "cl-gpu-mem",
                                    minutes,
                                },
                                cx,
                            ));
                    }
                    d.child(div().text_sm().text_color(theme.muted_foreground).child(format!(
                        "GPUs: {:.0} requested of {:.0} allocatable{}",
                        s.gpus.1,
                        s.gpus.0,
                        if s.gpu.is_none() { " · install NVIDIA's DCGM exporter for GPU usage" } else { "" }
                    )))
                    .child(row)
                })
                .child(
                    h_flex()
                        .items_start()
                        .gap_6()
                        .child(
                            v_flex()
                                .w(px(420.))
                                .gap_1()
                                .child(div().text_base().child("Nodes with issues"))
                                .when(s.node_issues.is_empty(), |d| {
                                    d.child(div().text_sm().text_color(theme.muted_foreground).child(
                                        if s.nodes_known {
                                            "None — every node is Ready and healthy."
                                        } else {
                                            "Unavailable."
                                        },
                                    ))
                                })
                                .children(s.node_issues.iter().enumerate().map(|(i, (node, issues))| {
                                    let node_name = node.clone();
                                    let ctx_name = s.context.clone();
                                    h_flex()
                                        .gap_2()
                                        .text_sm()
                                        .child(
                                            div()
                                                .id(("node-issue", si * 1000 + i))
                                                .text_color(theme.blue)
                                                .cursor_pointer()
                                                .on_click(cx.listener(move |_, _, _, cx| {
                                                    cx.emit(OpenObject {
                                                        kind: "nodes".into(),
                                                        name: node_name.clone(),
                                                        namespace: None,
                                                        context: Some(ctx_name.clone()),
                                                    })
                                                }))
                                                .child(node.clone()),
                                        )
                                        .child(div().text_color(theme.warning).child(issues.join(", ")))
                                })),
                        )
                        .child(
                            v_flex()
                                .flex_1()
                                .gap_1()
                                .child(div().text_base().child("Top namespaces by memory"))
                                .when(s.top_ns.is_empty(), |d| {
                                    d.child(div().text_sm().text_color(theme.muted_foreground).child("Unavailable."))
                                })
                                .children(s.top_ns.iter().map(|(ns, cpu, mem)| {
                                    h_flex()
                                        .gap_3()
                                        .text_sm()
                                        .child(
                                            div()
                                                .w(px(220.))
                                                .overflow_hidden()
                                                .text_ellipsis()
                                                .whitespace_nowrap()
                                                .child(ns.clone()),
                                        )
                                        .child(div().w(px(70.)).child(k8s::format_cpu(*cpu)))
                                        .child(div().w(px(80.)).child(k8s::format_bytes(*mem)))
                                        .child(
                                            div().flex_1().h(px(6.)).rounded_full().bg(theme.secondary).child(
                                                div()
                                                    .h_full()
                                                    .rounded_full()
                                                    .bg(theme.success)
                                                    .w(relative((*mem / max_mem) as f32)),
                                            ),
                                        )
                                })),
                        ),
                )
                .into_any_element()
        });
        v_flex()
            .id("cluster-overview")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_10()
            .children(sections)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{NodeLite, PodLite, Summary, add_nodes, add_pods};

    #[test]
    fn summarizes_lean_nodes_and_pods() {
        let nodes: Vec<NodeLite> = serde_json::from_value(serde_json::json!([
            {"metadata": {"name": "n1"}, "spec": {}, "status": {
                "conditions": [{"type": "Ready", "status": "True"}, {"type": "DiskPressure", "status": "True"}],
                "nodeInfo": {"kubeletVersion": "v1.33.1"},
                "allocatable": {"cpu": "4", "memory": "8Gi", "nvidia.com/gpu": "2"}}},
            {"metadata": {"name": "n2"}, "spec": {"unschedulable": true}, "status": {
                "conditions": [{"type": "Ready", "status": "False"}],
                "nodeInfo": {"kubeletVersion": "v1.33.1"},
                "allocatable": {"cpu": "2", "memory": "4Gi"}}}
        ]))
        .unwrap();
        let pods: Vec<PodLite> = serde_json::from_value(serde_json::json!([
            {"spec": {"containers": [{"resources": {"requests": {"cpu": "500m", "memory": "1Gi"},
                "limits": {"nvidia.com/gpu": "1"}}}]}, "status": {"phase": "Running"}},
            {"spec": {"containers": [{"resources": {"requests": {"cpu": "1"}}}]}, "status": {"phase": "Succeeded"}},
            {"spec": {"containers": [{}]}, "status": {"phase": "Pending"}}
        ]))
        .unwrap();
        let mut s = Summary::default();
        add_nodes(&mut s, &nodes);
        add_pods(&mut s, &pods);
        assert_eq!((s.nodes, s.nodes_ready), (2, 1));
        assert_eq!(s.kubelet_versions, [("v1.33.1".to_string(), 2)]);
        assert_eq!(s.node_issues[0], ("n1".to_string(), vec!["DiskPressure".to_string()]));
        assert_eq!(s.node_issues[1], ("n2".to_string(), vec!["NotReady".to_string(), "Cordoned".to_string()]));
        assert_eq!((s.cpu.0, s.cpu.1), (6000.0, 500.0));
        assert_eq!(s.mem.1, 1024.0 * 1024.0 * 1024.0);
        assert_eq!(s.gpus, (2.0, 1.0));
        assert_eq!((s.pods_total, s.pods_running), (3, 1));
    }

    #[test]
    fn requests_count_sidecars_init_containers_and_overhead() {
        let pods: Vec<PodLite> = serde_json::from_value(serde_json::json!([
            // A mesh sidecar adds to the app; overhead adds on top.
            {"spec": {
                "initContainers": [{"restartPolicy": "Always", "resources": {"requests": {"cpu": "100m", "memory": "128Mi"}}}],
                "containers": [{"resources": {"requests": {"cpu": "500m", "memory": "1Gi"}}}],
                "overhead": {"cpu": "50m"}},
             "status": {"phase": "Running"}},
            // A big init container sets the pod's request.
            {"spec": {
                "initContainers": [{"resources": {"requests": {"cpu": "2"}}}],
                "containers": [{"resources": {"requests": {"cpu": "250m"}}}]},
             "status": {"phase": "Pending"}}
        ]))
        .unwrap();
        let mut s = Summary::default();
        add_pods(&mut s, &pods);
        assert_eq!(s.cpu.1, 650.0 + 2000.0);
        assert_eq!(s.mem.1, (128.0 + 1024.0) * 1024.0 * 1024.0);
    }
}
