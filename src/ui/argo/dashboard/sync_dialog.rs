//! The Sync dialog of the environment page: a sync running, or the last one, over the
//! overview (see `k8s::argo::sync_phases`). Its header says the result, who, and how long
//! against the usual duration; a bar fills toward the usual duration while it runs; one line
//! says what it waits for, or why it failed; then the four phases as a stepper, each with its
//! hooks, resources or workloads. The table of every resource and the journal are a click
//! away.
//!
//! It reads nothing: it shows the cards of the tab, read again by the page as they are (its
//! stream, and every few seconds while a sync runs), and follows each new read.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::Timestamp;

use super::super::{ArgoView, Key};
use super::View;
use crate::k8s;
use crate::k8s::argo::diagnosis::Res;
use crate::k8s::argo::sync_phases::{Hook, Outcome, Phase, PhaseName, PhaseState, SyncPhases, Workload};
use crate::status::Tone;
use crate::ui::selectable::selectable;

/// The dialog's width, at most (narrower on a small window).
const WIDTH: f32 = 780.;
/// The name column of a hook's or a workload's row.
const NAME_W: f32 = 220.;
/// A workload's bar.
const BAR_W: f32 = 110.;
/// Resources that failed named, at most.
const FAILED_SHOWN: usize = 10;
/// Workloads listed, at most.
const WORKLOADS_SHOWN: usize = 20;
/// Hooks listed per phase, at most.
const HOOKS_SHOWN: usize = 20;

impl ArgoView {
    /// The Last sync card's one entry to the dialog: `Follow` while a sync runs.
    pub(super) fn sync_dialog_button(&self, running: bool, cx: &mut Context<Self>) -> Button {
        let (label, tip) = if running {
            ("Follow", "Follow this sync: its phases, what it waits for, how long against the usual")
        } else {
            ("Details", "This sync: its phases, why it failed if it did, how long against the usual")
        };
        Button::new("argo-dash-sync-open")
            .xsmall()
            .outline()
            .icon(Lucide::ExternalLink)
            .label(label)
            .tooltip(tip)
            .on_click(cx.listener(|this, _, window, cx| this.open_sync_dialog(window, cx)))
    }

    /// Open the Sync dialog of the tab shown, over the page.
    pub(super) fn open_sync_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = &self.dashboard else { return };
        let title = match d.current() {
            Some(k) if d.example.is_none() => format!("Sync of {}", k.2),
            _ => "Sync".to_string(),
        };
        let argo = cx.entity();
        let view = cx.new(|cx| SyncDialog::new(&argo, cx));
        window.open_dialog(cx, move |dialog, _, _| {
            dialog.title(title.clone()).w(px(WIDTH)).on_ok(|_, _, _| true).child(view.clone())
        });
    }
}

/// The dialog's content: the page's cards of the tab shown, as they are read again.
pub(super) struct SyncDialog {
    argo: WeakEntity<ArgoView>,
    _follow: Subscription,
}

impl SyncDialog {
    fn new(argo: &Entity<ArgoView>, cx: &mut Context<Self>) -> Self {
        // Each new read of the page shows here.
        let follow = cx.observe(argo, |_, _, cx| cx.notify());
        Self { argo: argo.downgrade(), _follow: follow }
    }

    /// Close the dialog, then do `then` on the page.
    fn on_page(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        then: impl FnOnce(&mut ArgoView, &mut Window, &mut Context<ArgoView>),
    ) {
        window.close_dialog(cx);
        let _ = self.argo.update(cx, |a, cx| then(a, window, cx));
    }

    /// A button that opens `res` on the context that reaches its cluster, the dialog closed.
    fn open_button(
        &self,
        id: impl Into<ElementId>,
        label: &str,
        tip: &str,
        k: &Key,
        res: &Res,
        cx: &mut Context<Self>,
    ) -> Button {
        let (k, res) = (k.clone(), res.clone());
        Button::new(id).xsmall().outline().label(label.to_string()).tooltip(tip.to_string()).on_click(cx.listener(
            move |this, _, window, cx| {
                let (k, res) = (k.clone(), res.clone());
                this.on_page(window, cx, move |a, _, cx| a.open_deployed(&k, res, cx));
            },
        ))
    }
}

/// What the page holds for the dialog: the tab's sync, the Application (when a context reaches
/// its cluster, to open what it deployed), and its name (for the journal).
struct Shown {
    phases: Option<SyncPhases>,
    opens: Option<Key>,
    name: Option<String>,
    has_steps: bool,
}

impl Render for SyncDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let shown = self.argo.upgrade().and_then(|argo| {
            let a = argo.read(cx);
            let d = a.dashboard.as_ref()?;
            let cards = d.shown()?.ok()?;
            let k = d.current().cloned().filter(|_| d.example.is_none());
            let opens = k.clone().filter(|k| a.find(k).is_some_and(|app| app.on.is_some()));
            Some(Shown {
                phases: cards.sync_phases.clone(),
                opens,
                name: k.map(|k| k.2),
                has_steps: cards.last_sync.as_ref().is_some_and(|s| !s.steps.is_empty()),
            })
        });
        let Some(shown) = shown else {
            return v_flex().pb_2().text_color(theme.muted_foreground).child("Nothing read yet.");
        };
        let Some(p) = &shown.phases else {
            return v_flex().pb_2().text_color(theme.muted_foreground).child("This Application has never synced.");
        };
        let now = Timestamp::now();
        let k = shown.opens.as_ref();
        let last = p.phases.len().saturating_sub(1);
        let phases: Vec<Div> =
            p.phases.iter().enumerate().map(|(i, ph)| self.phase_row(ph, i == last, k, cx)).collect();
        v_flex()
            .gap_3()
            .pb_2()
            .child(header(p, now, cx))
            .children((p.outcome == Outcome::Syncing).then(|| p.share(now)).flatten().map(|s| {
                // Amber past the usual duration.
                let fill = if p.slow(now) { theme.warning } else { theme.blue };
                bar(s, fill, 6., cx)
            }))
            .children(self.reason(p, k, cx))
            .child(v_flex().children(phases))
            .child(self.footer(&shown, cx))
    }
}

impl SyncDialog {
    /// The reason first, in one line: what it waits for, else Argo CD's message unless it went
    /// well.
    fn reason(&self, p: &SyncPhases, k: Option<&Key>, cx: &mut Context<Self>) -> Option<Div> {
        let theme = cx.theme().clone();
        let boxed = |tone: Tone| {
            let color = match tone {
                Tone::Bad => theme.danger,
                _ => theme.blue,
            };
            h_flex()
                .gap_2()
                .items_center()
                .min_w_0()
                .px_3()
                .py_2()
                .rounded_md()
                .text_sm()
                .bg(color.opacity(0.1))
                .border_1()
                .border_color(color.opacity(0.3))
        };
        let label = |text: &str, color: Hsla| {
            div().flex_none().font_weight(FontWeight::SEMIBOLD).text_color(color).child(text.to_string())
        };
        if let Some(w) = &p.waiting {
            let tone = if p.outcome == Outcome::Failed { Tone::Bad } else { Tone::Busy };
            let color = if tone == Tone::Bad { theme.danger } else { theme.blue };
            let what = if p.outcome == Outcome::Syncing { "Waiting for:" } else { "Was waiting for:" };
            let open =
                k.map(|k| self.open_button("argo-sync-waiting-open", "open", "Open it in its cluster", k, &w.res, cx));
            return Some(
                boxed(tone)
                    .child(label(what, color))
                    .child(div().flex_none().text_color(color).child(w.what.clone()))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(color)
                            .child(selectable(w.res.name.clone())),
                    )
                    .children(
                        w.more_text().map(|m| div().flex_none().text_xs().text_color(theme.muted_foreground).child(m)),
                    )
                    .children(open),
            );
        }
        let message = p.message.clone().filter(|_| p.outcome != Outcome::Succeeded)?;
        let tone = if p.outcome == Outcome::Failed { Tone::Bad } else { Tone::Busy };
        let color = if tone == Tone::Bad { theme.danger } else { theme.blue };
        Some(
            boxed(tone)
                .items_start()
                .child(label("Argo CD:", color))
                .child(div().min_w_0().line_clamp(4).text_color(color).child(selectable(message))),
        )
    }

    /// A phase of the stepper: its mark on a line down to the next, its name and state, then
    /// what it holds.
    fn phase_row(&self, ph: &Phase, last: bool, k: Option<&Key>, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let color = state_color(ph.state, cx);
        let icon = match ph.state {
            PhaseState::Done => Lucide::Check,
            PhaseState::Running => Lucide::LoaderCircle,
            PhaseState::Problem => Lucide::TriangleAlert,
            PhaseState::Pending | PhaseState::NotRun => Lucide::Minus,
        };
        let mark = v_flex()
            .flex_none()
            .items_center()
            .w(px(28.))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .size(px(26.))
                    .rounded_full()
                    .bg(color.opacity(0.14))
                    .border_1()
                    .border_color(color.opacity(0.4))
                    .child(Icon::new(icon).size_3p5().text_color(color)),
            )
            .when(!last, |v| v.child(div().flex_1().w(px(2.)).min_h(px(8.)).bg(theme.border)));
        let greyed = matches!(ph.state, PhaseState::Pending | PhaseState::NotRun);
        let title = h_flex()
            .gap_2()
            .items_center()
            .h(px(26.))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(if greyed { muted } else { theme.foreground })
                    .child(ph.name.title()),
            )
            .child(div().text_xs().text_color(color).child(ph.state.word()));
        let body = self.phase_body(ph, k, cx);
        h_flex()
            .items_stretch()
            .gap_3()
            .child(mark)
            .child(v_flex().flex_1().min_w_0().gap_1().pb_3().child(title).children(body))
    }

    /// What a phase holds: hooks, a count and the resources that failed, or the workloads.
    fn phase_body(&self, ph: &Phase, k: Option<&Key>, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let muted = cx.theme().muted_foreground;
        let note = |text: String| div().text_sm().text_color(muted).child(text).into_any_element();
        let not_run = ph.state == PhaseState::NotRun;
        match ph.name {
            PhaseName::PreSync | PhaseName::PostSync => {
                // Pending or not run: its title says so already.
                if ph.hooks.is_empty() {
                    return match ph.state {
                        PhaseState::Pending | PhaseState::NotRun => Vec::new(),
                        _ => vec![note("no hooks".into())],
                    };
                }
                let mut rows: Vec<AnyElement> = ph
                    .hooks
                    .iter()
                    .take(HOOKS_SHOWN)
                    .enumerate()
                    .map(|(i, h)| hook_row(ph.name, i, h, not_run, cx).into_any_element())
                    .collect();
                if ph.hooks.len() > HOOKS_SHOWN {
                    rows.push(note(format!("+{} more", ph.hooks.len() - HOOKS_SHOWN)));
                }
                rows
            }
            PhaseName::Sync => {
                if not_run || (ph.state == PhaseState::Pending && !ph_ran(ph)) {
                    return Vec::new();
                }
                let mut rows = vec![note(ph.count())];
                for (i, f) in ph.failed.iter().take(FAILED_SHOWN).enumerate() {
                    let open = k.map(|k| {
                        self.open_button(("argo-sync-failed-open", i), "open", "Open it in its cluster", k, &f.res, cx)
                    });
                    rows.push(
                        problem_line(cx)
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(Icon::new(Lucide::CircleX).size_3p5().text_color(cx.theme().danger))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .child(selectable(f.res.name.clone())),
                                    )
                                    .child(div().flex_none().text_xs().text_color(muted).child(f.res.kind.clone()))
                                    .child(div().flex_1())
                                    .children(open),
                            )
                            .children(
                                f.message
                                    .clone()
                                    .map(|m| div().text_xs().text_color(muted).line_clamp(3).child(selectable(m))),
                            )
                            .into_any_element(),
                    );
                }
                if ph.failed.len() > FAILED_SHOWN {
                    rows.push(note(format!("+{} more failed", ph.failed.len() - FAILED_SHOWN)));
                }
                for (i, h) in ph.hooks.iter().take(HOOKS_SHOWN).enumerate() {
                    rows.push(hook_row(ph.name, i, h, false, cx).into_any_element());
                }
                rows
            }
            PhaseName::Rollout => {
                if not_run {
                    return Vec::new();
                }
                if ph.workloads.is_empty() {
                    return if ph.state == PhaseState::Pending { Vec::new() } else { vec![note("all ready".into())] };
                }
                let mut rows: Vec<AnyElement> = ph
                    .workloads
                    .iter()
                    .take(WORKLOADS_SHOWN)
                    .enumerate()
                    .map(|(i, w)| self.workload_row(i, w, k, cx).into_any_element())
                    .collect();
                if ph.workloads.len() > WORKLOADS_SHOWN {
                    rows.push(note(format!("+{} more", ph.workloads.len() - WORKLOADS_SHOWN)));
                }
                rows
            }
        }
    }

    /// A workload not healthy yet: its mark, its name, its replicas ready as a bar, its pods'
    /// reasons (starting in blue, broken in red), and, when broken, its actions on the row.
    fn workload_row(&self, i: usize, w: &Workload, k: Option<&Key>, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let bad = w.tone == Tone::Bad;
        let (icon, color) =
            if bad { (Lucide::TriangleAlert, theme.danger) } else { (Lucide::LoaderCircle, theme.blue) };
        let ready = w.ready.map(|(n, m)| {
            let share = if m == 0 { 1. } else { n as f32 / m as f32 };
            h_flex()
                .flex_none()
                .gap_1p5()
                .items_center()
                .child(div().w(px(BAR_W)).child(bar(share, color, 5., cx)))
                .child(div().text_xs().text_color(muted).child(format!("{n}/{m}")))
        });
        let health =
            (w.ready.is_none() || bad).then(|| div().flex_none().text_xs().text_color(color).child(w.health.clone()));
        let reasons = w.reasons.iter().map(|r| {
            let c = if r.tone == Tone::Bad { theme.danger } else { theme.blue };
            div().flex_none().text_xs().text_color(c).child(r.text())
        });
        let actions = k.map(|k| {
            let pod = w.pod.as_ref().map(|pod| {
                self.open_button(
                    ("argo-sync-pod", i),
                    "pod",
                    "Open its pod in trouble: its state, events and logs",
                    k,
                    pod,
                    cx,
                )
            });
            let open = self.open_button(("argo-sync-workload", i), "open", "Open it in its cluster", k, &w.res, cx);
            h_flex().flex_none().gap_1().children(pod).child(open)
        });
        let row = h_flex()
            .gap_2()
            .items_center()
            .min_w_0()
            .child(Icon::new(icon).size_3p5().text_color(color))
            .child(div().w(px(NAME_W)).flex_none().min_w_0().truncate().text_sm().child(selectable(w.res.name.clone())))
            .children(ready)
            .children(health)
            .children(reasons)
            .child(div().flex_1());
        // Its actions on the row when it is broken; on hover otherwise would hide them.
        let row = row.children(actions);
        let line = if bad { problem_line(cx) } else { v_flex().gap_0p5().py_0p5().pl_2() };
        line.child(row).when(bad, |v| {
            v.children(w.message.clone().map(|m| div().text_xs().text_color(muted).line_clamp(2).child(selectable(m))))
        })
    }

    /// The links to what the dialog sums up: the table of every resource, the journal.
    fn footer(&self, shown: &Shown, cx: &mut Context<Self>) -> Div {
        let table = shown.has_steps.then(|| {
            Button::new("argo-sync-table")
                .small()
                .ghost()
                .icon(Lucide::ListChecks)
                .label("Every resource")
                .tooltip("The result of each resource of this sync, in a table on the page")
                .on_click(cx.listener(|this, _, window, cx| {
                    this.on_page(window, cx, |a, _, cx| a.set_view(View::SyncStatus, cx));
                }))
        });
        let journal = shown.name.clone().map(|name| {
            Button::new("argo-sync-journal")
                .small()
                .ghost()
                .icon(Lucide::ScrollText)
                .label("Journal")
                .tooltip("Every sync of this Application this app has seen, even those Argo CD's history no longer has")
                .on_click(cx.listener(move |this, _, window, cx| {
                    let name = name.clone();
                    this.on_page(window, cx, move |a, window, cx| a.open_journal(Some(name), window, cx));
                }))
        });
        h_flex().gap_2().pt_2().border_t_1().border_color(cx.theme().border).children(table).children(journal)
    }
}

/// Something ran in the Sync phase.
fn ph_ran(ph: &Phase) -> bool {
    ph.synced > 0 || !ph.failed.is_empty() || ph.unfinished > 0 || !ph.hooks.is_empty()
}

/// The result, who started it, how long against the usual, and when it ended.
fn header(p: &SyncPhases, now: Timestamp, cx: &App) -> Div {
    let theme = cx.theme();
    let color = p.outcome.tone().color(cx);
    let color = if p.outcome == Outcome::Syncing { theme.blue } else { color };
    let icon = match p.outcome {
        Outcome::Syncing => Lucide::LoaderCircle,
        Outcome::Succeeded => Lucide::CircleCheck,
        Outcome::Failed => Lucide::CircleX,
    };
    let mut facts = Vec::new();
    if let Some(by) = &p.by {
        facts.push(by.clone());
    }
    if let Some(t) = p.duration_text(now) {
        facts.push(t);
    }
    if let Some(t) = p.finished {
        facts.push(format!("{} ago", k8s::age(t)));
    }
    let slow = p.slow(now);
    h_flex()
        .gap_2()
        .items_center()
        .flex_wrap()
        .child(Icon::new(icon).size_5().text_color(color))
        .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).text_color(color).child(p.outcome.word()))
        .when(slow && p.outcome == Outcome::Syncing, |h| {
            h.child(div().text_xs().text_color(theme.warning).child("longer than usual"))
        })
        .child(div().flex_1())
        .child(div().text_sm().text_color(theme.muted_foreground).child(selectable(facts.join(" · "))))
}

/// A bar filled to `share` with `fill`.
fn bar(share: f32, fill: Hsla, height: f32, cx: &App) -> Div {
    let theme = cx.theme();
    div()
        .h(px(height))
        .w_full()
        .rounded_full()
        .bg(theme.muted)
        .child(div().h_full().w(relative(share.clamp(0., 1.))).rounded_full().bg(fill))
}

/// A row that failed: a red edge on its left, its actions on it.
fn problem_line(cx: &App) -> Div {
    let danger = cx.theme().danger;
    v_flex().gap_0p5().py_1().pl_2().pr_1().rounded_sm().border_l_2().border_color(danger).bg(danger.opacity(0.06))
}

/// A phase's color: green done, blue running, red in trouble, grey not reached.
fn state_color(state: PhaseState, cx: &App) -> Hsla {
    match state {
        PhaseState::Running => cx.theme().blue,
        s => s.tone().color(cx),
    }
}

/// A hook: its mark, its name, its status (`not run` when the sync stopped before its phase),
/// and what it says when it failed.
fn hook_row(phase: PhaseName, i: usize, h: &Hook, not_run: bool, cx: &App) -> Stateful<Div> {
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let (status, tone) = if not_run && !matches!(h.tone, Tone::Good | Tone::Bad) {
        ("not run".to_string(), Tone::Done)
    } else {
        (h.status.clone(), h.tone)
    };
    let (icon, color) = match tone {
        Tone::Good => (Lucide::Check, theme.success),
        Tone::Bad => (Lucide::CircleX, theme.danger),
        Tone::Busy if h.status != "pending" => (Lucide::LoaderCircle, theme.blue),
        _ => (Lucide::Minus, muted),
    };
    let id = SharedString::from(format!("argo-sync-hook-{}-{i}", phase.title()));
    div().id(id).child(
        v_flex()
            .gap_0p5()
            .pl_2()
            .when(tone == Tone::Bad, |v| v.border_l_2().border_color(theme.danger))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .min_w_0()
                    .child(Icon::new(icon).size_3p5().text_color(color))
                    .child(
                        div()
                            .w(px(NAME_W))
                            .flex_none()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .child(selectable(h.res.name.clone())),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(if tone == Tone::Done { muted } else { color })
                            .child(status),
                    ),
            )
            .when(tone == Tone::Bad, |v| {
                v.children(
                    h.message.clone().map(|m| div().text_xs().text_color(muted).line_clamp(2).child(selectable(m))),
                )
            }),
    )
}

/// The developer tools' example of the dialog (`RECOUVRANCE_ACT=page:argo,argo-example-sync:running`,
/// or `:failed`, `:done`): the example's first tab with a sync running (hooks done, two
/// workloads rolling, one of them crash-looping), stopped by a timeout, or the last one done.
/// Nothing is read.
#[cfg(feature = "dev")]
impl ArgoView {
    pub fn demo_sync_dialog(&mut self, which: &str, window: &mut Window, cx: &mut Context<Self>) {
        use std::sync::Arc;

        use serde_json::{Value, json};

        use crate::k8s::argo::dashboard::{Cards, example};

        self.demo_dashboard(cx);
        let now = Timestamp::now();
        let ago = |secs: i64| Timestamp::from_second(now.as_second() - secs).unwrap_or(now).to_string();
        let mut read = example::read(example::ENVIRONMENTS[1], now);
        // Each deployment of the history took about five minutes.
        if let Some(history) = read.app.pointer_mut("/status/history").and_then(Value::as_array_mut) {
            for (i, h) in history.iter_mut().enumerate() {
                let Some(at) = h["deployedAt"].as_str().and_then(|t| t.parse::<Timestamp>().ok()) else { continue };
                let took = 270 + 15 * i as i64;
                h["deployStartedAt"] = json!(Timestamp::from_second(at.as_second() - took).unwrap_or(at).to_string());
            }
        }
        if which != "done" {
            let hook = |name: &str| {
                json!({"group": "batch", "kind": "Job", "namespace": "shop", "name": name, "hookType": "PreSync",
                    "syncPhase": "PreSync", "hookPhase": "Succeeded", "status": "Synced", "message": "job completed"})
            };
            let applied = |kind: &str, group: &str, name: &str| {
                json!({"group": group, "kind": kind, "namespace": "shop", "name": name, "status": "Synced",
                    "syncPhase": "Sync", "message": "configured"})
            };
            let resources = json!([
                hook("db-migrate"),
                hook("refresh-secrets"),
                applied("ConfigMap", "", "shop-config"),
                applied("Deployment", "apps", "api"),
                applied("Deployment", "apps", "worker-crawl"),
                applied("Deployment", "apps", "worker-mail"),
            ]);
            let (phase, message, started, finished) = if which == "failed" {
                ("Failed", "Operation terminated, triggered by controller sync timeout", 22 * 60, Some(7 * 60))
            } else {
                ("Running", "waiting for healthy state of apps/Deployment/worker-crawl and 1 more resources", 252, None)
            };
            read.app["status"]["operationState"] = json!({
                "phase": phase,
                "message": message,
                "startedAt": ago(started),
                "finishedAt": finished.map(ago),
                "operation": {"initiatedBy": {"username": "dev-user"}, "sync": {}},
                "syncResult": {"resources": resources},
            });
            // worker-mail rolls out: one pod of two starting.
            if let Some(Ok(tree)) = &mut read.tree
                && let Some(nodes) = tree.pointer_mut("/nodes").and_then(Value::as_array_mut)
            {
                for n in nodes.iter_mut() {
                    match (n["kind"].as_str(), n["name"].as_str()) {
                        (Some("Deployment"), Some("worker-mail")) => {
                            n["health"] = json!({"status": "Progressing",
                                "message": "Waiting for rollout to finish: 1 of 2 updated replicas are available..."});
                        }
                        (Some("Pod"), Some("worker-mail-5f2a-1")) => {
                            n["info"] = json!([{"name": "Status Reason", "value": "ContainerCreating"}]);
                        }
                        _ => {}
                    }
                }
            }
        }
        if let Some((_, cards)) = self.dashboard.as_mut().and_then(|d| d.example.as_mut()).and_then(|t| t.get_mut(0)) {
            *cards = Arc::new(Cards::of(read, None, now));
        }
        self.open_sync_dialog(window, cx);
        cx.notify();
    }
}
