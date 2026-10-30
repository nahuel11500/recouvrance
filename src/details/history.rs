//! History tab: what changed on the object, newest first, a diff on demand (see
//! `crate::history`). Nothing is read until the tab is first shown.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::DetailPanel;
use super::compare::diff_block;
use crate::history::{self, Change, DiffLine, Fetched};
use crate::k8s;

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
        let own = self.object.as_ref().map(|o| history::own_changes(&self.obj.kind, o)).unwrap_or_default();
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
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let Some(fetched) = &self.history.fetched else {
            return div().text_sm().text_color(muted).child("Loading…").into_any_element();
        };
        let mut errors = fetched.errors.clone();
        if let Some(Err(e)) = &self.events {
            errors.push(format!("events: {e}"));
        }
        let header = h_flex()
            .gap_2()
            .child(div().flex_1().text_xs().text_color(muted).child(
                "From what the cluster keeps: revisions, Helm and Argo CD history, who last wrote which \
                 fields (managedFields), and events (kept about an hour); and what the app recorded while it \
                 watched the object (Recorded, see the Timeline page).",
            ))
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
            );
        let rows = self.history.merged.iter().enumerate().map(|(i, c)| {
            let key = c.key();
            let open = self.history.open.get(&key).cloned();
            let label = if open.is_some() { "Hide diff" } else { "Diff" };
            v_flex()
                .gap_1()
                .py_1()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .gap_3()
                        .items_start()
                        .text_sm()
                        .child(
                            div()
                                .w(px(56.))
                                .flex_none()
                                .text_color(muted)
                                .child(c.time.map(k8s::age).unwrap_or_default()),
                        )
                        .child(div().w(px(64.)).flex_none().text_xs().text_color(theme.blue).child(c.source.label()))
                        .child(
                            div()
                                .w(px(260.))
                                .flex_none()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(c.object.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .when(c.warning, |d| d.text_color(theme.warning))
                                .child(c.summary.clone()),
                        )
                        .when(c.diff.is_some(), |d| {
                            d.child(
                                Button::new(("history-diff", i)).xsmall().ghost().label(label).on_click(
                                    cx.listener(move |this, _, _, cx| this.toggle_change_diff(key.clone(), cx)),
                                ),
                            )
                        }),
                )
                .when_some(open, |d, lines| {
                    d.child(if lines.is_empty() {
                        div().text_xs().text_color(muted).child("No difference.").into_any_element()
                    } else {
                        diff_block(&lines, cx)
                    })
                })
        });
        v_flex()
            .id("history")
            .size_full()
            .overflow_y_scroll()
            .gap_2()
            .child(header)
            .children(
                errors
                    .into_iter()
                    .map(|e| div().text_xs().text_color(theme.danger).child(format!("Could not read {e}"))),
            )
            .when(self.history.merged.is_empty(), |d| {
                d.child(div().text_sm().text_color(muted).child("No changes found."))
            })
            .child(v_flex().children(rows))
            .into_any_element()
    }
}
