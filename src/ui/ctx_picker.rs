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
            if matches!(ev, InputEvent::Change) {
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

    /// A new context list (kubeconfig reloaded): ticks on contexts that are gone are dropped.
    pub fn set_all(&mut self, all: Vec<String>, cx: &mut Context<Self>) {
        self.picked.retain(|c| all.contains(c));
        self.all = all;
        cx.notify();
    }

    fn row(&self, i: usize, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let color: Hsla =
            crate::settings::context_color(name).map_or(theme.muted_foreground.opacity(0.4), |c| rgb(c).into());
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
                        cx.stop_propagation();
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

/// Up to this many contexts, the picker shows one list with no section headers.
const FLAT_LIST_MAX: usize = 12;
/// How many recent contexts the "Recent" section shows on a longer list.
const RECENT_MAX: usize = 5;

/// The picker's order with no search. With `FLAT_LIST_MAX` contexts or fewer: one list
/// (`None` second), recently used first in recent order, then the rest in their usual order.
/// With more: up to `RECENT_MAX` recent ones, then the others (`Some`). No context appears
/// twice, and a recent context that is no longer in `all` is left out.
fn order(all: &[String], recent: &[String]) -> (Vec<String>, Option<Vec<String>>) {
    let short = all.len() <= FLAT_LIST_MAX;
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let first: Vec<String> = recent
        .iter()
        .filter(|c| all.contains(*c) && seen.insert(c.as_str()))
        .take(if short { usize::MAX } else { RECENT_MAX })
        .cloned()
        .collect();
    let mut shown: BTreeSet<&str> = first.iter().map(String::as_str).collect();
    let rest: Vec<String> = all.iter().filter(|c| shown.insert(c.as_str())).cloned().collect();
    if short {
        let mut flat = first;
        flat.extend(rest);
        (flat, None)
    } else {
        (first, Some(rest))
    }
}

impl Render for ContextPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.search.read(cx).value().to_lowercase();
        let muted = cx.theme().muted_foreground;
        let matching: Vec<String> = self.all.iter().filter(|c| c.to_lowercase().contains(&query)).cloned().collect();
        let picked: Vec<String> = self.picked.iter().cloned().collect();
        let changed = picked != {
            let mut c = self.connected.clone();
            c.sort();
            c
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        if query.is_empty() {
            let (first, other) = order(&self.all, &crate::settings::get().recent_contexts);
            let sections = other.is_some() && !first.is_empty();
            if sections {
                rows.push(div().px_2().pt_1().text_xs().text_color(muted).child("Recent").into_any_element());
            }
            for (i, c) in first.iter().enumerate() {
                rows.push(self.row(i, c, cx));
            }
            if sections {
                rows.push(div().px_2().pt_2().text_xs().text_color(muted).child("Other contexts").into_any_element());
            }
            for (i, c) in other.unwrap_or_default().iter().enumerate() {
                rows.push(self.row(first.len() + i, c, cx));
            }
        } else {
            for (i, c) in matching.iter().enumerate() {
                rows.push(self.row(i, c, cx));
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn names(v: &[u8]) -> Vec<String> {
        v.iter().map(|n| format!("c{n}")).collect()
    }

    #[test]
    fn short_list_is_flat_recent_first() {
        let all = names(&[1, 2, 3, 4]);
        let (list, other) = order(&all, &names(&[3, 9, 1]));
        assert_eq!(list, names(&[3, 1, 2, 4]));
        assert!(other.is_none());
    }

    #[test]
    fn long_list_has_sections() {
        let all = names(&(0..20).collect::<Vec<_>>());
        let (recent, other) = order(&all, &names(&[7, 7, 3, 1, 2, 4, 5]));
        assert_eq!(recent, names(&[7, 3, 1, 2, 4]));
        let other = other.unwrap();
        assert_eq!(other.len(), 15);
        assert!(!other.iter().any(|c| recent.contains(c)));
    }

    proptest! {
        #[test]
        fn every_context_once(
            all in proptest::collection::btree_set(0u8..40, 0..30),
            recent in proptest::collection::vec(0u8..50, 0..12),
        ) {
            let all = names(&all.into_iter().collect::<Vec<_>>());
            let recent = names(&recent);
            let (first, other) = order(&all, &recent);
            let short = all.len() <= FLAT_LIST_MAX;
            prop_assert_eq!(other.is_none(), short);
            let mut shown = first.clone();
            shown.extend(other.unwrap_or_default());
            let mut sorted = shown.clone();
            sorted.sort();
            let mut expected = all.clone();
            expected.sort();
            prop_assert_eq!(sorted, expected);
            if short {
                let n = shown.iter().take_while(|c| recent.contains(c)).count();
                prop_assert!(shown[n..].iter().all(|c| !recent.contains(c)));
            } else {
                prop_assert!(first.len() <= RECENT_MAX);
                prop_assert!(first.iter().all(|c| recent.contains(c)));
            }
        }
    }
}
