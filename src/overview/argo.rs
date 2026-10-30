//! The Argo CD section: the Application that deployed the object, or those that deploy
//! into the namespace, as read on Argo CD's cluster.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Ctx, field, heading, mono, row, tone_color};
use crate::k8s;
use crate::k8s::argo::diagnosis::{self, Compared, Diagnosis, Drift, DriftRead, Finding, Res};
use crate::k8s::argo::{self, Found, Summary};
use crate::status::Tone;
use crate::ui::selectable::{dragged, selectable};

fn ago(t: Option<&str>) -> Option<String> {
    t?.parse::<jiff::Timestamp>().ok().map(|t| format!("{} ago", k8s::age(t)))
}

/// A word in its tone's color.
fn word(text: &str, tone: Tone, cx: &App) -> Div {
    div().text_color(tone_color(Some(tone), cx)).child(selectable(text.to_string()))
}

/// Nothing when Argo CD deploys nothing here; one line when it could not be read.
pub(super) fn section(ctx: &Ctx, cx: &App) -> Option<Div> {
    let lookup = ctx.argo;
    if lookup.apps.is_empty() && lookup.note.is_none() {
        return None;
    }
    // A namespace can hold many Applications: one line each. An object has its one in full.
    let full = !ctx.kind.is("", "Namespace");
    let title = if full { "Argo CD".to_string() } else { format!("Argo CD Applications ({})", lookup.apps.len()) };
    Some(
        v_flex()
            .gap_2()
            .child(heading(&title))
            .when_some(lookup.note.as_ref(), |d, note| {
                d.child(div().text_xs().text_color(cx.theme().muted_foreground).child(selectable(note.clone())))
            })
            .children(lookup.apps.iter().enumerate().map(|(ix, f)| app_card(ctx, ix, f, full, cx))),
    )
}

fn app_card(ctx: &Ctx, ix: usize, found: &Found, full: bool, cx: &App) -> Div {
    let app = &found.app;
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    // On a namespace's line, the last sync's message only when it failed.
    let op_message = app
        .operation
        .as_ref()
        .filter(|(phase, _, _)| full || argo::operation_tone(phase) == Tone::Bad)
        .and_then(|(_, message, _)| message.clone());
    v_flex()
        .p_2()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary.opacity(0.4))
        .child(header(ctx, ix, found, cx))
        .when_some(app.deleting.as_deref(), |d, since| {
            let when = ago(Some(since)).unwrap_or_default();
            d.child(div().text_sm().text_color(theme.danger).child(selectable(format!(
                "Being deleted since {when}: a finalizer or a resource it waits for keeps it"
            ))))
        })
        .child(
            row()
                .child(field("Sync", word(&app.sync, argo::sync_tone(&app.sync, app.automated), cx), cx))
                .child(field("Health", word(&app.health, argo::health_tone(&app.health), cx), cx))
                .child(field("Auto-sync", if app.automated { "On" } else { "Off" }, cx))
                .when_some(app.revision.as_ref(), |d, rev| d.child(field("Revision", mono(rev.clone(), cx), cx)))
                .when_some(app.operation.as_ref(), |d, (phase, _, finished)| {
                    let text = match ago(finished.as_deref()) {
                        Some(when) => format!("{phase}, {when}"),
                        None => phase.clone(),
                    };
                    d.child(field("Last sync", word(&text, argo::operation_tone(phase), cx), cx))
                })
                .when_some(app.own.as_ref(), |d, (sync, health, _)| {
                    d.child(field(
                        "This object",
                        h_flex()
                            .gap_2()
                            .child(word(sync, argo::sync_tone(sync, app.automated), cx))
                            .when(!health.is_empty(), |d| d.child(word(health, argo::health_tone(health), cx))),
                        cx,
                    ))
                }),
        )
        .when_some(app.health_message.as_ref(), |d, m| {
            d.child(div().text_sm().text_color(muted).child(selectable(m.clone())))
        })
        .when_some(app.own.as_ref().and_then(|o| o.2.as_ref()), |d, m| {
            d.child(div().text_sm().text_color(muted).child(selectable(format!("This object: {m}"))))
        })
        .when_some(op_message, |d, m| {
            d.child(div().text_sm().text_color(muted).child(selectable(format!("Last sync: {m}"))))
        })
        .children(app.conditions.iter().map(|(ty, message)| {
            let color = if ty.ends_with("Warning") { theme.warning } else { theme.danger };
            div().text_sm().text_color(color).child(selectable(format!("{ty}: {message}")))
        }))
        .when(full && !app.history.is_empty(), |d| d.child(history(app, cx)))
}

/// The name (opens the Application on its cluster), where it runs, and its actions.
fn header(ctx: &Ctx, ix: usize, found: &Found, cx: &App) -> Div {
    let app = &found.app;
    let panel = ctx.panel.clone();
    let (open, refresh, sync) = (panel.clone(), panel.clone(), panel);
    h_flex()
        .gap_2()
        .flex_wrap()
        .child(
            div()
                .id(("argo-app", ix))
                .text_color(cx.theme().blue)
                .cursor_pointer()
                .hover(Styled::underline)
                .font_weight(FontWeight::SEMIBOLD)
                .child(selectable(app.name.clone()))
                .on_click(move |click, _, cx| {
                    if !dragged(click) {
                        let _ = open.update(cx, |p, cx| p.open_argo_app(ix, cx));
                    }
                }),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(selectable(format!("{}/{} on {}", app.namespace, app.name, found.hub.context))),
        )
        .child(div().flex_1())
        .children(app.urls.iter().take(3).enumerate().map(|(i, url)| {
            let url = url.clone();
            Button::new(("argo-url", ix * 8 + i))
                .xsmall()
                .ghost()
                .icon(Lucide::ExternalLink)
                .label(short_url(&url))
                .tooltip(url.clone())
                .on_click(move |_, _, _| crate::forwards::open_url(&url))
        }))
        .child(Button::new(("argo-refresh", ix)).xsmall().ghost().icon(Lucide::RefreshCw).label("Refresh").on_click(
            move |_, window, cx| {
                let _ = refresh.update(cx, |p, cx| p.confirm_argo(ix, false, window, cx));
            },
        ))
        .child(Button::new(("argo-sync", ix)).xsmall().ghost().icon(Lucide::GitBranch).label("Sync").on_click(
            move |_, window, cx| {
                let _ = sync.update(cx, |p, cx| p.confirm_argo(ix, true, window, cx));
            },
        ))
}

/// An Application's page: where it deploys, its namespace opening on the context that
/// reaches its cluster (connected or not).
pub(super) fn destination(ctx: &Ctx, cx: &App) -> Option<Div> {
    let (dest, on) = ctx.related.argo_destination.as_ref()?;
    let muted = cx.theme().muted_foreground;
    let value: AnyElement = match on {
        Some(context) if !dest.namespace.is_empty() => {
            let panel = ctx.panel.clone();
            div()
                .id("argo-destination")
                .text_color(cx.theme().blue)
                .cursor_pointer()
                .hover(Styled::underline)
                .child(selectable(argo::destination_text(context, &dest.namespace)))
                .on_click(move |click, _, cx| {
                    if !dragged(click) {
                        let _ = panel.update(cx, crate::details::DetailPanel::open_destination);
                    }
                })
                .into_any_element()
        }
        Some(context) => div().child(selectable(context.clone())).into_any_element(),
        None => h_flex()
            .gap_2()
            .child(mono(argo::destination_text(&dest.cluster, &dest.namespace), cx))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(selectable("no context of your kubeconfig reaches this cluster")),
            )
            .into_any_element(),
    };
    Some(row().pt_4().child(field("Destination", value, cx)))
}

/// Where the drift shown comes from, said under it.
const DRIFT_SOURCE: &str = "Each live object is compared with what was last applied to it (its \
    last-applied-configuration, which Argo CD's sync writes), and with who wrote it since the last sync. Git's \
    manifests can't be read through the Kubernetes API: when git moved since the last sync, the rest of the \
    difference is there (Argo CD's own diff shows it).";

/// An Application's page: what is wrong with it (a deletion held, a sync stuck or failed, an
/// error Argo CD reports, a sync loop) and what is out of sync, with the fields that differ
/// live. Nothing when all is well. Out of sync with auto-sync off is said, not as a problem.
pub(super) fn diagnosis(ctx: &Ctx, cx: &App) -> Option<Div> {
    let d = Diagnosis::of(ctx.obj, jiff::Timestamp::now());
    if d.is_empty() {
        return None;
    }
    let read = ctx.related.argo_drift.as_ref();
    let on = read.and_then(|r| r.context.as_deref());
    let muted = cx.theme().muted_foreground;
    let mut section = v_flex().gap_3().child(heading("Diagnosis"));
    for (ix, f) in d.findings.iter().enumerate() {
        section = section.child(finding(ctx, ix, f, on, cx));
    }
    match read {
        None if !d.to_compare.is_empty() => {
            section = section.child(div().text_xs().text_color(muted).child("Reading the live objects…"));
        }
        None => {}
        Some(read) => {
            if let Some(note) = &read.note
                && !d.to_compare.is_empty()
            {
                section = section.child(div().text_xs().text_color(muted).child(note.clone()));
            }
            if !read.drifts.is_empty() {
                section = section.child(drifts(ctx, &d, read, cx));
            }
        }
    }
    Some(section)
}

fn finding(ctx: &Ctx, ix: usize, f: &Finding, on: Option<&str>, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    v_flex()
        .gap_1()
        .child(
            h_flex()
                .gap_2()
                .child(div().size_2().flex_shrink_0().rounded_full().bg(tone_color(Some(f.tone), cx)))
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(f.title.clone())),
        )
        .when_some(f.message.as_ref(), |d, m| {
            d.child(v_flex().pl_4().children(m.lines().map(|l| mono(l.to_string(), cx))))
        })
        .children(f.objects.iter().enumerate().map(|(j, (res, what))| {
            h_flex()
                .pl_4()
                .gap_2()
                .text_sm()
                .child(object_link(ctx, format!("argo-diag-{ix}-{j}"), res, on, cx))
                .child(div().min_w_0().text_color(muted).child(what.clone()))
        }))
        .when(f.more > 0, |d| d.child(div().pl_4().text_xs().text_color(muted).child(format!("+{} more", f.more))))
        .child(div().pl_4().text_xs().text_color(muted).child(f.hint.clone()))
}

/// An object of the Application, opening on the destination's context when it is connected.
fn object_link(ctx: &Ctx, id: String, res: &Res, on: Option<&str>, cx: &App) -> AnyElement {
    let Some(context) = on else { return div().flex_shrink_0().child(res.label()).into_any_element() };
    let ev = crate::details::OpenObject {
        kind: res.kind_query(),
        name: res.name.clone(),
        namespace: (!res.namespace.is_empty()).then(|| res.namespace.clone()),
        context: Some(context.to_string()),
    };
    let panel = ctx.panel.clone();
    div()
        .id(SharedString::from(id))
        .flex_shrink_0()
        .text_color(cx.theme().blue)
        .cursor_pointer()
        .hover(Styled::underline)
        .child(res.label())
        .on_click(move |_, _, cx| {
            let _ = panel.update(cx, |_, cx| cx.emit(ev.clone()));
        })
        .into_any_element()
}

/// The fields that differ on each object read, who wrote it since the last sync, and the
/// `ignoreDifferences` that would stop a sync loop.
fn drifts(ctx: &Ctx, d: &Diagnosis, read: &DriftRead, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    let on = read.context.as_deref();
    let looping: Vec<&Drift> = read.drifts.iter().filter(|dr| d.looping.contains(&dr.res)).collect();
    v_flex()
        .gap_2()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(format!("What differs live, on {}", on.unwrap_or_default())),
        )
        .child(div().text_xs().text_color(muted).child(DRIFT_SOURCE))
        .children(read.drifts.iter().enumerate().map(|(i, drift)| {
            let lines = drift.lines();
            let unchanged = drift.compared == Compared::LastApplied && drift.fields.is_empty();
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(object_link(ctx, format!("argo-drift-{i}"), &drift.res, on, cx))
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(diagnosis::compared_text(&drift.compared).to_string()),
                        ),
                )
                .when(!lines.is_empty(), |d| d.child(crate::details::diff_block(&lines, cx)))
                .when(drift.more > 0, |d| {
                    d.child(div().text_xs().text_color(muted).child(format!("+{} more fields differ", drift.more)))
                })
                .when(unchanged, |d| {
                    d.child(div().text_xs().text_color(muted).child("Every field applied still has the value applied."))
                })
                .children(drift.writers.iter().map(|w| {
                    div().text_xs().text_color(muted).child(format!(
                        "Written since the last sync by {}, {} ago: {}",
                        w.manager,
                        k8s::age(w.time),
                        w.fields
                    ))
                }))
        }))
        .when_some(diagnosis::ignore_differences(&looping), |d, snippet| {
            d.child(div().text_sm().font_weight(FontWeight::MEDIUM).child("To stop the sync loop, ignore what differs"))
                .child(
                    v_flex()
                        .p_2()
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().border)
                        .children(snippet.lines().map(|l| mono(l.to_string(), cx).whitespace_nowrap())),
                )
        })
}

/// A link's host, for a button's label.
fn short_url(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split('/').next().unwrap_or(rest).to_string()
}

fn history(app: &Summary, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    v_flex().gap_0p5().child(div().text_xs().text_color(muted).child(selectable("Deployed"))).children(
        app.history.iter().map(|h| {
            h_flex()
                .gap_3()
                .text_sm()
                .child(mono(if h.revision.is_empty() { "-".to_string() } else { h.revision.clone() }, cx))
                .child(div().text_color(muted).child(selectable(ago(h.deployed_at.as_deref()).unwrap_or_default())))
                .when_some(h.by.as_ref(), |d, by| {
                    d.child(div().text_color(muted).child(selectable(format!("by {by}"))))
                })
        }),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_url_button_shows_its_host() {
        assert_eq!(super::short_url("https://web.example.com/login?x=1"), "web.example.com");
        assert_eq!(super::short_url("web.example.com"), "web.example.com");
    }
}
