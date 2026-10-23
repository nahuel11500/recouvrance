//! Command palette (Ctrl+K): jump to a resource type, page, context or object.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::k8s::Kind;

#[derive(Clone)]
pub enum Target {
    Kind(Kind),
    Page(&'static str),
    Context(String),
    Object { ctx: usize, kind: String, name: String, namespace: Option<String> },
}

#[derive(Clone)]
pub struct Item {
    pub label: String,
    pub detail: String,
    pub icon: Lucide,
    pub target: Target,
}

pub enum PaletteEvent {
    Pick(Target),
    Dismiss,
}

pub struct Palette {
    items: Vec<Item>,
    status: Option<String>,
    input: Entity<InputState>,
    selected: usize,
    scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<PaletteEvent> for Palette {}

/// Subsequence match score: consecutive and early matches rank higher.
fn score(query: &str, text: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let text = text.to_lowercase();
    if let Some(pos) = text.find(query) {
        return Some(1000 - pos as i64 - text.len() as i64 / 10);
    }
    let mut score = 0i64;
    let mut last = None;
    let mut chars = text.char_indices();
    for q in query.chars() {
        let (i, _) = chars.find(|(_, c)| *c == q)?;
        if last.is_some_and(|l: usize| i == l + 1) {
            score += 5;
        }
        last = Some(i);
        score += 1;
    }
    Some(score - text.len() as i64 / 10)
}

impl Item {
    /// What the item opens: an object once per cluster, the rest by what it shows.
    fn key(&self) -> String {
        match &self.target {
            Target::Object { ctx, kind, name, namespace } => {
                format!("{ctx}/{kind}/{}/{name}", namespace.as_deref().unwrap_or_default())
            }
            _ => format!("{}\n{}", self.label, self.detail),
        }
    }
}

/// Add the items not already listed, each once.
fn add_new(items: &mut Vec<Item>, more: Vec<Item>) {
    let mut seen: std::collections::HashSet<String> = items.iter().map(Item::key).collect();
    items.extend(more.into_iter().filter(|i| seen.insert(i.key())));
}

impl Palette {
    pub fn new(more: Vec<Item>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // The current table and the search index can list the same object.
        let mut items = Vec::new();
        add_new(&mut items, more);
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Jump to a resource, page, context or object…"));
        let subs = vec![cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _, cx| match ev {
            InputEvent::Change => {
                this.selected = 0;
                this.scroll.scroll_to_item(0);
                cx.notify();
            }
            InputEvent::PressEnter { .. } => this.pick(cx),
            _ => {}
        })];
        input.update(cx, |s, cx| s.focus(window, cx));
        Self { items, status: None, input, selected: 0, scroll: ScrollHandle::new(), _subs: subs }
    }

    #[cfg(feature = "dev")]
    pub fn set_query(&mut self, q: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |s, cx| s.set_value(q.to_string(), window, cx));
        cx.notify();
    }

    /// Results that arrive later (the cluster-wide search index).
    pub fn add_items(&mut self, items: Vec<Item>, cx: &mut Context<Self>) {
        add_new(&mut self.items, items);
        cx.notify();
    }

    pub fn set_status(&mut self, status: Option<String>, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    fn matches(&self, cx: &App) -> Vec<&Item> {
        let q = self.input.read(cx).value().to_lowercase();
        let mut scored: Vec<(i64, &Item)> =
            self.items.iter().filter_map(|i| score(&q, &format!("{} {}", i.label, i.detail)).map(|s| (s, i))).collect();
        scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
        scored.into_iter().take(60).map(|(_, i)| i).collect()
    }

    fn pick(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = self.matches(cx).get(self.selected) {
            cx.emit(PaletteEvent::Pick(item.target.clone()));
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.matches(cx).len();
        match ev.keystroke.key.as_str() {
            "down" if n > 0 => self.selected = (self.selected + 1) % n,
            "up" if n > 0 => self.selected = (self.selected + n - 1) % n,
            "escape" => cx.emit(PaletteEvent::Dismiss),
            _ => return,
        }
        // Keep the highlighted row in view (Up on the first row goes to the last).
        self.scroll.scroll_to_item(self.selected);
        cx.stop_propagation();
        cx.notify();
    }
}

impl Render for Palette {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let matches: Vec<Item> = self.matches(cx).into_iter().cloned().collect();
        let selected = self.selected.min(matches.len().saturating_sub(1));
        // Full-window scrim; clicking outside dismisses.
        div()
            .id("palette-scrim")
            .absolute()
            .inset_0()
            .bg(black().opacity(0.25))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(PaletteEvent::Dismiss)))
            .child(
                v_flex()
                    .id("palette")
                    .absolute()
                    .top(px(80.))
                    .left(relative(0.5))
                    .ml(px(-320.))
                    .w(px(640.))
                    .max_h(px(520.))
                    .rounded_lg()
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.background)
                    .shadow_lg()
                    .overflow_hidden()
                    .capture_key_down(cx.listener(Self::on_key))
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .p_2()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(Input::new(&self.input).appearance(false)),
                    )
                    .child(
                        v_flex()
                            .id("palette-items")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .p_1()
                            .when(matches.is_empty(), |d| {
                                d.child(div().p_3().text_sm().text_color(theme.muted_foreground).child("No matches"))
                            })
                            .children(matches.into_iter().enumerate().map(|(i, item)| {
                                let target = item.target.clone();
                                h_flex()
                                    .id(("palette-item", i))
                                    .gap_2()
                                    .px_2()
                                    .py_1()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .when(i == selected, |d| d.bg(theme.list_active))
                                    .hover(|s| s.bg(theme.list_hover))
                                    .on_click(
                                        cx.listener(move |_, _, _, cx| cx.emit(PaletteEvent::Pick(target.clone()))),
                                    )
                                    .child(Icon::new(item.icon).size_4().text_color(theme.muted_foreground))
                                    .child(div().text_sm().child(item.label))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .whitespace_nowrap()
                                            .text_xs()
                                            .text_color(theme.muted_foreground)
                                            .child(item.detail),
                                    )
                            })),
                    )
                    .child(
                        h_flex()
                            .gap_3()
                            .px_3()
                            .py_1()
                            .border_t_1()
                            .border_color(theme.border)
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("↑↓ to move")
                            .child("Enter to open")
                            .child("Esc to close")
                            .child(div().flex_1())
                            .children(self.status.clone()),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{Item, Lucide, Target, add_new, score};

    fn object(ctx: usize, detail: &str) -> Item {
        Item {
            label: "web".into(),
            detail: detail.into(),
            icon: Lucide::Box,
            target: Target::Object {
                ctx,
                kind: "deployment".into(),
                name: "web".into(),
                namespace: Some("prod".into()),
            },
        }
    }

    #[test]
    fn same_name_on_other_clusters_is_kept() {
        let mut items = vec![object(0, "Deployments · prod")];
        add_new(&mut items, vec![object(0, "Deployments · prod · a"), object(1, "Deployments · prod · b")]);
        assert_eq!(items.len(), 2);
        assert!(matches!(items[1].target, Target::Object { ctx: 1, .. }));
    }

    #[test]
    fn same_object_twice_in_one_batch_is_listed_once() {
        let mut items = Vec::new();
        add_new(&mut items, vec![object(0, "Deployments · prod"), object(0, "Deployments · prod · a")]);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn scoring_prefers_substrings_and_early_matches() {
        assert!(score("dep", "Deployments").unwrap() > score("dep", "EndpointSlices").unwrap_or(i64::MIN));
        assert!(score("cfgm", "ConfigMaps").is_some());
        assert!(score("xyz", "Pods").is_none());
        assert_eq!(score("", "anything"), Some(0));
    }
}
