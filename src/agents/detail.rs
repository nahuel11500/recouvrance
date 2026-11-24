//! An agent's page: token, connection, rules, limits.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Icon, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::AgentsView;
use super::clients::{self, Client};
use crate::mcp::{Agent, Policy, Rule};

impl AgentsView {
    /// The token a copied config carries: the one just made, else the main agent's saved
    /// one. Other agents' tokens are kept only hashed.
    fn connect_token(&mut self, agent_id: &str) -> Option<String> {
        if let Some((id, token)) = &self.fresh_token
            && id == agent_id
        {
            return Some(token.clone());
        }
        let token = crate::assistant::main_token(&self.hub, agent_id);
        if token.is_none() {
            self.error = Some(
                "Recouvrance has no copy of this agent's token: New token makes one (the old one stops working)."
                    .into(),
            );
        }
        token
    }

    /// Put a client's config or command (`Some((client, index))`), or the token alone, on
    /// the clipboard. That is the only place the token goes: the page shows it masked.
    fn copy_connect(&mut self, agent_id: &str, snippet: Option<(Client, usize)>, cx: &mut Context<Self>) {
        let Some(token) = self.connect_token(agent_id) else {
            cx.notify();
            return;
        };
        let text = if let Some((client, ix)) = snippet {
            let url = clients::server_url(self.hub.port());
            let Some(snippet) = client.snippets(&url, &token).into_iter().nth(ix) else { return };
            snippet.text
        } else {
            token
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    /// How to connect a client as this agent: pick it, then copy its config or command.
    fn render_connect(&self, agent: &Agent, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (blue, secondary, muted) = (theme.blue, theme.secondary, theme.muted_foreground);
        let mono = theme.mono_font_family.clone();
        let fresh = self.fresh_token.as_ref().is_some_and(|(id, _)| *id == agent.id);
        let main = crate::assistant::is_main_agent(&self.hub, &agent.id);
        if !fresh && !main {
            return div()
                .text_sm()
                .text_color(muted)
                .child("Recouvrance keeps only a hash of this agent's token: New token makes one to copy into your tool (the old one stops working).")
                .into_any_element();
        }
        let picked = self.connect_client;
        let picker = h_flex().gap_1().flex_wrap().child(div().mr_1().text_sm().child("Connect a tool:")).children(
            Client::ALL.into_iter().enumerate().map(|(i, client)| {
                let on = picked == Some(client);
                Button::new(("connect-client", i))
                    .xsmall()
                    .label(client.label())
                    .selected(on)
                    .when(on, ButtonVariants::primary)
                    .when(!on, ButtonVariants::ghost)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.connect_client = if on { None } else { Some(client) };
                        cx.notify();
                    }))
            }),
        );
        let url = clients::server_url(self.hub.port());
        let snippets: Vec<_> = picked
            .into_iter()
            .flat_map(|client| client.snippets(&url, clients::MASKED_TOKEN).into_iter().map(move |s| (client, s)))
            .collect();
        let last = snippets.len().saturating_sub(1);
        let blocks = snippets.into_iter().enumerate().map(|(ix, (client, snippet))| {
            let id = agent.id.clone();
            v_flex()
                .gap_1()
                .child(div().text_xs().text_color(muted).child(snippet.hint))
                .child(div().p_2().rounded_md().bg(secondary).font_family(mono.clone()).text_xs().child(snippet.text))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            Button::new(("connect-copy", ix))
                                .small()
                                .icon(Lucide::Copy)
                                .label(if snippet.command { "Copy command" } else { "Copy config" })
                                .on_click(
                                    cx.listener(move |this, _, _, cx| this.copy_connect(&id, Some((client, ix)), cx)),
                                ),
                        )
                        .when(ix == last, |d| {
                            let id = agent.id.clone();
                            d.child(
                                Button::new("connect-copy-token")
                                    .small()
                                    .ghost()
                                    .label("Copy token")
                                    .tooltip("The token alone, for another MCP client")
                                    .on_click(cx.listener(move |this, _, _, cx| this.copy_connect(&id, None, cx))),
                            )
                        }),
                )
        });
        let notice = match (fresh, main) {
            (true, false) => Some("Copy the new token into your tool now: it can't be copied later."),
            (true, true) => Some("New token: reconnect the tools that used the old one."),
            _ => None,
        };
        v_flex()
            .gap_2()
            .when_some(notice, |d, notice| {
                d.p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(blue)
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(notice))
            })
            .child(picker)
            .children(blocks)
            .into_any_element()
    }

    pub(super) fn render_detail(&self, agent: &Agent, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let editor = self.editor.as_ref().filter(|e| e.agent_id == agent.id);
        let section = |title: &'static str| {
            div()
                .pt_5()
                .pb_1()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(muted)
                .child(title.to_uppercase())
        };
        let agent_id = agent.id.clone();
        let rules: Vec<AnyElement> = match editor {
            Some(e) => agent
                .policy
                .rules
                .iter()
                .zip(&e.rules)
                .enumerate()
                .map(|(i, (r, inputs))| self.render_rule(i, r, inputs, cx).into_any_element())
                .collect(),
            None => Vec::new(),
        };
        let limit = |label: &'static str, input: Option<&Entity<InputState>>| {
            h_flex()
                .gap_2()
                .child(div().w(px(140.)).text_sm().child(label))
                .child(div().w(px(120.)).children(input.map(|i| Input::new(i).small())))
        };

        v_flex()
            .id("agent-detail")
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_y_scroll()
            .p_4()
            .gap_1()
            .child(
                h_flex()
                    .gap_3()
                    .child(Icon::new(Lucide::Bot).size_5())
                    .child(div().text_lg().font_weight(FontWeight::BOLD).child(agent.name.clone()))
                    .child(
                        Switch::new("agent-enabled")
                            .checked(agent.enabled)
                            .label(if agent.enabled { "Enabled" } else { "Disabled" })
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                let checked = *checked;
                                this.update(|a| a.enabled = checked, cx);
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("export-rules")
                            .small()
                            .ghost()
                            .icon(Lucide::Copy)
                            .label("Copy rules")
                            .tooltip("Copy this agent's rules and limits as JSON, to share or reuse")
                            .on_click(cx.listener({
                                let id = agent_id.clone();
                                move |this, _, _, cx| {
                                    if let Some(a) = this.hub.agent(&id)
                                        && let Ok(json) = a.policy.copied()
                                    {
                                        cx.write_to_clipboard(ClipboardItem::new_string(json));
                                    }
                                }
                            })),
                    )
                    .child(
                        Button::new("import-rules")
                            .small()
                            .ghost()
                            .icon(Lucide::ClipboardPaste)
                            .label("Paste rules")
                            .tooltip("Replace this agent's rules and limits with JSON from the clipboard")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let text = cx.read_from_clipboard().and_then(|c| c.text()).unwrap_or_default();
                                match Policy::pasted(&text) {
                                    Some(p) => {
                                        this.update(|a| a.policy = p, cx);
                                        if let Some(agent) = this.selected.as_ref().and_then(|id| this.hub.agent(id)) {
                                            this.rebuild_editor(&agent, window, cx);
                                        }
                                        this.error = None;
                                    }
                                    _ => this.error = Some("The clipboard doesn't hold Recouvrance rules (use “Copy rules” on another agent).".into()),
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("rotate")
                            .small()
                            .ghost()
                            .icon(Lucide::RefreshCw)
                            .label("New token")
                            .on_click(cx.listener({
                                let id = agent_id.clone();
                                move |this, _, _, cx| {
                                    let result = this.hub.rotate_token(&id);
                                    if let Some(token) = this.report(result) {
                                        let saved = crate::assistant::token_rotated(&this.hub, &id, &token);
                                        this.report(saved.map_err(anyhow::Error::from));
                                        this.fresh_token = Some((id.clone(), token));
                                        this.connect_client = this.connect_client.or(Some(Client::ClaudeCode));
                                    }
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new("delete")
                            .small()
                            .ghost()
                            .icon(Lucide::Trash)
                            .label("Delete")
                            .on_click(cx.listener({
                                let id = agent_id;
                                move |this, _, window, cx| {
                                    let result = this.hub.delete_agent(&id);
                                    this.report(result);
                                    this.selected = None;
                                    this.editor = None;
                                    if let Some(next) = this.hub.config().agents.first().map(|a| a.id.clone()) {
                                        this.select(next, window, cx);
                                    }
                                    cx.notify();
                                }
                            })),
                    ),
            )
            .child(section("Connect"))
            .child(self.render_connect(agent, cx))
            .child(section("Access rules"))
            .child(div().text_sm().text_color(muted).child(
                "Each request is checked against every rule: Deny wins, then Ask me, then Allow. Anything no rule matches is denied. \
                 Patterns use * (e.g. team-*), lists are comma-separated, and namespace * also covers cluster-scoped objects.",
            ))
            .child(div().py_2().child(self.render_presets(agent, cx)))
            .child(v_flex().gap_2().children(rules))
            .child(
                h_flex().pt_2().child(
                    Button::new("add-rule")
                        .small()
                        .outline()
                        .icon(Lucide::Plus)
                        .label("Add rule")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.edit_rules(|rules| rules.push(Rule { note: "New rule".into(), ..Rule::default() }), window, cx);
                        })),
                ),
            )
            .child(section("Test access"))
            .child(self.render_tester(agent, cx))
            .child(section("Limits"))
            .child(limit("Max log lines", editor.map(|e| &e.max_lines)))
            .child(limit("Max rows per list", editor.map(|e| &e.max_rows)))
            .child(limit("Exec timeout (s)", editor.map(|e| &e.exec_timeout)))
            .child(limit("Calls per minute", editor.map(|e| &e.rate)))
            .child(section("Activity"))
            .child(self.render_activity(Some(&agent.id), cx))
    }
}
