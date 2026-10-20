//! Context switcher in the top bar: search, recent first, one click to switch,
//! checkboxes to view several clusters together.

use std::collections::BTreeSet;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

pub enum ContextEvent {
    /// Connect to these contexts (replacing the current ones).
    Connect(Vec<String>),
    /// Open the full context page.
    Manage,
}

pub struct ContextPicker {
    pub all: Vec<String>,
    pub connected: Vec<String>,
    picked: BTreeSet<String>,
    search: Entity<InputState>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ContextEvent> for ContextPicker {}

impl ContextPicker {
    pub fn new(all: Vec<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search contexts"));
        let subs = vec![cx.subscribe(&search, |_, _, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                cx.notify();
            }
        })];
        Self { all, connected: Vec::new(), picked: BTreeSet::new(), search, _subs: subs }
    }

    pub fn set_connected(&mut self, connected: Vec<String>, cx: &mut Context<Self>) {
        self.picked = connected.iter().cloned().collect();
        self.connected = connected;
        cx.notify();
    }

    fn row(&self, i: usize, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let color: Hsla =
            crate::settings::context_color(name).map(|c| rgb(c).into()).unwrap_or(theme.muted_foreground.opacity(0.4));
        let protected = crate::settings::is_protected(name);
        let connected = self.connected.iter().any(|c| c == name);
        let (n1, n2) = (name.to_string(), name.to_string());
        h_flex()
            .id(SharedString::from(format!("ctx-row-{i}-{name}")))
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded_md()
            .cursor_pointer()
            .when(connected, |d| d.bg(theme.blue.opacity(0.08)))
            .hover(|s| s.bg(theme.list_hover))
            .child(
                Checkbox::new(SharedString::from(format!("ctx-cb-{i}-{name}")))
                    .checked(self.picked.contains(name))
                    .on_click(cx.listener(move |this, v: &bool, _, cx| {
                        if *v {
                            this.picked.insert(n1.clone());
                        } else {
                            this.picked.remove(&n1);
                        }
                        cx.notify();
                    })),
            )
            .child(div().size(px(10.)).flex_shrink_0().rounded_full().bg(color))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(name.to_string()),
            )
            .when(protected, |d| d.child(Icon::new(Lucide::Lock).size_3p5().text_color(theme.warning)))
            .when(connected, |d| d.child(div().text_xs().text_color(theme.blue).child("connected")))
            // A click on the row switches to just this context.
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(ContextEvent::Connect(vec![n2.clone()]))))
            .into_any_element()
    }
}

impl Render for ContextPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.search.read(cx).value().to_lowercase();
        let muted = cx.theme().muted_foreground;
        let recent: Vec<String> =
            crate::settings::get().recent_contexts.into_iter().filter(|c| self.all.contains(c)).take(5).collect();
        let matching: Vec<String> = self.all.iter().filter(|c| c.to_lowercase().contains(&query)).cloned().collect();
        let picked: Vec<String> = self.picked.iter().cloned().collect();
        let changed = picked != {
            let mut c = self.connected.clone();
            c.sort();
            c
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        if query.is_empty() && !recent.is_empty() {
            rows.push(div().px_2().pt_1().text_xs().text_color(muted).child("Recent").into_any_element());
            for (i, c) in recent.iter().enumerate() {
                rows.push(self.row(i, c, cx));
            }
            rows.push(div().px_2().pt_2().text_xs().text_color(muted).child("All contexts").into_any_element());
        }
        for (i, c) in matching.iter().enumerate() {
            rows.push(self.row(100 + i, c, cx));
        }
        v_flex()
            .w(px(380.))
            .gap_2()
            .child(Input::new(&self.search).cleanable(true))
            .child(v_flex().id("ctx-list").max_h(px(420.)).overflow_y_scroll().children(rows))
            .child(
                h_flex()
                    .gap_2()
                    .pt_1()
                    .child(
                        div().text_xs().text_color(muted).child("Click to switch · tick several to view them together"),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("ctx-manage")
                            .small()
                            .ghost()
                            .label("Manage…")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ContextEvent::Manage))),
                    )
                    .when(changed && !picked.is_empty(), |d| {
                        d.child(
                            Button::new("ctx-connect")
                                .small()
                                .primary()
                                .label(format!("View {}", picked.len()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let list: Vec<String> = this.picked.iter().cloned().collect();
                                    cx.emit(ContextEvent::Connect(list));
                                })),
                        )
                    }),
            )
    }
}
