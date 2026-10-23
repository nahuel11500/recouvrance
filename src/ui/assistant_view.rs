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
        Self {
            hub,
            scope,
            conv: Default::default(),
            entries: Vec::new(),
            input,
            key_input,
            setup_error: None,
            busy: false,
            started: None,
            open: Default::default(),
            scroll: ScrollHandle::new(),
            _task: None,
            _subs: subs,
        }
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
            let (result, _) = futures::join!(ask, steps);
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
                    rows.push(div().text_sm().text_color(theme.danger).child(m.clone()).into_any_element())
                }
                Entry::Assistant(steps) => {
                    let mut col = v_flex().gap_2().max_w(px(900.));
                    for (si, s) in steps.iter().enumerate() {
                        col = col.child(match s {
                            Step::Running { name, args } => h_flex()
                                .gap_2()
                                .text_xs()
                                .text_color(theme.blue)
                                .child(Icon::new(Lucide::LoaderCircle).size_3())
                                .child(
                                    div()
                                        .font_family(theme.mono_font_family.clone())
                                        .child(format!("{name} {}", truncate(args, 120))),
                                )
                                .child(div().text_color(muted).child("running…"))
                                .into_any_element(),
                            Step::Text(t) => div()
                                .text_sm()
                                .child(
                                    component::text::TextView::markdown(
                                        SharedString::from(format!("md-{ei}-{si}")),
                                        t.clone(),
                                    )
                                    .selectable(true)
                                    // Model output is untrusted: only web links open.
                                    .on_link_click(|url, _, _, cx| {
                                        if is_web_link(url) {
                                            cx.open_url(url);
                                        }
                                    }),
                                )
                                .into_any_element(),
                            Step::Tool { name, args, result } => {
                                let key = (ei, si);
                                let open = self.open.contains(&key);
                                v_flex()
                                    .child(
                                        h_flex()
                                            .id(SharedString::from(format!("tool-{ei}-{si}")))
                                            .gap_2()
                                            .cursor_pointer()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(
                                                Icon::new(if open {
                                                    Lucide::ChevronDown
                                                } else {
                                                    Lucide::ChevronRight
                                                })
                                                .size_3(),
                                            )
                                            .child(Icon::new(Lucide::Wrench).size_3())
                                            .child(
                                                div()
                                                    .font_family(theme.mono_font_family.clone())
                                                    .child(format!("{name} {}", truncate(args, 120))),
                                            )
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if !this.open.remove(&key) {
                                                    this.open.insert(key);
                                                }
                                                cx.notify();
                                            })),
                                    )
                                    .when(open, |d| {
                                        d.child(
                                            div()
                                                .ml_5()
                                                .p_2()
                                                .rounded_md()
                                                .bg(theme.secondary)
                                                .font_family(theme.mono_font_family.clone())
                                                .text_xs()
                                                .children(
                                                    result
                                                        .lines()
                                                        .take(60)
                                                        .map(|l| div().whitespace_nowrap().child(l.to_string())),
                                                ),
                                        )
                                    })
                                    .into_any_element()
                            }
                        });
                    }
                    if self.busy && ei + 1 == self.entries.len() {
                        let secs = self.started.map(|t| t.elapsed().as_secs()).unwrap_or(0);
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
                                this.conv = Default::default();
                                this.entries.clear();
                                this.open.clear();
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
            )
            .child(
                h_flex()
                    .gap_2()
                    .p_3()
                    .border_t_1()
                    .border_color(theme.border)
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
            )
            .into_any_element()
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s.to_string() }
}

/// http(s) only: no file://, custom schemes or anything else the desktop would launch.
fn is_web_link(url: &str) -> bool {
    let url = url.trim_start().to_ascii_lowercase();
    url.starts_with("https://") || url.starts_with("http://")
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_web_links_open() {
        assert!(super::is_web_link("https://kubernetes.io/docs"));
        assert!(super::is_web_link("HTTP://example.com"));
        for bad in ["file:///etc/passwd", "javascript:alert(1)", "ssh://host", "vscode://x", "/tmp/x", ""] {
            assert!(!super::is_web_link(bad), "{bad}");
        }
    }
}
