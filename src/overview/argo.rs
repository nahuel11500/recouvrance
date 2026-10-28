//! The Argo CD section: the Application that deployed the object, or those that deploy
//! into the namespace, as read on Argo CD's cluster.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{Ctx, field, heading, mono, row, tone_color};
use crate::k8s;
use crate::k8s::argo::{self, Found, Summary};
use crate::status::Tone;

fn ago(t: Option<&str>) -> Option<String> {
    t?.parse::<jiff::Timestamp>().ok().map(|t| format!("{} ago", k8s::age(t)))
}

/// A word in its tone's color.
fn word(text: &str, tone: Tone, cx: &App) -> Div {
    div().text_color(tone_color(Some(tone), cx)).child(text.to_string())
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
                d.child(div().text_xs().text_color(cx.theme().muted_foreground).child(note.clone()))
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
            d.child(
                div()
                    .text_sm()
                    .text_color(theme.danger)
                    .child(format!("Being deleted since {when}: a finalizer or a resource it waits for keeps it")),
            )
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
        .when_some(app.health_message.as_ref(), |d, m| d.child(div().text_sm().text_color(muted).child(m.clone())))
        .when_some(app.own.as_ref().and_then(|o| o.2.as_ref()), |d, m| {
            d.child(div().text_sm().text_color(muted).child(format!("This object: {m}")))
        })
        .when_some(op_message, |d, m| d.child(div().text_sm().text_color(muted).child(format!("Last sync: {m}"))))
        .children(app.conditions.iter().map(|(ty, message)| {
            let color = if ty.ends_with("Warning") { theme.warning } else { theme.danger };
            div().text_sm().text_color(color).child(format!("{ty}: {message}"))
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
                .child(app.name.clone())
                .on_click(move |_, _, cx| {
                    let _ = open.update(cx, |p, cx| p.open_argo_app(ix, cx));
                }),
        )
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(format!("{}/{} on {}", app.namespace, app.name, found.hub.context)),
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

/// A link's host, for a button's label.
fn short_url(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split('/').next().unwrap_or(rest).to_string()
}

fn history(app: &Summary, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    v_flex().gap_0p5().child(div().text_xs().text_color(muted).child("Deployed")).children(app.history.iter().map(
        |h| {
            h_flex()
                .gap_3()
                .text_sm()
                .child(mono(if h.revision.is_empty() { "-".to_string() } else { h.revision.clone() }, cx))
                .child(div().text_color(muted).child(ago(h.deployed_at.as_deref()).unwrap_or_default()))
                .when_some(h.by.as_ref(), |d, by| d.child(div().text_color(muted).child(format!("by {by}"))))
        },
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_url_button_shows_its_host() {
        assert_eq!(super::short_url("https://web.example.com/login?x=1"), "web.example.com");
        assert_eq!(super::short_url("web.example.com"), "web.example.com");
    }
}
