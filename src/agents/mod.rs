//! Agents screen: who can reach the clusters through MCP, with precise
//! RBAC-like rules (effect × verbs × resources × namespaces × contexts × names),
//! a tester for those rules, and what each agent did.

use std::sync::Arc;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor as CodeEditor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Icon, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::k8s;
use crate::mcp::{Access, Agent, AuditEntry, Decision, Hub, Mode, Policy, Rule, VERBS, glob, is_write};

mod activity;
mod code;
mod detail;
mod rules;
#[allow(unused_imports)]
pub(crate) use activity::*;
#[allow(unused_imports)]
pub(crate) use code::*;
#[allow(unused_imports)]
pub(crate) use detail::*;
#[allow(unused_imports)]
pub(crate) use rules::*;

pub struct AgentsView {
    /// Show every agent's activity instead of one agent's settings.
    all_activity: bool,
    activity: Activity,
    hub: Arc<Hub>,
    contexts: Vec<String>,
    risky: Vec<String>,
    selected: Option<String>,
    /// Token shown once after creating or rotating, per agent id.
    fresh_token: Option<(String, String)>,
    /// Show several agents (a list on the left); otherwise the one main agent.
    advanced: bool,
    /// Whether your Claude Code has Recouvrance's MCP server registered (None: checking).
    claude_connected: Option<bool>,
    /// The rules.yaml editor ("Rules as code"), and the text it was loaded with.
    code: Option<(Entity<EditorState>, String)>,
    code_message: Option<(bool, String)>,
    new_name: Entity<InputState>,
    editor: Option<Editor>,
    tester: Tester,
    pub server_status: Result<u16, String>,
    error: Option<SharedString>,
    _subs: Vec<Subscription>,
}

/// Text inputs for one rule's pattern lists.
struct RuleInputs {
    note: Entity<InputState>,
    contexts: Entity<InputState>,
    resources: Entity<InputState>,
    namespaces: Entity<InputState>,
    names: Entity<InputState>,
}

/// Inputs for the selected agent.
struct Editor {
    agent_id: String,
    rules: Vec<RuleInputs>,
    /// The rules the inputs show, to rebuild them when the rules change elsewhere.
    shown: Vec<Rule>,
    max_lines: Entity<InputState>,
    max_rows: Entity<InputState>,
    exec_timeout: Entity<InputState>,
    rate: Entity<InputState>,
    _subs: Vec<Subscription>,
}

/// "Would this request be allowed?"
struct Tester {
    context: Entity<InputState>,
    resource: Entity<InputState>,
    namespace: Entity<InputState>,
    name: Entity<InputState>,
    verb: String,
}

#[derive(Clone, Copy)]
enum Field {
    Note,
    Contexts,
    Resources,
    Namespaces,
    Names,
}

fn split_list(text: &str) -> Vec<String> {
    text.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

fn rule_with(
    effect: Mode,
    verbs: &[&str],
    resources: &[&str],
    namespaces: &[&str],
    note: &str,
    contexts: &[String],
) -> Rule {
    let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect();
    Rule {
        effect,
        verbs: v(verbs),
        resources: v(resources),
        namespaces: v(namespaces),
        contexts: contexts.to_vec(),
        names: vec!["*".into()],
        note: note.into(),
        expires: None,
        approved: false,
    }
}

impl AgentsView {
    pub fn new(hub: Arc<Hub>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let server_status = hub.server_status().unwrap_or_else(|| Err("starting…".into()));
        let (contexts, risky) = k8s::load_contexts().map(|c| (c.names, c.risky)).unwrap_or_default();
        let new_name = cx.new(|cx| InputState::new(window, cx).placeholder("Agent name, e.g. claude-code"));
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let tester = Tester {
            context: input("context", window, cx),
            resource: input("resource, e.g. deployments", window, cx),
            namespace: input("namespace (empty = cluster)", window, cx),
            name: input("name", window, cx),
            verb: "get".into(),
        };
        let mut subs = vec![cx.subscribe_in(&new_name, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.create_agent(window, cx);
            }
        })];
        for i in [&tester.context, &tester.resource, &tester.namespace, &tester.name] {
            subs.push(cx.subscribe(i, |_, _, ev: &InputEvent, cx| {
                if let InputEvent::Change = ev {
                    cx.notify();
                }
            }));
        }
        // One connection by default: the main agent, shared by Claude Code and the assistant.
        let advanced = hub.config().agents.len() > 1;
        let selected = if advanced {
            hub.config().agents.first().map(|a| a.id.clone())
        } else {
            crate::assistant::main_agent(&hub).ok().map(|(id, _)| id)
        };
        let activity = Activity::new(window, cx);
        subs.push(cx.subscribe(&activity.search, |_, _, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                cx.notify();
            }
        }));
        let mut this = Self {
            activity,
            advanced,
            claude_connected: None,
            code: None,
            code_message: None,
            all_activity: false,
            hub,
            contexts,
            risky,
            selected: None,
            fresh_token: None,
            new_name,
            editor: None,
            tester,
            server_status,
            error: None,
            _subs: subs,
        };
        if let Some(id) = selected {
            this.select(id, window, cx);
        }
        this.check_claude(cx);
        #[cfg(feature = "dev")]
        if std::env::var("RECOUVRANCE_ALL_ACTIVITY").is_ok() {
            this.all_activity = true;
        }
        this
    }

    fn report<T>(&mut self, result: Result<T>) -> Option<T> {
        match result {
            Ok(v) => Some(v),
            Err(e) => {
                self.error = Some(format!("{e:#}").into());
                None
            }
        }
    }

    fn create_agent(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.new_name.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }
        if let Some((agent, token)) = self.report(self.hub.create_agent(name)) {
            self.new_name.update(cx, |s, cx| s.set_value("", window, cx));
            self.fresh_token = Some((agent.id.clone(), token));
            self.select(agent.id, window, cx);
        }
        cx.notify();
    }

    fn select(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(agent) = self.hub.agent(&id) else { return };
        self.all_activity = false;
        self.selected = Some(id.clone());
        self.rebuild_editor(&agent, window, cx);
        cx.notify();
    }

    /// Recreate inputs from the stored policy (after select, add or remove rule).
    fn rebuild_editor(&mut self, agent: &Agent, window: &mut Window, cx: &mut Context<Self>) {
        let p = &agent.policy;
        let field = |value: String, placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder(placeholder);
                s.set_value(value, window, cx);
                s
            })
        };
        let mut subs = Vec::new();
        let mut rules = Vec::new();
        for (ix, r) in p.rules.iter().enumerate() {
            let inputs = RuleInputs {
                note: field(r.note.clone(), "What is this rule for?", window, cx),
                contexts: field(r.contexts.join(", "), "contexts, e.g. eu-*, staging", window, cx),
                resources: field(r.resources.join(", "), "pods, deployments, *.cert-manager.io, *", window, cx),
                namespaces: field(r.namespaces.join(", "), "team-a, team-*, * (also cluster-scoped)", window, cx),
                names: field(r.names.join(", "), "object names, e.g. web-*, *", window, cx),
            };
            for (input, f) in [
                (&inputs.note, Field::Note),
                (&inputs.contexts, Field::Contexts),
                (&inputs.resources, Field::Resources),
                (&inputs.namespaces, Field::Namespaces),
                (&inputs.names, Field::Names),
            ] {
                subs.push(cx.subscribe_in(input, window, move |this, state, ev: &InputEvent, _, cx| {
                    if let InputEvent::Change = ev {
                        let text = state.read(cx).value().to_string();
                        this.update_rule(
                            ix,
                            |r| match f {
                                Field::Note => r.note = text,
                                Field::Contexts => r.contexts = split_list(&text),
                                Field::Resources => r.resources = split_list(&text),
                                Field::Namespaces => r.namespaces = split_list(&text),
                                Field::Names => r.names = split_list(&text),
                            },
                            cx,
                        );
                    }
                }));
            }
            rules.push(inputs);
        }
        let max_lines = field(p.max_log_lines.to_string(), "500", window, cx);
        let max_rows = field(p.max_rows.to_string(), "500", window, cx);
        let exec_timeout = field(p.exec_timeout.to_string(), "30", window, cx);
        let rate = field(p.max_calls_per_minute.to_string(), "120", window, cx);
        let limit = |input: &Entity<InputState>,
                     apply: fn(&mut Policy, u32),
                     subs: &mut Vec<Subscription>,
                     window: &mut Window,
                     cx: &mut Context<Self>| {
            subs.push(cx.subscribe_in(input, window, move |this, state, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev
                    && let Ok(n) = state.read(cx).value().trim().parse()
                {
                    this.update(|a| apply(&mut a.policy, n), cx);
                }
            }));
        };
        limit(&max_lines, |p, n| p.max_log_lines = n, &mut subs, window, cx);
        limit(&max_rows, |p, n| p.max_rows = n, &mut subs, window, cx);
        limit(&exec_timeout, |p, n| p.exec_timeout = n, &mut subs, window, cx);
        limit(&rate, |p, n| p.max_calls_per_minute = n, &mut subs, window, cx);
        self.editor = Some(Editor {
            agent_id: agent.id.clone(),
            rules,
            shown: p.rules.clone(),
            max_lines,
            max_rows,
            exec_timeout,
            rate,
            _subs: subs,
        });
    }

    fn update(&mut self, f: impl FnOnce(&mut Agent), cx: &mut Context<Self>) {
        if let Some(id) = self.selected.clone() {
            let result = self.hub.update_agent(&id, f);
            self.report(result);
            // The inputs already show this edit: no rebuild for it.
            if let (Some(e), Some(a)) = (&mut self.editor, self.hub.agent(&id)) {
                e.shown = a.policy.rules;
            }
        }
        cx.notify();
    }

    fn update_rule(&mut self, ix: usize, f: impl FnOnce(&mut Rule), cx: &mut Context<Self>) {
        self.update(
            |a| {
                if let Some(r) = a.policy.rules.get_mut(ix) {
                    let before = r.clone();
                    f(r);
                    // An approval covers one exact request: once edited it is an ordinary rule.
                    if !r.same_request(&before) {
                        r.approved = false;
                    }
                }
            },
            cx,
        );
    }

    /// Structural change: save, then rebuild the inputs.
    fn edit_rules(&mut self, f: impl FnOnce(&mut Vec<Rule>), window: &mut Window, cx: &mut Context<Self>) {
        self.update(|a| f(&mut a.policy.rules), cx);
        if let Some(agent) = self.selected.as_ref().and_then(|id| self.hub.agent(id)) {
            self.rebuild_editor(&agent, window, cx);
        }
    }

    /// Contexts in kubeconfig matched by a rule's patterns.
    fn matched_contexts(&self, patterns: &[String]) -> Vec<String> {
        self.contexts.iter().filter(|c| patterns.iter().any(|p| glob(p, c))).cloned().collect()
    }

    // MARK: Rendering

    fn check_claude(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let ok = crate::assistant::claude_code_connected().await;
            let _ = this.update(cx, |this, cx| {
                this.claude_connected = Some(ok);
                cx.notify();
            });
        })
        .detach();
    }

    fn connect_claude(&mut self, cx: &mut Context<Self>) {
        let hub = self.hub.clone();
        self.claude_connected = None;
        cx.spawn(async move |this, cx| {
            let r = crate::assistant::connect_claude_code(hub).await;
            let _ = this.update(cx, |this, cx| {
                match r {
                    Ok(_) => this.claude_connected = Some(true),
                    Err(e) => {
                        this.claude_connected = Some(false);
                        this.error = Some(format!("{e:#}").into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let enabled = self.hub.agents_enabled();
        let (status_color, status_text) = match &self.server_status {
            Ok(port) => (theme.success, format!("MCP server on http://127.0.0.1:{port}/mcp")),
            Err(e) => (theme.danger, format!("MCP server: {e}")),
        };
        let hub = self.hub.clone();
        h_flex()
            .px_4()
            .py_3()
            .gap_3()
            .border_b_1()
            .border_color(theme.border)
            .child(div().text_lg().font_weight(FontWeight::BOLD).child("Agents"))
            .child(
                h_flex()
                    .gap_1p5()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child(div().size_2().rounded_full().bg(status_color))
                    .child(status_text),
            )
            .child(div().flex_1())
            .child({
                let installed = crate::assistant::claude_code().is_some();
                match self.claude_connected {
                    _ if !installed => div().text_sm().text_color(theme.muted_foreground).child("Claude Code not found").into_any_element(),
                    None => div().text_sm().text_color(theme.muted_foreground).child("Checking Claude Code…").into_any_element(),
                    Some(true) => h_flex()
                        .gap_1()
                        .child(Icon::new(Lucide::CircleCheck).size_4().text_color(theme.success))
                        .child(div().text_sm().child("Connected to Claude Code"))
                        .child(Button::new("cc-reconnect").xsmall().ghost().label("Reconnect").tooltip("Register Recouvrance's MCP server in Claude Code again (new token)").on_click(cx.listener(|this, _, _, cx| this.connect_claude(cx))))
                        .into_any_element(),
                    Some(false) => Button::new("cc-connect")
                        .small()
                        .primary()
                        .icon(Lucide::Plug)
                        .label("Connect Claude Code")
                        .tooltip("Runs `claude mcp add --scope user recouvrance …` for you")
                        .on_click(cx.listener(|this, _, _, cx| this.connect_claude(cx)))
                        .into_any_element(),
                }
            })
            .child(
                Button::new("toggle-code")
                    .xsmall()
                    .when(self.code.is_some(), |b| b.primary())
                    .when(self.code.is_none(), |b| b.ghost())
                    .icon(Lucide::FileCode)
                    .label("Rules as code")
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_code(window, cx))),
            )
            .child(
                Button::new("toggle-advanced")
                    .xsmall()
                    .ghost()
                    .label(if self.advanced { "One agent" } else { "Several agents…" })
                    .tooltip("Most people need one connection. Several agents give separate tokens and rules to different tools.")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.advanced = !this.advanced;
                        cx.notify();
                    })),
            )
            .child(
                Switch::new("kill-switch")
                    .checked(enabled)
                    .label(if enabled { "Agent access on" } else { "Agent access OFF" })
                    .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                        let result = hub.set_agents_enabled(*checked);
                        this.report(result);
                        cx.notify();
                    })),
            )
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, active, fg, success) = (theme.muted_foreground, theme.list_active, theme.foreground, theme.success);
        let agents = self.hub.config().agents;
        v_flex()
            .w(px(250.))
            .h_full()
            .border_r_1()
            .border_color(theme.border)
            .child(
                v_flex().p_3().gap_2().child(Input::new(&self.new_name).small()).child(
                    Button::new("create")
                        .small()
                        .primary()
                        .icon(Lucide::Plus)
                        .label("New agent")
                        .on_click(cx.listener(|this, _, window, cx| this.create_agent(window, cx))),
                ),
            )
            .child(
                h_flex()
                    .id("all-activity")
                    .mx_2()
                    .mb_1()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .when(self.all_activity, |d| d.bg(active))
                    .hover(|s| s.bg(active.opacity(0.6)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.all_activity = true;
                        cx.notify();
                    }))
                    .child(Icon::new(Lucide::Activity).size_4())
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("All activity")),
            )
            .child(
                v_flex()
                    .id("agent-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .gap_0p5()
                    .when(agents.is_empty(), |d| {
                        d.child(div().p_2().text_sm().text_color(muted).child("No agents yet."))
                    })
                    .children(agents.into_iter().map(|a| {
                        let is_selected = !self.all_activity && self.selected.as_deref() == Some(a.id.as_str());
                        let seen = self.hub.last_seen(&a.id);
                        let online = seen.is_some_and(|t| jiff::Timestamp::now().as_second() - t.as_second() < 300);
                        let writes = a
                            .policy
                            .rules
                            .iter()
                            .any(|r| r.effect != Mode::Deny && r.verbs.iter().any(|v| v == "*" || is_write(v)));
                        let id = a.id.clone();
                        h_flex()
                            .id(SharedString::from(format!("agent-{}", a.id)))
                            .gap_2()
                            .px_2()
                            .py_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .when(is_selected, |d| d.bg(active))
                            .hover(|s| s.bg(active.opacity(0.6)))
                            .on_click(cx.listener(move |this, _, window, cx| this.select(id.clone(), window, cx)))
                            .child(Icon::new(Lucide::Bot).size_4().text_color(if a.enabled { fg } else { muted }))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(a.name.clone()))
                                    .child(div().text_xs().text_color(muted).child(format!(
                                        "{} rules{} · {}",
                                        a.policy.rules.len(),
                                        if writes { " · can write" } else { "" },
                                        match seen {
                                            Some(t) => format!("seen {} ago", k8s::age(t)),
                                            None => "never connected".into(),
                                        }
                                    ))),
                            )
                            .when(online, |d| d.child(div().size_2().rounded_full().bg(success)))
                    })),
            )
    }
}

impl Render for AgentsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Changes saved to rules.yaml elsewhere show up while the editor is untouched.
        self.refresh_code(window, cx);
        let agent = self.selected.as_ref().and_then(|id| self.hub.agent(id));
        // Rules can change elsewhere (e.g. "Always allow this", rules.yaml); keep inputs in step.
        if let Some(a) = &agent
            && self.editor.as_ref().is_some_and(|e| e.agent_id != a.id || e.shown != a.policy.rules)
        {
            self.rebuild_editor(a, window, cx);
        }
        let muted = cx.theme().muted_foreground;
        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .child(self.render_header(cx))
            .when_some(self.error.clone(), |d, e| d.child(div().px_4().py_1().text_sm().text_color(cx.theme().danger).child(e)))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .items_start()
                    .when(self.advanced, |d| d.child(self.render_list(cx)))
                    .child(match agent {
                        _ if self.code.is_some() => self.render_code(cx),
                        _ if self.all_activity => self.render_activity_page(cx),
                        Some(a) => self.render_detail(&a, cx).into_any_element(),
                        None => {
                            let theme = cx.theme();
                            let step = |n: &str, title: &str, text: &str| {
                                h_flex()
                                    .gap_3()
                                    .items_start()
                                    .child(div().size(px(26.)).flex_shrink_0().rounded_full().bg(theme.blue.opacity(0.12)).text_color(theme.blue).flex().items_center().justify_center().text_sm().font_weight(FontWeight::BOLD).child(n.to_string()))
                                    .child(v_flex().gap_0p5().child(div().font_weight(FontWeight::MEDIUM).child(title.to_string())).child(div().text_sm().text_color(muted).child(text.to_string())))
                            };
                            v_flex()
                                .flex_1()
                                .p_8()
                                .gap_5()
                                .max_w(px(720.))
                                .child(h_flex().gap_3().child(Icon::new(Lucide::Bot).size_8().text_color(theme.blue)).child(div().text_xl().font_weight(FontWeight::SEMIBOLD).child("Let AI agents work on your clusters, on your terms")))
                                .child(div().text_color(muted).child(
                                    "Recouvrance serves MCP locally. Each agent gets its own token and kubectl-grained rules: which verbs, resources, namespaces, contexts and names it may touch, \
                                     what needs your approval, and what is never allowed. Every call is logged.",
                                ))
                                .child(step("1", "Create an agent", "Name it on the left (e.g. claude-code). It starts read-only on the contexts you pick, never Secrets."))
                                .child(step("2", "Connect your tool", "Copy the `claude mcp add …` command shown once with its token (or the URL and token for any MCP client)."))
                                .child(step("3", "Tune its rules", "Allow, ask me, or deny per verb, resource, namespace, context and name. Approvals pop up here, with \"allow for 1h\" or \"always\"."))
                                .child(step("4", "Watch what it does", "Activity shows every call, its decision and the rule that decided, grouped by task."))
                                .into_any_element()
                        }
                    }),
            )
    }
}
