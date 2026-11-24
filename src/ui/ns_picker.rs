//! Multi-namespace picker: a searchable checkbox list, the favorite namespaces of the
//! connected contexts first, then the ones you visit most on this context.

use std::collections::BTreeSet;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

/// The selection changed; empty means all namespaces.
pub struct NamespacesChanged(pub Vec<String>);

/// A single namespace was clicked: the popover can close.
pub struct NamespacePicked;

/// Frequent namespaces shown above the full list.
const FREQUENT_SHOWN: usize = 5;

/// What the rows show: favorites, frequent, then all matching, then a name to add by hand.
struct Rows {
    favorites: Vec<String>,
    frequent: Vec<String>,
    all: Vec<String>,
    add: Option<String>,
}

impl Rows {
    fn len(&self) -> usize {
        self.favorites.len() + self.frequent.len() + self.all.len() + usize::from(self.add.is_some())
    }

    /// The namespace at keyboard position `ix` (None: the add row).
    fn at(&self, ix: usize) -> Option<&String> {
        self.favorites.iter().chain(&self.frequent).chain(&self.all).nth(ix)
    }

    /// The section headers above row `ix`: one over favorites and frequent each when shown,
    /// and one over all namespaces when either is.
    fn headers_before(&self, ix: usize) -> usize {
        let (fav, freq) = (self.favorites.len(), self.frequent.len());
        let mut n = usize::from(fav > 0);
        if freq > 0 && ix >= fav {
            n += 1;
        }
        if (fav > 0 || freq > 0) && ix >= fav + freq {
            n += 1;
        }
        n
    }
}

pub struct NamespacePicker {
    all: Vec<String>,
    /// Context whose frequent namespaces are offered first.
    pub context: String,
    /// Every context connected: their favorite namespaces come first, and the star marks a
    /// namespace on each of them.
    pub contexts: Vec<String>,
    favorites: Entity<crate::favorites::Favorites>,
    pub selected: BTreeSet<String>,
    search: Entity<InputState>,
    /// Row chosen with the arrow keys (Enter picks it).
    highlight: usize,
    scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<NamespacesChanged> for NamespacePicker {}
impl EventEmitter<NamespacePicked> for NamespacePicker {}

impl NamespacePicker {
    pub fn new(favorites: Entity<crate::favorites::Favorites>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search namespaces"));
        let mut subs = vec![cx.subscribe(&search, |this, _, ev: &InputEvent, cx| match ev {
            InputEvent::Change => {
                this.highlight = 0;
                cx.notify();
            }
            // Enter shows only the highlighted namespace (Ctrl+Enter, which ticks it, is in `on_key`).
            InputEvent::PressEnter { secondary: false, .. } => this.pick_highlighted(false, cx),
            _ => {}
        })];
        subs.push(cx.observe(&favorites, |_, _, cx| cx.notify()));
        Self {
            all: Vec::new(),
            context: String::new(),
            contexts: Vec::new(),
            favorites,
            selected: BTreeSet::new(),
            search,
            highlight: 0,
            scroll: ScrollHandle::new(),
            _subs: subs,
        }
    }

    /// The search field, focused when the popover opens.
    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.search.read(cx).focus_handle(cx)
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

    /// Every namespace offered: the ones listed and the ones added by hand.
    pub fn known(&self) -> &[String] {
        &self.all
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
        self.pick_only(ns, cx);
    }

    #[cfg(feature = "dev")]
    pub fn demo_search(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.set_value(text.to_string(), window, cx));
        cx.notify();
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

    /// Show only `ns`, count the visit, and close.
    fn pick_only(&mut self, ns: String, cx: &mut Context<Self>) {
        crate::settings::visit_namespaces(&self.context, std::slice::from_ref(&ns));
        self.selected = [ns].into();
        self.changed(cx);
        cx.emit(NamespacePicked);
    }

    /// Tick or untick `ns`; ticking counts as a visit.
    fn toggle(&mut self, ns: String, cx: &mut Context<Self>) {
        if !self.selected.remove(&ns) {
            crate::settings::visit_namespaces(&self.context, std::slice::from_ref(&ns));
            self.selected.insert(ns);
        }
        self.changed(cx);
    }

    fn pick_highlighted(&mut self, tick: bool, cx: &mut Context<Self>) {
        let rows = self.rows(cx);
        let ix = self.highlight.min(rows.len().saturating_sub(1));
        match (rows.at(ix).cloned(), rows.add) {
            (Some(ns), _) if tick => {
                // Ticked namespaces move to the top: keep the highlight on this one.
                let (fav, freq) = (rows.favorites.len(), rows.frequent.len());
                let section = if ix < fav {
                    0
                } else if ix < fav + freq {
                    1
                } else {
                    2
                };
                self.toggle(ns.clone(), cx);
                let rows = self.rows(cx);
                let (fav, freq) = (rows.favorites.len(), rows.frequent.len());
                let found = match section {
                    0 => rows.favorites.iter().position(|n| *n == ns),
                    1 => rows.frequent.iter().position(|n| *n == ns).map(|i| i + fav),
                    _ => rows.all.iter().position(|n| *n == ns).map(|i| i + fav + freq),
                };
                self.highlight = found.unwrap_or(ix);
            }
            (Some(ns), _) => self.pick_only(ns, cx),
            (None, Some(typed)) => self.add_namespace(typed, cx),
            (None, None) => {}
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.rows(cx).len();
        match ev.keystroke.key.as_str() {
            "down" if n > 0 => self.highlight = (self.highlight + 1) % n,
            "up" if n > 0 => self.highlight = (self.highlight + n - 1) % n,
            // Not every platform maps Ctrl+Enter to the input's secondary Enter.
            "enter" if ev.keystroke.modifiers.secondary() => {
                self.pick_highlighted(true, cx);
                cx.stop_propagation();
                return;
            }
            _ => return,
        }
        // Children of the list: a header comes before each section.
        let headers = self.rows(cx).headers_before(self.highlight);
        self.scroll.scroll_to_item(self.highlight + headers);
        cx.stop_propagation();
        cx.notify();
    }

    /// Whether `ns` is a favorite on any connected context.
    fn is_favorite(&self, ns: &str, cx: &App) -> bool {
        (self.favorites.read(cx).namespaces().iter()).any(|f| f.namespace == ns && self.contexts.contains(&f.context))
    }

    /// Star `ns` on every connected context, or unstar it on all of them when it is starred on
    /// one (with several combined, the picker lists them as one).
    pub fn toggle_favorite(&mut self, ns: &str, cx: &mut Context<Self>) {
        let on = !self.is_favorite(ns, cx);
        let list = (self.contexts.iter())
            .map(|c| {
                crate::favorites::Favorite::Namespace(crate::favorites::FavNamespace {
                    context: c.clone(),
                    namespace: ns.to_string(),
                })
            })
            .collect();
        self.favorites.update(cx, |f, cx| f.set_all(list, on, cx));
        cx.notify();
    }

    fn changed(&mut self, cx: &mut Context<Self>) {
        cx.emit(NamespacesChanged(self.selected.iter().cloned().collect()));
        cx.notify();
    }

    fn rows(&self, cx: &App) -> Rows {
        let typed = self.search.read(cx).value().trim().to_string();
        let query = typed.to_lowercase();
        let favorites: Vec<String> =
            crate::favorites::favorite_namespaces(&self.all, self.favorites.read(cx).namespaces(), &self.contexts)
                .into_iter()
                .filter(|ns| ns.contains(&query))
                .collect();
        let frequent: Vec<String> = crate::settings::frequent_namespaces(&self.context)
            .into_iter()
            .filter(|ns| self.all.contains(ns) && ns.contains(&query) && !favorites.contains(ns))
            .take(FREQUENT_SHOWN)
            .collect();
        // Selected first, then the rest, filtered by the search.
        let mut all: Vec<String> = self.all.iter().filter(|ns| ns.contains(&query)).cloned().collect();
        all.sort_by_key(|ns| !self.selected.contains(ns));
        // A name not in the list can be added by hand (no permission to list namespaces).
        let add = to_add(typed, &self.all);
        Rows { favorites, frequent, all, add }
    }

    /// One row: the checkbox adds to the selection, a click on the name shows only that namespace.
    fn row(&self, id: SharedString, ns: &str, highlighted: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (n1, n2) = (ns.to_string(), ns.to_string());
        let checked = self.selected.contains(ns);
        let starred = self.is_favorite(ns, cx);
        let star = {
            let ns = ns.to_string();
            div()
                .id(SharedString::from(format!("{id}-star")))
                .flex_none()
                .cursor_pointer()
                .when(!starred, |d| d.opacity(0.).group_hover(id.clone(), |s| s.opacity(0.5)))
                .tooltip(move |w, cx| {
                    let text = if starred { "Remove from favorites" } else { "Add to favorites" };
                    component::tooltip::Tooltip::new(text).build(w, cx)
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_favorite(&ns, cx);
                    cx.stop_propagation();
                }))
                .child(if starred {
                    Icon::new(component::IconName::StarFill).size_3p5().text_color(yellow())
                } else {
                    Icon::new(Lucide::Star).size_3p5()
                })
        };
        h_flex()
            .id(id.clone())
            .group(id.clone())
            .gap_2p5()
            .px_2()
            .h(px(34.))
            .flex_shrink_0()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(theme.list_hover))
            .when(checked, |d| d.bg(theme.blue.opacity(0.08)))
            .when(highlighted, |d| d.bg(theme.list_hover))
            .child(Checkbox::new(SharedString::from(format!("{id}-cb"))).checked(checked).on_click(cx.listener(
                move |this, _: &bool, _, cx| {
                    this.toggle(n1.clone(), cx);
                    cx.stop_propagation();
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
            .when(highlighted, |d| {
                d.child(Icon::new(Lucide::CornerDownLeft).size_3p5().text_color(theme.muted_foreground))
            })
            .child(star)
            .on_click(cx.listener(move |this, _, _, cx| this.pick_only(n2.clone(), cx)))
            .into_any_element()
    }
}

impl Render for NamespacePicker {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows(cx);
        let highlight = self.highlight.min(rows.len().saturating_sub(1));
        let muted = cx.theme().muted_foreground;
        let header = |label: &'static str, icon: Option<Lucide>| {
            h_flex()
                .gap_1p5()
                .px_2()
                .pt_2()
                .pb_1()
                .flex_shrink_0()
                .text_xs()
                .font_weight(FontWeight::MEDIUM)
                .text_color(muted)
                .when_some(icon, |d, i| d.child(Icon::new(i).size_3p5()))
                .child(label)
                .into_any_element()
        };
        let mut items: Vec<AnyElement> = Vec::new();
        let fav = rows.favorites.len();
        if fav > 0 {
            items.push(header("Favorites", Some(Lucide::Star)));
            for (i, ns) in rows.favorites.iter().enumerate() {
                items.push(self.row(SharedString::from(format!("ns-fav-{i}")), ns, highlight == i, cx));
            }
        }
        if !rows.frequent.is_empty() {
            items.push(header("Frequent", Some(Lucide::TrendingUp)));
            for (i, ns) in rows.frequent.iter().enumerate() {
                items.push(self.row(SharedString::from(format!("ns-freq-{i}")), ns, highlight == fav + i, cx));
            }
        }
        if fav > 0 || !rows.frequent.is_empty() {
            items.push(header("All namespaces", None));
        }
        let offset = fav + rows.frequent.len();
        for (i, ns) in rows.all.iter().enumerate() {
            items.push(self.row(SharedString::from(format!("ns-{i}")), ns, highlight == offset + i, cx));
        }
        if let Some(typed) = rows.add.clone() {
            let highlighted = highlight == offset + rows.all.len();
            items.push(
                h_flex()
                    .id("ns-add")
                    .px_2()
                    .h(px(34.))
                    .flex_shrink_0()
                    .rounded_md()
                    .text_sm()
                    .text_color(cx.theme().blue)
                    .cursor_pointer()
                    .hover(|s| s.bg(cx.theme().list_hover))
                    .when(highlighted, |d| d.bg(cx.theme().list_hover))
                    .child(format!("+ Add namespace “{typed}”"))
                    .on_click(cx.listener(move |this, _, _, cx| this.add_namespace(typed.clone(), cx)))
                    .into_any_element(),
            );
        }
        if rows.len() == 0 {
            items.push(
                div().px_2().py_3().text_sm().text_color(muted).child("No matching namespace").into_any_element(),
            );
        }
        v_flex()
            .w(px(440.))
            .gap_2()
            .capture_key_down(cx.listener(Self::on_key))
            .child(
                Input::new(&self.search).cleanable(true).prefix(Icon::new(Lucide::Search).size_4().text_color(muted)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .px_1()
                    .text_xs()
                    .text_color(muted)
                    .child(if self.selected.is_empty() {
                        format!("All {} namespaces shown", self.all.len())
                    } else {
                        format!("{} of {} selected", self.selected.len(), self.all.len())
                    })
                    .child(div().flex_1())
                    .when(!self.selected.is_empty(), |d| {
                        d.child(Button::new("ns-clear").xsmall().ghost().label("Show all").on_click(cx.listener(
                            |this, _, _, cx| {
                                this.selected.clear();
                                this.changed(cx);
                            },
                        )))
                    }),
            )
            .child(
                v_flex().id("ns-list").max_h(px(480.)).overflow_y_scroll().track_scroll(&self.scroll).children(items),
            )
            .child(div().px_1().text_xs().text_color(muted).child(crate::app::with_keys(
                "Click or {enter} shows only one · tick or {secondary-enter} to combine · the star pins one",
            )))
    }
}

/// The name typed, when it can be added by hand: a namespace name Kubernetes accepts, not
/// already listed.
fn to_add(typed: String, listed: &[String]) -> Option<String> {
    (crate::k8s::names::validate_namespace(&typed).is_ok() && !listed.contains(&typed)).then_some(typed)
}

#[cfg(test)]
mod tests {
    use super::{Rows, to_add};

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    /// Favorites come first, then frequent, then all: the keyboard walks them in that order, and
    /// the list scrolls past one header per section shown.
    #[test]
    fn favorites_are_the_first_rows() {
        let rows =
            Rows { favorites: names(&["web"]), frequent: names(&["db"]), all: names(&["db", "web", "x"]), add: None };
        assert_eq!(rows.len(), 5);
        assert_eq!(rows.at(0).map(String::as_str), Some("web"));
        assert_eq!(rows.at(1).map(String::as_str), Some("db"));
        assert_eq!(rows.at(4).map(String::as_str), Some("x"));
        assert_eq!([0, 1, 2].map(|ix| rows.headers_before(ix)), [1, 2, 3]);
        let no_favorites = Rows { favorites: vec![], ..rows };
        assert_eq!([0, 1].map(|ix| no_favorites.headers_before(ix)), [1, 2]);
        let plain = Rows { favorites: vec![], frequent: vec![], all: names(&["a"]), add: None };
        assert_eq!(plain.headers_before(0), 0);
    }

    #[test]
    fn only_names_kubernetes_accepts_can_be_added() {
        let listed = vec!["default".to_string()];
        assert_eq!(to_add("team-a".into(), &listed).as_deref(), Some("team-a"));
        for bad in ["-x", "x-", "", "default", "Team", "a.b"] {
            assert_eq!(to_add(bad.into(), &listed), None, "{bad:?}");
        }
    }
}
