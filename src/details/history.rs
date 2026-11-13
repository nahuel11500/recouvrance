//! History tab: what changed on the object, newest first, a diff on demand (see
//! `crate::history`). Nothing is read until the tab is first shown.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::DetailPanel;
use super::compare::diff_block;
use crate::history::{self, Change, DiffLine, Fetched, Source};
use crate::k8s;
use crate::ui::selectable::selectable;

#[derive(Default)]
pub(super) struct HistoryState {
    /// What was read from the cluster besides the object; None until the tab is first shown.
    fetched: Option<Fetched>,
    /// A read is running (the first one, or a reload).
    loading: bool,
    load: crate::latest::Latest,
    /// What is shown: the changes read, the object's own and its events, newest first.
    merged: Vec<Change>,
    /// The diffs open, by change key: worked out on the first click, kept across reloads.
    open: HashMap<String, Arc<Vec<DiffLine>>>,
    /// The one source shown, picked in the toolbar; None shows them all.
    only: Option<Source>,
}

impl DetailPanel {
    /// Read the history (again), from the object as last read.
    fn load_history(&mut self, cx: &mut Context<Self>) {
        let Some(obj) = self.object.clone() else { return };
        self.history.loading = true;
        let (client, kind, ns) = (self.client.clone(), self.obj.kind.clone(), self.obj.namespace.clone());
        let work = history::fetch(client, kind, ns, obj, history::all_allowed());
        // A reload replaces the read running.
        self.history.load.spawn(cx, work, |this, fetched, cx| {
            this.history.loading = false;
            this.history.fetched = Some(fetched);
            this.merge_history();
            cx.notify();
        });
    }

    /// Forget the history read: the object was re-created under its name.
    pub(super) fn reset_history(&mut self) {
        self.history = HistoryState::default();
    }

    /// Put the list together again from what is already read: after a read of the history,
    /// and when the object, its events or its Argo CD section change. Nothing once the tab
    /// was never shown.
    pub(super) fn merge_history(&mut self) {
        let Some(fetched) = &self.history.fetched else { return };
        let subject = format!("{} {}", self.obj.kind.ar.kind, self.obj.name);
        let own = self.object.as_ref().map_or_default(|o| history::own_changes(&self.obj.kind, o));
        let events = match &self.events {
            Some(Ok(events)) => history::event_changes(events, &subject),
            _ => Vec::new(),
        };
        // A namespace's Argo CD section lists the Applications deploying into it, not what
        // deployed the namespace.
        let argo =
            if self.obj.kind.is("", "Namespace") { Vec::new() } else { history::argo_lookup_changes(&self.argo) };
        // What the app recorded while it watched this object (this one: by its uid).
        let uid = self.object.as_ref().and_then(|o| o.pointer("/metadata/uid")).and_then(|u| u.as_str());
        let recorded = crate::timeline::changes_for(
            &self.context,
            &self.obj.kind.kubectl_name(),
            self.obj.namespace.as_deref(),
            &self.obj.name,
            uid.unwrap_or_default(),
        );
        self.history.merged = history::merge([fetched.changes.clone(), own, argo, events, recorded]);
    }

    fn toggle_change_diff(&mut self, key: String, cx: &mut Context<Self>) {
        if self.history.open.remove(&key).is_none()
            && let Some(diff) = self.history.merged.iter().find(|c| c.key() == key).and_then(|c| c.diff.as_ref())
        {
            let lines = Arc::new(diff.lines());
            self.history.open.insert(key, lines);
        }
        cx.notify();
    }

    pub(super) fn render_history(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if self.history.fetched.is_none() && !self.history.loading {
            self.load_history(cx);
        }
        let muted = cx.theme().muted_foreground;
        let Some(fetched) = &self.history.fetched else {
            return div().text_sm().text_color(muted).child("Loading…").into_any_element();
        };
        let mut errors = fetched.errors.clone();
        if let Some(Err(e)) = &self.events {
            errors.push(format!("events: {e}"));
        }
        // Picking a source none of the changes have would show nothing.
        let only = self.history.only.filter(|s| self.history.merged.iter().any(|c| c.source == *s));
        let subject = format!("{} {}", self.obj.kind.ar.kind, self.obj.name);
        let today = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::system()).date();
        let mut list = v_flex().id("history-list").flex_1().min_h_0().overflow_y_scroll().px_2();
        let mut last_day = None;
        for (i, c) in self.history.merged.iter().enumerate().filter(|(_, c)| only.is_none_or(|s| c.source == s)) {
            let day = c.time.map(|t| t.to_zoned(jiff::tz::TimeZone::system()).date());
            if last_day != Some(day) {
                last_day = Some(day);
                list = list.child(
                    div()
                        .pt_3()
                        .pb_1()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(muted)
                        .child(day.map_or_else(|| "Time unknown".to_string(), |d| day_label(d, today))),
                );
            }
            let key = c.key();
            let open = self.history.open.get(&key).cloned();
            let toggle = c.diff.is_some().then(|| {
                Button::new(("history-diff", i))
                    .xsmall()
                    .ghost()
                    .icon(if open.is_some() { Lucide::ChevronDown } else { Lucide::ChevronRight })
                    .label("Diff")
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_change_diff(key.clone(), cx)))
            });
            list = list.child(change_row(i, c, &subject, toggle, open, cx));
        }
        v_flex()
            .size_full()
            .gap_1()
            .child(self.history_toolbar(only, cx))
            .children(errors.into_iter().map(|e| {
                div().px_2().text_xs().text_color(cx.theme().danger).child(selectable(format!("Could not read {e}")))
            }))
            .child(if self.history.merged.is_empty() {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .child(div().text_lg().child(selectable("No changes found")))
                    .child(div().text_sm().text_color(muted).child(selectable(ABOUT)))
                    .into_any_element()
            } else {
                list.into_any_element()
            })
            .into_any_element()
    }

    /// One pill a source, with its count: a click shows that source alone. Then what the
    /// list is read from, and Reload.
    fn history_toolbar(&self, only: Option<Source>, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let total = self.history.merged.len();
        let counts: Vec<(Source, usize)> = SOURCES
            .iter()
            .map(|s| (*s, self.history.merged.iter().filter(|c| c.source == *s).count()))
            .filter(|(_, n)| *n > 0)
            .collect();
        h_flex()
            .gap_1p5()
            .px_2()
            .flex_wrap()
            .text_sm()
            .child(
                div()
                    .text_color(muted)
                    .mr_1()
                    .child(format!("{total} {}", if total == 1 { "change" } else { "changes" })),
            )
            .children(counts.into_iter().map(|(source, n)| {
                let (icon, color) = source_look(source, cx);
                let on = only == Some(source);
                h_flex()
                    .id(SharedString::from(format!("history-only-{}", source.label())))
                    .gap_1()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .cursor_pointer()
                    .text_xs()
                    .bg(color.opacity(if on { 0.25 } else { 0.1 }))
                    .border_1()
                    .border_color(if on { color } else { color.opacity(0.) })
                    .hover(|s| s.bg(color.opacity(0.2)))
                    .child(Icon::new(icon).size_3().text_color(color))
                    .child(source.label())
                    .child(div().text_color(muted).child(n.to_string()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.history.only = if this.history.only == Some(source) { None } else { Some(source) };
                        cx.notify();
                    }))
            }))
            .child(div().flex_1())
            .child(
                div()
                    .id("history-about")
                    .child(Icon::new(Lucide::Info).size_4().text_color(muted))
                    .tooltip(|w, cx| component::tooltip::Tooltip::new(ABOUT).build(w, cx)),
            )
            .child(
                Button::new("history-reload")
                    .xsmall()
                    .ghost()
                    .icon(Lucide::RefreshCw)
                    .label(if self.history.loading { "Reloading…" } else { "Reload" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.load_history(cx);
                        this.load_events(cx);
                        cx.notify();
                    })),
            )
    }
}

/// What the list is read from.
const ABOUT: &str = "From what the cluster keeps: revisions, Helm and Argo CD history, which manager last wrote \
                     which fields, and events (kept about an hour). Recorded: what the app saw while it watched \
                     the object (see the Timeline page).";

/// The sources, in the toolbar's order.
const SOURCES: [Source; 6] =
    [Source::Rollout, Source::Helm, Source::ArgoCd, Source::Fields, Source::Event, Source::Recorded];

/// A source's icon and color.
fn source_look(source: Source, cx: &App) -> (Lucide, Hsla) {
    let theme = cx.theme();
    match source {
        Source::Rollout => (Lucide::Layers, theme.blue),
        Source::Helm => (Lucide::Ship, theme.cyan),
        Source::ArgoCd => (Lucide::GitBranch, theme.magenta),
        Source::Fields => (Lucide::PencilLine, theme.muted_foreground),
        Source::Event => (Lucide::Bell, theme.green),
        Source::Recorded => (Lucide::Eye, theme.yellow),
    }
}

/// "Today", "Yesterday", "Tuesday 3 November", with the year when it isn't this one.
fn day_label(day: jiff::civil::Date, today: jiff::civil::Date) -> String {
    if day == today {
        "Today".into()
    } else if today.yesterday().ok() == Some(day) {
        "Yesterday".into()
    } else if day.year() == today.year() {
        day.strftime("%A %-d %B").to_string()
    } else {
        day.strftime("%A %-d %B %Y").to_string()
    }
}

/// A change's title and what goes under it. On the object itself the object's name says
/// nothing: `manager applied: fields` reads as a title `manager applied` over the fields.
fn title_and_body<'a>(c: &'a Change, subject: &str) -> (&'a str, &'a str) {
    if c.object != subject {
        return (&c.object, &c.summary);
    }
    c.summary.split_once(": ").unwrap_or((&c.summary, ""))
}

/// The lines under a title: one each container change of a revision (`; `), one each image
/// of the oldest revision kept.
fn body_lines(source: Source, body: &str) -> Vec<&str> {
    if source != Source::Rollout {
        return vec![body];
    }
    if let Some(images) = body.strip_prefix("oldest revision kept: ") {
        return images.split(", ").collect();
    }
    body.split("; ").collect()
}

/// `name: rest` with the name in bold, when it is one word (a revision's container); else as is.
fn body_line(line: &str, named: bool, cx: &App) -> AnyElement {
    match line.split_once([':', '=']).filter(|_| named) {
        Some((name, rest)) if !name.is_empty() && !name.contains(' ') => h_flex()
            .flex_wrap()
            .gap_x_1p5()
            .child(div().font_weight(FontWeight::MEDIUM).child(selectable(name.to_string())))
            .child(div().min_w_0().text_color(cx.theme().muted_foreground).child(selectable(rest.trim().to_string())))
            .into_any_element(),
        _ => div().text_color(cx.theme().muted_foreground).child(selectable(line.to_string())).into_any_element(),
    }
}

/// The fields a manager wrote, one chip each; `(+3 more)` stays as text.
fn field_chips(body: &str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let (fields, more) = match body.rfind(" (+") {
        Some(i) if body.ends_with(" more)") => (&body[..i], Some(&body[i + 1..])),
        _ => (body, None),
    };
    h_flex()
        .flex_wrap()
        .gap_1()
        .children(fields.split(", ").filter(|f| !f.is_empty()).map(|f| {
            div()
                .px_1p5()
                .rounded_sm()
                .text_xs()
                .font_family(theme.mono_font_family.clone())
                .bg(theme.muted)
                .child(selectable(f.to_string()))
        }))
        .children(more.map(|m| div().text_xs().text_color(theme.muted_foreground).child(m.to_string())))
        .into_any_element()
}

/// One change on the timeline: the source's mark on the rail, a title with when, what
/// changed under it, and its diff when open.
fn change_row(
    i: usize,
    c: &Change,
    subject: &str,
    toggle: Option<Button>,
    open: Option<Arc<Vec<DiffLine>>>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let (icon, color) = source_look(c.source, cx);
    let color = if c.warning { theme.warning } else { color };
    let (title, body) = title_and_body(c, subject);
    let when = c.time.map_or_default(|t| format!("{} ago", k8s::age(t)));
    let exact = c.time.map(|t| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%Y-%m-%d %H:%M:%S").to_string());
    let body = match c.source {
        _ if body.is_empty() => None,
        Source::Fields => Some(field_chips(body, cx)),
        source => Some(
            v_flex()
                .gap_0p5()
                .children(body_lines(source, body).into_iter().map(|l| body_line(l, source == Source::Rollout, cx)))
                .into_any_element(),
        ),
    };
    h_flex()
        .w_full()
        .items_stretch()
        .gap_3()
        .child(
            v_flex()
                .w(px(24.))
                .flex_none()
                .items_center()
                .child(
                    div()
                        .mt_1()
                        .size(px(24.))
                        .flex_none()
                        .rounded_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .bg(color.opacity(0.15))
                        .child(
                            Icon::new(if c.warning { Lucide::TriangleAlert } else { icon })
                                .size_3p5()
                                .text_color(color),
                        ),
                )
                .child(div().w(px(1.)).flex_1().bg(theme.border)),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_1()
                .pt_1()
                .pb_3()
                .text_sm()
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_none().text_xs().text_color(color).child(c.source.label()))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_weight(FontWeight::SEMIBOLD)
                                .when(c.warning, |d| d.text_color(theme.warning))
                                .child(selectable(title.to_string())),
                        )
                        .children(toggle)
                        .child(
                            div()
                                .id(("history-when", i))
                                .flex_none()
                                .text_xs()
                                .text_color(theme.muted_foreground)
                                .child(when)
                                .when_some(exact, |d, exact| {
                                    d.tooltip(move |w, cx| component::tooltip::Tooltip::new(exact.clone()).build(w, cx))
                                }),
                        ),
                )
                .children(body)
                .when_some(open, |d, lines| {
                    d.child(if lines.is_empty() {
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(selectable("No difference."))
                            .into_any_element()
                    } else {
                        diff_block(&lines, cx)
                    })
                }),
        )
}

#[cfg(test)]
mod tests {
    use super::{Change, Source, body_lines, day_label, title_and_body};

    #[test]
    fn days_are_named() {
        let today = jiff::civil::date(2026, 11, 6);
        assert_eq!(day_label(today, today), "Today");
        assert_eq!(day_label(jiff::civil::date(2026, 11, 5), today), "Yesterday");
        assert_eq!(day_label(jiff::civil::date(2026, 11, 3), today), "Tuesday 3 November");
        assert_eq!(day_label(jiff::civil::date(2025, 12, 31), today), "Wednesday 31 December 2025");
    }

    fn change(source: Source, object: &str, summary: &str) -> Change {
        Change { time: None, source, object: object.into(), summary: summary.into(), warning: false, diff: None }
    }

    /// On the object itself, what comes before `: ` is the title; another object is named.
    #[test]
    fn titles() {
        let fields = change(Source::Fields, "Pod p", "kubelet updated: spec, metadata.labels");
        assert_eq!(title_and_body(&fields, "Pod p"), ("kubelet updated", "spec, metadata.labels"));
        let rev = change(Source::Rollout, "Revision 2 · ReplicaSet web-b", "app: nginx:1.25 → 1.26");
        assert_eq!(title_and_body(&rev, "Deployment web"), ("Revision 2 · ReplicaSet web-b", "app: nginx:1.25 → 1.26"));
        let recorded = change(Source::Recorded, "Pod p", "created by kubectl");
        assert_eq!(title_and_body(&recorded, "Pod p"), ("created by kubectl", ""));
    }

    #[test]
    fn a_revision_s_containers_go_on_lines() {
        assert_eq!(
            body_lines(Source::Rollout, "a: x:1 → 2; b: y:1 → 2; restarted"),
            ["a: x:1 → 2", "b: y:1 → 2", "restarted"]
        );
        assert_eq!(body_lines(Source::Rollout, "oldest revision kept: a=x:1, b=y:1"), ["a=x:1", "b=y:1"]);
        assert_eq!(body_lines(Source::Event, "a; b"), ["a; b"]);
    }
}
