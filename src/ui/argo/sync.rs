//! Sync selected: the Applications picked (those the filter keeps) synced together, with or
//! without hooks, with prune and dry run (see `k8s::argo::grouped_sync`).
//!
//! The page reads each Application first, a few at a time: one already syncing, being
//! deleted, or held by a sync window now is left out, saying why; with prune on, what it
//! deletes on each is listed. Sync… asks for a confirmation (their count typed, or the wait
//! set in Settings), then sends the syncs at most four at a time, write mode checked as each
//! is sent, and follows each Application until its operation ends: its phase, message and
//! resources done, with Terminate while it runs, then how many succeeded and failed.
//!
//! Leaving the page while a sync runs only hides it: the syncs go on, and the selection bar
//! shows it again.

use std::time::{Duration, Instant};

use futures::StreamExt as _;
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use super::dashboard::{chip, link};
use super::grouped::{labels, some};
use super::{ArgoView, Key, key};
use crate::details::{ConfirmButtons as _, Guard, confirm_extras, guard_ok};
use crate::k8s::argo::grouped_diff::MAX_APPS;
use crate::k8s::argo::grouped_sync::{self, Checked, Options, PARALLEL, Progress, Target};
use crate::k8s::argo::operation_tone;
use crate::k8s::argo::ops::{self, Op, SyncRequest};
use crate::k8s::argo::windows::{NextSync, Planner};
use crate::status::Tone;
use crate::ui::selectable::selectable;

/// How often the Applications synced are read while their operations run.
const READ_EVERY: Duration = Duration::from_secs(2);
/// They are followed this long at most.
const FOLLOW_FOR: Duration = Duration::from_secs(30 * 60);
/// Applications named in the confirmation, at most.
const NAMED: usize = 10;
const TERMINATE_NOTE: &str =
    "Argo CD stops the operation running, hooks included. What it applied so far stays as it is.";

/// One Application of the sync.
struct Row {
    key: Key,
    label: String,
    /// Its hub context or API server: what a terminate's confirmation names.
    source: String,
    target: Target,
    /// When a manual sync of it can start, under its project's sync windows.
    window: NextSync,
    /// What its read before the sync found.
    checked: Option<Result<Checked, String>>,
    /// Part of the sync sent.
    in_sync: bool,
    /// The answer to its sync's request; None while it is sent.
    sent: Option<Result<String, String>>,
    /// How its operation goes, once sent; an error when it couldn't be sent.
    progress: Option<Result<Progress, String>>,
    /// Why the last read while following it failed.
    note: Option<String>,
}

impl Row {
    fn ready(&self) -> Option<&Checked> {
        match &self.checked {
            Some(Ok(c)) if c.refused.is_none() => Some(c),
            _ => None,
        }
    }

    /// Its operation ended (or it couldn't be sent).
    fn ended(&self) -> bool {
        match &self.progress {
            Some(Ok(p)) => p.ended,
            Some(Err(_)) => true,
            None => false,
        }
    }
}

/// The sync of the picked Applications, open on the page.
pub(super) struct GroupedSync {
    rows: Vec<Row>,
    /// Picked but hidden by the filter: not synced.
    hidden: usize,
    /// Picked past `MAX_APPS`: not synced.
    left_out: usize,
    opts: Options,
    /// The options it was sent with; None before.
    sent_with: Option<Options>,
    /// Shown instead of the list (hidden while it runs, when left).
    pub(super) shown: bool,
    /// Followed too long: no longer read.
    gave_up: bool,
    checks: crate::latest::Latest,
    sends: Option<Task<()>>,
    follow: Option<Task<()>>,
}

impl GroupedSync {
    /// Sent, and some of it is still going.
    pub(super) fn running(&self) -> bool {
        self.sent_with.is_some() && !self.gave_up && self.rows.iter().any(|r| r.in_sync && !r.ended())
    }

    /// The Applications of the sync.
    pub(super) fn keys(&self) -> Vec<Key> {
        self.rows.iter().map(|r| r.key.clone()).collect()
    }

    /// How many are synced and how many are still going: the selection bar's word on it.
    pub(super) fn status(&self) -> String {
        let synced: Vec<&Row> = self.rows.iter().filter(|r| r.in_sync).collect();
        let going = synced.iter().filter(|r| !r.ended()).count();
        format!("Sync of {}: {going} running", crate::plural::count(synced.len(), "Application"))
    }

    fn checking(&self) -> bool {
        self.rows.iter().any(|r| r.checked.is_none())
    }

    /// The Applications to follow now (sent, not ended), and whether some are still sent.
    fn to_follow(&self) -> (Vec<(usize, Target, Option<String>)>, bool) {
        let follow = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.in_sync && matches!(r.sent, Some(Ok(_))) && !r.ended())
            .map(|(i, r)| {
                let before = r.checked.as_ref().and_then(|c| c.as_ref().ok()).and_then(|c| c.before.clone());
                (i, r.target.clone(), before)
            })
            .collect();
        let sending = self.rows.iter().any(|r| r.in_sync && r.sent.is_none());
        (follow, sending)
    }
}

impl ArgoView {
    /// Open the sync of the Applications picked that the filter keeps, and read each. A sync
    /// still running is shown again instead.
    pub(super) fn sync_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(g) = self.sync.as_mut().filter(|g| g.running()) {
            g.shown = true;
            cx.notify();
            return;
        }
        if !crate::details::ensure_writes(window, cx) {
            return;
        }
        let mut apps = self.selected_apps();
        let Some(listed) = &self.rows else { return };
        if apps.is_empty() {
            return;
        }
        let left_out = apps.len().saturating_sub(MAX_APPS);
        apps.truncate(MAX_APPS);
        let names = labels(&apps);
        let mut plan = Planner::new(jiff::Timestamp::now());
        let rows = apps
            .iter()
            .zip(names)
            .map(|(a, label)| Row {
                key: key(a),
                label,
                source: a.source.label(),
                target: Target {
                    source: a.source.clone(),
                    instance: a.instance.clone(),
                    namespace: a.app.namespace.clone(),
                    name: a.app.name.clone(),
                },
                window: listed.fleet.sync_window(a, &mut plan, true),
                checked: None,
                in_sync: false,
                sent: None,
                progress: None,
                note: None,
            })
            .collect();
        self.sync = Some(GroupedSync {
            rows,
            hidden: self.picked_hidden,
            left_out,
            opts: Options::default(),
            sent_with: None,
            shown: true,
            gave_up: false,
            checks: Default::default(),
            sends: None,
            follow: None,
        });
        self.check_grouped(cx);
        cx.notify();
    }

    /// Read each Application of the sync, at most a few at a time, and check it.
    fn check_grouped(&mut self, cx: &mut Context<Self>) {
        let Some(g) = &mut self.sync else { return };
        if g.sent_with.is_some() {
            return;
        }
        for r in &mut g.rows {
            r.checked = None;
        }
        let targets: Vec<Target> = g.rows.iter().map(|r| r.target.clone()).collect();
        let reads = grouped_sync::bounded(targets.len(), PARALLEL, move |i| targets[i].read());
        let now = jiff::Timestamp::now();
        g.checks.spawn_each(cx, reads, move |this: &mut Self, (i, read), cx| {
            if let Some(r) = this.sync.as_mut().and_then(|g| g.rows.get_mut(i)) {
                r.checked = Some(read.map(|app| grouped_sync::check(&app, &r.window, now)));
            }
            cx.notify();
        });
    }

    /// Back to the list: a sync running is only hidden.
    fn close_sync(&mut self, cx: &mut Context<Self>) {
        if self.sync.as_ref().is_some_and(GroupedSync::running) {
            if let Some(g) = &mut self.sync {
                g.shown = false;
            }
        } else {
            self.sync = None;
        }
        cx.notify();
    }

    fn update_sync(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut GroupedSync)) {
        if let Some(g) = &mut self.sync {
            f(g);
        }
        cx.notify();
    }

    /// Read what a sync would change (the grouped diff of the same Applications): its summary
    /// shows here once read.
    fn sync_diff(&mut self, cx: &mut Context<Self>) {
        if let Some(g) = &mut self.sync {
            g.shown = false;
        }
        self.diff_selected(false, cx);
    }

    /// The grouped diff's summary, when it is of the same Applications as the sync.
    fn sync_diff_summary(&self) -> Option<String> {
        let (g, d) = (self.sync.as_ref()?, self.grouped.as_ref()?);
        (d.keys() == g.keys()).then(|| d.summary_line()).flatten()
    }

    /// Confirm the sync of the Applications ready (their count typed, or a wait), then send it.
    fn confirm_grouped_sync(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !crate::details::ensure_writes(window, cx) {
            return;
        }
        let Some(g) = &self.sync else { return };
        if g.sent_with.is_some() || g.checking() {
            return;
        }
        let opts = g.opts;
        let jobs: Vec<(usize, SyncRequest)> =
            g.rows.iter().enumerate().filter_map(|(i, r)| Some((i, opts.request(&r.ready()?.pruned)))).collect();
        if jobs.is_empty() {
            return;
        }
        let n = jobs.len();
        let what = crate::plural::count(n, "Application");
        let names = some(jobs.iter().map(|(i, _)| g.rows[*i].label.clone()), NAMED);
        let left = g.rows.len() - n;
        let deletes: Vec<String> = if opts.prune {
            jobs.iter()
                .filter_map(|(i, _)| {
                    let r = &g.rows[*i];
                    let c = r.ready()?;
                    (!c.pruned.is_empty()).then(|| format!("{}: {}", r.label, ops::named(&c.pruned)))
                })
                .collect()
        } else {
            Vec::new()
        };
        let danger = opts.prune && !opts.dry_run && !deletes.is_empty();
        let mut sources: Vec<String> = jobs.iter().map(|(i, _)| g.rows[*i].source.clone()).collect();
        sources.sort();
        sources.dedup();
        let guard = Guard::typed_text(&n.to_string(), format!("Type {n} to sync {what}:"), window, cx);
        let view = cx.entity().downgrade();
        let title = if opts.dry_run { format!("Dry run of a sync of {what}?") } else { format!("Sync {what}?") };
        let note = format!(
            "Argo CD syncs each to its target revision, {}. {}",
            opts.text(),
            if opts.hooks {
                "Its PreSync, Sync and PostSync hooks run."
            } else {
                "Its hooks are skipped (the apply strategy): only the manifests are applied."
            }
        );
        let ok = if opts.dry_run { "Dry run" } else { "Sync" };
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (guard, view, jobs) = (guard.clone(), view.clone(), jobs.clone());
            let theme = cx.theme();
            let (muted, warning) = (theme.muted_foreground, theme.warning);
            let prune_text = match (opts.prune, deletes.is_empty()) {
                (false, _) => None,
                (true, true) => Some("Prune: nothing to delete.".to_string()),
                (true, false) => Some(if opts.dry_run { "Prune would delete:" } else { "Prune deletes:" }.to_string()),
            };
            alert
                .title(title.clone())
                .description(note.clone())
                .child(
                    v_flex()
                        .gap_1()
                        .pt_2()
                        .text_sm()
                        .child(selectable(names.clone()))
                        .child(div().text_xs().text_color(muted).child(format!("On {}", sources.join(", "))))
                        .when(left > 0, |v| {
                            v.child(div().text_xs().text_color(muted).child(format!(
                                "{} left out (see why on the page).",
                                crate::plural::count(left, "Application")
                            )))
                        })
                        .children(prune_text.map(|t| div().text_xs().text_color(warning).child(t)))
                        .when(!deletes.is_empty(), |v| {
                            v.child(
                                v_flex()
                                    .id("argo-sync-deletes")
                                    .max_h(px(160.))
                                    .overflow_y_scroll()
                                    .text_xs()
                                    .text_color(warning)
                                    .children(deletes.iter().map(|d| div().child(selectable(d.clone())))),
                            )
                        }),
                )
                .child(confirm_extras("", Some(&guard), cx))
                .confirm_buttons(ok, danger, Some(&guard))
                .on_ok(move |_, window, cx| {
                    if !guard_ok(Some(&guard), cx) {
                        return false;
                    }
                    // Asked with write mode on, but it may have been turned off since.
                    if !crate::details::writes_still_on(window, cx) {
                        return true;
                    }
                    let jobs = jobs.clone();
                    let _ = view.update(cx, |this, cx| this.send_grouped(opts, jobs, cx));
                    true
                })
        });
    }

    /// Send the syncs confirmed (`jobs`: each row and its request), at most a few at a time,
    /// then follow each Application.
    fn send_grouped(&mut self, opts: Options, jobs: Vec<(usize, SyncRequest)>, cx: &mut Context<Self>) {
        let Some(g) = &mut self.sync else { return };
        if g.sent_with.is_some() {
            return;
        }
        g.sent_with = Some(opts);
        g.checks.cancel();
        let mut list = Vec::with_capacity(jobs.len());
        let mut index = Vec::with_capacity(jobs.len());
        for (i, req) in jobs {
            if let Some(r) = g.rows.get_mut(i) {
                r.in_sync = true;
                list.push((r.target.clone(), req));
                index.push(i);
            }
        }
        let sent = grouped_sync::send_all(list);
        g.sends = Some(cx.spawn(async move |this, cx| {
            let mut sent = std::pin::pin!(sent);
            while let Some((k, result)) = sent.next().await {
                let i = index[k];
                if this.update(cx, |this, cx| this.grouped_sent(i, result, cx)).is_err() {
                    return;
                }
            }
        }));
        self.follow_grouped(cx);
        cx.notify();
    }

    fn grouped_sent(&mut self, i: usize, result: Result<String, String>, cx: &mut Context<Self>) {
        if let Some(r) = self.sync.as_mut().and_then(|g| g.rows.get_mut(i)) {
            if let Err(e) = &result {
                r.progress = Some(Err(e.clone()));
            }
            r.sent = Some(result);
        }
        cx.notify();
    }

    /// Read the Applications synced every `READ_EVERY` until each operation ends (or for
    /// `FOLLOW_FOR`), a few at a time; each read goes to the sync journal.
    fn follow_grouped(&mut self, cx: &mut Context<Self>) {
        let Some(g) = &mut self.sync else { return };
        g.follow = Some(cx.spawn(async move |this, cx| {
            let start = Instant::now();
            let exec = cx.background_executor().clone();
            loop {
                exec.timer(READ_EVERY).await;
                let Ok(Some((todo, sending))) =
                    this.update(cx, |this, _| this.sync.as_ref().map(GroupedSync::to_follow))
                else {
                    return;
                };
                if todo.is_empty() && !sending {
                    return;
                }
                if start.elapsed() > FOLLOW_FOR {
                    let _ = this.update(cx, |this, cx| this.update_sync(cx, |g| g.gave_up = true));
                    return;
                }
                let reads: Vec<(usize, Result<Value, String>)> =
                    grouped_sync::bounded(todo.len(), PARALLEL, |k| todo[k].1.read()).collect().await;
                let read = this.update(cx, |this, cx| {
                    let Some(g) = &mut this.sync else { return };
                    for (k, read) in reads {
                        let (i, _, before) = &todo[k];
                        let Some(r) = g.rows.get_mut(*i) else { continue };
                        match read {
                            Ok(app) => {
                                r.progress = Some(Ok(grouped_sync::progress(&app, before.as_deref())));
                                r.note = None;
                            }
                            Err(e) => r.note = Some(format!("could not be read: {e}")),
                        }
                    }
                    cx.notify();
                });
                if read.is_err() {
                    return;
                }
            }
        }));
    }

    /// Confirm, then terminate the operation of the Application at `i`, the way it was read.
    fn terminate_grouped(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(r) = self.sync.as_ref().and_then(|g| g.rows.get(i)) else { return };
        let (target, label, source) = (r.target.clone(), r.label.clone(), r.source.clone());
        if !crate::details::ensure_writes(window, cx) {
            return;
        }
        let guard = Guard::protected(&source, window, cx);
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (target, guard) = (target.clone(), guard.clone());
            alert
                .title(format!("Terminate the sync of {label}?"))
                .description(TERMINATE_NOTE)
                .child(confirm_extras("", guard.as_ref(), cx))
                .confirm_buttons("Terminate", true, guard.as_ref())
                .on_ok(move |_, window, cx| {
                    if !guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    if !crate::details::writes_still_on(window, cx) {
                        return true;
                    }
                    let send = target.send(Op::Terminate);
                    window
                        .spawn(cx, async move |cx| {
                            let result = send.await;
                            let _ = cx.update(|window, cx| match result {
                                Ok(msg) => window.push_notification(Notification::success(msg), cx),
                                Err(e) => {
                                    window.push_notification(Notification::error(format!("Terminate failed: {e}")), cx)
                                }
                            });
                        })
                        .detach();
                    true
                })
        });
    }

    // MARK: Rendering

    /// The sync page: the options, each Application with what its check found (and, once
    /// sent, how its operation goes), and the summary once all ended.
    pub(super) fn sync_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(g) = &self.sync else { return div().into_any_element() };
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let sent = g.sent_with.is_some();
        let opts = g.sent_with.unwrap_or(g.opts);
        let checking = g.checking();
        let ready = g.rows.iter().filter(|r| r.ready().is_some()).count();
        let n = g.rows.len();
        let title = if sent {
            format!(
                "{} of {}",
                if opts.dry_run { "Dry run" } else { "Sync" },
                crate::plural::count(g.rows.iter().filter(|r| r.in_sync).count(), "Application")
            )
        } else {
            format!("Sync {}", crate::plural::count(n, "Application"))
        };
        let header = h_flex()
            .gap_2()
            .child(
                Button::new("argo-sync-back")
                    .small()
                    .ghost()
                    .icon(Lucide::ArrowLeft)
                    .label("Applications")
                    .on_click(cx.listener(|this, _, _, cx| this.close_sync(cx))),
            )
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(div().flex_1())
            .when(checking, |h| {
                let read = g.rows.iter().filter(|r| r.checked.is_some()).count();
                h.child(div().text_xs().text_color(muted).child(format!("Read {read} of {n}…")))
            })
            .when(!sent, |h| {
                h.child(
                    Button::new("argo-sync-check")
                        .small()
                        .outline()
                        .icon(Lucide::RefreshCw)
                        .label("Read again")
                        .loading(checking)
                        .disabled(checking)
                        .on_click(cx.listener(|this, _, _, cx| this.check_grouped(cx))),
                )
                .child(
                    Button::new("argo-sync-go")
                        .small()
                        .primary()
                        .icon(Lucide::GitBranch)
                        .label(if opts.dry_run { "Dry run…" } else { "Sync…" })
                        .disabled(checking || ready == 0)
                        .on_click(cx.listener(|this, _, window, cx| this.confirm_grouped_sync(window, cx))),
                )
            })
            .when(sent, |h| {
                h.child(
                    Button::new("argo-sync-journal")
                        .small()
                        .outline()
                        .icon(Lucide::History)
                        .label("Journal")
                        .on_click(cx.listener(|this, _, window, cx| this.open_journal(None, window, cx))),
                )
            });
        let view = cx.entity().downgrade();
        let options = (!sent).then(|| {
            let set = |f: fn(&mut Options, bool)| {
                let view = view.clone();
                move |v: &bool, _: &mut Window, cx: &mut App| {
                    let v = *v;
                    let _ = view.update(cx, |this, cx| this.update_sync(cx, |g| f(&mut g.opts, v)));
                }
            };
            let hooks = |id: &'static str, label: &'static str, on: bool| {
                Button::new(id)
                    .small()
                    .when(opts.hooks == on, ButtonVariants::primary)
                    .when(opts.hooks != on, Button::outline)
                    .label(label)
                    .on_click(cx.listener(move |this, _, _, cx| this.update_sync(cx, |g| g.opts.hooks = on)))
            };
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_2()
                        .child(hooks("argo-sync-hooks", "With hooks", true))
                        .child(hooks("argo-sync-no-hooks", "Without hooks", false))
                        .child(div().text_xs().text_color(muted).child(if opts.hooks {
                            "Argo CD's own Sync: PreSync, Sync and PostSync hooks run (a migration, a secret refresh…)."
                        } else {
                            "The apply strategy: only the manifests are applied, hooks are skipped. For a simple change."
                        })),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Checkbox::new("argo-sync-prune")
                                .label("Prune")
                                .checked(opts.prune)
                                .on_click(set(|o, v| o.prune = v)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(if opts.prune { theme.warning } else { muted })
                                .child("Deletes what is no longer in git: listed under each Application."),
                        ),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Checkbox::new("argo-sync-dry-run")
                                .label("Dry run")
                                .checked(opts.dry_run)
                                .on_click(set(|o, v| o.dry_run = v)),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child("Argo CD applies nothing and says what it would do."),
                        ),
                )
        });
        let diff = match self.sync_diff_summary() {
            Some(s) => div().text_sm().font_weight(FontWeight::MEDIUM).child(selectable(s)),
            None => div().child(
                link("argo-sync-diff", "Read what a sync of each would change", cx)
                    .on_click(cx.listener(|this, _, _, cx| this.sync_diff(cx))),
            ),
        };
        let tallied: Vec<Result<Progress, String>> =
            g.rows.iter().filter(|r| r.in_sync).filter_map(|r| r.progress.clone()).collect();
        let (ok, failed, _) = grouped_sync::tally(tallied.iter());
        let in_sync = g.rows.iter().filter(|r| r.in_sync).count();
        let done = sent && g.rows.iter().filter(|r| r.in_sync).all(Row::ended);
        let summary = sent.then(|| {
            let left = n - in_sync;
            let mut s = if done {
                format!("Done: {ok} succeeded, {failed} failed")
            } else {
                format!("{ok} succeeded, {failed} failed, {} running", in_sync - ok - failed)
            };
            if left > 0 {
                s.push_str(&format!(", {left} left out"));
            }
            let tone = if failed > 0 {
                Tone::Bad
            } else if done {
                Tone::Good
            } else {
                Tone::Busy
            };
            div().text_sm().font_weight(FontWeight::MEDIUM).text_color(tone.color(cx)).child(s)
        });
        let body = v_flex()
            .gap_2()
            .when(!sent, |v| {
                v.child(div().text_xs().text_color(muted).child(
                    "Each Application is read first: one already syncing, being deleted, or held by a sync window now \
                     is left out, saying why. The syncs are sent four at a time, write mode checked as each is sent.",
                ))
            })
            .when(g.hidden > 0, |v| {
                v.child(div().text_xs().text_color(theme.warning).child(format!(
                    "{} more picked are hidden by the filter: not synced.",
                    crate::plural::count(g.hidden, "Application")
                )))
            })
            .when(g.left_out > 0, |v| {
                v.child(
                    div()
                        .text_xs()
                        .text_color(theme.warning)
                        .child(format!("{} more picked are left out: at most {MAX_APPS} at a time.", g.left_out)),
                )
            })
            .children(options)
            .when(!sent, |v| v.child(diff))
            .children(summary)
            .when(g.gave_up, |v| {
                v.child(div().text_xs().text_color(theme.warning).child(format!(
                    "No longer followed after {} min: the journal keeps what Argo CD says of them.",
                    FOLLOW_FOR.as_secs() / 60
                )))
            })
            .children(g.rows.iter().enumerate().map(|(i, r)| self.sync_row(g, i, r, opts, cx)));
        v_flex()
            .size_full()
            .gap_2()
            .child(header)
            .child(div().id("argo-sync-body").flex_1().min_h_0().overflow_y_scroll().p_1().child(body))
            .into_any_element()
    }

    /// One Application: before the sync, what its check found; after, how it goes.
    fn sync_row(&self, g: &GroupedSync, i: usize, r: &Row, opts: Options, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let sent = g.sent_with.is_some();
        let line = |text: String, color: Hsla| div().text_xs().text_color(color).child(selectable(text));
        let mut out = v_flex().gap_0p5().py_1().border_b_1().border_color(theme.border).child(
            h_flex()
                .gap_2()
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(selectable(r.label.clone())))
                .child(div().text_xs().text_color(muted).child(r.source.clone())),
        );
        let checked = match &r.checked {
            None => return out.child(line("Reading…".into(), muted)),
            Some(Err(e)) => return out.child(line(format!("Could not be read: {e}"), theme.danger)),
            Some(Ok(c)) => c,
        };
        if let Some(why) = &checked.refused {
            return out.child(line(format!("Left out: {why}"), theme.warning));
        }
        if !sent || !r.in_sync {
            let mut facts = vec!["Ready".to_string()];
            if checked.auto_sync {
                facts.push("auto-sync on".into());
            }
            out = out.child(line(facts.join(" · "), muted));
            if opts.prune {
                let text = if checked.pruned.is_empty() {
                    "Nothing to prune.".to_string()
                } else {
                    format!("Prune deletes {}", ops::named(&checked.pruned))
                };
                out = out.child(line(text, if checked.pruned.is_empty() { muted } else { theme.warning }));
            }
            return out;
        }
        let progress = match (&r.sent, &r.progress) {
            (None, _) => return out.child(line("Sending…".into(), muted)),
            (_, Some(Err(e))) => return out.child(line(format!("Failed: {e}"), theme.danger)),
            (Some(Ok(said)), None) => return out.child(line(said.clone(), muted)),
            (_, Some(Ok(p))) => p,
            (Some(Err(e)), None) => return out.child(line(format!("Failed: {e}"), theme.danger)),
        };
        let tone = if progress.phase == "Waiting" { Tone::Busy } else { operation_tone(&progress.phase) };
        let steps = (progress.total > 0).then(|| format!("{} of {} done", progress.done, progress.total));
        out = out.child(
            h_flex()
                .gap_2()
                .child(chip(&progress.phase, tone, cx))
                .children(steps.map(|s| div().text_xs().text_color(muted).child(s)))
                .child(div().flex_1())
                .when(!progress.ended && progress.phase != "Waiting", |h| {
                    h.child(
                        Button::new(("argo-sync-terminate", i))
                            .xsmall()
                            .outline()
                            .icon(Lucide::CircleStop)
                            .label("Terminate")
                            .on_click(cx.listener(move |this, _, window, cx| this.terminate_grouped(i, window, cx))),
                    )
                }),
        );
        if !progress.ended && progress.total > 0 {
            let share = progress.done as f32 / progress.total as f32;
            out = out.child(
                div()
                    .h(px(4.))
                    .w_full()
                    .rounded_full()
                    .bg(theme.muted)
                    .child(div().h_full().w(relative(share)).rounded_full().bg(theme.warning)),
            );
        }
        if let Some(m) = &progress.message {
            let color = if progress.ended && !progress.succeeded { theme.danger } else { muted };
            out = out.child(line(m.clone(), color));
        }
        if let Some(note) = &r.note {
            out = out.child(line(note.clone(), muted));
        }
        out
    }
}
