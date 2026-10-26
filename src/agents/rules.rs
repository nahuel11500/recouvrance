//! The rule editor: one rule's fields, presets and the access tester.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{AgentsView, RuleInputs, rule_with};
use crate::k8s;
use crate::mcp::{Access, Agent, Mode, Rule, Verb};

impl AgentsView {
    pub(super) fn render_rule(
        &self,
        ix: usize,
        rule: &Rule,
        inputs: &RuleInputs,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let effect_color = match rule.effect {
            Mode::Allow => theme.success,
            Mode::Ask => theme.warning,
            Mode::Deny => theme.danger,
        };
        let matched = self.matched_contexts(&rule.contexts);
        let any_protected = matched.iter().any(|c| self.protected.contains(c));
        let label = |text: &'static str| {
            div().w(px(96.)).flex_shrink_0().text_xs().text_color(theme.muted_foreground).child(text)
        };

        let effects = h_flex().gap_1().children(Mode::ALL.into_iter().map(|m| {
            Button::new(("effect", ix * 10 + m as usize))
                .xsmall()
                .label(m.label())
                .selected(m == rule.effect)
                .when(m == rule.effect, ButtonVariants::primary)
                .when(m != rule.effect, ButtonVariants::ghost)
                .on_click(cx.listener(move |this, _, _, cx| this.update_rule(ix, |r| r.effect = m, cx)))
        }));

        let choices = Verb::ALL.iter().map(|v| (v.as_str(), v.is_sensitive())).chain([("*", true)]);
        let verbs = h_flex().flex_wrap().gap_1().children(choices.enumerate().map(|(vi, (label, sensitive))| {
            let on = rule.verbs.iter().any(|v| v == label);
            let verb = label.to_string();
            let accent = if sensitive { theme.warning } else { theme.blue };
            div()
                .id(("verb", ix * 100 + vi))
                .px_2()
                .py_0p5()
                .rounded_md()
                .text_xs()
                .cursor_pointer()
                .border_1()
                .border_color(if on { accent } else { theme.border })
                .when(on, |d| d.bg(accent.opacity(0.14)))
                .text_color(if on { theme.foreground } else { theme.muted_foreground })
                .on_click(cx.listener(move |this, _, _, cx| {
                    let verb = verb.clone();
                    this.update_rule(
                        ix,
                        |r| {
                            if let Some(i) = r.verbs.iter().position(|v| *v == verb) {
                                r.verbs.remove(i);
                            } else {
                                r.verbs.push(verb);
                            }
                        },
                        cx,
                    );
                }))
                .child(label)
        }));

        v_flex()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(4.)).h(px(22.)).rounded_sm().bg(effect_color))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(format!("#{}", ix + 1)))
                    .child(effects)
                    // Made by approving a request: unlike a plain Allow, it wins over Ask rules.
                    .when(rule.approved && rule.effect == Mode::Allow, |d| {
                        d.child(
                            div()
                                .px_1()
                                .rounded_sm()
                                .text_xs()
                                .bg(theme.success.opacity(0.14))
                                .text_color(theme.success)
                                .child("approved: wins over Ask"),
                        )
                    })
                    .child(div().flex_1().child(Input::new(&inputs.note).small()))
                    .child(match rule.expires {
                        Some(t) if rule.expired() => div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(format!("expired {} ago", k8s::age(t))),
                        Some(t) => div().text_xs().text_color(theme.warning).child(format!(
                            "expires in {}",
                            k8s::age(jiff::Timestamp::now() - (t - jiff::Timestamp::now()))
                        )),
                        None => div(),
                    })
                    .child(
                        Button::new(("expire-rule", ix))
                            .xsmall()
                            .ghost()
                            .icon(Lucide::Timer)
                            .tooltip(if rule.expires.is_some() { "Make permanent" } else { "Expire in 1 hour" })
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.update_rule(
                                    ix,
                                    |r| {
                                        r.expires = match r.expires {
                                            Some(_) => None,
                                            None => Some(jiff::Timestamp::now() + jiff::SignedDuration::from_hours(1)),
                                        }
                                    },
                                    cx,
                                );
                            })),
                    )
                    .child(Button::new(("dup-rule", ix)).xsmall().ghost().icon(Lucide::Copy).on_click(cx.listener(
                        move |this, _, window, cx| {
                            this.edit_rules(
                                |rules| {
                                    if let Some(r) = rules.get(ix).cloned() {
                                        rules.insert(ix + 1, r);
                                    }
                                },
                                window,
                                cx,
                            );
                        },
                    )))
                    .child(Button::new(("del-rule", ix)).xsmall().ghost().icon(Lucide::Trash).on_click(cx.listener(
                        move |this, _, window, cx| {
                            this.edit_rules(
                                |rules| {
                                    if ix < rules.len() {
                                        rules.remove(ix);
                                    }
                                },
                                window,
                                cx,
                            );
                        },
                    ))),
            )
            .child(h_flex().gap_2().child(label("Verbs")).child(verbs))
            .child(
                h_flex().gap_2().items_start().child(label("Contexts")).child(
                    v_flex().flex_1().gap_0p5().child(Input::new(&inputs.contexts).small()).child(
                        h_flex()
                            .flex_wrap()
                            .gap_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(if let Some(e) = &self.contexts_error {
                                format!("can't read the kubeconfig: {e}")
                            } else if matched.is_empty() {
                                "matches no context".to_string()
                            } else {
                                format!("matches {}:", matched.len())
                            })
                            .children(matched.iter().map(|c| {
                                div()
                                    .px_1()
                                    .rounded_sm()
                                    .bg(theme.secondary)
                                    .when(self.protected.contains(c), |d| d.text_color(theme.warning))
                                    .child(c.clone())
                            }))
                            .when(any_protected && rule.effect != Mode::Deny, |d| {
                                d.child(div().text_color(theme.warning).child("· includes protected contexts"))
                            }),
                    ),
                ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(label("Resources"))
                    .child(div().flex_1().child(Input::new(&inputs.resources).small())),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(label("Namespaces"))
                    .child(div().flex_1().child(Input::new(&inputs.namespaces).small())),
            )
            .child(
                h_flex().gap_2().child(label("Names")).child(div().flex_1().child(Input::new(&inputs.names).small())),
            )
    }

    pub(super) fn render_presets(&self, agent: &Agent, cx: &mut Context<Self>) -> impl IntoElement {
        // Presets keep the contexts the agent already has.
        let mut contexts = agent.policy.context_patterns();
        if contexts.iter().any(|p| p == "*") {
            contexts = vec!["*".into()];
        }
        contexts.sort();
        contexts.dedup();
        let all = vec!["*".to_string()];
        let writes = ["exec", "scale", "restart", "patch", "apply", "delete", "drain"];
        let read = ["get", "list", "logs"];
        let presets: Vec<(&'static str, Vec<Rule>)> = vec![
            (
                "Read-only",
                vec![
                    rule_with(Mode::Allow, &read, &["*"], &["*"], "Read everything", &contexts),
                    rule_with(Mode::Deny, &["*"], &["secrets"], &["*"], "Never Secrets", &all),
                ],
            ),
            (
                "Read, ask before any write",
                vec![
                    rule_with(Mode::Allow, &read, &["*"], &["*"], "Read everything", &contexts),
                    rule_with(Mode::Ask, &writes, &["*"], &["*"], "Writes need my approval", &contexts),
                    rule_with(Mode::Deny, &["*"], &["secrets"], &["*"], "Never Secrets", &all),
                ],
            ),
            (
                "Operator: restart/scale freely, ask for the rest",
                vec![
                    rule_with(Mode::Allow, &read, &["*"], &["*"], "Read everything", &contexts),
                    rule_with(
                        Mode::Allow,
                        &["scale", "restart"],
                        &["deployments", "statefulsets"],
                        &["*"],
                        "Routine operations",
                        &contexts,
                    ),
                    rule_with(
                        Mode::Ask,
                        &["exec", "patch", "apply", "delete", "drain"],
                        &["*"],
                        &["*"],
                        "Other writes need approval",
                        &contexts,
                    ),
                    rule_with(Mode::Deny, &["*"], &["secrets"], &["*"], "Never Secrets", &all),
                    rule_with(
                        Mode::Deny,
                        &writes,
                        &["*"],
                        &["kube-*", "*-system"],
                        "No writes in system namespaces",
                        &all,
                    ),
                ],
            ),
        ];
        h_flex()
            .flex_wrap()
            .gap_2()
            .items_center()
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Presets:"))
            .children(presets.into_iter().enumerate().map(|(i, (name, rules))| {
                Button::new(("preset", i)).xsmall().outline().label(name).on_click(cx.listener(
                    move |this, _, window, cx| {
                        let rules = rules.clone();
                        this.edit_rules(|r| *r = rules, window, cx);
                    },
                ))
            }))
    }

    pub(super) fn render_tester(&self, agent: &Agent, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let t = &self.tester;
        let val = |e: &Entity<InputState>| e.read(cx).value().trim().to_string();
        let (context, resource, namespace, name) = (val(&t.context), val(&t.resource), val(&t.namespace), val(&t.name));
        let access = Access {
            context: context.clone(),
            verb: t.verb,
            resource: resource.clone(),
            resource_aliases: vec![],
            namespace: (!namespace.is_empty()).then_some(namespace),
            name: (!name.is_empty()).then_some(name),
            cluster_scoped: false,
        };
        let verdict = (!context.is_empty() && !resource.is_empty()).then(|| agent.policy.decide(&access));
        let verbs = h_flex().flex_wrap().gap_1().children(Verb::ALL.into_iter().enumerate().map(|(i, verb)| {
            Button::new(("test-verb", i))
                .xsmall()
                .label(verb.as_str())
                .when(t.verb == verb, ButtonVariants::primary)
                .when(t.verb != verb, ButtonVariants::ghost)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tester.verb = verb;
                    cx.notify();
                }))
        }));
        v_flex()
            .gap_2()
            .p_3()
            .rounded_md()
            .bg(theme.secondary.opacity(0.5))
            .child(verbs)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(200.)).child(Input::new(&t.context).small()))
                    .child(div().w(px(220.)).child(Input::new(&t.resource).small()))
                    .child(div().w(px(200.)).child(Input::new(&t.namespace).small()))
                    .child(div().w(px(180.)).child(Input::new(&t.name).small())),
            )
            .child(match verdict {
                None => div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Fill in a context and a resource to test."),
                Some(v) => {
                    let (color, word) = match v.mode {
                        Mode::Allow => (theme.success, "Allowed"),
                        Mode::Ask => (theme.warning, "Needs your approval"),
                        Mode::Deny => (theme.danger, "Denied"),
                    };
                    let why = match v.rule {
                        Some(i) => {
                            let note = agent.policy.rules.get(i).map(|r| r.note.clone()).unwrap_or_default();
                            format!(
                                "by rule #{}{}",
                                i + 1,
                                if note.is_empty() { String::new() } else { format!(" — {note}") }
                            )
                        }
                        None => "no rule matches (default deny)".into(),
                    };
                    div().child(
                        h_flex()
                            .gap_2()
                            .text_sm()
                            .child(div().font_weight(FontWeight::SEMIBOLD).text_color(color).child(word))
                            .child(div().text_color(theme.muted_foreground).child(why)),
                    )
                }
            })
    }
}
