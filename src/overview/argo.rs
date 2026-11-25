//! The Argo CD section: the Application that deployed the object, or those that deploy
//! into the namespace, as read by Argo CD's API (or on Argo CD's cluster). On an
//! Application's page, where it deploys, what is wrong with it, the table of its resources,
//! and the way to its dashboard on the Argo CD page: nothing is written from that page.

use std::rc::Rc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use super::{Ctx, column_cell, field, heading, mono, row};
use crate::details::{ArgoAction, DetailPanel, OpenOn};
use crate::k8s;
use crate::k8s::argo::diagnosis::{self, Compared, Diagnosis, Drift, Finding, Res};
use crate::k8s::argo::{self, Found, ResourceRow, Summary};
use crate::status::Tone;
use crate::ui::selectable::{dragged, selectable};

fn ago(t: Option<&str>) -> Option<String> {
    t?.parse::<jiff::Timestamp>().ok().map(|t| format!("{} ago", k8s::age(t)))
}

/// A word in its tone's color.
fn word(text: &str, tone: Tone, cx: &App) -> Div {
    div().text_color(tone.color(cx)).child(selectable(text.to_string()))
}

/// Nothing when Argo CD deploys nothing here; one line when it could not be read. On an
/// Application's page, its own status first: the Application that deployed it (its parent)
/// comes after, under "Deployed by", so its status isn't read as this one's.
pub(super) fn section(ctx: &Ctx, cx: &App) -> Option<Div> {
    let lookup = ctx.argo;
    let own = ctx.kind.is("argoproj.io", "Application").then(|| Summary::of(ctx.obj));
    if own.is_none() && lookup.apps.is_empty() && lookup.note.is_none() {
        return None;
    }
    let muted = cx.theme().muted_foreground;
    // A namespace can hold many Applications: one line each. An object has its one in full.
    let full = !ctx.kind.is("", "Namespace");
    // No server read: how many is unknown, not 0.
    let title = match (full, lookup.unknown) {
        (true, _) => "Argo CD".to_string(),
        (false, true) => "Argo CD Applications".to_string(),
        (false, false) => format!("Argo CD Applications ({})", lookup.apps.len()),
    };
    let deployed_by = own.is_some() && !lookup.apps.is_empty();
    Some(
        v_flex()
            .gap_2()
            .child(heading(&title))
            .when_some(own.as_ref(), |d, app| d.child(own_card(ctx, app, cx)))
            .when(deployed_by, |d| d.child(div().text_xs().text_color(muted).child(selectable("Deployed by"))))
            .when_some(lookup.note.as_ref(), |d, note| {
                d.child(div().text_xs().text_color(muted).child(selectable(note.clone())))
            })
            .children(lookup.apps.iter().enumerate().map(|(ix, f)| app_card(ctx, ix, f, full, cx))),
    )
}

/// An Application's sync, health, auto-sync, revision and last sync, in one row.
fn status_row(app: &Summary, cx: &App) -> Div {
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
}

/// The Application of the page itself: its status, health message and last deployments, and
/// where to act on it: the Argo CD page's dashboard, on the Argo CD server that holds it
/// (found by its uid). Nothing is written from here. What is wrong with it (a failed sync,
/// errors, a deletion held) is under Diagnosis.
fn own_card(ctx: &Ctx, app: &Summary, cx: &App) -> Div {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let panel = ctx.panel.clone();
    let open_in_argo = match &ctx.argo.own {
        None => None,
        Some(Ok(endpoint)) => Some(
            h_flex().gap_2().child(
                Button::new("argo-own-open")
                    .xsmall()
                    .outline()
                    .icon(Lucide::ExternalLink)
                    .label("Open in Argo CD")
                    .tooltip(format!("Its dashboard on the Argo CD page, read from {}", endpoint.name()))
                    .on_click(move |_, _, cx| {
                        let _ = panel.update(cx, DetailPanel::open_own_in_argo);
                    }),
            ),
        ),
        Some(Err(why)) => Some(h_flex().child(div().text_xs().text_color(muted).child(selectable(why.clone())))),
    };
    v_flex()
        .p_2()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary.opacity(0.4))
        .child(status_row(app, cx))
        .when_some(app.health_message.as_ref(), |d, m| {
            d.child(div().text_sm().text_color(muted).child(selectable(m.clone())))
        })
        .when(!app.history.is_empty(), |d| d.child(history(app, ("argo-own-rollback", 0), None, cx)))
        .children(open_in_argo)
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
        .child(status_row(app, cx).when_some(app.own.as_ref(), |d, (sync, health, _)| {
            d.child(field(
                "This object",
                h_flex()
                    .gap_2()
                    .child(word(sync, argo::sync_tone(sync, app.automated), cx))
                    .when(!health.is_empty(), |d| d.child(word(health, argo::health_tone(health), cx))),
                cx,
            ))
        }))
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
        .when(full && !app.history.is_empty(), |d| {
            let panel = ctx.panel.clone();
            let roll_back: RollBack = Rc::new(move |id: i64, window: &mut Window, cx: &mut App| {
                let _ = panel.update(cx, |p, cx| p.confirm_argo(ix, ArgoAction::Rollback(Some(id)), window, cx));
            });
            d.child(history(app, ("argo-rollback", ix * 16), Some(&roll_back), cx))
        })
}

/// The name (opens the Application on its cluster), where it runs, and its actions.
fn header(ctx: &Ctx, ix: usize, found: &Found, cx: &App) -> Div {
    let app = &found.app;
    let panel = ctx.panel.clone();
    let (open, refresh, sync, terminate) = (panel.clone(), panel.clone(), panel.clone(), panel);
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
        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(selectable(format!(
            "{}/{} on {}",
            app.namespace,
            app.name,
            found.server.name()
        ))))
        .child(div().flex_1())
        .children(app.urls.iter().take(3).enumerate().map(|(i, url)| {
            let url = url.clone();
            Button::new(("argo-url", ix * 8 + i))
                .xsmall()
                .ghost()
                .icon(Lucide::ExternalLink)
                .label(short_url(&url))
                .tooltip(url.clone())
                .on_click(move |_, window, cx| crate::forwards::open_url(&url, window, cx))
        }))
        .child(Button::new(("argo-refresh", ix)).xsmall().ghost().icon(Lucide::RefreshCw).label("Refresh").on_click(
            move |_, window, cx| {
                let _ = refresh.update(cx, |p, cx| p.confirm_argo(ix, ArgoAction::Refresh, window, cx));
            },
        ))
        .child(Button::new(("argo-sync", ix)).xsmall().ghost().icon(Lucide::GitBranch).label("Sync").on_click(
            move |_, window, cx| {
                let _ = sync.update(cx, |p, cx| p.confirm_argo(ix, ArgoAction::Sync, window, cx));
            },
        ))
        // While an operation runs: it can be stopped, as `argocd app terminate-op` does.
        .when(app.in_progress, |d| {
            d.child(
                Button::new(("argo-terminate", ix))
                    .xsmall()
                    .ghost()
                    .icon(Lucide::CircleStop)
                    .label("Terminate")
                    .on_click(move |_, window, cx| {
                        let _ = terminate.update(cx, |p, cx| p.confirm_argo(ix, ArgoAction::Terminate, window, cx));
                    }),
            )
        })
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
                        let _ = panel.update(cx, DetailPanel::open_destination);
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
    let read = ctx.related.argo_drift.as_ref();
    let d = Diagnosis::of_live(ctx.obj, jiff::Timestamp::now(), read.map_or_default(|r| r.drifts.as_slice()));
    if d.is_empty() {
        return None;
    }
    let muted = cx.theme().muted_foreground;
    let mut section = v_flex().gap_3().child(heading("Diagnosis"));
    for (ix, f) in d.findings.iter().enumerate() {
        section = section.child(finding(ctx, ix, f, cx));
    }
    match read {
        None if !d.to_read().is_empty() => {
            section = section.child(div().text_xs().text_color(muted).child(selectable("Reading the live objects…")));
        }
        None => {}
        Some(read) => {
            if let Some(note) = &read.note
                && !d.to_read().is_empty()
            {
                section = section.child(div().text_xs().text_color(muted).child(selectable(note.clone())));
            }
            let compared = d.compared(read);
            if !compared.is_empty() {
                section = section.child(drifts(ctx, &d, read.context.as_deref(), &compared, cx));
            }
        }
    }
    Some(section)
}

fn finding(ctx: &Ctx, ix: usize, f: &Finding, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    v_flex()
        .gap_1()
        .child(
            h_flex()
                .gap_2()
                .child(div().size_2().flex_shrink_0().rounded_full().bg(f.tone.color(cx)))
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(selectable(f.title.clone()))),
        )
        .when_some(f.message.as_ref(), |d, m| {
            d.child(v_flex().pl_4().children(m.lines().map(|l| mono(l.to_string(), cx))))
        })
        .children(f.objects.iter().enumerate().map(|(j, (res, what))| {
            h_flex()
                .pl_4()
                .gap_2()
                .text_sm()
                .child(
                    object_link(ctx, SharedString::from(format!("argo-diag-{ix}-{j}")), res.label(), res, cx)
                        .flex_shrink_0(),
                )
                .child(div().min_w_0().text_color(muted).child(selectable(what.clone())))
        }))
        .when(f.more > 0, |d| {
            d.child(div().pl_4().text_xs().text_color(muted).child(selectable(format!("+{} more", f.more))))
        })
        .child(div().pl_4().text_xs().text_color(muted).child(selectable(f.hint.clone())))
}

/// The context that reaches the Application's destination (a connected one first), where
/// its objects open. None until that is read, and when no context of the kubeconfig does.
fn destination_context<'a>(ctx: &Ctx<'a>) -> Option<&'a str> {
    ctx.related.argo_destination.as_ref().and_then(|(_, on)| on.as_deref())
}

/// `text`, opening an object of the Application on the context that reaches its destination
/// (connecting to it when it isn't connected); plain text when no context reaches it.
fn object_link(ctx: &Ctx, id: impl Into<ElementId>, text: String, res: &Res, cx: &App) -> Stateful<Div> {
    let link = div().id(id).child(selectable(text));
    let Some(context) = destination_context(ctx) else { return link };
    let ev = OpenOn { context: context.to_string(), res: res.clone() };
    let panel = ctx.panel.clone();
    link.text_color(cx.theme().blue).cursor_pointer().hover(Styled::underline).on_click(move |click, _, cx| {
        if !dragged(click) {
            let _ = panel.update(cx, |_, cx| cx.emit(ev.clone()));
        }
    })
}

/// The fields that differ on each object read, who wrote it since the last sync, and the
/// `ignoreDifferences` that would stop a sync loop.
fn drifts(ctx: &Ctx, d: &Diagnosis, on: Option<&str>, compared: &[&Drift], cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    let looping: Vec<&Drift> = compared.iter().copied().filter(|dr| d.looping.contains(&dr.res)).collect();
    v_flex()
        .gap_2()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .child(selectable(format!("What differs live, on {}", on.unwrap_or_default()))),
        )
        .child(div().text_xs().text_color(muted).child(selectable(DRIFT_SOURCE)))
        .children(compared.iter().enumerate().map(|(i, drift)| {
            let lines = drift.lines();
            let unchanged = drift.compared == Compared::LastApplied && drift.fields.is_empty();
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(object_link(ctx, ("argo-drift", i), drift.res.label(), &drift.res, cx).flex_shrink_0())
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(selectable(diagnosis::compared_text(&drift.compared).to_string())),
                        ),
                )
                .when(!lines.is_empty(), |d| d.child(crate::details::diff_block(&lines, cx)))
                .when(drift.more > 0, |d| {
                    d.child(div().text_xs().text_color(muted).child(selectable(format!(
                        "+{} more {}",
                        drift.more,
                        crate::plural::pick(drift.more, "field differs", "fields differ")
                    ))))
                })
                .when(unchanged, |d| {
                    d.child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(selectable("Every field applied still has the value applied.")),
                    )
                })
                .children(drift.writers.iter().map(|w| {
                    div().text_xs().text_color(muted).child(selectable(format!(
                        "Written since the last sync by {}, {} ago: {}",
                        w.manager,
                        k8s::age(w.time),
                        w.fields
                    )))
                }))
        }))
        .when_some(diagnosis::ignore_differences(&looping), |d, snippet| {
            d.child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child(selectable("To stop the sync loop, ignore what differs")),
            )
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

// MARK: Resources

/// An Application's resources table, worked out when the Application is read: it can list
/// thousands, and the rows on screen are drawn again on every frame while the page scrolls.
#[derive(Default)]
pub struct ArgoResources {
    rows: Vec<ResourceRow>,
    /// The rows out of sync or unhealthy (`ResourceRow::needs_attention`), as indexes in `rows`.
    attention: Vec<usize>,
    /// Auto-sync is on: out of sync is then something to look at, not the normal state.
    automated: bool,
    /// Argo CD keeps no health per resource in this Application (`argo::keeps_no_health`).
    no_health: bool,
    /// The rows out of sync or unhealthy are shown alone. Kept across reads.
    pub alone: bool,
}

impl ArgoResources {
    /// Take the resources of the Application as read now.
    pub fn set(&mut self, app: &Value) {
        let rows = argo::resource_rows(app);
        self.attention = rows.iter().enumerate().filter_map(|(i, r)| r.needs_attention().then_some(i)).collect();
        self.automated = argo::auto_sync(app);
        self.no_health = argo::keeps_no_health(app);
        self.rows = rows;
    }

    /// Whether the rows out of sync or unhealthy are shown alone: picking them when there are
    /// none would show nothing.
    fn attention_alone(&self) -> bool {
        self.alone && !self.attention.is_empty()
    }

    /// How many rows are shown.
    pub fn shown(&self) -> usize {
        if self.attention_alone() { self.attention.len() } else { self.rows.len() }
    }

    /// The row shown at `i`.
    pub fn row(&self, i: usize) -> Option<&ResourceRow> {
        let ix = if self.attention_alone() { *self.attention.get(i)? } else { i };
        self.rows.get(ix)
    }
}

/// The resources table's columns: header, width, and whether it gives up width in a narrow
/// window (`column_cell`). The header and every row take their widths from here, so the cells
/// line up; the message after them takes the room left.
const COLUMNS: [(&str, f32, bool); 5] =
    [("Kind", 160., true), ("Name", 340., true), ("Sync", 90., false), ("Health", 100., false), ("Hook", 50., false)];
/// The message's width, at least.
const MESSAGE: f32 = 160.;

/// Said in a resource's message when it would be pruned (`requiresPruning`).
const PRUNE: &str = "no longer in git: a sync with prune deletes it";
/// Said over the table when no context reaches the destination: its rows open nothing.
const NO_CONTEXT: &str =
    "No context of your kubeconfig reaches its destination cluster: its resources can't be opened.";
/// Said over the table when its Health column is empty for that reason.
const NO_HEALTH: &str = "Argo CD keeps no health per resource in this Application: from Argo CD 3.0 it does only with \
    `controller.resource.health.persist` on.";

/// The message of a resources table's row (or its header): the room left, on one line.
fn message_cell() -> Div {
    h_flex().flex_1().min_w(px(MESSAGE)).gap_2().overflow_hidden().whitespace_nowrap()
}

/// An Application's page: the heading of its resources table, a toggle showing those out of
/// sync or unhealthy alone, why a link or the health is missing, and the columns. Its rows are
/// items of the page's list (`resource_row`), so only those on screen are laid out.
pub(super) fn resources_table(ctx: &Ctx, cx: &App) -> Div {
    let table = ctx.argo_resources;
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let note = |text: &'static str| div().pb_2().text_xs().text_color(muted).child(selectable(text));
    let toggle = (!table.attention.is_empty()).then(|| {
        let panel = ctx.panel.clone();
        Button::new("argo-attention")
            .xsmall()
            .when(table.alone, ButtonVariants::primary)
            .when(!table.alone, Button::outline)
            .label(format!("Out of sync or unhealthy ({})", table.attention.len()))
            .on_click(move |_, _, cx| {
                let _ = panel.update(cx, DetailPanel::toggle_argo_attention);
            })
    });
    // Read, and no context reaches it (before the read, it isn't known yet).
    let unreachable = matches!(ctx.related.argo_destination, Some((_, None)));
    v_flex()
        .child(heading(&format!("Resources ({})", table.rows.len())))
        .when_some(toggle, |d, toggle| d.child(h_flex().pb_2().child(toggle)))
        .when(unreachable && !table.rows.is_empty(), |d| d.child(note(NO_CONTEXT)))
        .when(table.no_health && !table.rows.is_empty(), |d| d.child(note(NO_HEALTH)))
        .child(if table.rows.is_empty() {
            note("Argo CD lists no resources for it.")
        } else {
            h_flex()
                .gap_3()
                .py_1()
                .text_xs()
                .text_color(muted)
                .border_b_1()
                .border_color(theme.border)
                .children(COLUMNS.map(|(title, width, shrinks)| column_cell(width, shrinks).child(title)))
                .child(message_cell().child("Message"))
        })
}

/// A row of an Application's resources table: its object (opening on the destination's
/// context), its sync and health in their tone, whether it is a hook, and why it would be
/// pruned or isn't healthy.
pub(super) fn resource_row(ctx: &Ctx, i: usize, cx: &App) -> AnyElement {
    let table = ctx.argo_resources;
    let Some(r) = table.row(i) else { return div().into_any_element() };
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let [kind, name, sync, health, hook] = COLUMNS.map(|(_, width, shrinks)| column_cell(width, shrinks));
    let shown_name =
        if r.res.namespace.is_empty() { r.res.name.clone() } else { format!("{}/{}", r.res.namespace, r.res.name) };
    let link = object_link(ctx, ("argo-resource", i), shown_name, &r.res, cx);
    h_flex()
        .gap_3()
        .py_1()
        .text_sm()
        .border_b_1()
        .border_color(theme.border)
        .child(kind.child(r.res.kind.clone()))
        .child(name.child(link.overflow_hidden().text_ellipsis().whitespace_nowrap()))
        .child(sync.text_color(argo::sync_tone(&r.sync, table.automated).color(cx)).child(r.sync.clone()))
        .child(
            health.text_color(argo::listed_health_tone(&r.health, table.automated).color(cx)).child(r.health.clone()),
        )
        .child(hook.text_color(muted).child(if r.hook { "yes" } else { "" }))
        .child(
            message_cell()
                .when(r.prune, |d| d.child(div().flex_none().text_color(theme.warning).child(PRUNE)))
                .when_some(r.message.clone(), |d, m| {
                    d.child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().text_color(muted).child(m))
                }),
        )
        .into_any_element()
}

/// A link's host, for a button's label.
fn short_url(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split('/').next().unwrap_or(rest).to_string()
}

/// Roll an Application back to the deployment with this id (after its confirmation).
type RollBack = Rc<dyn Fn(i64, &mut Window, &mut App)>;

/// The last deployments, newest first. With `roll_back`, each but the newest offers to roll
/// back to it, which Argo CD allows only with auto-sync off; `id` numbers those buttons.
fn history(app: &Summary, id: (&'static str, usize), roll_back: Option<&RollBack>, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    v_flex().gap_0p5().child(div().text_xs().text_color(muted).child(selectable("Deployed"))).children(
        app.history.iter().enumerate().map(|(i, h)| {
            let button = h.id.filter(|_| i > 0).zip(roll_back).map(|(deployment, roll_back)| {
                let roll_back = roll_back.clone();
                Button::new((id.0, id.1 + i))
                    .xsmall()
                    .ghost()
                    .icon(Lucide::Undo2)
                    .label("Roll back…")
                    .when(app.automated, |b| {
                        b.disabled(true)
                            .tooltip("Auto-sync is on: Argo CD refuses a rollback. Revert the change in git.")
                    })
                    .when(!app.automated, |b| {
                        b.tooltip(format!("Deploy this revision again (deployment {deployment})"))
                    })
                    .on_click(move |_, window, cx| roll_back(deployment, window, cx))
            });
            h_flex()
                .gap_3()
                .text_sm()
                .child(mono(if h.revision.is_empty() { "-".to_string() } else { h.revision.clone() }, cx))
                .child(div().text_color(muted).child(selectable(ago(h.deployed_at.as_deref()).unwrap_or_default())))
                .when_some(h.by.as_ref(), |d, by| {
                    d.child(div().text_color(muted).child(selectable(format!("by {by}"))))
                })
                .children(button)
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::ArgoResources;
    use serde_json::json;

    #[test]
    fn a_url_button_shows_its_host() {
        assert_eq!(super::short_url("https://web.example.com/login?x=1"), "web.example.com");
        assert_eq!(super::short_url("web.example.com"), "web.example.com");
    }

    fn shown(table: &ArgoResources) -> Vec<String> {
        (0..table.shown()).filter_map(|i| table.row(i)).map(|r| r.res.name.clone()).collect()
    }

    #[test]
    fn the_resources_out_of_sync_or_unhealthy_can_be_shown_alone() {
        let app = json!({"status": {"resources": [
            {"kind": "ConfigMap", "namespace": "shop", "name": "a", "status": "Synced"},
            {"kind": "ConfigMap", "namespace": "shop", "name": "b", "status": "OutOfSync"},
            {"kind": "ConfigMap", "namespace": "shop", "name": "c", "status": "Synced"},
            {"group": "apps", "kind": "Deployment", "namespace": "shop", "name": "d", "status": "Synced",
             "health": {"status": "Degraded"}}
        ]}});
        let mut table = ArgoResources::default();
        table.set(&app);
        assert_eq!(shown(&table), ["a", "b", "c", "d"]);
        // It keeps each resource's health: no note over the table.
        assert!(!table.no_health);
        table.alone = true;
        assert_eq!(shown(&table), ["b", "d"]);
        assert!(table.row(2).is_none());
        // Read again with nothing wrong: every row, rather than none; the choice is kept. Its
        // health is now in Argo CD's resource tree, as the Application says.
        table.set(&json!({"status": {"resourceHealthSource": "appTree", "resources": [
            {"kind": "ConfigMap", "name": "a", "status": "Synced"}
        ]}}));
        assert_eq!(shown(&table), ["a"]);
        assert!(table.alone);
        assert!(table.no_health);
    }
}

/// Rules that hold for every list of resources.
#[cfg(test)]
mod properties {
    use super::ArgoResources;
    use proptest::prelude::*;
    use serde_json::{Value, json};

    proptest! {
        /// Shown alone, the rows out of sync or unhealthy are all of them and only them, in
        /// Argo CD's order; with none, every row is shown rather than nothing.
        #[test]
        fn rows_shown_alone_are_those_that_need_attention(
            states in proptest::collection::vec(
                (any::<bool>(), any::<bool>(), proptest::sample::select(vec!["", "Healthy", "Progressing", "Degraded", "Missing"])),
                0..30,
            ),
            alone in any::<bool>(),
        ) {
            let resources: Vec<Value> = states
                .iter()
                .enumerate()
                .map(|(i, (out, prune, health))| json!({
                    "kind": "ConfigMap",
                    "name": format!("r{i}"),
                    "status": if *out { "OutOfSync" } else { "Synced" },
                    "requiresPruning": prune,
                    "health": {"status": health},
                }))
                .collect();
            let mut table = ArgoResources::default();
            table.set(&json!({"status": {"resources": resources}}));
            table.alone = alone;
            let shown: Vec<_> = (0..table.shown()).filter_map(|i| table.row(i)).cloned().collect();
            prop_assert_eq!(shown.len(), table.shown());
            prop_assert!(table.row(table.shown()).is_none());
            let wanted: Vec<_> = table.rows.iter().filter(|r| r.needs_attention()).cloned().collect();
            if alone && !wanted.is_empty() {
                prop_assert_eq!(shown, wanted);
            } else {
                prop_assert_eq!(shown.as_slice(), table.rows.as_slice());
            }
        }
    }
}
