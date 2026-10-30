//! The Reachability tab of a Service, an Ingress or an HTTPRoute: the path a request takes,
//! step by step, with the first step that breaks on top.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{DetailPanel, OpenObject};
use crate::reachability::{self as reach, Item, Probe, State, Step, Target};

/// The tab's last check, and its last test from this machine.
#[derive(Default)]
pub(super) struct Reach {
    steps: Option<Vec<Step>>,
    checking: bool,
    check: crate::latest::Latest,
    probes: Option<Vec<Probe>>,
    probing: bool,
    probe: crate::latest::Latest,
}

impl Reach {
    /// Not checked yet, nor being checked.
    pub(super) fn unchecked(&self) -> bool {
        self.steps.is_none() && !self.checking
    }
}

impl DetailPanel {
    /// Check the path again, on the object as last read: when the tab is shown, and on its button.
    pub(super) fn check_reachability(&mut self, cx: &mut Context<Self>) {
        let (Some(target), Some(obj)) = (Target::of(&self.obj.kind), self.object.clone()) else { return };
        // In this page's own cluster, with the user's credentials.
        let reader = reach::Reader::new(self.client.clone(), None);
        let context = self.context.clone();
        let work = async move {
            let world = reach::gather(reader, target, obj.clone()).await;
            reach::check(&context, target, &obj, &world, jiff::Timestamp::now())
        };
        self.reach.checking = true;
        self.reach.check.spawn(cx, work, |this, steps, cx| {
            this.reach.steps = Some(steps);
            this.reach.checking = false;
            cx.notify();
        });
        cx.notify();
    }

    /// Resolve each host of the Ingress and send it a request, from this machine.
    fn probe_hosts(&mut self, cx: &mut Context<Self>) {
        let Some(obj) = self.object.clone() else { return };
        self.reach.probing = true;
        self.reach.probe.spawn(cx, reach::probe_all(obj), |this, probes, cx| {
            this.reach.probes = Some(probes);
            this.reach.probing = false;
            cx.notify();
        });
        cx.notify();
    }

    pub(super) fn render_reachability(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let ingress = Target::of(&self.obj.kind) == Some(Target::Ingress);
        let toolbar = h_flex()
            .gap_2()
            .child(
                Button::new("reach-check")
                    .xsmall()
                    .outline()
                    .icon(Lucide::RefreshCw)
                    .label("Check again")
                    .loading(self.reach.checking)
                    .on_click(cx.listener(|this, _, _, cx| this.check_reachability(cx))),
            )
            .when(ingress, |d| {
                d.child(
                    Button::new("reach-probe")
                        .xsmall()
                        .outline()
                        .icon(Lucide::Network)
                        .label("Test from this machine")
                        .loading(self.reach.probing)
                        .tooltip("Resolve each host and send it an HTTP(S) request from this computer")
                        .on_click(cx.listener(|this, _, _, cx| this.probe_hosts(cx))),
                )
            });
        let Some(steps) = &self.reach.steps else {
            return v_flex()
                .gap_2()
                .child(toolbar)
                .child(div().text_sm().text_color(muted).child("Checking…"))
                .into_any_element();
        };
        let (state, verdict) = reach::verdict(steps);
        let color = state_color(state, cx);
        let mut body = v_flex().id("reachability").size_full().overflow_y_scroll().px_2().gap_1().child(toolbar).child(
            div().mt_2().p_2().rounded_md().border_1().border_color(color).text_color(color).text_sm().child(verdict),
        );
        let mut section = "";
        for (i, step) in steps.iter().enumerate() {
            if step.section != section {
                section = step.section.as_str();
                body = body.child(heading(section));
            }
            body = body.child(step_row(i, step, cx));
        }
        if let Some(probes) = &self.reach.probes {
            body = body.child(heading("From this machine"));
            if probes.is_empty() {
                body = body.child(div().text_sm().text_color(muted).child("No host to try: its rules name none."));
            }
            for p in probes {
                body = body.child(probe_row(p, cx));
            }
        }
        body.into_any_element()
    }
}

fn state_color(state: State, cx: &App) -> Hsla {
    let theme = cx.theme();
    match state {
        State::Ok => theme.success,
        State::Warn => theme.warning,
        State::Broken => theme.danger,
        State::Unknown => theme.muted_foreground,
    }
}

fn heading(text: &str) -> Div {
    div().pt_4().pb_1().text_base().font_weight(FontWeight::SEMIBOLD).child(text.to_string())
}

/// A colored dot, a title, and what was found.
fn line(state: State, title: Option<String>, detail: String, cx: &App) -> Div {
    let color = state_color(state, cx);
    let muted = cx.theme().muted_foreground;
    h_flex()
        .gap_2()
        .items_start()
        .text_sm()
        .child(div().mt_1p5().size(px(7.)).flex_shrink_0().rounded_full().bg(color))
        .when_some(title, |d, t| d.child(div().flex_shrink_0().font_weight(FontWeight::MEDIUM).child(t)))
        .child(div().flex_1().min_w_0().text_color(if state == State::Ok { muted } else { color }).child(detail))
}

fn step_row(i: usize, step: &Step, cx: &mut Context<DetailPanel>) -> Div {
    let (border, muted, mono) = (cx.theme().border, cx.theme().muted_foreground, cx.theme().mono_font_family.clone());
    let items: Vec<AnyElement> =
        step.items.iter().enumerate().map(|(j, item)| item_row(i * 1000 + j, item, cx)).collect();
    v_flex()
        .py_1()
        .gap_0p5()
        .border_b_1()
        .border_color(border)
        .child(line(step.state, Some(step.title.clone()), step.detail.clone(), cx))
        .children(items)
        .when_some(step.command.clone(), |d, cmd| {
            d.child(
                h_flex()
                    .pl_4()
                    .gap_1()
                    .items_start()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .font_family(mono)
                            .text_xs()
                            .text_color(muted)
                            .child(format!("$ {cmd}")),
                    )
                    .child(
                        Button::new(("reach-copy", i))
                            .xsmall()
                            .ghost()
                            .icon(Lucide::Copy)
                            .tooltip("Copy the command")
                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(cmd.clone()))),
                    ),
            )
        })
}

/// A line under a step; the object it names opens on a click.
fn item_row(id: usize, item: &Item, cx: &mut Context<DetailPanel>) -> AnyElement {
    let row = div().pl_4().text_xs();
    let Some(link) = item.link.clone() else {
        return row.text_color(cx.theme().muted_foreground).child(item.text.clone()).into_any_element();
    };
    row.id(("reach-item", id))
        .text_color(cx.theme().blue)
        .cursor_pointer()
        .hover(Styled::underline)
        .on_click(cx.listener(move |_, _, _, cx| {
            cx.emit(OpenObject {
                kind: link.kind.to_string(),
                name: link.name.clone(),
                namespace: link.namespace.clone(),
                context: None,
            });
        }))
        .child(item.text.clone())
        .into_any_element()
}

/// What a request to one host from this machine met: its name, then its answer.
fn probe_row(p: &Probe, cx: &App) -> Div {
    let (dns_state, dns) = p.dns_text();
    let (answer_state, answer) = p.answer_text();
    v_flex()
        .py_1()
        .gap_0p5()
        .border_b_1()
        .border_color(cx.theme().border)
        .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(p.url.clone()))
        .child(line(dns_state, None, dns, cx))
        .child(line(answer_state, None, answer, cx))
}
