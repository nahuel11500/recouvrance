//! The rule editor: one rule's fields, presets and the access tester.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::{AgentsView, RuleInputs, rule_with};
use crate::k8s;
use crate::mcp::{Access, Agent, Mode, PROTECTED, Rule, Verb, is_protected_selector};

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
        let matched = self.matched_contexts(rule);
        let names_protected = rule.contexts.iter().any(|c| is_protected_selector(c));
        // A rule that names `@protected` is meant for them: no warning.
        let any_protected = !names_protected && matched.iter().any(|c| self.protected.contains(c));
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
                            .accessibility_label(if rule.expires.is_some() {
                                "Make permanent"
                            } else {
                                "Expire in 1 hour"
                            })
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
                    .child(
                        Button::new(("dup-rule", ix))
                            .xsmall()
                            .ghost()
                            .icon(Lucide::Copy)
                            .accessibility_label("Duplicate this rule")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit_rules(
                                    |rules| {
                                        if let Some(r) = rules.get(ix).cloned() {
                                            rules.insert(ix + 1, r);
                                        }
                                    },
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new(("del-rule", ix))
                            .xsmall()
                            .ghost()
                            .icon(Lucide::Trash)
                            .accessibility_label("Delete this rule")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.edit_rules(
                                    |rules| {
                                        if ix < rules.len() {
                                            rules.remove(ix);
                                        }
                                    },
                                    window,
                                    cx,
                                );
                            })),
                    ),
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
                            })
                            .when(names_protected && rule.effect == Mode::Allow, |d| {
                                d.child(
                                    div()
                                        .text_color(theme.warning)
                                        .child("· @protected only works in Ask and Deny rules: it allows nothing"),
                                )
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
        let presets = presets(&contexts);
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
        let verdict =
            (!context.is_empty() && !resource.is_empty()).then(|| self.resolved(&agent.policy).decide(&access));
        // The MCP server refuses writes to a read-only context before the rules decide.
        let read_only = t.verb.is_write() && crate::settings::is_read_only(&context);
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
                Some(_) if read_only => div().child(
                    h_flex()
                        .gap_2()
                        .text_sm()
                        .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme.danger).child("Denied"))
                        .child(
                            div()
                                .text_color(theme.muted_foreground)
                                .child("the context is read-only (Settings → Contexts), whatever the rules say"),
                        ),
                ),
                Some(v) => {
                    let (color, word) = match v.mode {
                        Mode::Allow => (theme.success, "Allowed"),
                        Mode::Ask => (theme.warning, "Needs your approval"),
                        Mode::Deny => (theme.danger, "Denied"),
                    };
                    let why = match v.rule {
                        Some(i) => {
                            let note = agent.policy.rules.get(i).map_or_default(|r| r.note.clone());
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

/// Argo CD's Application, as rules name it (not another group's `applications`).
const ARGO_APPS: &str = "applications.argoproj.io";

/// The presets, for an agent whose Allow and Ask rules name `contexts`.
fn presets(contexts: &[String]) -> Vec<(&'static str, Vec<Rule>)> {
    let all = vec!["*".to_string()];
    let protected = vec![PROTECTED.to_string()];
    let writes: Vec<&str> = Verb::ALL.iter().filter(|v| v.is_write()).map(|v| v.as_str()).collect();
    let read = ["get", "list", "logs"];
    let read_all = || rule_with(Mode::Allow, &read, &["*"], &["*"], "Read everything", contexts);
    let no_secrets = || rule_with(Mode::Deny, &["*"], &["secrets"], &["*"], "Never Secrets", &all);
    vec![
        ("Read-only", vec![read_all(), no_secrets()]),
        (
            "Read, ask before any write",
            vec![
                read_all(),
                rule_with(Mode::Ask, &writes, &["*"], &["*"], "Writes need my approval", contexts),
                no_secrets(),
            ],
        ),
        (
            "Operator: restart/scale freely, ask for the rest",
            vec![
                read_all(),
                rule_with(
                    Mode::Allow,
                    &["scale", "restart"],
                    &["deployments", "statefulsets"],
                    &["*"],
                    "Routine operations",
                    contexts,
                ),
                rule_with(
                    Mode::Ask,
                    &["exec", "patch", "refresh", "sync", "suspend", "apply", "delete", "drain"],
                    &["*"],
                    &["*"],
                    "Other writes need approval",
                    contexts,
                ),
                no_secrets(),
                rule_with(Mode::Deny, &writes, &["*"], &["kube-*", "*-system"], "No writes in system namespaces", &all),
            ],
        ),
        (
            "GitOps: read, refresh freely, ask to sync, never delete Applications",
            vec![
                read_all(),
                // A refresh reads the repository again and deploys nothing.
                rule_with(Mode::Allow, &["refresh"], &[ARGO_APPS], &["*"], "Refresh Argo CD Applications", contexts),
                rule_with(
                    Mode::Ask,
                    &["sync", "suspend"],
                    &["*"],
                    &["*"],
                    "Syncs, reconciles and suspends need my approval",
                    contexts,
                ),
                rule_with(Mode::Deny, &["delete"], &[ARGO_APPS], &["*"], "Never delete Applications", &all),
                no_secrets(),
            ],
        ),
        (
            "Ask for writes on protected contexts",
            vec![
                read_all(),
                rule_with(Mode::Allow, &writes, &["*"], &["*"], "Writes", contexts),
                // Protection is read at each call: a context protected later is covered too,
                // and every context when it can't be read.
                rule_with(
                    Mode::Ask,
                    &writes,
                    &["*"],
                    &["*"],
                    "Writes on protected contexts need my approval",
                    &protected,
                ),
                no_secrets(),
            ],
        ),
    ]
}

#[cfg(test)]
mod tests {
    // Not `super::*`: it brings in gpui_kit's `test` macro, which `#[test]` would then name.
    use super::{ARGO_APPS, presets};
    use crate::mcp::{Access, Mode, Policy, Verb};

    fn access(context: &str, verb: Verb, resource: &str, name: &str) -> Access {
        Access {
            context: context.into(),
            verb,
            resource: resource.into(),
            resource_aliases: vec![],
            namespace: Some("argocd".into()),
            name: Some(name.into()),
            cluster_scoped: false,
        }
    }

    fn preset(name: &str, contexts: &[&str]) -> Policy {
        let contexts: Vec<String> = contexts.iter().map(ToString::to_string).collect();
        let mut policy = Policy::default();
        policy.rules = presets(&contexts).into_iter().find(|(n, _)| n.starts_with(name)).unwrap().1;
        policy
    }

    #[test]
    fn the_gitops_preset_refreshes_freely_asks_to_sync_and_never_deletes_applications() {
        let p = preset("GitOps", &["*"]);
        let d = |verb: Verb, resource: &str| p.decide(&access("hub", verb, resource, "web")).mode;
        let flux = "kustomizations.kustomize.toolkit.fluxcd.io";
        assert_eq!(d(Verb::Refresh, ARGO_APPS), Mode::Allow);
        assert_eq!(d(Verb::Sync, ARGO_APPS), Mode::Ask);
        assert_eq!(d(Verb::Sync, flux), Mode::Ask);
        assert_eq!(d(Verb::Suspend, flux), Mode::Ask);
        assert_eq!(d(Verb::Delete, ARGO_APPS), Mode::Deny);
        // A sync rule is not a patch rule: the Application's spec stays out of reach.
        assert_eq!(d(Verb::Patch, ARGO_APPS), Mode::Deny);
        assert_eq!(d(Verb::Get, "secrets"), Mode::Deny);
    }

    #[test]
    fn the_protected_preset_asks_only_on_protected_contexts_and_everywhere_when_unknown() {
        let mut p = preset("Ask for writes", &["*"]);
        let d = |p: &Policy, context: &str| p.decide(&access(context, Verb::Delete, "pods", "web")).mode;
        // Protection not read: every context counts as protected.
        assert_eq!(d(&p, "dev"), Mode::Ask);
        assert_eq!(d(&p, "prod"), Mode::Ask);
        p.set_unprotected(["dev".to_string()]);
        assert_eq!(d(&p, "dev"), Mode::Allow);
        assert_eq!(d(&p, "prod"), Mode::Ask);
        // A context the kubeconfig didn't have when protection was read.
        assert_eq!(d(&p, "new"), Mode::Ask);
        assert_eq!(p.decide(&access("prod", Verb::Get, "pods", "web")).mode, Mode::Allow);
    }

    #[test]
    fn presets_ask_for_every_write_verb() {
        // The writes list follows the verbs: a new write verb is not left out of the Ask.
        let p = preset("Read, ask", &["*"]);
        for verb in Verb::ALL.into_iter().filter(|v| v.is_write()) {
            assert_eq!(p.decide(&access("c", verb, "deployments", "web")).mode, Mode::Ask, "{verb}");
        }
    }
}
