//! Usage history charts and request/limit bars.

#[allow(unused_imports)]
use super::*;

/// Sum of container requests and limits over these pod specs (millicores, bytes).
pub(super) fn totals(pods: &[&Value]) -> [Option<f64>; 4] {
    let mut t = [None::<f64>; 4];
    for p in pods {
        for c in arr(p, "/spec/containers") {
            for (i, path, f) in [
                (0, "/resources/requests/cpu", k8s::parse_cpu as fn(&str) -> f64),
                (1, "/resources/limits/cpu", k8s::parse_cpu),
                (2, "/resources/requests/memory", k8s::parse_bytes),
                (3, "/resources/limits/memory", k8s::parse_bytes),
            ] {
                if let Some(v) = c.pointer(path).and_then(Value::as_str) {
                    *t[i].get_or_insert(0.0) += f(v);
                }
            }
        }
    }
    t
}

/// The usage heading; says so when the charts cover only some of the pods.
fn usage_title(charted: usize, total: usize) -> String {
    if charted < total { format!("Usage ({charted} of {total} pods)") } else { "Usage".into() }
}

pub(super) fn history(ctx: &Ctx, cx: &App) -> Option<Div> {
    let empty = (Vec::new(), Vec::new());
    let (cpu, mem) = ctx.related.history.as_ref().unwrap_or(&empty);
    let has_cpu_mem = !cpu.is_empty() || !mem.is_empty();
    if !has_cpu_mem && ctx.related.gpu.is_none() {
        return None;
    }
    // Requests and limits of the pods the charts measure.
    let pods: Vec<&Value> = if ctx.kind.is("", "Pod") {
        vec![ctx.obj]
    } else {
        ctx.related.pods.iter().take(crate::prom::MAX_CHART_PODS).collect()
    };
    let is_node = ctx.kind.is("", "Node");
    // Nodes: requests of their pods, and what the node can give (allocatable) as the ceiling.
    let [cpu_req, cpu_lim, mem_req, mem_lim] = if is_node {
        // Like the scheduler: running pods only, sidecars and init containers counted.
        let req = |resource: &str| Some(ctx.related.allocated(resource).0).filter(|v| *v > 0.0);
        [
            req("cpu"),
            s(ctx.obj, "/status/allocatable/cpu").map(k8s::parse_cpu),
            req("memory"),
            s(ctx.obj, "/status/allocatable/memory").map(k8s::parse_bytes),
        ]
    } else {
        totals(&pods)
    };
    let lim_word = if is_node { "allocatable" } else { "limit" };
    // A node's charts measure the node, not its pods.
    let title = if is_node { "Usage".into() } else { usage_title(pods.len(), ctx.related.pods.len()) };
    let theme = cx.theme();
    let minutes = ctx.history_minutes;
    let chart = |title: &str,
                 data: &crate::prom::Series,
                 req: Option<f64>,
                 lim: Option<f64>,
                 color: Hsla,
                 fmt: fn(f64) -> String,
                 id: &'static str| {
        let spec =
            ChartSpec { title, req, req_word: "request", lim, lim_word, pin_lim: is_node, color, fmt, id, minutes };
        usage_chart(data, spec, cx)
    };
    let ranges = h_flex().gap_0p5().children(
        [(15, "15m"), (60, "1h"), (360, "6h"), (1440, "24h"), (10080, "7d")].into_iter().map(|(m, l)| {
            let panel = ctx.panel.clone();
            Button::new(SharedString::from(format!("range-{m}")))
                .xsmall()
                .label(l)
                .when(m == minutes, |b| b.primary())
                .when(m != minutes, |b| b.ghost())
                .on_click(move |_, _, cx| {
                    let _ = panel.update(cx, |p, cx| p.set_history_minutes(m, cx));
                })
        }),
    );
    Some(
        v_flex()
            .child(h_flex().items_end().child(heading(&title)).child(div().flex_1()).child(div().pb_1().child(ranges)))
            .when(has_cpu_mem, |d| {
                d.child(
                    h_flex()
                        .gap_3()
                        .child(chart("CPU", cpu, cpu_req, cpu_lim, theme.blue, k8s::format_cores, "hist-cpu"))
                        .child(chart("Memory", mem, mem_req, mem_lim, theme.success, k8s::format_bytes, "hist-mem")),
                )
            })
            .children(ctx.related.gpu.as_ref().map(|g| gpu_charts(ctx, g, cx))),
    )
}

/// GPU utilization (average across GPUs) and GPU memory, under the CPU and memory charts.
fn gpu_charts(ctx: &Ctx, g: &crate::prom::GpuUsage, cx: &App) -> Div {
    let theme = cx.theme();
    let minutes = ctx.history_minutes;
    let pct = |v: f64| format!("{v:.0}%");
    let title = if g.gpus > 1 { format!("GPU ({} GPUs, average)", g.gpus) } else { "GPU".to_string() };
    let util = ChartSpec {
        title: &title,
        req: None,
        req_word: "",
        lim: Some(100.0),
        lim_word: "max",
        pin_lim: true,
        color: theme.warning,
        fmt: pct,
        id: "hist-gpu",
        minutes,
    };
    let mem = ChartSpec {
        title: "GPU memory",
        req: None,
        req_word: "",
        lim: g.mem_total,
        lim_word: "total",
        pin_lim: true,
        color: theme.danger,
        fmt: k8s::format_bytes,
        id: "hist-gpu-mem",
        minutes,
    };
    h_flex().pt_3().gap_3().child(usage_chart(&g.util, util, cx)).child(usage_chart(&g.mem, mem, cx))
}

/// One usage chart: a series against optional request and limit/capacity lines.
pub struct ChartSpec<'a> {
    pub title: &'a str,
    pub req: Option<f64>,
    pub req_word: &'a str,
    pub lim: Option<f64>,
    pub lim_word: &'a str,
    /// Always keep the limit line in view (capacity); otherwise a far limit is hidden.
    pub pin_lim: bool,
    pub color: Hsla,
    pub fmt: fn(f64) -> String,
    pub id: &'static str,
    pub minutes: i64,
}

pub fn usage_chart(data: &crate::prom::Series, spec: ChartSpec, cx: &App) -> Div {
    let theme = cx.theme();
    let ChartSpec { title, req, req_word, lim, lim_word, pin_lim, color, fmt, id, minutes } = spec;
    let time_fmt = if minutes > 1440 { "%a %H:%M" } else { "%H:%M" };
    let label = move |t: i64| {
        jiff::Timestamp::from_second(t)
            .map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime(time_fmt).to_string())
            .unwrap_or_default()
    };
    let max = data.iter().map(|p| p.1).fold(0.0, f64::max);
    // Scale to usage and request; a far-away limit would flatten the curve.
    let top = match lim.filter(|l| pin_lim && max <= *l) {
        // A pinned ceiling (capacity, 100%) is the top of the chart, so ticks land on round values.
        Some(l) => l,
        None => [Some(max), req, lim.filter(|_| pin_lim)].into_iter().flatten().fold(0.0, f64::max) * 1.25,
    };
    let shown_lim = lim.filter(|l| *l <= top);
    let data: Vec<(String, f64)> = data.iter().map(|(t, v)| (label(*t), *v)).collect();
    let last = data.last().map(|p| fmt(p.1)).unwrap_or_default();
    let legend = [req.map(|r| format!("{req_word} {}", fmt(r))), lim.map(|l| format!("{lim_word} {}", fmt(l)))]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
    let mut c = AreaChart::new(data)
        .id(id)
        .x(|p: &(String, f64)| SharedString::from(p.0.clone()))
        .y(|p: &(String, f64)| p.1)
        .stroke(color)
        .fill(color.opacity(0.15))
        .linear()
        .tick_margin(30)
        .y_domain(0.0, top.max(1.0))
        // No headroom above a pinned ceiling, so the top tick reads exactly the ceiling.
        .y_padding(if pin_lim && lim == Some(top) { 0.0 } else { 10.0 }, 0.0)
        .y_axis(true)
        .y_tick_count(3)
        .y_tick_format(fmt)
        .tooltip_value(move |_, _, v| fmt(v).into());
    for line in [req, shown_lim].into_iter().flatten() {
        c = c.reference_line(line);
    }
    v_flex()
        .flex_1()
        .min_w_0()
        .p_3()
        .gap_1()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .child(
            h_flex()
                .gap_2()
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title.to_string()))
                .child(div().text_sm().text_color(color).child(last))
                .child(div().flex_1())
                .child(div().text_xs().text_color(theme.muted_foreground).child(legend)),
        )
        .child(div().h(px(140.)).child(c))
}

pub(super) fn quantity_cpu(v: Option<&str>) -> Option<f64> {
    v.map(k8s::parse_cpu)
}

pub(super) fn quantity_mem(v: Option<&str>) -> Option<f64> {
    v.map(k8s::parse_bytes)
}

/// A bar of usage against limit (or request), with a tick at the request.
pub(super) fn usage_bar(value: Option<f64>, request: Option<f64>, limit: Option<f64>, color: Hsla, cx: &App) -> Div {
    let track = cx.theme().secondary;
    let Some(value) = value else {
        return div().h(px(6.)).w_full().rounded_full().bg(track);
    };
    let max = limit.or(request.map(|r| (r * 2.0).max(value))).unwrap_or(value * 1.25).max(1e-9);
    let fill = (value / max).clamp(0.0, 1.0) as f32;
    let over = limit.is_some_and(|l| value >= l * 0.95);
    div()
        .relative()
        .h(px(6.))
        .w_full()
        .rounded_full()
        .bg(track)
        .child(
            div()
                .absolute()
                .left_0()
                .top_0()
                .h_full()
                .rounded_full()
                .bg(if over { cx.theme().danger } else { color })
                .w(relative(fill)),
        )
        .when_some(request, |d, r| {
            let at = (r / max).clamp(0.0, 1.0) as f32;
            d.child(div().absolute().top(px(-3.)).h(px(12.)).w(px(2.)).bg(cx.theme().foreground).left(relative(at)))
        })
}

#[allow(clippy::too_many_arguments)]
pub(super) fn usage_card(
    icon: Lucide,
    title: &str,
    usage: Option<f64>,
    request: Option<f64>,
    limit: Option<f64>,
    fmt: fn(f64) -> String,
    color: Hsla,
    cx: &App,
) -> Div {
    let muted = cx.theme().muted_foreground;
    let show = |v: Option<f64>| v.map(fmt).unwrap_or_else(|| "-".into());
    v_flex()
        .w(px(250.))
        .gap_1()
        .child(h_flex().gap_1().text_xs().child(Icon::new(icon).size_3p5().text_color(color)).child(title.to_string()))
        .child(
            card(cx)
                .child(div().text_xs().text_color(muted).child("Usage"))
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().w(px(56.)).text_sm().child(show(usage)))
                        .child(div().flex_1().child(usage_bar(usage, request, limit, color, cx))),
                )
                .child(div().pt_1().text_xs().text_color(muted).child("Allocation"))
                .child(div().text_xs().child(format!("Requests: {}", show(request))))
                .child(div().text_xs().child(format!("Limits: {}", show(limit)))),
        )
}

#[cfg(test)]
mod tests {
    #[test]
    fn usage_title_says_when_some_pods_are_left_out() {
        assert_eq!(super::usage_title(1, 1), "Usage");
        assert_eq!(super::usage_title(1, 0), "Usage");
        assert_eq!(super::usage_title(300, 500), "Usage (300 of 500 pods)");
    }
}
