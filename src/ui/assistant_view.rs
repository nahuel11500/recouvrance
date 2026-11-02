//! Chat with the in-app assistant. Tool calls are shown as they happen.

use std::sync::Arc;

use futures::StreamExt as _;
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::assistant::{self, Step};
use crate::mcp::Hub;

enum Entry {
    User(String),
    Assistant(Vec<Step>),
    Error(String),
}

pub struct AssistantView {
    hub: Arc<Hub>,
    /// What the user is looking at, given to the model.
    scope: String,
    conv: assistant::Conversation,
    entries: Vec<Entry>,
    input: Entity<InputState>,
    key_input: Entity<InputState>,
    /// Why the API key could not be saved, shown under the field.
    setup_error: Option<String>,
    busy: bool,
    started: Option<std::time::Instant>,
    /// Tool results expanded in the transcript, by (entry, step).
    open: std::collections::HashSet<(usize, usize)>,
    /// Blocks of tool calls unfolded, by (entry, first step).
    open_groups: std::collections::HashSet<(usize, usize)>,
    scroll: ScrollHandle,
    _task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

const SUGGESTIONS: [&str; 4] = [
    "What is broken right now, and why?",
    "Which workloads request much more CPU or memory than they use?",
    "Do any TLS certificates expire in the next 30 days?",
    "Summarize the health of this cluster in 10 lines.",
];

impl AssistantView {
    pub fn new(hub: Arc<Hub>, scope: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Ask about your clusters… (Enter to send)"));
        let key_input = cx.new(|cx| InputState::new(window, cx).placeholder("sk-ant-…").masked(true));
        let subs = vec![cx.subscribe_in(&input, window, |this, state, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                let q = state.read(cx).value().trim().to_string();
                if !q.is_empty() {
                    state.update(cx, |s, cx| s.set_value("", window, cx));
                    this.send(q, cx);
                }
            }
        })];
        // RECOUVRANCE_CHAT_FILE=<markdown>: a question and that answer, to look at the rendering.
        #[cfg(feature = "dev")]
        let entries = std::env::var("RECOUVRANCE_CHAT_FILE")
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|md| {
                vec![
                    Entry::User("Why are five Elasticsearch pods not ready?".into()),
                    Entry::Assistant(
                        [
                            (
                                "describe_resource",
                                r#"{"context":"kind-dev","kind":"pod","name":"es-0","namespace":"logging"}"#,
                            ),
                            ("get_events", r#"{"context":"kind-dev","namespace":"logging"}"#),
                            (
                                "get_logs",
                                r#"{"context":"kind-dev","namespace":"logging","pod":"es-0","previous":true}"#,
                            ),
                        ]
                        .into_iter()
                        .map(|(name, args)| Step::Tool { name: name.into(), args: args.into(), result: "…".into() })
                        .chain([Step::Text(md)])
                        .collect(),
                    ),
                ]
            })
            .unwrap_or_default();
        #[cfg(not(feature = "dev"))]
        let entries = Vec::new();
        Self {
            hub,
            scope,
            conv: Default::default(),
            entries,
            input,
            key_input,
            setup_error: None,
            busy: false,
            started: None,
            open: Default::default(),
            open_groups: Default::default(),
            scroll: ScrollHandle::new(),
            _task: None,
            _subs: subs,
        }
    }

    /// The cluster and namespace changed: later questions say so.
    pub fn set_scope(&mut self, scope: String, cx: &mut Context<Self>) {
        if self.scope != scope {
            self.scope = scope;
            cx.notify();
        }
    }

    /// A new chat with this question, stopping the one running.
    pub fn start_over(&mut self, question: String, cx: &mut Context<Self>) {
        self._task = None;
        self.busy = false;
        self.clear();
        self.send(question, cx);
    }

    fn clear(&mut self) {
        self.conv = Default::default();
        self.entries.clear();
        self.open.clear();
        self.open_groups.clear();
    }

    /// Tool calls in a row: "Used 3 tools" and their names, unfolded while the answer is
    /// being written or when clicked.
    fn render_tools(&self, ei: usize, start: usize, steps: &[Step], live: bool, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let group = (ei, start);
        let unfolded = live || self.open_groups.contains(&group);
        let mut names: Vec<String> = Vec::new();
        for s in steps {
            if let Step::Running { name, .. } | Step::Tool { name, .. } = s
                && !names.contains(&tool_title(name))
            {
                names.push(tool_title(name));
            }
        }
        let used = match steps.len() {
            1 => "Used 1 tool".to_string(),
            n => format!("Used {n} tools"),
        };
        let summary = h_flex()
            .id(SharedString::from(format!("tools-{ei}-{start}")))
            .gap_2()
            .text_xs()
            .text_color(theme.muted_foreground)
            .cursor_pointer()
            .child(Icon::new(if unfolded { Lucide::ChevronDown } else { Lucide::ChevronRight }).size_3())
            .child(div().flex_none().child(used))
            .when(!unfolded, |d| d.child(div().flex_1().min_w_0().truncate().child(names.join(", "))))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.open_groups.remove(&group) {
                    this.open_groups.insert(group);
                }
                cx.notify();
            }));
        let rows: Vec<AnyElement> = if unfolded {
            steps.iter().enumerate().map(|(i, s)| self.render_tool(ei, start + i, s, cx)).collect()
        } else {
            Vec::new()
        };
        v_flex()
            .gap_1()
            .child(summary)
            .when(unfolded, |d| {
                d.child(v_flex().ml(px(5.)).pl_3().gap_1().border_l_1().border_color(theme.border).children(rows))
            })
            .into_any_element()
    }

    /// One tool call: what it looked at, in words; its result when clicked.
    fn render_tool(&self, ei: usize, si: usize, step: &Step, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let (name, args, result) = match step {
            Step::Running { name, args } => (name, args, None),
            Step::Tool { name, args, result } => (name, args, Some(result)),
            Step::Text(_) => return div().into_any_element(),
        };
        let key = (ei, si);
        let open = result.is_some() && self.open.contains(&key);
        let row = h_flex()
            .id(SharedString::from(format!("tool-{ei}-{si}")))
            .gap_2()
            .text_xs()
            .child(match result {
                None => div().text_color(theme.blue).child(Icon::new(Lucide::LoaderCircle).size_3()),
                Some(_) => div()
                    .text_color(theme.muted_foreground)
                    .child(Icon::new(if open { Lucide::ChevronDown } else { Lucide::ChevronRight }).size_3()),
            })
            .child(div().flex_none().child(tool_title(name)))
            .child(div().flex_1().min_w_0().truncate().text_color(theme.muted_foreground).child(tool_detail(args)))
            .when(result.is_some(), |d| {
                d.cursor_pointer().on_click(cx.listener(move |this, _, _, cx| {
                    if !this.open.remove(&key) {
                        this.open.insert(key);
                    }
                    cx.notify();
                }))
            });
        v_flex()
            .child(row)
            .when_some(result.filter(|_| open), |d, result| {
                d.child(
                    div()
                        .ml_5()
                        .mt_1()
                        .p_2()
                        .rounded_md()
                        .bg(theme.secondary)
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .children(result.lines().take(60).map(|l| div().whitespace_nowrap().child(l.to_string()))),
                )
            })
            .into_any_element()
    }

    pub fn send(&mut self, question: String, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.started = Some(std::time::Instant::now());
        // Tick the elapsed time while waiting.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                let busy = this.update(cx, |this, cx| {
                    cx.notify();
                    this.busy
                });
                if !matches!(busy, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
        self.entries.push(Entry::User(question.clone()));
        self.entries.push(Entry::Assistant(Vec::new()));
        let (hub, conv, scope) = (self.hub.clone(), self.conv.clone(), self.scope.clone());
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<Step>();
        self._task = Some(cx.spawn(async move |this, cx| {
            let ask = assistant::ask(hub, conv, question, scope, move |s| {
                let _ = tx.unbounded_send(s);
            });
            let steps = async {
                while let Some(step) = rx.next().await {
                    let _ = this.update(cx, |this, cx| {
                        if let Some(Entry::Assistant(steps)) = this.entries.last_mut() {
                            // A finished tool replaces its "running" line.
                            if let Step::Tool { name, args, .. } = &step
                                && let Some(i) = steps.iter().rposition(
                                    |s| matches!(s, Step::Running { name: n, args: a } if n == name && a == args),
                                )
                            {
                                steps[i] = step;
                            } else {
                                steps.push(step);
                            }
                        }
                        this.scroll.scroll_to_bottom();
                        cx.notify();
                    });
                }
            };
            let (result, ()) = futures::join!(ask, steps);
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(conv) => this.conv = conv,
                    Err(e) => this.entries.push(Entry::Error(format!("{e:#}"))),
                }
                this.scroll.scroll_to_bottom();
                cx.notify();
            });
        }));
        cx.notify();
    }

    fn render_setup(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        v_flex()
            .max_w(px(680.))
            .p_8()
            .gap_4()
            .child(h_flex().gap_3().child(Icon::new(Lucide::Sparkles).size_8().text_color(theme.blue)).child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("Ask your clusters")))
            .child(div().text_color(muted).child(
                "The assistant is Claude, working through Recouvrance's own tools as the agent \"Claude\": read-only and never Secrets \
                 to start with, and every call is checked against its rules and shown in Agents → Activity. Change its rules there to allow more.",
            ))
            .child(div().text_sm().child(
                "No API key needed: install Claude Code (claude.com/claude-code) and log in once with `claude`; \
                 Recouvrance then uses your subscription. It is detected automatically.",
            ))
            .child(div().text_sm().text_color(theme.warning).child(
                "Like any Claude chat, what the assistant reads from your clusters (resource lists, events, logs) goes to Anthropic.",
            ))
            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Or use an Anthropic API key (optional)"))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.key_input)))
                    .child(Button::new("save-key").primary().label("Save").on_click(cx.listener(|this, _, _, cx| {
                        let key = this.key_input.read(cx).value().to_string();
                        this.setup_error =
                            assistant::save_api_key(&key).err().map(|e| format!("Could not save the key: {e}"));
                        cx.notify();
                    }))),
            )
            .children(self.setup_error.clone().map(|e| div().text_sm().text_color(theme.danger).child(e)))
            .child(div().text_xs().text_color(muted).child("Stored in ~/.config/recouvrance/anthropic-key (readable only by you). ANTHROPIC_API_KEY works too."))
            .into_any_element()
    }
}

impl Render for AssistantView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(backend) = assistant::backend() else {
            return self.render_setup(cx);
        };
        let via = match &backend {
            assistant::Backend::ClaudeCode(_) => "via your Claude Code".to_string(),
            assistant::Backend::Api => format!("via API · {}", crate::settings::get().assistant_model),
        };
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let mut rows: Vec<AnyElement> = Vec::new();
        for (ei, e) in self.entries.iter().enumerate() {
            match e {
                Entry::User(q) => rows.push(
                    h_flex()
                        .justify_end()
                        .child(
                            div()
                                .max_w(px(720.))
                                .px_3()
                                .py_2()
                                .rounded_lg()
                                .bg(theme.blue.opacity(0.12))
                                .text_sm()
                                .child(q.clone()),
                        )
                        .into_any_element(),
                ),
                Entry::Error(m) => {
                    rows.push(div().text_sm().text_color(theme.danger).child(m.clone()).into_any_element());
                }
                Entry::Assistant(steps) => {
                    let mut col = v_flex().gap_3();
                    let live = self.busy && ei + 1 == self.entries.len();
                    let mut si = 0;
                    while si < steps.len() {
                        if let Step::Text(t) = &steps[si] {
                            col = col.child(
                                div().text_sm().child(
                                    component::text::TextView::markdown(
                                        SharedString::from(format!("md-{ei}-{si}")),
                                        t.clone(),
                                    )
                                    .style(chat_text_style(&theme))
                                    .selectable(true)
                                    // Model output is untrusted: only web links open.
                                    .on_link_click(|url, _, _, cx| {
                                        if is_web_link(url) {
                                            cx.open_url(url);
                                        }
                                    }),
                                ),
                            );
                            si += 1;
                            continue;
                        }
                        // Tool calls in a row make one block, folded to a line once the
                        // answer goes on.
                        let start = si;
                        while si < steps.len() && !matches!(steps[si], Step::Text(_)) {
                            si += 1;
                        }
                        col = col.child(self.render_tools(ei, start, &steps[start..si], live && si == steps.len(), cx));
                    }
                    if self.busy && ei + 1 == self.entries.len() {
                        let secs = self.started.map_or(0, |t| t.elapsed().as_secs());
                        col = col.child(div().text_sm().text_color(muted).child(format!("Thinking… {secs}s")));
                    }
                    rows.push(col.into_any_element());
                }
            }
        }
        v_flex()
            .size_full()
            .child(
                h_flex()
                    .gap_3()
                    .px_4()
                    .py_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(Icon::new(Lucide::Sparkles).size_5().text_color(theme.blue))
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child("Assistant"))
                    .child(div().text_sm().text_color(muted).child(format!(
                        "{} · as agent \"{}\" · {via}",
                        self.scope,
                        assistant::AGENT_NAME
                    )))
                    .child(div().flex_1())
                    .child(
                        Button::new("new-chat")
                            .small()
                            .ghost()
                            .icon(Lucide::Plus)
                            .label("New chat")
                            .disabled(self.busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.clear();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div().flex_1().min_h_0().child(
                    v_flex()
                        .id("chat")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll)
                        .p_4()
                        .items_center()
                        .child(
                            // One centered column: questions on its right, answers on its left.
                            v_flex()
                                .w_full()
                                .max_w(px(CHAT_WIDTH))
                                .gap_4()
                                .when(self.entries.is_empty(), |d| {
                                    d.child(div().text_sm().text_color(muted).child("Try:")).children(
                                        SUGGESTIONS.iter().enumerate().map(|(i, s)| {
                                            let q = s.to_string();
                                            Button::new(("suggest", i))
                                                .outline()
                                                .label(q.clone())
                                                .on_click(cx.listener(move |this, _, _, cx| this.send(q.clone(), cx)))
                                        }),
                                    )
                                })
                                .children(rows),
                        ),
                ),
            )
            .child(
                h_flex().justify_center().p_3().border_t_1().border_color(theme.border).child(
                    h_flex()
                        .w_full()
                        .max_w(px(CHAT_WIDTH))
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&self.input).disabled(self.busy)))
                        .when(self.busy, |d| {
                            d.child(Button::new("stop").small().outline().icon(Lucide::Square).label("Stop").on_click(
                                cx.listener(|this, _, _, cx| {
                                    // Dropping the task stops the question, and Claude Code with it.
                                    this._task = None;
                                    this.busy = false;
                                    this.entries.push(Entry::Error("Stopped.".into()));
                                    cx.notify();
                                }),
                            ))
                        }),
                ),
            )
            .into_any_element()
    }
}

/// How wide the conversation gets: lines stay readable on a wide window.
const CHAT_WIDTH: f32 = 920.;

/// Answers' Markdown: code blocks in the same small size as the text around them (the theme
/// draws them larger, which made a reply's commands the biggest thing on screen), inline code
/// on a tint that shows, and tables on the page's color.
fn chat_text_style(theme: &component::Theme) -> component::text::TextViewStyle {
    component::text::TextViewStyle::default()
        .code_block(StyleRefinement::default().text_xs())
        .inline_code(HighlightStyle { background_color: Some(theme.border), ..Default::default() })
        .table(StyleRefinement::default().bg(theme.background))
        .table_cell(StyleRefinement::default().bg(theme.background))
}

/// "describe_resource" → "Describe resource".
fn tool_title(name: &str) -> String {
    let words = name.replace('_', " ");
    let mut chars = words.chars();
    chars.next().map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

/// A tool's arguments as what it looked at: "pod · web-7f9c · shop · kind-dev", the object
/// first and the context last; other arguments as "key: value". Not JSON: as is.
fn tool_detail(args: &str) -> String {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(args) else {
        return args.to_string();
    };
    fn scalar(v: &serde_json::Value) -> Option<String> {
        match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            serde_json::Value::Array(a) => {
                let items: Vec<String> = a.iter().filter_map(scalar).collect();
                (!items.is_empty()).then(|| items.join(", "))
            }
            _ => None,
        }
    }
    const OBJECT: [&str; 4] = ["kind", "object_kind", "name", "namespace"];
    const LAST: [&str; 2] = ["context", "contexts"];
    let mut parts: Vec<String> = OBJECT.iter().filter_map(|k| map.get(*k).and_then(scalar)).collect();
    parts.extend(
        map.iter()
            .filter(|(k, _)| !OBJECT.contains(&k.as_str()) && !LAST.contains(&k.as_str()))
            .filter_map(|(k, v)| scalar(v).map(|v| format!("{k}: {v}"))),
    );
    parts.extend(LAST.iter().filter_map(|k| map.get(*k).and_then(scalar)));
    parts.retain(|p| !p.is_empty());
    parts.join(" · ")
}

/// http(s) only: no file://, custom schemes or anything else the desktop would launch.
fn is_web_link(url: &str) -> bool {
    let url = url.trim_start().to_ascii_lowercase();
    url.starts_with("https://") || url.starts_with("http://")
}

#[cfg(test)]
mod tests {
    #[test]
    fn tool_calls_read_as_words() {
        assert_eq!(super::tool_title("describe_resource"), "Describe resource");
        assert_eq!(super::tool_title(""), "");
        assert_eq!(
            super::tool_detail(
                r#"{"context":"kind-dev","namespace":"shop","name":"web-7f9c","kind":"pod","previous":true,"tail":50}"#
            ),
            "pod · web-7f9c · shop · previous: true · tail: 50 · kind-dev"
        );
        assert_eq!(super::tool_detail(r#"{"contexts":["a","b"],"query":"web"}"#), "query: web · a, b");
        assert_eq!(super::tool_detail("not json"), "not json");
        assert_eq!(super::tool_detail("{}"), "");
    }

    #[test]
    fn only_web_links_open() {
        assert!(super::is_web_link("https://kubernetes.io/docs"));
        assert!(super::is_web_link("HTTP://example.com"));
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "ssh://host", "vscode://x", "/tmp/x", ""] {
            assert!(!super::is_web_link(bad), "{bad}");
        }
    }
}
