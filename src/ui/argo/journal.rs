//! The sync journal of the Argo CD page: every operation seen, kept on this machine (see
//! `k8s::argo::journal`), newest first, filtered by Application, phase, who started it and
//! since when; one unfolded shows its steps, even after Argo CD's own history dropped it.
//!
//! It is opened from the selection bar, the sync page, and a dashboard's Last sync and History
//! cards (filtered to that Application). The file is read off the UI thread when it opens and
//! on Read again; a line that doesn't read is skipped and counted.
//!
//! An operation's steps lead with its cause (`steps_view`, also the dashboard's Last sync →
//! steps): the steps that didn't succeed, by phase, then the succeeded ones folded by kind.
//! They are summed up once when the operation unfolds or the journal is read, not per frame.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::ArgoView;
use super::dashboard::chip;
use crate::k8s;
use crate::k8s::argo::diagnosis::span_text;
use crate::k8s::argo::journal::{self, Filter, Loaded, OpId, Operation, Seen, Step};
use crate::k8s::argo::steps::{self, State, Summary};
use crate::k8s::argo::{operation_tone, short_revision};
use crate::ui::selectable::selectable;

/// Operations listed, at most.
const SHOWN: usize = 300;
/// The time spans offered to filter by.
const SINCE: [(&str, i64); 4] =
    [("Last hour", 3600), ("Last day", 86_400), ("Last 7 days", 7 * 86_400), ("Last 30 days", 30 * 86_400)];

/// Steps listed in one group (a phase, a kind), at most: past that, the filter narrows them.
const STEPS_SHOWN: usize = 300;

/// The journal open on the page.
pub(super) struct JournalPage {
    /// The journal as last read; None while the first read runs.
    loaded: Option<Result<Arc<Loaded>, String>>,
    reading: bool,
    filter: Filter,
    /// The operations unfolded, and their steps summed up (of the journal as last read).
    open: HashMap<OpId, Arc<Summary>>,
    /// The folded steps unfolded of each operation (`StepsOf::is_open`).
    folds: HashSet<(OpId, String)>,
    load: crate::latest::Latest,
}

impl JournalPage {
    /// The operation `id` of the journal as last read.
    fn find(&self, id: &OpId) -> Option<&Operation> {
        let loaded = self.loaded.as_ref()?.as_ref().ok()?;
        loaded.ops.iter().find(|o| o.last.id() == *id)
    }

    /// Sum up the steps of the operations unfolded again, from the journal as last read.
    fn summarise_open(&mut self) {
        let ids: Vec<OpId> = self.open.keys().cloned().collect();
        for id in ids {
            let sum = self.find(&id).map(|o| Arc::new(Summary::of(&o.last.resources))).unwrap_or_default();
            self.open.insert(id, sum);
        }
    }

    /// Unfold the operation `id` (its steps summed up), or fold it.
    fn toggle(&mut self, id: OpId) {
        if self.open.remove(&id).is_none() {
            let sum = self.find(&id).map(|o| Arc::new(Summary::of(&o.last.resources))).unwrap_or_default();
            self.open.insert(id, sum);
        }
    }
}

/// How `steps_view` shows an operation's steps.
pub(super) struct StepsOf<'a> {
    /// Its elements' ids start with this.
    pub id: String,
    pub steps: &'a [Step],
    pub summary: &'a Summary,
    /// When the journal saw each step (empty when not known: the dashboard).
    pub seen: &'a [Seen],
    pub running: bool,
    /// When it ended, in seconds.
    pub ended: Option<i64>,
    /// The step filter's words, lowercase.
    pub query: &'a str,
    /// Whether a fold is unfolded: `succeeded`, `not started` (`Fold::what`), or one of its
    /// kinds (`succeeded:ConfigMap`).
    pub is_open: &'a dyn Fn(&str) -> bool,
}

/// Fold or unfold one of `StepsOf::is_open`'s keys.
pub(super) type StepsToggle = Rc<dyn Fn(&mut ArgoView, String, &mut Context<ArgoView>)>;

/// `t` in local time: the time alone today, else with the day.
fn local_time(t: jiff::Timestamp, now: jiff::Timestamp) -> String {
    let tz = jiff::tz::TimeZone::system();
    let (at, today) = (t.to_zoned_datetime(tz.clone()), now.to_zoned_datetime(tz));
    at.strftime(if at.date() == today.date() { "%H:%M:%S" } else { "%b %-d %H:%M" }).to_string()
}

/// What a row says of an operation, after its name and phase.
fn facts(op: &Operation, now: jiff::Timestamp) -> Vec<String> {
    let r = &op.last;
    let mut out = Vec::new();
    if let Some(t) = r.started_at() {
        out.push(format!("started {} ago", k8s::duration_text(now.as_second() - t.as_second())));
        let end = r.finished_at().map_or(now.as_second(), jiff::Timestamp::as_second);
        let verb = if r.running() { "running for" } else { "took" };
        if r.finished_at().is_some() || r.running() {
            out.push(format!("{verb} {}", span_text(end - t.as_second())));
        }
    }
    if let Some(by) = &r.by {
        out.push(format!("by {by}"));
    }
    if !r.revisions.is_empty() {
        out.push(r.revisions.iter().map(|v| short_revision(v)).collect::<Vec<_>>().join(", "));
    }
    if r.dry_run {
        out.push("dry run".into());
    }
    if r.strategy.as_deref() == Some("apply") {
        out.push("without hooks".into());
    }
    if !r.resources.is_empty() {
        out.push(crate::plural::count(r.resources.len(), "step"));
    }
    // A hub's context says nothing an Argo CD API server's name would (#1054).
    if !r.instance.starts_with("context:") {
        out.push(r.instance.clone());
    }
    out
}

impl ArgoView {
    /// Open the journal, filtered to the Application `app` when given, and read it.
    pub(super) fn open_journal(&mut self, app: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let text = app.unwrap_or_default();
        self.journal_search.update(cx, |s, cx| s.set_value(text.clone(), window, cx));
        self.journal = Some(JournalPage {
            loaded: None,
            reading: false,
            filter: Filter { app: text.to_lowercase(), ..Default::default() },
            open: HashMap::new(),
            folds: HashSet::new(),
            load: Default::default(),
        });
        self.read_journal(cx);
        cx.notify();
    }

    /// The journal on a made-up example (the developer tools' snapshots), its first two
    /// operations unfolded: nothing is read.
    #[cfg(feature = "dev")]
    pub fn demo_journal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.open_journal(None, window, cx);
        let Some(j) = &mut self.journal else { return };
        j.load.cancel();
        j.reading = false;
        let loaded = journal::example::loaded(jiff::Timestamp::now().as_second());
        let ids: Vec<OpId> = loaded.ops.iter().take(2).map(|o| o.last.id()).collect();
        j.loaded = Some(Ok(Arc::new(loaded)));
        for id in ids {
            j.toggle(id);
        }
        cx.notify();
    }

    /// Read the journal file off the UI thread.
    fn read_journal(&mut self, cx: &mut Context<Self>) {
        let Some(j) = &mut self.journal else { return };
        j.reading = true;
        let exec = cx.background_executor().clone();
        let work = async move { exec.spawn(async { journal::load() }).await };
        j.load.spawn(cx, work, |this, read, cx| {
            if let Some(j) = &mut this.journal {
                j.loaded = Some(read.map(Arc::new));
                j.reading = false;
                j.summarise_open();
            }
            cx.notify();
        });
        cx.notify();
    }

    fn close_journal(&mut self, cx: &mut Context<Self>) {
        self.journal = None;
        cx.notify();
    }

    /// The Application words typed changed.
    pub(super) fn journal_typed(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Some(j) = &mut self.journal {
            j.filter.app = text.to_lowercase();
        }
        cx.notify();
    }

    fn update_journal(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut JournalPage)) {
        if let Some(j) = &mut self.journal {
            f(j);
        }
        cx.notify();
    }

    /// The journal: its filters, then the operations they keep, newest first, each unfolding
    /// to its message and steps.
    pub(super) fn journal_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(j) = &self.journal else { return div().into_any_element() };
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let now = jiff::Timestamp::now();
        let loaded = j.loaded.as_ref().and_then(|l| l.as_ref().ok()).cloned().unwrap_or_default();
        let kept: Vec<&Operation> = loaded.ops.iter().filter(|o| j.filter.keeps(o, now.as_second())).collect();
        let (phases, who) = journal::choices(&loaded.ops);
        let view = cx.entity().downgrade();
        let count = if j.filter == Filter::default() {
            crate::plural::count(loaded.ops.len(), "operation")
        } else {
            format!("{} of {}", kept.len(), crate::plural::count(loaded.ops.len(), "operation"))
        };
        let since = j.filter.since.and_then(|s| SINCE.iter().find(|(_, secs)| *secs == s)).map(|(l, _)| l.to_string());
        let header = h_flex()
            .gap_2()
            .child(
                Button::new("argo-journal-back")
                    .small()
                    .ghost()
                    .icon(Lucide::ArrowLeft)
                    .label("Back")
                    .on_click(cx.listener(|this, _, _, cx| this.close_journal(cx))),
            )
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Sync journal"))
            .child(div().text_sm().text_color(muted).child(count))
            .child(div().flex_1())
            .child(
                Button::new("argo-journal-again")
                    .small()
                    .outline()
                    .icon(Lucide::RefreshCw)
                    .label("Read again")
                    .loading(j.reading)
                    .disabled(j.reading)
                    .on_click(cx.listener(|this, _, _, cx| this.read_journal(cx))),
            );
        let filters = h_flex()
            .gap_2()
            .flex_wrap()
            .child(div().w(px(260.)).child(Input::new(&self.journal_search).small().cleanable(true)))
            .child(div().w(px(260.)).child(Input::new(&self.steps_search).small().cleanable(true)))
            .child(journal_picker(
                "argo-journal-phase",
                "Any phase",
                j.filter.phase.clone(),
                phases,
                view.clone(),
                |f, v| {
                    f.phase = v;
                },
            ))
            .child(journal_picker("argo-journal-who", "Anyone", j.filter.who.clone(), who, view.clone(), |f, v| {
                f.who = v;
            }))
            .child(journal_picker(
                "argo-journal-since",
                "Any time",
                since,
                SINCE.iter().map(|(l, _)| (*l).to_string()).collect(),
                view,
                |f, v| f.since = v.and_then(|v| SINCE.iter().find(|(l, _)| *l == v).map(|(_, s)| *s)),
            ));
        let error = j.loaded.as_ref().and_then(|l| l.as_ref().err()).cloned();
        let more = kept.len().saturating_sub(SHOWN);
        let body = v_flex()
            .gap_1()
            .child(div().text_xs().text_color(muted).child(format!(
                "Every Argo CD operation the app has seen, kept on this machine for {} days (50 MB at most): its steps \
                 stay here after Argo CD's own history has dropped them.",
                journal::KEEP_DAYS
            )))
            .children(error.map(|e| div().text_sm().text_color(theme.danger).child(selectable(e))))
            .when(loaded.bad > 0, |v| {
                v.child(div().text_xs().text_color(theme.warning).child(format!(
                    "{} of the journal could not be read and {} skipped.",
                    crate::plural::count(loaded.bad, "line"),
                    crate::plural::pick(loaded.bad, "was", "were")
                )))
            })
            .when(j.loaded.is_none(), |v| v.child(div().text_sm().text_color(muted).child("Reading the journal…")))
            .when(j.loaded.is_some() && kept.is_empty(), |v| {
                v.child(div().text_sm().text_color(muted).child(if loaded.ops.is_empty() {
                    "No operation seen yet."
                } else {
                    "No operation matches the filters."
                }))
            })
            .children(kept.iter().take(SHOWN).enumerate().map(|(i, op)| self.journal_row(j, i, op, now, cx)))
            .when(more > 0, |v| {
                v.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("… {} more: narrow the filters", crate::plural::count(more, "operation"))),
                )
            });
        v_flex()
            .size_full()
            .gap_2()
            .child(header)
            .child(filters)
            .child(div().id("argo-journal-body").flex_1().min_h_0().overflow_y_scroll().p_1().child(body))
            .into_any_element()
    }

    /// One operation: a line to unfold, then its message and each step.
    fn journal_row(
        &self,
        j: &JournalPage,
        i: usize,
        op: &Operation,
        now: jiff::Timestamp,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let r = &op.last;
        let id = r.id();
        let summary = j.open.get(&id).cloned();
        let tone = operation_tone(&r.phase);
        let toggle = {
            let id = id.clone();
            cx.listener(move |this, _, _, cx| {
                let id = id.clone();
                this.update_journal(cx, move |j| j.toggle(id));
            })
        };
        let started = r.started_at().map(|t| format!("started {}", local_time(t, now)));
        let finished = r.finished_at().map(|t| format!("finished {}", local_time(t, now)));
        let when = [Some(format!("{}/{}", r.namespace, r.name)), started, finished].into_iter().flatten();
        v_flex()
            .gap_0p5()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .id(("argo-journal-op", i))
                    .gap_2()
                    .cursor_pointer()
                    .on_click(toggle)
                    .child(div().text_xs().text_color(muted).child(if summary.is_some() { "▾" } else { "▸" }))
                    .child(div().size(px(7.)).flex_none().rounded_full().bg(tone.color(cx)))
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(r.name.clone()))
                    .child(div().text_sm().text_color(tone.color(cx)).child(r.phase.clone()))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_xs()
                            .text_color(muted)
                            .child(facts(op, now).join(" · ")),
                    ),
            )
            .when_some(summary, |v, summary| {
                let is_open = |key: &str| j.folds.contains(&(id.clone(), key.to_string()));
                let toggle_id = id.clone();
                let steps_toggle: StepsToggle = Rc::new(move |this, key, cx| {
                    let k = (toggle_id.clone(), key);
                    this.update_journal(cx, move |j| {
                        if !j.folds.remove(&k) {
                            j.folds.insert(k);
                        }
                    });
                });
                let of = StepsOf {
                    id: format!("argo-journal-steps-{i}"),
                    steps: &r.resources,
                    summary: &summary,
                    seen: &op.seen,
                    running: r.running(),
                    ended: r.finished_at().map(jiff::Timestamp::as_second),
                    query: &self.steps_query,
                    is_open: &is_open,
                };
                v.child(
                    v_flex()
                        .pl_6()
                        .gap_0p5()
                        .child(
                            div().text_xs().text_color(muted).child(selectable(when.collect::<Vec<_>>().join(" · "))),
                        )
                        .when(!r.message.is_empty(), |v| {
                            v.child(div().text_sm().text_color(tone.color(cx)).child(selectable(r.message.clone())))
                        })
                        .when(r.resources.is_empty(), |v| {
                            v.child(div().text_xs().text_color(muted).child("No step recorded."))
                        })
                        .when(!r.resources.is_empty(), |v| v.child(self.steps_view(&of, steps_toggle, now, cx))),
                )
            })
    }

    /// An operation's steps, the cause first: where it is or stopped, the steps that didn't
    /// succeed by phase, then one line folding the succeeded ones by kind. With a filter, the
    /// steps it keeps, by phase.
    pub(super) fn steps_view(
        &self,
        o: &StepsOf,
        toggle: StepsToggle,
        now: jiff::Timestamp,
        cx: &mut Context<Self>,
    ) -> Div {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let sum = o.summary;
        let heading = |phase: &str| {
            div().pt_1().text_xs().font_weight(FontWeight::MEDIUM).text_color(muted).child(phase.to_string())
        };
        let more = |n: usize| {
            (n > 0).then(|| {
                div()
                    .pl_2()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("… {} more: filter the steps", crate::plural::count(n, "step")))
            })
        };
        if !o.query.trim().is_empty() {
            let groups = sum.filter(o.steps, o.query);
            let kept: usize = groups.iter().map(|g| g.steps.len()).sum();
            return v_flex()
                .gap_0p5()
                .child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("{kept} of {} match the filter", crate::plural::count(sum.total, "step"))),
                )
                .children(groups.into_iter().map(|g| {
                    v_flex()
                        .when(sum.phased, |v| v.child(heading(&g.phase)))
                        .children(g.steps.iter().take(STEPS_SHOWN).map(|&i| self.step_line(o, i, false, now, cx)))
                        .children(more(g.steps.len().saturating_sub(STEPS_SHOWN)))
                }));
        }
        let progress = steps::progress(sum, o.running);
        let folds = sum.folds.iter().map(|f| {
            let open = (o.is_open)(f.what);
            let t = toggle.clone();
            let what = f.what.to_string();
            let line = div()
                .id(SharedString::from(format!("{}-{}", o.id, f.what)))
                .pt_1()
                .text_xs()
                .text_color(muted)
                .cursor_pointer()
                .hover(Styled::underline)
                .on_click(cx.listener(move |this, _, _, cx| t(this, what.clone(), cx)))
                .child(format!("{} {}", if open { "▾" } else { "▸" }, f.line()));
            let kinds = open.then(|| {
                v_flex().pl_3().children(f.kinds.iter().map(|k| {
                    let key = format!("{}:{}", f.what, k.kind);
                    let open = (o.is_open)(&key);
                    let t = toggle.clone();
                    v_flex()
                        .child(
                            div()
                                .id(SharedString::from(format!("{}-{key}", o.id)))
                                .text_xs()
                                .text_color(muted)
                                .cursor_pointer()
                                .hover(Styled::underline)
                                .on_click(cx.listener(move |this, _, _, cx| t(this, key.clone(), cx)))
                                .child(format!(
                                    "{} {}",
                                    if open { "▾" } else { "▸" },
                                    steps::kind_count(k.steps.len(), &k.kind)
                                )),
                        )
                        .when(open, |v| {
                            v.children(k.steps.iter().take(STEPS_SHOWN).map(|&i| self.step_line(o, i, false, now, cx)))
                                .children(more(k.steps.len().saturating_sub(STEPS_SHOWN)))
                        })
                }))
            });
            v_flex().child(line).children(kinds)
        });
        let folds: Vec<Div> = folds.collect();
        let single = sum.cause.len() == 1 && !sum.phased;
        v_flex()
            .gap_0p5()
            .children(progress.map(|p| div().text_xs().text_color(theme.warning).child(p)))
            .children(sum.cause.iter().map(|g| {
                v_flex()
                    .gap_0p5()
                    .when(!single, |v| v.child(heading(&g.phase)))
                    .children(g.steps.iter().take(STEPS_SHOWN).map(|&i| self.step_line(o, i, true, now, cx)))
                    .children(more(g.steps.len().saturating_sub(STEPS_SHOWN)))
            }))
            .children(folds)
    }

    /// One step on one line: its result, what it is, its hook, how long it ran and what its
    /// message says beyond that it was applied. A step of the cause has its message whole
    /// under it.
    fn step_line(&self, o: &StepsOf, i: usize, cause: bool, now: jiff::Timestamp, cx: &mut Context<Self>) -> Div {
        let muted = cx.theme().muted_foreground;
        let s = &o.steps[i];
        let state = State::of(s);
        let how_long = steps::how_long(s, o.seen.get(i).copied(), o.running, o.ended, now.as_second());
        let message = steps::telling(s).map(str::to_string);
        let tags: Vec<String> = [
            (!s.hook_type.is_empty()).then(|| format!("{} hook", s.hook_type)),
            (!cause && o.summary.phased && s.phase() != "Sync").then(|| s.phase().to_string()),
            how_long,
        ]
        .into_iter()
        .flatten()
        .collect();
        let whole = cause && state != State::Succeeded;
        v_flex()
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .child(chip(steps::result_text(s), steps::tone(s), cx))
                    .child(div().min_w_0().overflow_hidden().child(selectable(s.label())))
                    .when(!tags.is_empty(), |h| h.child(div().flex_none().text_color(muted).child(tags.join(" · "))))
                    .when_some(message.clone().filter(|_| !whole), |h, m| {
                        h.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_color(muted)
                                .child(m),
                        )
                    }),
            )
            .when_some(message.filter(|_| whole), |v, m| v.child(div().pl_2().text_xs().child(selectable(m))))
    }
}

/// A button picking one value of a filter of the journal (or all of them) from a menu.
fn journal_picker(
    id: &'static str,
    all: &'static str,
    current: Option<String>,
    values: Vec<String>,
    view: WeakEntity<ArgoView>,
    set: fn(&mut Filter, Option<String>),
) -> impl IntoElement {
    Button::new(id).small().outline().label(current.clone().unwrap_or_else(|| all.to_string())).dropdown_menu(
        move |menu, _, _| {
            let pick = |value: Option<String>| {
                let view = view.clone();
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let value = value.clone();
                    let _ = view.update(cx, |this, cx| this.update_journal(cx, |j| set(&mut j.filter, value)));
                }
            };
            let menu = menu
                .scrollable(true)
                .max_h(px(420.))
                .item(PopupMenuItem::new(all).checked(current.is_none()).on_click(pick(None)));
            values.iter().fold(menu, |menu, v| {
                menu.item(
                    PopupMenuItem::new(v.clone()).checked(current.as_ref() == Some(v)).on_click(pick(Some(v.clone()))),
                )
            })
        },
    )
}
