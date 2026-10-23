//! An agent's page: token, connection, rules, limits.

#[allow(unused_imports)]
use super::*;

impl AgentsView {
    pub(super) fn render_token(&self, agent: &Agent, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let (_, token) = self.fresh_token.as_ref().filter(|(id, _)| *id == agent.id)?;
        let theme = cx.theme();
        let port = self.hub.port();
        let cmd = format!(
            "claude mcp add --transport http recouvrance http://127.0.0.1:{port}/mcp --header \"Authorization: Bearer {token}\""
        );
        let copy = cmd.clone();
        Some(
            v_flex()
                .gap_2()
                .p_3()
                .rounded_md()
                .border_1()
                .border_color(theme.blue)
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Connect your agent: run this once. The token is shown only now."),
                )
                .child(
                    div()
                        .p_2()
                        .rounded_md()
                        .bg(theme.secondary)
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .child(cmd),
                )
                .child(
                    h_flex().child(
                        Button::new("copy")
                            .small()
                            .icon(Lucide::Copy)
                            .label("Copy command")
                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))),
                    ),
                ),
        )
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
                                        && let Ok(json) = serde_json::to_string_pretty(&a.policy)
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
                                match serde_json::from_str::<Policy>(&text) {
                                    Ok(p) if !p.rules.is_empty() => {
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
                                let id = agent_id.clone();
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
            .children(self.render_token(agent, cx))
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
                            this.edit_rules(|rules| rules.push(Rule { note: "New rule".into(), ..Rule::default() }), window, cx)
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
