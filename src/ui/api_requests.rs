//! Settings → API requests: how each cluster's API server answers, a card a cluster with its
//! requests and latency over the last hour and a row a kind of request.

use std::time::Duration;

use gpui_kit::component::button::Button;
use gpui_kit::component::chart::AreaChart;
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::k8s::meter::{self, Class, Minute, Stats};
use crate::ui::selectable::selectable;

/// One cluster's numbers.
struct Cluster {
    context: String,
    classes: Vec<(Class, Stats)>,
    /// The last hour, a point a minute (the current one still counting).
    minutes: Vec<(i64, Minute)>,
    count: u64,
    errors: u64,
}

impl Cluster {
    fn in_window(&self) -> u64 {
        self.minutes.iter().map(|m| m.1.count).sum()
    }

    /// Requests in the last full minute.
    fn last_minute(&self, now: i64) -> u64 {
        self.minutes.iter().find(|m| m.0 == now - 1).map_or(0, |m| m.1.count)
    }
}

/// The clusters of `rows` (sorted by cluster), busiest first: by requests in the last hour,
/// then since Recouvrance started, then by name.
fn clusters(rows: Vec<(String, Class, Stats)>, now: i64) -> Vec<Cluster> {
    let mut grouped: Vec<(String, Vec<(Class, Stats)>)> = Vec::new();
    for (context, class, stats) in rows {
        match grouped.last_mut() {
            Some((c, v)) if *c == context => v.push((class, stats)),
            _ => grouped.push((context, vec![(class, stats)])),
        }
    }
    let mut out: Vec<Cluster> = grouped
        .into_iter()
        .map(|(context, classes)| Cluster {
            minutes: meter::per_minute(classes.iter().map(|c| &c.1), now),
            count: classes.iter().map(|c| c.1.count).sum(),
            errors: classes.iter().map(|c| c.1.errors).sum(),
            context,
            classes,
        })
        .collect();
    out.sort_by(|a, b| {
        b.in_window().cmp(&a.in_window()).then(b.count.cmp(&a.count)).then_with(|| a.context.cmp(&b.context))
    });
    out
}

/// `1234567` as `1,234,567`.
fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// A latency to read at a glance: `305 ms`, `4.95 s`, `31.6 s`, `3 min 0 s`.
fn duration(d: Duration) -> String {
    let ms = d.as_millis();
    match ms {
        0..1_000 => format!("{ms} ms"),
        1_000..10_000 => format!("{:.2} s", d.as_secs_f64()),
        10_000..60_000 => format!("{:.1} s", d.as_secs_f64()),
        _ => {
            let s = d.as_secs();
            format!("{} min {} s", s.div_euclid(60), s.rem_euclid(60))
        }
    }
}

/// `errors` out of `count` as a share: `0%`, `<0.1%`, `0.4%`, `18%`.
fn share(errors: u64, count: u64) -> String {
    if errors == 0 || count == 0 {
        return "0%".into();
    }
    let pct = errors as f64 * 100.0 / count as f64;
    if pct < 0.1 {
        "<0.1%".into()
    } else if pct < 10.0 {
        format!("{pct:.1}%")
    } else {
        format!("{pct:.0}%")
    }
}

fn minute_label(minute: i64) -> String {
    jiff::Timestamp::from_second(minute * 60)
        .map_or_default(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M").to_string())
}

pub fn render(cx: &App) -> AnyElement {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let now = meter::now_minute();
    let clusters = clusters(meter::snapshot(), now);
    let count: u64 = clusters.iter().map(|c| c.count).sum();
    let errors: u64 = clusters.iter().map(|c| c.errors).sum();
    let last_minute: u64 = clusters.iter().map(|c| c.last_minute(now)).sum();
    let tile = |label: &str, value: String, color: Option<Hsla>| {
        v_flex()
            .flex_1()
            .min_w(px(120.))
            .p_3()
            .gap_0p5()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary.opacity(0.4))
            .child(div().text_xs().text_color(muted).child(label.to_string()))
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .when_some(color, Styled::text_color)
                    .child(selectable(value)),
            )
    };
    v_flex()
        .gap_4()
        .child(
            h_flex()
                .gap_4()
                .items_start()
                .child(div().flex_1().min_w_0().text_sm().text_color(muted).child(
                    "How long the API servers take to start answering, since Recouvrance started (watches and log \
                     streams count until they open). Failed: an error status, a timeout or no connection. \
                     p50 and p95: half and 95% of the requests answered within that.",
                ))
                .child(Button::new("meter-reset").small().outline().label("Reset").on_click(|_, window, _| {
                    meter::reset();
                    window.refresh();
                })),
        )
        .child(
            h_flex()
                .gap_3()
                .flex_wrap()
                .child(tile("Requests", grouped(count), None))
                .child(tile("Failed", share(errors, count), (errors > 0).then_some(theme.warning)))
                .child(tile("Last minute", grouped(last_minute), None))
                .child(tile("Clusters", clusters.len().to_string(), None)),
        )
        .when(clusters.is_empty(), |d| d.child(div().py_2().text_sm().text_color(muted).child("No requests yet.")))
        .children(clusters.iter().map(|c| cluster_card(c, now, cx)))
        .into_any_element()
}

fn cluster_card(c: &Cluster, now: i64, cx: &App) -> Div {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let summary = format!(
        "{} requests · {}/min · {} failed",
        grouped(c.count),
        grouped(c.last_minute(now)),
        share(c.errors, c.count)
    );
    v_flex()
        .p_3()
        .gap_3()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .child(
            h_flex()
                .gap_3()
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(selectable(c.context.clone())),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .flex_shrink_0()
                        .text_sm()
                        .text_color(if c.errors > 0 { theme.warning } else { muted })
                        .child(selectable(summary)),
                ),
        )
        .when(c.in_window() > 0, |d| d.child(h_flex().gap_3().child(requests_chart(c, cx)).child(latency_chart(c, cx))))
        .child(class_table(c, cx))
}

fn chart_box(title: &str, chart: impl IntoElement, cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .flex_1()
        .min_w_0()
        .p_2()
        .gap_1()
        .rounded_md()
        .bg(theme.secondary.opacity(0.4))
        .child(div().text_xs().text_color(theme.muted_foreground).child(title.to_string()))
        .child(div().h(px(110.)).child(chart))
}

fn requests_chart(c: &Cluster, cx: &App) -> Div {
    let theme = cx.theme();
    let (blue, warning) = (theme.blue, theme.warning);
    let data: Vec<(String, f64, f64)> =
        c.minutes.iter().map(|(m, s)| (minute_label(*m), s.count as f64, s.errors as f64)).collect();
    let top = data.iter().map(|p| p.1).fold(0.0, f64::max);
    let count = |v: f64| grouped(v.round() as u64);
    let chart = AreaChart::new(data)
        .id(ElementId::Name(format!("meter-requests-{}", c.context).into()))
        .x(|p: &(String, f64, f64)| SharedString::from(p.0.clone()))
        .y(|p: &(String, f64, f64)| p.1)
        .stroke(blue)
        .fill(blue.opacity(0.15))
        .linear()
        .name("Requests")
        .y(|p: &(String, f64, f64)| p.2)
        .stroke(warning)
        .fill(warning.opacity(0.25))
        .linear()
        .name("Failed")
        .x_tick_count(4)
        .y_domain(0.0, (top * 1.2).max(1.0))
        .y_axis(true)
        .y_tick_count(3)
        .y_tick_format(count)
        .tooltip_value(move |_, _, v| count(v).into());
    chart_box("Requests per minute", chart, cx)
}

/// The minutes of `minutes` with requests and their average latency: a minute without
/// requests has no latency, rather than 0 ms.
fn latencies(minutes: &[(i64, Minute)]) -> Vec<(i64, Duration)> {
    minutes.iter().filter_map(|(m, s)| Some((*m, s.average()?))).collect()
}

fn latency_chart(c: &Cluster, cx: &App) -> Div {
    let theme = cx.theme();
    let color = theme.green;
    let title = "Average latency per minute";
    let points = latencies(&c.minutes);
    if let [(m, only)] = points.as_slice() {
        // One point draws no line: say it.
        let text = format!("{} at {}", duration(*only), minute_label(*m));
        return chart_box(title, div().pt_4().text_lg().text_color(color).child(selectable(text)), cx);
    }
    let ms = |d: &Duration| d.as_secs_f64() * 1000.0;
    let data: Vec<(String, f64)> = points.iter().map(|(m, d)| (minute_label(*m), ms(d))).collect();
    let top = data.iter().map(|p| p.1).fold(0.0, f64::max);
    let fmt = |v: f64| duration(Duration::from_secs_f64(v.max(0.0) / 1000.0));
    let chart = AreaChart::new(data)
        .id(ElementId::Name(format!("meter-latency-{}", c.context).into()))
        .x(|p: &(String, f64)| SharedString::from(p.0.clone()))
        .y(|p: &(String, f64)| p.1)
        .stroke(color)
        .fill(color.opacity(0.15))
        .linear()
        .name("Average")
        .x_tick_count(4)
        .y_domain(0.0, (top * 1.2).max(1.0))
        .y_axis(true)
        .y_tick_count(3)
        .y_tick_format(fmt)
        .tooltip_value(move |_, _, v| fmt(v).into());
    chart_box(title, chart, cx)
}

fn class_table(c: &Cluster, cx: &App) -> Div {
    let theme = cx.theme();
    let cell = |w: f32, text: String| div().w(px(w)).flex_shrink_0().child(selectable(text));
    let header = h_flex()
        .gap_3()
        .pb_1()
        .text_xs()
        .text_color(theme.muted_foreground)
        .border_b_1()
        .border_color(theme.border)
        .child(cell(150., "Requests to".into()))
        .child(cell(90., "Count".into()))
        .child(cell(110., "Failed".into()))
        .child(cell(75., "p50".into()))
        .child(cell(75., "p95".into()))
        .child(cell(85., "Slowest".into()))
        .child(cell(75., "Last".into()));
    v_flex().child(header).children(c.classes.iter().map(|(class, s)| {
        let failed = if s.errors == 0 {
            "0".to_string()
        } else {
            format!("{} ({})", grouped(s.errors), share(s.errors, s.count))
        };
        h_flex()
            .gap_3()
            .py_1()
            .text_sm()
            .border_b_1()
            .border_color(theme.border.opacity(0.5))
            .child(cell(150., class.label().into()))
            .child(cell(90., grouped(s.count)))
            .child(cell(110., failed).when(s.errors > 0, |d| d.text_color(theme.warning)))
            .child(cell(75., duration(s.percentile(0.5))))
            .child(cell(75., duration(s.percentile(0.95))))
            .child(cell(85., duration(s.slowest)))
            .child(cell(75., duration(s.last)))
    }))
}

#[cfg(test)]
mod tests {
    use super::{duration, grouped, latencies, share};
    use crate::k8s::meter::Minute;
    use proptest::prelude::*;
    use std::time::Duration;

    proptest! {
        /// Every minute with requests has its average latency, in order, and a minute without
        /// requests has none (not 0 ms).
        #[test]
        fn idle_minutes_have_no_latency(counts in proptest::collection::vec((0u64..4, 0u64..5_000), 0..60)) {
            let minutes: Vec<(i64, Minute)> = counts
                .iter()
                .enumerate()
                .map(|(i, &(count, ms))| {
                    (i as i64, Minute { count, errors: 0, total: Duration::from_millis(ms * count) })
                })
                .collect();
            let points = latencies(&minutes);
            let busy: Vec<i64> = minutes.iter().filter(|(_, s)| s.count > 0).map(|(m, _)| *m).collect();
            prop_assert_eq!(points.iter().map(|p| p.0).collect::<Vec<_>>(), busy);
            for (m, d) in points {
                prop_assert_eq!(d, Duration::from_millis(counts[m as usize].1));
            }
        }
    }

    #[test]
    fn numbers_read_at_a_glance() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(1_123_560), "1,123,560");
        assert_eq!(duration(Duration::from_millis(305)), "305 ms");
        assert_eq!(duration(Duration::from_millis(4_953)), "4.95 s");
        assert_eq!(duration(Duration::from_millis(31_556)), "31.6 s");
        assert_eq!(duration(Duration::from_millis(180_001)), "3 min 0 s");
        assert_eq!(share(0, 10), "0%");
        assert_eq!(share(1, 277), "0.4%");
        assert_eq!(share(1_369, 1_123_560), "0.1%");
        assert_eq!(share(1, 2_000), "<0.1%");
        assert_eq!(share(523, 2_908), "18%");
    }
}
