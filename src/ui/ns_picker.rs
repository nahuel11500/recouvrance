//! Multi-namespace picker: a searchable checkbox list.

use std::collections::BTreeSet;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The selection changed; empty means all namespaces.
pub struct NamespacesChanged(pub Vec<String>);

/// A single namespace was clicked: the popover can close.
pub struct NamespacePicked;

pub struct NamespacePicker {
    all: Vec<String>,
    /// Context whose recent namespaces are offered first.
    pub context: String,
    pub selected: BTreeSet<String>,
    search: Entity<InputState>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<NamespacesChanged> for NamespacePicker {}
impl EventEmitter<NamespacePicked> for NamespacePicker {}

impl NamespacePicker {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search namespaces"));
        let subs = vec![cx.subscribe(&search, |_, _, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                cx.notify();
            }
        })];
        Self { all: Vec::new(), context: String::new(), selected: BTreeSet::new(), search, _subs: subs }
    }

    pub fn set_namespaces(&mut self, mut all: Vec<String>, cx: &mut Context<Self>) {
        // Namespaces added by hand (when listing them is not allowed).
        all.extend(crate::settings::with(|s| s.extra_namespaces.get(&self.context).cloned().unwrap_or_default()));
        all.sort();
        all.dedup();
        self.selected.retain(|ns| all.contains(ns));
        self.all = all;
        cx.notify();
    }

    /// Add a namespace by hand, remember it for this context, and show it.
    pub fn add_namespace(&mut self, ns: String, cx: &mut Context<Self>) {
        let ctx = self.context.clone();
        let n = ns.clone();
        crate::settings::update(|s| {
            let list = s.extra_namespaces.entry(ctx).or_default();
            if !list.contains(&n) {
                list.push(n);
            }
        });
        if !self.all.contains(&ns) {
            self.all.push(ns.clone());
            self.all.sort();
        }
        self.selected = [ns].into();
        self.changed(cx);
        cx.emit(NamespacePicked);
    }

    fn forget_namespace(&mut self, ns: &str, cx: &mut Context<Self>) {
        let ctx = self.context.clone();
        crate::settings::update(|s| {
            if let Some(list) = s.extra_namespaces.get_mut(&ctx) {
                list.retain(|n| n != ns);
            }
        });
        self.all.retain(|n| n != ns);
        if self.selected.remove(ns) {
            self.changed(cx);
        }
        cx.notify();
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        let list: Vec<String> = self.selected.iter().cloned().collect();
        if !list.is_empty() && !self.context.is_empty() {
            let ctx = self.context.clone();
            let l = list.clone();
            crate::settings::update(|s| {
                crate::settings::push_recent(s.recent_namespaces.entry(ctx).or_default(), &l, 6)
            });
        }
        cx.emit(NamespacesChanged(list));
        cx.notify();
    }

    /// One row: the checkbox adds to the selection, a click on the name shows only that namespace.
    fn row(&self, id: SharedString, ns: &str, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (n1, n2) = (ns.to_string(), ns.to_string());
        let checked = self.selected.contains(ns);
        h_flex()
            .id(id.clone())
            .gap_2()
            .px_1()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(theme.list_hover))
            .when(checked, |d| d.bg(theme.blue.opacity(0.08)))
            .child(Checkbox::new(SharedString::from(format!("{id}-cb"))).checked(checked).on_click(cx.listener(
                move |this, v: &bool, _, cx| {
                    if *v {
                        this.selected.insert(n1.clone());
                    } else {
                        this.selected.remove(&n1);
                    }
                    this.changed(cx);
                },
            )))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(ns.to_string()),
            )
            .when(
                crate::settings::with(|s| {
                    s.extra_namespaces.get(&self.context).is_some_and(|l| l.iter().any(|n| n == ns))
                }),
                |d| {
                    let n3 = ns.to_string();
                    d.child(div().text_xs().text_color(theme.muted_foreground).child("added")).child(
                        Button::new(SharedString::from(format!("{id}-forget")))
                            .xsmall()
                            .ghost()
                            .icon(component::IconName::Close)
                            .tooltip("Forget this namespace")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.forget_namespace(&n3, cx);
                                cx.stop_propagation();
                            })),
                    )
                },
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.selected = [n2.clone()].into();
                this.changed(cx);
                cx.emit(NamespacePicked);
            }))
            .into_any_element()
    }
}

impl Render for NamespacePicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self.search.read(cx).value().to_lowercase();
        // Selected first, then the rest, filtered by the search.
        let mut visible: Vec<&String> = self.all.iter().filter(|ns| ns.contains(&query)).collect();
        visible.sort_by_key(|ns| !self.selected.contains(*ns));
        v_flex()
            .w(px(340.))
            .gap_2()
            .child(Input::new(&self.search).cleanable(true))
            .child(
                h_flex()
                    .gap_2()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{} selected", self.selected.len()))
                    .child(div().flex_1())
                    .when(!self.selected.is_empty(), |d| {
                        d.child(Button::new("ns-clear").xsmall().ghost().label("All namespaces").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.selected.clear();
                                this.changed(cx);
                            },
                        )))
                    }),
            )
            .child({
                let recent: Vec<String> = if query.is_empty() {
                    crate::settings::with(|s| {
                        s.recent_namespaces
                            .get(&self.context)
                            .into_iter()
                            .flatten()
                            .filter(|n| self.all.contains(n))
                            .take(4)
                            .cloned()
                            .collect()
                    })
                } else {
                    Vec::new()
                };
                let muted = cx.theme().muted_foreground;
                let mut rows: Vec<AnyElement> = Vec::new();
                if !recent.is_empty() {
                    rows.push(div().px_1().text_xs().text_color(muted).child("Recent").into_any_element());
                    for (i, ns) in recent.iter().enumerate() {
                        rows.push(self.row(SharedString::from(format!("ns-recent-{i}")), ns, cx));
                    }
                    rows.push(div().px_1().pt_2().text_xs().text_color(muted).child("All").into_any_element());
                }
                for (i, ns) in visible.iter().enumerate() {
                    rows.push(self.row(SharedString::from(format!("ns-{i}")), ns, cx));
                }
                // A name not in the list can be added by hand (no permission to list namespaces).
                let typed = self.search.read(cx).value().trim().to_string();
                let valid = !typed.is_empty()
                    && typed.len() <= 63
                    && typed.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
                if valid && !self.all.contains(&typed) {
                    rows.push(
                        div()
                            .id("ns-add")
                            .px_1()
                            .py_1()
                            .rounded_md()
                            .text_sm()
                            .text_color(cx.theme().blue)
                            .cursor_pointer()
                            .hover(|s| s.bg(cx.theme().list_hover))
                            .child(format!("+ Add namespace “{typed}”"))
                            .on_click(cx.listener(move |this, _, _, cx| this.add_namespace(typed.clone(), cx)))
                            .into_any_element(),
                    );
                }
                v_flex().id("ns-list").max_h(px(420.)).overflow_y_scroll().children(rows)
            })
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Click a name to show only it · tick several to combine"),
            )
    }
}
