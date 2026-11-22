//! Command palette (Ctrl+K): jump to a resource type, page, context or object, or run a command.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::k8s::Kind;

#[derive(Clone)]
pub enum Target {
    Kind(Kind),
    /// A command (see `app::commands`): the action its keys run.
    Action(fn() -> Box<dyn Action>),
    /// A section of Settings.
    Settings(crate::ui::settings_view::Section),
    /// A saved view of a table.
    View(crate::favorites::SavedView),
    Context(String),
    Object {
        ctx: usize,
        kind: String,
        name: String,
        namespace: Option<String>,
    },
}

#[derive(Clone)]
pub struct Item {
    pub label: String,
    pub detail: String,
    pub icon: Lucide,
    pub target: Target,
    /// The keys that run it, in braces (see `app::with_keys`), shown on its right; empty for none.
    pub keys: &'static str,
    /// More words it is found by, not shown (`rbac` for My access).
    pub also: &'static str,
}

impl Item {
    pub fn new(label: String, detail: String, icon: Lucide, target: Target) -> Self {
        Self { label, detail, icon, target, keys: "", also: "" }
    }
}

/// An item and the lowercase text it is searched by, made once when it is added rather than on
/// each keystroke: the palette can hold tens of thousands of objects.
struct Row {
    item: Item,
    /// The label, detail and other words, lowercase.
    text: String,
    /// Where the label ends in `text`.
    label_end: usize,
}

impl Row {
    fn new(item: Item) -> Self {
        let mut text = item.label.to_lowercase();
        let label_end = text.len();
        for part in [item.detail.as_str(), item.also] {
            if !part.is_empty() {
                text.push(' ');
                text.push_str(&part.to_lowercase());
            }
        }
        Self { item, text, label_end }
    }
}

pub enum PaletteEvent {
    Pick(Target),
    Dismiss,
}

pub struct Palette {
    items: Vec<Row>,
    status: Option<String>,
    input: Entity<InputState>,
    /// What is typed, lowercase: kept so a redraw doesn't lowercase it again.
    query: String,
    selected: usize,
    scroll: ScrollHandle,
    _subs: Vec<Subscription>,
}

impl EventEmitter<PaletteEvent> for Palette {}

/// How well a lowercase `query` matches a row: a substring of its label, detail or other words
/// ranks first, earliest best; else the query's letters in order in the label alone, consecutive
/// ones ranking higher. Letters spread over the detail match nearly anything (`fraud` finds
/// `PodCertificateRequest`), so they don't count.
fn score(query: &str, row: &Row) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let text = row.text.as_str();
    if let Some(pos) = text.find(query) {
        return Some(1000 - pos as i64 - text.len() as i64 / 10);
    }
    let label = &text[..row.label_end];
    let mut score = 0i64;
    let mut last = None;
    let mut chars = label.char_indices();
    for q in query.chars() {
        let (i, _) = chars.find(|(_, c)| *c == q)?;
        if last.is_some_and(|l: usize| i == l + 1) {
            score += 5;
        }
        last = Some(i);
        score += 1;
    }
    Some(score - label.len() as i64 / 10)
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

/// The items matching a lowercase `query`, best first, as many as are shown.
fn ranked<'a>(items: &'a [Row], query: &str) -> Vec<&'a Item> {
    let mut scored: Vec<(i64, &Item)> = items.iter().filter_map(|r| score(query, r).map(|s| (s, &r.item))).collect();
    scored.sort_by_key(|(s, _)| std::cmp::Reverse(*s));
    scored.into_iter().take(60).map(|(_, i)| i).collect()
}

/// Add `more` and return where the highlighted row is now, so Enter still opens what was
/// highlighted when better matches arrive above it.
fn add_keeping_selection(items: &mut Vec<Row>, more: Vec<Item>, query: &str, selected: usize) -> usize {
    let shown = ranked(items, query);
    let key = shown.get(selected.min(shown.len().saturating_sub(1))).map(|i| i.key());
    add_new(items, more);
    key.and_then(|key| ranked(items, query).iter().position(|i| i.key() == key)).unwrap_or(selected)
}

/// Add the items not already listed, each once.
fn add_new(items: &mut Vec<Row>, more: Vec<Item>) {
    let mut seen: std::collections::HashSet<String> = items.iter().map(|r| r.item.key()).collect();
    items.extend(more.into_iter().filter(|i| seen.insert(i.key())).map(Row::new));
}

impl Palette {
    pub fn new(more: Vec<Item>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // The current table and the search index can list the same object.
        let mut items = Vec::new();
        add_new(&mut items, more);
        let input = cx
            .new(|cx| InputState::new(window, cx).placeholder("Jump to a resource, page, context, object or command…"));
        let subs = vec![cx.subscribe_in(&input, window, |this, _, ev: &InputEvent, _, cx| match ev {
            InputEvent::Change => {
                this.query = this.input.read(cx).value().to_lowercase();
                this.selected = 0;
                this.scroll.scroll_to_item(0);
                cx.notify();
            }
            InputEvent::PressEnter { .. } => this.pick(cx),
            _ => {}
        })];
        input.update(cx, |s, cx| s.focus(window, cx));
        Self { items, status: None, input, query: String::new(), selected: 0, scroll: ScrollHandle::new(), _subs: subs }
    }

    #[cfg(feature = "dev")]
    pub fn set_query(&mut self, q: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |s, cx| s.set_value(q.to_string(), window, cx));
        self.query = q.to_lowercase();
        cx.notify();
    }

    /// Results that arrive later (the cluster-wide search index).
    pub fn add_items(&mut self, items: Vec<Item>, cx: &mut Context<Self>) {
        let selected = add_keeping_selection(&mut self.items, items, &self.query, self.selected);
        if selected != self.selected {
            self.selected = selected;
            self.scroll.scroll_to_item(selected);
        }
        cx.notify();
    }

    pub fn set_status(&mut self, status: Option<String>, cx: &mut Context<Self>) {
        self.status = status;
        cx.notify();
    }

    fn matches(&self) -> Vec<&Item> {
        ranked(&self.items, &self.query)
    }

    fn pick(&mut self, cx: &mut Context<Self>) {
        if let Some(item) = self.matches().get(self.selected) {
            cx.emit(PaletteEvent::Pick(item.target.clone()));
        }
    }

    fn on_key(&mut self, ev: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let n = self.matches().len();
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
        let matches: Vec<Item> = self.matches().into_iter().cloned().collect();
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
                                    .when(!item.keys.is_empty(), |d| {
                                        d.child(crate::app::key_cap(item.keys, cx).text_color(theme.muted_foreground))
                                    })
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
    use super::{Item, Lucide, Row, Target, add_keeping_selection, add_new, ranked, score};
    use crate::app::commands::palette_items;

    fn object(ctx: usize, detail: &str) -> Item {
        Item::new(
            "web".into(),
            detail.into(),
            Lucide::Box,
            Target::Object { ctx, kind: "deployment".into(), name: "web".into(), namespace: Some("prod".into()) },
        )
    }

    fn row(label: &str, detail: &str) -> Row {
        Row::new(Item::new(label.into(), detail.into(), Lucide::Box, Target::Context(label.into())))
    }

    #[test]
    fn same_name_on_other_clusters_is_kept() {
        let mut items = Vec::new();
        add_new(&mut items, vec![object(0, "Deployments · prod")]);
        add_new(&mut items, vec![object(0, "Deployments · prod · a"), object(1, "Deployments · prod · b")]);
        assert_eq!(items.len(), 2);
        assert!(matches!(items[1].item.target, Target::Object { ctx: 1, .. }));
    }

    #[test]
    fn same_object_twice_in_one_batch_is_listed_once() {
        let mut items = Vec::new();
        add_new(&mut items, vec![object(0, "Deployments · prod"), object(0, "Deployments · prod · a")]);
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn highlight_stays_on_its_object_when_better_matches_arrive() {
        let mut items = Vec::new();
        add_new(&mut items, vec![object(0, "Deployments · prod · a"), object(1, "Deployments · prod · b")]);
        // The second row (cluster b's web) is highlighted; a closer match then arrives.
        let selected = add_keeping_selection(&mut items, vec![object(2, "x")], "web", 1);
        let picked = ranked(&items, "web")[selected];
        assert!(matches!(picked.target, Target::Object { ctx: 1, .. }));
        assert_eq!(selected, 2);
    }

    #[test]
    fn scoring_prefers_substrings_and_early_matches() {
        let deployments = row("Deployments", "deployments.apps · Workloads");
        let slices = row("EndpointSlices", "endpointslices · Network");
        assert!(score("dep", &deployments).unwrap() > score("dep", &slices).unwrap_or(i64::MIN));
        assert!(score("cfgm", &row("ConfigMaps", "configmaps · Config")).is_some());
        assert!(score("xyz", &row("Pods", "pods · Workloads")).is_none());
        assert_eq!(score("", &row("anything", "")), Some(0));
    }

    /// Letters spread over the detail line match nearly any row: only the label is searched
    /// that way, while a word of the detail still finds its row.
    #[test]
    fn scattered_letters_match_the_label_only() {
        let certs = row("PodCertificateRequests", "podcertificaterequests.certificates.k8s.io · Security");
        assert_eq!(score("fraud", &certs), None);
        assert!(score("security", &certs).is_some());
        assert!(score("pcr", &row("PodCertificateRequests", "")).is_some());
    }

    /// The commands without a key are found by their name and by the words people search for.
    #[test]
    fn commands_are_found_by_name_and_other_words() {
        let mut items = Vec::new();
        add_new(&mut items, palette_items().collect());
        for (query, name) in [
            ("rbac", "My access"),
            ("can-i", "My access"),
            ("deprecated", "Upgrade check"),
            ("audit", "Agent activity"),
            ("api requests", "API requests"),
            ("shortcuts", "Keyboard shortcuts"),
            ("write", "Write mode on or off"),
        ] {
            let found = ranked(&items, query);
            assert_eq!(found.first().map(|i| i.label.as_str()), Some(name), "{query}");
        }
        // A command with a key shows it.
        let problems = items.iter().find(|r| r.item.label == "Problems").expect("Problems is listed");
        assert_eq!(problems.item.keys, "{g p}");
    }
}
