//! Context switcher in the top bar: search, context sets, then favorites and recent first,
//! one click to switch, checkboxes to view several clusters together, and "Save as set" to
//! name the ones ticked.

use std::collections::BTreeSet;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::conn::{Conn, ConnStates};
use crate::settings::ContextSet;

pub enum ContextEvent {
    /// Connect to these contexts (replacing the current ones).
    Connect(Vec<String>),
    /// Open the full context page.
    Manage,
}

pub struct ContextPicker {
    pub all: Vec<String>,
    pub connected: Vec<String>,
    /// The contexts of the set that can't show rows yet: connecting, or why they failed.
    states: ConnStates,
    picked: BTreeSet<String>,
    search: Entity<InputState>,
    /// The name of the set being saved, shown while `naming`.
    set_name: Entity<InputState>,
    naming: bool,
    favorites: Entity<crate::favorites::Favorites>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ContextEvent> for ContextPicker {}

impl ContextPicker {
    pub fn new(
        all: Vec<String>,
        favorites: Entity<crate::favorites::Favorites>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search contexts"));
        let set_name = cx.new(|cx| InputState::new(window, cx).placeholder("Set name, e.g. prod"));
        let subs = vec![
            cx.subscribe(&search, |_, _, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe(&set_name, |this, _, ev: &InputEvent, cx| match ev {
                InputEvent::PressEnter { .. } => this.save_set(cx),
                // The button says Replace when the name is taken.
                InputEvent::Change => cx.notify(),
                _ => {}
            }),
            cx.observe(&favorites, |_, _, cx| cx.notify()),
        ];
        Self {
            all,
            connected: Vec::new(),
            states: ConnStates::default(),
            picked: BTreeSet::new(),
            search,
            set_name,
            naming: false,
            favorites,
            _subs: subs,
        }
    }

    pub fn set_connected(&mut self, connected: Vec<String>, cx: &mut Context<Self>) {
        self.picked = connected.iter().cloned().collect();
        self.connected = connected;
        cx.notify();
    }

    pub fn set_states(&mut self, states: ConnStates, cx: &mut Context<Self>) {
        self.states = states;
        cx.notify();
    }

    /// A new context list (kubeconfig reloaded): ticks on contexts that are gone are dropped.
    pub fn set_all(&mut self, all: Vec<String>, cx: &mut Context<Self>) {
        self.picked.retain(|c| all.contains(c));
        self.all = all;
        cx.notify();
    }

    /// Save the ticked contexts as a set under the name typed (in place of a set of that name).
    fn save_set(&mut self, cx: &mut Context<Self>) {
        let name = self.set_name.read(cx).value().to_string();
        let contexts = set_order(&self.picked, &self.connected);
        let Some(set) = ContextSet::new(&name, &contexts) else { return };
        crate::settings::save_context_set(set);
        self.naming = false;
        cx.notify();
    }

    /// A context set: a click connects the contexts of it the kubeconfig still has.
    fn set_row(&self, i: usize, set: &ContextSet, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let available = set.available(&self.all);
        let connected = set.is(&self.connected);
        let protected = available.iter().any(|c| crate::settings::is_protected(c));
        let said = if connected {
            "connected".to_string()
        } else if available.len() == set.contexts.len() {
            crate::plural::count(available.len(), "context")
        } else {
            // Some are no longer in the kubeconfig: only the others connect.
            format!("{} of {}", available.len(), crate::plural::count(set.contexts.len(), "context"))
        };
        let group = SharedString::from(format!("ctx-set-{i}"));
        let names = SharedString::from(set.contexts.join(", "));
        let name = set.name.clone();
        let delete = div()
            .id(SharedString::from(format!("ctx-set-del-{i}")))
            .flex_none()
            .cursor_pointer()
            .opacity(0.)
            .group_hover(group.clone(), |s| s.opacity(0.6))
            .tooltip(|w, cx| component::tooltip::Tooltip::new("Forget this set").build(w, cx))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.stop_propagation();
                crate::settings::delete_context_set(&name);
                cx.notify();
            }))
            .child(Icon::new(Lucide::X).size_3p5());
        h_flex()
            .id(group.clone())
            .group(group)
            .gap_2()
            .px_2()
            .py_1p5()
            .rounded_md()
            .when(connected, |d| d.bg(theme.blue.opacity(0.08)))
            .when(available.is_empty(), |d| d.opacity(0.5))
            .when(!available.is_empty(), |d| d.cursor_pointer().hover(|s| s.bg(theme.list_hover)))
            .child(Icon::new(Lucide::Layers).size_4().text_color(theme.muted_foreground))
            .child(
                div()
                    .id(SharedString::from(format!("ctx-set-name-{i}")))
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .tooltip(move |w, cx| component::tooltip::Tooltip::new(names.clone()).build(w, cx))
                    .child(set.name.clone()),
            )
            .when(protected, |d| d.child(Icon::new(Lucide::Lock).size_3p5().text_color(theme.warning)))
            .child(div().text_xs().text_color(if connected { theme.blue } else { theme.muted_foreground }).child(said))
            .child(delete)
            .when(!available.is_empty(), |d| {
                d.on_click(cx.listener(move |_, _, _, cx| cx.emit(ContextEvent::Connect(available.clone()))))
            })
            .into_any_element()
    }

    fn row(&self, i: usize, name: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let color: Hsla =
            crate::settings::context_color(name).map_or(theme.muted_foreground.opacity(0.4), |c| rgb(c).into());
        let protected = crate::settings::is_protected(name);
        let connected = self.connected.iter().any(|c| c == name);
        // In the set, but not (yet) connected: say so instead of "connected".
        let state = self.states.get(name).cloned();
        let (n1, n2) = (name.to_string(), name.to_string());
        let favorite = crate::favorites::Favorite::Context(name.to_string());
        let starred = self.favorites.read(cx).has(&favorite);
        let group = SharedString::from(format!("ctx-row-{i}-{name}"));
        // A shortened name shows in full on hover.
        let short = crate::k8s::short_names::short_name(name);
        let full = (short != name).then(|| SharedString::from(name.to_string()));
        let favorites = self.favorites.clone();
        let star = div()
            .id(SharedString::from(format!("ctx-star-{i}-{name}")))
            .flex_none()
            .cursor_pointer()
            .when(!starred, |d| d.opacity(0.).group_hover(group.clone(), |s| s.opacity(0.5)))
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                favorites.update(cx, |f, cx| f.toggle(favorite.clone(), cx));
            })
            .child(if starred {
                Icon::new(component::IconName::StarFill).size_3p5().text_color(yellow())
            } else {
                Icon::new(Lucide::Star).size_3p5()
            });
        h_flex()
            .id(group.clone())
            .group(group)
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
                    .id(SharedString::from(format!("ctx-name-{i}-{name}")))
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .when_some(full, |d, full| {
                        d.tooltip(move |w, cx| component::tooltip::Tooltip::new(full.clone()).build(w, cx))
                    })
                    .child(short),
            )
            .when(protected, |d| d.child(Icon::new(Lucide::Lock).size_3p5().text_color(theme.warning)))
            .when(connected, |d| match state {
                None => d.child(div().text_xs().text_color(theme.blue).child("connected")),
                Some(Conn::Connecting) => {
                    d.child(div().text_xs().text_color(theme.muted_foreground).child("connecting…"))
                }
                Some(Conn::Failed(why)) => d.child(
                    div()
                        .id(SharedString::from(format!("ctx-failed-{i}-{name}")))
                        .text_xs()
                        .text_color(theme.danger)
                        .child("can't connect")
                        .tooltip(move |w, cx| component::tooltip::Tooltip::new(why.clone()).build(w, cx)),
                ),
            })
            .child(star)
            // A click on the row switches to just this context.
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(ContextEvent::Connect(vec![n2.clone()]))))
            .into_any_element()
    }
}

/// The ticked contexts in the order a set connects them: the connected ones first, in the
/// order they were connected (the primary stays first), then the others.
fn set_order(picked: &BTreeSet<String>, connected: &[String]) -> Vec<String> {
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let first = connected.iter().filter(|c| picked.contains(*c));
    first.chain(picked.iter()).filter(|c| seen.insert(c.as_str())).cloned().collect()
}

/// Up to this many contexts, the picker shows one list with no section headers.
const FLAT_LIST_MAX: usize = 12;
/// How many recent contexts the "Recent" section shows on a longer list.
const RECENT_MAX: usize = 5;

/// The picker's order with no search: the favorites (in the order they were starred) in their
/// own section, then the others. Of those, with `FLAT_LIST_MAX` contexts or fewer: one list
/// (`None` last), recently used first in recent order, then the rest in their usual order.
/// With more: up to `RECENT_MAX` recent ones, then the others (`Some`). No context appears
/// twice, and a favorite or recent context that is no longer in `all` is left out.
fn order(all: &[String], favorites: &[String], recent: &[String]) -> (Vec<String>, Vec<String>, Option<Vec<String>>) {
    let short = all.len() <= FLAT_LIST_MAX;
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let starred: Vec<String> =
        favorites.iter().filter(|c| all.contains(*c) && seen.insert(c.as_str())).cloned().collect();
    let first: Vec<String> = recent
        .iter()
        .filter(|c| all.contains(*c) && seen.insert(c.as_str()))
        .take(if short { usize::MAX } else { RECENT_MAX })
        .cloned()
        .collect();
    let mut shown: BTreeSet<&str> = starred.iter().chain(&first).map(String::as_str).collect();
    let rest: Vec<String> = all.iter().filter(|c| shown.insert(c.as_str())).cloned().collect();
    if short {
        let mut flat = first;
        flat.extend(rest);
        (starred, flat, None)
    } else {
        (starred, first, Some(rest))
    }
}

impl Render for ContextPicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.search.read(cx).value().to_lowercase();
        let muted = cx.theme().muted_foreground;
        // The search finds a context by its full name or by the shorter one it shows.
        let matching: Vec<String> = self
            .all
            .iter()
            .filter(|c| {
                c.to_lowercase().contains(&query)
                    || crate::k8s::short_names::short_name(c).to_lowercase().contains(&query)
            })
            .cloned()
            .collect();
        let picked: Vec<String> = self.picked.iter().cloned().collect();
        let changed = picked != {
            let mut c = self.connected.clone();
            c.sort();
            c
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        // Sets first: one click connects a whole group. The search finds them by name.
        let sets: Vec<ContextSet> = crate::settings::context_sets()
            .into_iter()
            .filter(|s| query.is_empty() || s.name.to_lowercase().contains(&query))
            .collect();
        if !sets.is_empty() {
            rows.push(div().px_2().pt_1().text_xs().text_color(muted).child("Context sets").into_any_element());
            for (i, set) in sets.iter().enumerate() {
                rows.push(self.set_row(i, set, cx));
            }
            if !query.is_empty() && !matching.is_empty() {
                rows.push(div().px_2().pt_2().text_xs().text_color(muted).child("Contexts").into_any_element());
            }
        }
        if query.is_empty() {
            let favorites = self.favorites.read(cx).contexts().to_vec();
            let (starred, first, other) = order(&self.all, &favorites, &crate::settings::get().recent_contexts);
            let sections = other.is_some() && !first.is_empty();
            if !starred.is_empty() {
                rows.push(div().px_2().pt_1().text_xs().text_color(muted).child("Favorites").into_any_element());
            } else if !sets.is_empty() && !sections {
                // Under the sets, the contexts need a header of their own ("Recent" when shown).
                rows.push(div().px_2().pt_2().text_xs().text_color(muted).child("Contexts").into_any_element());
            }
            for (i, c) in starred.iter().enumerate() {
                rows.push(self.row(i, c, cx));
            }
            let at = starred.len();
            if sections {
                rows.push(div().px_2().pt_1().text_xs().text_color(muted).child("Recent").into_any_element());
            } else if !starred.is_empty() {
                rows.push(div().px_2().pt_2().text_xs().text_color(muted).child("Other contexts").into_any_element());
            }
            for (i, c) in first.iter().enumerate() {
                rows.push(self.row(at + i, c, cx));
            }
            if sections {
                rows.push(div().px_2().pt_2().text_xs().text_color(muted).child("Other contexts").into_any_element());
            }
            for (i, c) in other.unwrap_or_default().iter().enumerate() {
                rows.push(self.row(at + first.len() + i, c, cx));
            }
        } else {
            for (i, c) in matching.iter().enumerate() {
                rows.push(self.row(i, c, cx));
            }
        }
        let typed = self.set_name.read(cx).value().trim().to_string();
        let taken = crate::settings::with(|s| s.context_sets.iter().any(|x| x.name == typed));
        let ticked = crate::plural::count(picked.len(), "context");
        v_flex()
            .w(px(380.))
            .gap_2()
            .child(Input::new(&self.search).cleanable(true))
            .child(v_flex().id("ctx-list").max_h(px(420.)).overflow_y_scroll().children(rows))
            .when(self.naming && !picked.is_empty(), |d| {
                d.child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&self.set_name).small()))
                        .child(Button::new("ctx-set-cancel").small().ghost().label("Cancel").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.naming = false;
                                cx.notify();
                            },
                        )))
                        .child(
                            Button::new("ctx-set-save")
                                .small()
                                .primary()
                                .label(if taken { "Replace" } else { "Save" })
                                .disabled(typed.is_empty())
                                .tooltip(format!("Save the {ticked} ticked as a set"))
                                .on_click(cx.listener(|this, _, _, cx| this.save_set(cx))),
                        ),
                )
            })
            .child(
                h_flex()
                    .gap_2()
                    .pt_1()
                    .child(
                        div().text_xs().text_color(muted).child("Click to switch · tick several to view them together"),
                    )
                    .child(div().flex_1())
                    .when(!self.naming && !picked.is_empty(), |d| {
                        d.child(
                            Button::new("ctx-set-new")
                                .small()
                                .ghost()
                                .label("Save as set…")
                                .tooltip("Name the ticked contexts, to connect them together in one click")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.naming = true;
                                    this.set_name.update(cx, |s, cx| {
                                        s.set_value(String::new(), window, cx);
                                        s.focus(window, cx);
                                    });
                                    cx.notify();
                                })),
                        )
                    })
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
    use super::{FLAT_LIST_MAX, RECENT_MAX, order, set_order};
    use proptest::prelude::*;

    fn names(v: &[u8]) -> Vec<String> {
        v.iter().map(|n| format!("c{n}")).collect()
    }

    #[test]
    fn short_list_is_flat_recent_first() {
        let all = names(&[1, 2, 3, 4]);
        let (starred, list, other) = order(&all, &[], &names(&[3, 9, 1]));
        assert!(starred.is_empty());
        assert_eq!(list, names(&[3, 1, 2, 4]));
        assert!(other.is_none());
    }

    /// Favorites come first, in the order starred, and only once; one gone from the kubeconfig
    /// (not connected, not even listed) is left out.
    #[test]
    fn favorites_come_first() {
        let all = names(&[1, 2, 3, 4]);
        let (starred, list, other) = order(&all, &names(&[4, 9, 2]), &names(&[2, 3]));
        assert_eq!(starred, names(&[4, 2]));
        assert_eq!(list, names(&[3, 1]));
        assert!(other.is_none());
    }

    #[test]
    fn long_list_has_sections() {
        let all = names(&(0..20).collect::<Vec<_>>());
        let (_, recent, other) = order(&all, &[], &names(&[7, 7, 3, 1, 2, 4, 5]));
        assert_eq!(recent, names(&[7, 3, 1, 2, 4]));
        let other = other.unwrap();
        assert_eq!(other.len(), 15);
        assert!(!other.iter().any(|c| recent.contains(c)));
    }

    #[test]
    fn a_set_keeps_the_primary_first() {
        let picked = names(&[1, 2, 3, 5]).into_iter().collect();
        assert_eq!(set_order(&picked, &names(&[3, 4, 1])), names(&[3, 1, 2, 5]));
        assert_eq!(set_order(&picked, &[]), names(&[1, 2, 3, 5]));
    }

    proptest! {
        /// A set saved from the picker holds every ticked context once, and only those, the
        /// connected primary first when it is ticked.
        #[test]
        fn a_set_is_what_is_ticked(
            picked in proptest::collection::btree_set(0u8..20, 0..8),
            connected in proptest::collection::vec(0u8..20, 0..8),
        ) {
            let picked: std::collections::BTreeSet<String> = names(&picked.into_iter().collect::<Vec<_>>()).into_iter().collect();
            let connected = names(&connected);
            let order = set_order(&picked, &connected);
            let mut sorted = order.clone();
            sorted.sort();
            prop_assert_eq!(sorted, picked.iter().cloned().collect::<Vec<_>>());
            if let Some(primary) = connected.first().filter(|c| picked.contains(*c)) {
                prop_assert_eq!(order.first(), Some(primary));
            }
        }

        #[test]
        fn every_context_once(
            all in proptest::collection::btree_set(0u8..40, 0..30),
            favorites in proptest::collection::vec(0u8..50, 0..6),
            recent in proptest::collection::vec(0u8..50, 0..12),
        ) {
            let all = names(&all.into_iter().collect::<Vec<_>>());
            let (favorites, recent) = (names(&favorites), names(&recent));
            let (starred, first, other) = order(&all, &favorites, &recent);
            let short = all.len() <= FLAT_LIST_MAX;
            prop_assert_eq!(other.is_none(), short);
            prop_assert!(starred.iter().all(|c| favorites.contains(c)));
            prop_assert!(all.iter().filter(|c| favorites.contains(c)).all(|c| starred.contains(c)));
            let mut shown = starred;
            shown.extend(first.clone());
            shown.extend(other.unwrap_or_default());
            let mut sorted = shown.clone();
            sorted.sort();
            let mut expected = all;
            expected.sort();
            prop_assert_eq!(sorted, expected);
            if short {
                let n = first.iter().take_while(|c| recent.contains(c)).count();
                prop_assert!(first[n..].iter().all(|c| !recent.contains(c)));
            } else {
                prop_assert!(first.len() <= RECENT_MAX);
                prop_assert!(first.iter().all(|c| recent.contains(c)));
            }
        }
    }
}
