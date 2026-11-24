//! The sync journal of the Argo CD page: every operation seen, kept on this machine (see
//! `k8s::argo::journal`), newest first, filtered by Application, phase, who started it and
//! since when; one unfolded shows its steps, even after Argo CD's own history dropped it.
//!
//! It is opened from the selection bar, the sync page, and a dashboard's Last sync and History
//! cards (filtered to that Application). The file is read off the UI thread when it opens and
//! on Read again; a line that doesn't read is skipped and counted.

use std::collections::HashSet;
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
use crate::k8s::argo::journal::{self, Filter, Loaded, OpId, Operation, Step};
use crate::k8s::argo::{operation_tone, short_revision};
use crate::status::Tone;
use crate::ui::selectable::selectable;

/// Operations listed, at most.
const SHOWN: usize = 300;
/// The time spans offered to filter by.
const SINCE: [(&str, i64); 4] =
    [("Last hour", 3600), ("Last day", 86_400), ("Last 7 days", 7 * 86_400), ("Last 30 days", 30 * 86_400)];

/// The journal open on the page.
pub(super) struct JournalPage {
    /// The journal as last read; None while the first read runs.
    loaded: Option<Result<Arc<Loaded>, String>>,
    reading: bool,
    filter: Filter,
    /// The operations unfolded.
    open: HashSet<OpId>,
    load: crate::latest::Latest,
}

/// A step's tone: a hook still running, or one not started, is on its way.
fn step_tone(s: &Step) -> Tone {
    match s.result() {
        "Running" | "Pending" | "" => Tone::Busy,
        r => crate::status::tone(r).unwrap_or(Tone::Done),
    }
}

/// What a row says of an operation, after its name and phase.
fn facts(op: &Operation, now: jiff::Timestamp) -> Vec<String> {
    let r = &op.last;
    let mut out = Vec::new();
    if let Some(t) = r.started_at() {
        out.push(format!("started {} ago", k8s::duration_text(now.as_second() - t.as_second())));
        let end = r.finished_at().map_or(now.as_second(), |f| f.as_second());
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
    out.push(r.instance.clone());
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
            open: HashSet::new(),
            load: Default::default(),
        });
        self.read_journal(cx);
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
        let open = j.open.contains(&id);
        let tone = operation_tone(&r.phase);
        let toggle = cx.listener(move |this, _, _, cx| {
            let id = id.clone();
            this.update_journal(cx, move |j| {
                if !j.open.remove(&id) {
                    j.open.insert(id);
                }
            });
        });
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
                    .child(div().text_xs().text_color(muted).child(if open { "▾" } else { "▸" }))
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
            .when(open, |v| {
                v.child(
                    v_flex()
                        .pl_6()
                        .gap_0p5()
                        .child(div().text_xs().text_color(muted).child(selectable(format!(
                            "{}/{} · started {}{}",
                            r.namespace,
                            r.name,
                            r.started,
                            r.finished.as_ref().map_or_default(|f| format!(" · finished {f}"))
                        ))))
                        .when(!r.message.is_empty(), |v| v.child(div().text_xs().child(selectable(r.message.clone()))))
                        .when(r.resources.is_empty(), |v| {
                            v.child(div().text_xs().text_color(muted).child("No step recorded."))
                        })
                        .children(r.resources.iter().map(|s| {
                            let result = if s.result().is_empty() { "Waiting" } else { s.result() };
                            v_flex()
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .text_xs()
                                        .child(chip(result, step_tone(s), cx))
                                        .child(selectable(s.label()))
                                        .when(!s.hook_type.is_empty(), |h| {
                                            h.child(div().text_color(muted).child(format!("{} hook", s.hook_type)))
                                        }),
                                )
                                .when(!s.message.is_empty(), |v| {
                                    v.child(
                                        div().pl_2().text_xs().text_color(muted).child(selectable(s.message.clone())),
                                    )
                                })
                        })),
                )
            })
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
