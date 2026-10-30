//! Prometheus alerts about the object, on its page.

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Ctx, card, heading, s};
use crate::alerts::Alert;
use crate::k8s;
use crate::problems::Severity;
use crate::ui::selectable::selectable;

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

fn alert_row(ctx: &Ctx, i: usize, a: &Alert, cx: &App) -> Div {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    // Pending, silenced or inhibited: shown, but nobody is being told.
    let quiet = !a.is_problem();
    let color = match a.severity() {
        Severity::Critical => theme.danger,
        Severity::Warning => theme.warning,
        Severity::Info => muted,
    };
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
                        .on_click(move |_, _, _| crate::forwards::open_url(&url))
                        .child("Runbook")
                })),
        )
        .when(!a.summary.is_empty(), |d| {
            d.child(div().pl_4().text_sm().when(quiet, |d| d.text_color(muted)).child(selectable(a.summary.clone())))
        })
        .when(!description.is_empty() && a.description != a.summary, |d| {
            d.child(div().pl_4().text_xs().text_color(muted).child(selectable(description)))
        })
}
