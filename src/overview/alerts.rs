//! Prometheus alerts about the object, on its page, and the rule of the one clicked.

use std::collections::BTreeMap;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{ChartSpec, Ctx, card, heading, mono, s, usage_chart};
use crate::alerts::{Alert, CHART_MINUTES, Detail};
use crate::k8s;
use crate::problems::Severity;
use crate::ui::selectable::selectable;

/// The alert whose rule is shown, by its labels (they stay the same across the page's
/// reloads, its place in the list may not), and the rule read for it.
#[derive(Default)]
pub struct AlertRule {
    pub open: Option<BTreeMap<String, String>>,
    /// None while it is read.
    pub read: Option<Result<Detail, String>>,
}

/// The alerts section; nothing when there are none (or no Prometheus).
pub(super) fn alerts(ctx: &Ctx, cx: &App) -> Option<Div> {
    let alerts = &ctx.related.alerts;
    if alerts.is_empty() {
        return None;
    }
    let firing = alerts.iter().filter(|a| a.is_problem()).count();
    let title = if firing == alerts.len() {
        format!("Alerts ({firing})")
    } else {
        format!("Alerts ({firing} firing, {} in all)", alerts.len())
    };
    let rows = alerts.iter().enumerate().map(|(i, a)| alert_row(ctx, i, a, cx));
    let note =
        ctx.related.alerts_note.clone().map(|n| div().text_xs().text_color(cx.theme().warning).child(selectable(n)));
    Some(v_flex().child(heading(&title)).child(card(cx).p_3().gap_3().children(note).children(rows)))
}

fn severity_color(a: &Alert, cx: &App) -> Hsla {
    let theme = cx.theme();
    match a.severity() {
        Severity::Critical => theme.danger,
        Severity::Warning => theme.warning,
        Severity::Info => theme.muted_foreground,
    }
}

fn alert_row(ctx: &Ctx, i: usize, a: &Alert, cx: &App) -> Div {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    // Pending, silenced or inhibited: shown, but nobody is being told.
    let quiet = !a.is_problem();
    let color = severity_color(a, cx);
    let state = a.muted.unwrap_or(if a.firing { "firing" } else { "pending" });
    let state = match a.since {
        Some(t) => format!("{state} for {}", k8s::age(t)),
        None => state.to_string(),
    };
    // What it is about, when that is not the page's object (a namespace's, a workload's pods).
    let page = (ctx.kind.ar.kind.as_str(), s(ctx.obj, "/metadata/name").unwrap_or_default());
    let object =
        a.object().filter(|(_, kind, name)| (*kind, *name) != page).map(|(_, kind, name)| format!("{kind} {name}"));
    let description: String = a.description.chars().take(400).collect();
    let open = ctx.alert_rule.open.as_ref() == Some(&a.labels);
    let panel = ctx.panel.clone();
    let labels = a.labels.clone();
    v_flex()
        .gap_0p5()
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    div()
                        .size_2()
                        .flex_shrink_0()
                        .rounded_full()
                        .when(quiet, |d| d.border_1().border_color(color))
                        .when(!quiet, |d| d.bg(color)),
                )
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .when(quiet, |d| d.text_color(muted))
                        .child(selectable(a.name().to_string())),
                )
                .when(!a.severity_label().is_empty(), |d| {
                    d.child(div().text_xs().text_color(color).child(selectable(a.severity_label().to_string())))
                })
                .child(div().text_xs().text_color(muted).child(selectable(state)))
                .children(object.map(|o| {
                    div()
                        .min_w_0()
                        .text_xs()
                        .text_color(muted)
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(selectable(o))
                }))
                .child(div().flex_1())
                .children(a.runbook.clone().map(|url| {
                    div()
                        .id(("alert-runbook", i))
                        .flex_shrink_0()
                        .text_xs()
                        .text_color(theme.blue)
                        .cursor_pointer()
                        .hover(Styled::underline)
                        .on_click(move |_, window, cx| crate::forwards::open_url(&url, window, cx))
                        .child("Runbook")
                }))
                .child(
                    div().flex_shrink_0().child(
                        Button::new(("alert-rule", i))
                            .xsmall()
                            .ghost()
                            .label("Rule")
                            .icon(if open { Lucide::ChevronDown } else { Lucide::ChevronRight })
                            .tooltip("Its expression and a chart of it, read from Prometheus")
                            .on_click(move |_, _, cx| {
                                let _ = panel.update(cx, |p, cx| p.toggle_alert_rule(labels.clone(), cx));
                            }),
                    ),
                ),
        )
        .when(!a.summary.is_empty(), |d| {
            d.child(div().pl_4().text_sm().when(quiet, |d| d.text_color(muted)).child(selectable(a.summary.clone())))
        })
        .when(!description.is_empty() && a.description != a.summary, |d| {
            d.child(div().pl_4().text_xs().text_color(muted).child(selectable(description)))
        })
        .when(open, |d| d.child(rule_block(ctx, a, cx)))
}

/// Annotations the row shows already.
const SHOWN: [&str; 5] = ["summary", "description", "message", "runbook_url", "runbook"];

/// The rule of the alert clicked: its expression, for, labels, annotations, and a chart.
fn rule_block(ctx: &Ctx, a: &Alert, cx: &App) -> Div {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let block = v_flex().pl_4().pt_1().gap_1p5();
    let note = |text: String, color: Hsla| div().text_xs().text_color(color).child(selectable(text));
    let detail = match &ctx.alert_rule.read {
        None => return block.child(note("Reading the rule…".into(), muted)),
        Some(Err(e)) => return block.child(note(format!("Could not read the rule: {e}"), theme.warning)),
        Some(Ok(detail)) => detail,
    };
    let Some(rule) = &detail.rule else {
        return block.child(note("No rule with this name and labels in Prometheus' rules.".into(), muted));
    };
    let held = if rule.for_secs > 0 {
        format!("fires after {}", k8s::duration_text(i64::try_from(rule.for_secs).unwrap_or(i64::MAX)))
    } else {
        "fires at once".to_string()
    };
    let labels = rule.labels.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("  ");
    let annotations = rule.annotations.iter().filter(|(k, _)| !SHOWN.contains(&k.as_str())).map(|(k, v)| {
        let v: String = v.chars().take(400).collect();
        h_flex()
            .gap_2()
            .items_start()
            .text_xs()
            .child(div().flex_shrink_0().text_color(muted).child(selectable(format!("{k}:"))))
            .child(div().min_w_0().child(selectable(v)))
    });
    block
        .child(div().p_2().rounded_md().bg(theme.secondary).child(mono(rule.query.clone(), cx).whitespace_normal()))
        .child(
            h_flex()
                .gap_3()
                .flex_wrap()
                .child(note(held, muted))
                .when(!labels.is_empty(), |d| d.child(div().min_w_0().child(mono(labels, cx).text_color(muted)))),
        )
        .children(annotations)
        .children(rule.error.as_ref().map(|e| note(format!("Its last evaluation failed: {e}"), theme.danger)))
        .children(detail.chart.as_ref().map(|chart| {
            // Only when something was cut: comments and outer parentheses taken off are not.
            let how = match (chart.cut.set_op, chart.cut.comparison.is_some()) {
                (Some(op), true) => Some(format!("the part before {op}, without the comparison")),
                (Some(op), false) => Some(format!("the part before {op}")),
                (None, true) => Some("without the comparison".to_string()),
                (None, false) => None,
            };
            let charted =
                how.map(|how| note(format!("Charted: {}, {how}, for this alert's series.", chart.expr), muted));
            let body = match &chart.series {
                Err(e) => note(format!("No chart: {e}"), muted),
                Ok(series) if series.is_empty() && !chart.matched_on.is_empty() => note(
                    format!(
                        "No points in the last 3 hours for the series with this alert's labels ({}). A label the \
                         alert source adds, such as an external label, would match none of them.",
                        chart.matched_on.join(", ")
                    ),
                    muted,
                ),
                Ok(series) if series.is_empty() => note("No points in the last 3 hours.".into(), muted),
                Ok(series) => {
                    let word = chart.threshold.as_ref().map_or_default(|(op, _)| format!("threshold {op}"));
                    let spec = ChartSpec {
                        title: "Last 3 hours",
                        req: chart.threshold.as_ref().map(|(_, n)| *n),
                        req_word: &word,
                        lim: None,
                        lim_word: "",
                        pin_lim: false,
                        color: severity_color(a, cx),
                        fmt: number_text,
                        id: "alert-rule-chart",
                        minutes: CHART_MINUTES,
                    };
                    h_flex().child(usage_chart(series, spec, cx))
                }
            };
            v_flex().gap_1().children(charted).child(body)
        }))
}

/// A value of any unit, short: 3 decimals at most, k, M and G for big ones.
fn number_text(v: f64) -> String {
    let size = v.abs();
    if size != 0.0 && size < 0.001 {
        return format!("{v:.2e}");
    }
    let (n, unit) = if size >= 1e9 {
        (v / 1e9, "G")
    } else if size >= 1e6 {
        (v / 1e6, "M")
    } else if size >= 1e4 {
        (v / 1e3, "k")
    } else {
        (v, "")
    };
    let text = format!("{n:.3}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    format!("{text}{unit}")
}

#[cfg(test)]
mod tests {
    #[test]
    fn numbers_read_short() {
        use super::number_text;
        assert_eq!(number_text(0.0), "0");
        assert_eq!(number_text(0.9), "0.9");
        assert_eq!(number_text(1.0), "1");
        assert_eq!(number_text(10.0), "10");
        assert_eq!(number_text(12_345.0), "12.345k");
        assert_eq!(number_text(2.5e9), "2.5G");
        assert_eq!(number_text(-3.0e6), "-3M");
        assert_eq!(number_text(0.0001), "1.00e-4");
    }
}
