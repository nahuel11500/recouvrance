//! The activity log of agent calls.

#[allow(unused_imports)]
use super::*;

impl AgentsView {
    /// Calls grouped by MCP session (one agent conversation), newest first.
    pub(super) fn render_activity(&self, agent_id: Option<&str>, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let entries: Vec<AuditEntry> = self.hub.recent_audit(300, |e| agent_id.is_none_or(|id| e.agent_id == id));
        // The log on disk is the record of what agents did: say when it stops being written.
        let audit_error = self
            .hub
            .audit_error()
            .map(|e| div().text_sm().text_color(theme.danger).child(format!("The audit log is not being saved: {e}")));
        if entries.is_empty() {
            return v_flex()
                .children(audit_error)
                .child(div().text_sm().text_color(theme.muted_foreground).child("No calls yet."))
                .into_any_element();
        }
        // Consecutive calls of the same session form one group.
        let mut groups: Vec<Vec<AuditEntry>> = Vec::new();
        for e in entries {
            match groups.last_mut() {
                Some(g) if g[0].session == e.session && g[0].agent_id == e.agent_id => g.push(e),
                _ => groups.push(vec![e]),
            }
        }
        let fmt_time = |t: jiff::Timestamp| t.to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M:%S").to_string();
        v_flex()
            .gap_3()
            .children(audit_error)
            .children(groups.into_iter().map(|g| {
                let first = g.last().map(|e| e.time).unwrap_or_else(jiff::Timestamp::now);
                let last = g[0].time;
                let denied = g.iter().filter(|e| matches!(e.decision, Decision::Denied | Decision::Rejected)).count();
                let writes = g.iter().filter(|e| crate::mcp::server::tool_writes(&e.tool)).count();
                let session = g[0]
                    .session
                    .clone()
                    .map(|s| s.chars().take(8).collect::<String>())
                    .unwrap_or_else(|| "no session".into());
                v_flex()
                    .rounded_md()
                    .border_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .gap_3()
                            .px_2()
                            .py_1()
                            .text_xs()
                            .bg(theme.secondary.opacity(0.6))
                            .child(Icon::new(Lucide::MessagesSquare).size_3p5())
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(g[0].agent.clone()))
                            .child(div().text_color(theme.muted_foreground).child(format!("session {session}")))
                            .child(div().text_color(theme.muted_foreground).child(format!(
                                "{} – {}",
                                fmt_time(first),
                                fmt_time(last)
                            )))
                            .child(format!("{} call{}", g.len(), if g.len() == 1 { "" } else { "s" }))
                            .when(writes > 0, |d| {
                                d.child(
                                    div()
                                        .text_color(theme.warning)
                                        .child(format!("{writes} write{}", if writes == 1 { "" } else { "s" })),
                                )
                            })
                            .when(denied > 0, |d| {
                                d.child(div().text_color(theme.danger).child(format!("{denied} denied")))
                            }),
                    )
                    .child(v_flex().px_2().font_family(theme.mono_font_family.clone()).text_xs().children(
                        g.into_iter().map(|e| {
                            let color = match e.decision {
                                Decision::Allowed | Decision::Approved => theme.success,
                                Decision::Denied | Decision::Rejected => theme.warning,
                                Decision::Failed => theme.danger,
                            };
                            h_flex()
                                .gap_3()
                                .py_0p5()
                                .border_b_1()
                                .border_color(theme.border)
                                .child(
                                    div()
                                        .w(px(60.))
                                        .flex_shrink_0()
                                        .text_color(theme.muted_foreground)
                                        .child(fmt_time(e.time)),
                                )
                                .child(
                                    div()
                                        .w(px(70.))
                                        .flex_shrink_0()
                                        .text_color(color)
                                        .child(format!("{:?}", e.decision)),
                                )
                                .child(
                                    div()
                                        .w(px(130.))
                                        .flex_shrink_0()
                                        .when(crate::mcp::server::tool_writes(&e.tool), |d| d.text_color(theme.warning))
                                        .child(e.tool.clone()),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .child(compact_args(&e.args)),
                                )
                                .child(
                                    div()
                                        .w(px(280.))
                                        .flex_shrink_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .whitespace_nowrap()
                                        .text_color(theme.muted_foreground)
                                        .child(e.detail.clone()),
                                )
                                .child(
                                    div()
                                        .w(px(60.))
                                        .flex_shrink_0()
                                        .text_color(theme.muted_foreground)
                                        .child(format!("{}ms", e.duration_ms)),
                                )
                        }),
                    ))
            }))
            .into_any_element()
    }
}

/// `{"context":"x","kind":"pods","namespace":null}` -> `context=x kind=pods`
pub(super) fn compact_args(args: &serde_json::Value) -> String {
    match args.as_object() {
        Some(map) => map
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| match v.as_str() {
                Some(s) => format!("{k}={s}"),
                None => format!("{k}={v}"),
            })
            .collect::<Vec<_>>()
            .join(" "),
        None => args.to_string(),
    }
}
