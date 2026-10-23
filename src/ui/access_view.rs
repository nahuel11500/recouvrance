//! "My access": what the current user may do in a namespace, per the API server
//! (like `kubectl auth can-i --list`).

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use crate::k8s::{self, Rules};

/// "Who can": the grants found, and which lists could not be read.
type WhoAnswer = (Vec<crate::rbac::Grant>, Option<String>);

const VERBS: [&str; 7] = ["get", "list", "watch", "create", "update", "patch", "delete"];
const RESOURCES: [&str; 16] = [
    "pods",
    "pods/log",
    "pods/exec",
    "deployments.apps",
    "statefulsets.apps",
    "daemonsets.apps",
    "jobs.batch",
    "cronjobs.batch",
    "services",
    "ingresses.networking.k8s.io",
    "configmaps",
    "secrets",
    "persistentvolumeclaims",
    "events",
    "roles.rbac.authorization.k8s.io",
    "rolebindings.rbac.authorization.k8s.io",
];

/// Split `resource[/sub][.group]` (as written in rules: `pods`, `pods/log`,
/// `deployments/scale.apps`, `pods.*`, `*.apps`) into (resource, group). Resource
/// names have no dots, group names do.
fn split_rule(r: &str) -> (&str, &str) {
    r.split_once('.').unwrap_or((r, ""))
}

/// Whether a rule allows every verb on every resource in every group.
fn full_access(rules: &Rules) -> bool {
    rules.iter().any(|(v, res, _)| v.iter().any(|x| x == "*") && res.iter().any(|x| x == "*.*"))
}

/// Whether any rule (with no name restriction) allows `verb` on `resource` ("pods", "deployments.apps", "pods/log").
pub fn allows(rules: &Rules, verb: &str, resource: &str) -> bool {
    let (res, group) = match resource.split_once('/') {
        Some((r, sub)) => {
            let (base, group) = r.split_once('.').unwrap_or((r, ""));
            (format!("{base}/{sub}"), group.to_string())
        }
        None => {
            let (base, group) = resource.split_once('.').unwrap_or((resource, ""));
            (base.to_string(), group.to_string())
        }
    };
    let sub = res.split_once('/').map(|(_, s)| s);
    rules.iter().filter(|(_, _, names)| names.is_empty()).any(|(verbs, resources, _)| {
        verbs.iter().any(|v| v == "*" || v == verb)
            && resources.iter().any(|r| {
                let (rres, rgroup) = split_rule(r);
                let res_ok = rres == "*" || rres == res || rres.strip_prefix("*/").is_some_and(|s| Some(s) == sub);
                let group_ok = rgroup == "*" || rgroup == group;
                res_ok && group_ok
            })
    })
}

/// The namespace whose RoleBindings count for `group`/`resource`: none for a cluster-scoped
/// kind (nodes, namespaces), which only ClusterRoleBindings grant.
fn binding_namespace<'a>(kinds: &[k8s::Kind], group: &str, resource: &str, ns: Option<&'a str>) -> Option<&'a str> {
    let base = resource.split('/').next().unwrap_or(resource);
    let cluster_scoped = kinds.iter().any(|k| !k.namespaced && k.ar.group == group && k.ar.plural == base);
    if cluster_scoped { None } else { ns }
}

pub struct AccessView {
    context: String,
    client: Client,
    /// Discovered kinds, to tell cluster-scoped resources apart in "Who can".
    kinds: Vec<k8s::Kind>,
    namespace: String,
    rules: Option<Result<Rules, String>>,
    /// Set when the server says the rules may be incomplete (webhook or cloud IAM authorizer).
    incomplete: Option<String>,
    /// "Who can": verb, resource, namespace inputs and the answer.
    who_verb: Entity<InputState>,
    who_resource: Entity<InputState>,
    who_ns: Entity<InputState>,
    who: Option<Result<WhoAnswer, String>>,
    who_busy: bool,
    _who_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
    _task: Task<()>,
}

impl AccessView {
    pub fn new(
        context: String,
        client: Client,
        kinds: Vec<k8s::Kind>,
        namespace: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let namespace = namespace.unwrap_or_else(|| "default".into());
        let ns = namespace.clone();
        let input = |text: &str, hint: &str, window: &mut Window, cx: &mut Context<Self>| {
            let (text, hint) = (text.to_string(), hint.to_string());
            cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder(hint);
                s.set_value(text, window, cx);
                s
            })
        };
        let who_verb = input("delete", "verb (get, create, delete, …)", window, cx);
        let who_resource = input("pods", "resource (pods, deployments.apps, pods/exec, secrets)", window, cx);
        let who_ns = input(&namespace, "namespace (empty = cluster-wide)", window, cx);
        let subs = [&who_verb, &who_resource, &who_ns]
            .into_iter()
            .map(|i| {
                cx.subscribe_in(i, window, |this, _, ev: &InputEvent, _, cx| {
                    if let InputEvent::PressEnter { .. } = ev {
                        this.ask_who(cx);
                    }
                })
            })
            .collect();
        let client2 = client.clone();
        let task = cx.spawn(async move |this, cx| {
            let (rules, incomplete) = match k8s::my_rules(client2, ns).await {
                Ok((rules, note)) => (Ok(rules), note),
                Err(e) => (Err(format!("{e:#}")), None),
            };
            let _ = this.update(cx, |this, cx| {
                this.incomplete = incomplete;
                this.rules = Some(rules.map(|mut r| {
                    for (v, res, n) in r.iter_mut() {
                        v.sort();
                        res.sort();
                        n.sort();
                    }
                    r.sort();
                    r.dedup();
                    // Broadest first: "*" verbs, then by resource.
                    r.sort_by_key(|(v, res, _)| (!v.iter().any(|x| x == "*"), res.join(",")));
                    r
                }));
                cx.notify();
            });
        });
        Self {
            context,
            client,
            kinds,
            namespace,
            rules: None,
            incomplete: None,
            who_verb,
            who_resource,
            who_ns,
            who: None,
            who_busy: false,
            _who_task: None,
            _subs: subs,
            _task: task,
        }
    }

    /// Who may do this? From the roles and bindings (like kubectl-who-can).
    pub fn ask_who(&mut self, cx: &mut Context<Self>) {
        let verb = self.who_verb.read(cx).value().trim().to_string();
        let (group, resource) = crate::rbac::split_resource(self.who_resource.read(cx).value().as_ref());
        let ns = Some(self.who_ns.read(cx).value().trim().to_string()).filter(|n| !n.is_empty());
        let rules_ns = binding_namespace(&self.kinds, &group, &resource, ns.as_deref()).map(String::from);
        let client = self.client.clone();
        self.who_busy = true;
        cx.notify();
        self._who_task = Some(cx.spawn(async move |this, cx| {
            let snap = k8s::rbac_snapshot(client, ns.clone()).await;
            let result = if snap.no_bindings() {
                Err(snap.load_note().unwrap_or_default())
            } else {
                Ok((crate::rbac::who_can(&snap, &verb, &group, &resource, rules_ns.as_deref()), snap.load_note()))
            };
            let _ = this.update(cx, |this, cx| {
                this.who = Some(result);
                this.who_busy = false;
                cx.notify();
            });
        }));
    }

    fn render_who(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        v_flex()
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(Lucide::Users).text_color(theme.blue))
                    .child(div().font_weight(FontWeight::MEDIUM).child("Who can…"))
                    .child(div().text_xs().text_color(muted).child("from Roles, ClusterRoles and their bindings")),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(150.)).child(Input::new(&self.who_verb).small()))
                    .child(div().w(px(300.)).child(Input::new(&self.who_resource).small()))
                    .child(div().w(px(220.)).child(Input::new(&self.who_ns).small()))
                    .child(
                        Button::new("who-ask")
                            .small()
                            .primary()
                            .label("Ask")
                            .loading(self.who_busy)
                            .on_click(cx.listener(|this, _, _, cx| this.ask_who(cx))),
                    ),
            )
            .children(
                self.who
                    .as_ref()
                    .and_then(|r| r.as_ref().ok())
                    .and_then(|(_, note)| note.clone())
                    .map(|note| div().text_xs().text_color(theme.warning).child(note)),
            )
            .children(self.who.as_ref().map(|r| {
                match r {
                    Err(e) => div().text_sm().text_color(theme.danger).child(e.clone()).into_any_element(),
                    Ok((g, _)) if g.is_empty() => div()
                        .text_sm()
                        .text_color(muted)
                        .child("Nobody, through RBAC bindings (cluster admins via system:masters are not listed).")
                        .into_any_element(),
                    Ok((grants, _)) => v_flex()
                        .id("who-results")
                        .max_h(px(320.))
                        .overflow_y_scroll()
                        .child(
                            h_flex()
                                .gap_3()
                                .py_1()
                                .text_xs()
                                .text_color(muted)
                                .border_b_1()
                                .border_color(theme.border)
                                .child(div().w(px(130.)).child("Kind"))
                                .child(div().w(px(300.)).child("Subject"))
                                .child(div().w(px(330.)).child("Through"))
                                .child(div().flex_1().child("Where")),
                        )
                        .children(grants.iter().map(|g| {
                            h_flex()
                                .gap_3()
                                .py_1()
                                .text_sm()
                                .border_b_1()
                                .border_color(theme.border)
                                .child(div().w(px(130.)).child(g.subject_kind.clone()))
                                .child(div().w(px(300.)).child(format!(
                                    "{}{}",
                                    g.subject_ns.as_ref().map(|n| format!("{n}/")).unwrap_or_default(),
                                    g.subject
                                )))
                                .child(div().w(px(330.)).text_xs().child(format!("{} → {}", g.binding, g.role)))
                                .child(div().flex_1().text_xs().text_color(muted).child(format!(
                                    "{}{}",
                                    g.scope.clone().unwrap_or_else(|| "every namespace".into()),
                                    if g.names.is_empty() {
                                        String::new()
                                    } else {
                                        format!(" · only {}", g.names.join(", "))
                                    }
                                )))
                        }))
                        .into_any_element(),
                }
            }))
    }
}

impl Render for AccessView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let verb_color = |v: &str| match v {
            "*" | "delete" | "deletecollection" | "escalate" | "bind" | "impersonate" => theme.danger,
            "create" | "update" | "patch" => theme.warning,
            _ => theme.success,
        };
        let header = h_flex()
            .gap_3()
            .child(Icon::new(Lucide::KeyRound).size_5().text_color(theme.blue))
            .child(div().text_xl().font_weight(FontWeight::MEDIUM).child("My access"))
            .child(div().text_sm().text_color(muted).child(format!(
                "{} · namespace {} · as the API server sees your kubeconfig user",
                self.context, self.namespace
            )));
        let body: AnyElement = match &self.rules {
            None => div().text_color(muted).child("Asking the API server…").into_any_element(),
            Some(Err(e)) => div().text_color(theme.danger).child(e.clone()).into_any_element(),
            Some(Ok(rules)) => v_flex()
                .id("access-rules")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .child(
                    h_flex()
                        .gap_0()
                        .py_1()
                        .text_xs()
                        .text_color(muted)
                        .border_b_1()
                        .border_color(theme.border)
                        .child(div().w(px(300.)).child("Can I…"))
                        .children(VERBS.iter().map(|v| div().w(px(70.)).child(v.to_string()))),
                )
                .children(RESOURCES.iter().map(|res| {
                    h_flex()
                        .py_1()
                        .text_sm()
                        .border_b_1()
                        .border_color(theme.border.opacity(0.5))
                        .child(
                            div()
                                .w(px(300.))
                                .font_family(theme.mono_font_family.clone())
                                .text_xs()
                                .child(res.to_string()),
                        )
                        .children(VERBS.iter().map(|v| {
                            let ok = allows(rules, v, res);
                            div().w(px(70.)).child(
                                Icon::new(if ok { Lucide::Check } else { Lucide::X }).size_4().text_color(if ok {
                                    theme.success
                                } else {
                                    theme.muted_foreground.opacity(0.5)
                                }),
                            )
                        }))
                }))
                .child(
                    div()
                        .pt_6()
                        .pb_2()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child("Rules as the API server returns them"),
                )
                .child(
                    h_flex()
                        .gap_3()
                        .py_1()
                        .text_xs()
                        .text_color(muted)
                        .border_b_1()
                        .border_color(theme.border)
                        .child(div().w(px(320.)).child("Verbs"))
                        .child(div().flex_1().child("Resources"))
                        .child(div().w(px(240.)).child("Only these names")),
                )
                .children(rules.iter().map(|(verbs, resources, names)| {
                    h_flex()
                        .gap_3()
                        .py_1p5()
                        .items_start()
                        .text_sm()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(h_flex().w(px(320.)).flex_wrap().gap_1().children(verbs.iter().map(|v| {
                            div()
                                .px_1p5()
                                .rounded_sm()
                                .text_xs()
                                .bg(verb_color(v).opacity(0.12))
                                .text_color(verb_color(v))
                                .child(v.clone())
                        })))
                        .child(div().flex_1().min_w_0().font_family(theme.mono_font_family.clone()).text_xs().child(
                            if resources.is_empty() { "(non-resource URLs)".to_string() } else { resources.join(", ") },
                        ))
                        .child(div().w(px(240.)).text_xs().text_color(muted).child(if names.is_empty() {
                            "any".to_string()
                        } else {
                            names.join(", ")
                        }))
                }))
                .into_any_element(),
        };
        v_flex()
            .size_full()
            .p_4()
            .gap_4()
            .child(header)
            .child(self.render_who(cx))
            .when(matches!(&self.rules, Some(Ok(r)) if full_access(r)), |d| {
                d.child(div().text_sm().text_color(theme.warning).child("You have full access (* on *) here: Recouvrance's write mode and protected contexts are your safety net."))
            })
            .children(self.incomplete.clone().map(|note| div().text_sm().text_color(theme.warning).child(note)))
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::{allows, binding_namespace, full_access};
    use crate::k8s::Kind;
    use kube::discovery::ApiResource;

    #[test]
    fn cluster_scoped_kinds_ignore_role_bindings() {
        let kind = |group: &str, plural: &str, namespaced: bool| Kind {
            ar: ApiResource {
                group: group.into(),
                version: "v1".into(),
                api_version: "v1".into(),
                kind: plural.into(),
                plural: plural.into(),
            },
            namespaced,
            section: "Cluster",
            label: plural.into(),
        };
        let kinds = vec![kind("", "nodes", false), kind("", "pods", true)];
        assert_eq!(binding_namespace(&kinds, "", "nodes", Some("default")), None);
        assert_eq!(binding_namespace(&kinds, "", "nodes/proxy", Some("default")), None);
        assert_eq!(binding_namespace(&kinds, "", "pods", Some("default")), Some("default"));
        // A kind discovery doesn't know keeps the namespace.
        assert_eq!(binding_namespace(&kinds, "example.com", "nodes", Some("default")), Some("default"));
    }

    #[test]
    fn core_wildcard_is_core_only() {
        // apiGroups [""] with resources ["*"]: every core resource, nothing in other groups.
        let core = vec![(vec!["*".into()], vec!["*".into()], vec![])];
        assert!(allows(&core, "delete", "pods"));
        assert!(allows(&core, "get", "secrets"));
        assert!(!allows(&core, "get", "deployments.apps"));
        assert!(!allows(&core, "get", "roles.rbac.authorization.k8s.io"));
        assert!(!full_access(&core));
        // apiGroups ["*"] with resources ["*"].
        let all = vec![(vec!["*".into()], vec!["*.*".into()], vec![])];
        assert!(allows(&all, "delete", "pods"));
        assert!(allows(&all, "delete", "deployments.apps"));
        assert!(full_access(&all));
    }

    #[test]
    fn matrix_from_rules() {
        let rules = vec![
            (vec!["get".into(), "list".into()], vec!["pods".into(), "pods/log".into()], vec![]),
            (vec!["*".into()], vec!["*.apps".into()], vec![]),
            (vec!["delete".into()], vec!["secrets".into()], vec!["only-this".into()]),
        ];
        assert!(allows(&rules, "get", "pods"));
        assert!(allows(&rules, "list", "pods/log"));
        assert!(!allows(&rules, "delete", "pods"));
        assert!(allows(&rules, "delete", "deployments.apps"));
        assert!(!allows(&rules, "delete", "secrets"), "name-restricted rules don't count");
        assert!(!allows(&rules, "get", "jobs.batch"));
    }

    #[test]
    fn wildcard_groups() {
        // apiGroups ["*"] with resources ["pods", "*/scale"]; apiGroups ["batch"] with ["*"].
        let rules = vec![
            (vec!["get".into()], vec!["pods.*".into(), "*/scale.*".into()], vec![]),
            (vec!["list".into()], vec!["*.batch".into()], vec![]),
        ];
        assert!(allows(&rules, "get", "pods"));
        assert!(!allows(&rules, "get", "pods/log"), "pods.* is not its subresources");
        assert!(allows(&rules, "get", "deployments.apps/scale"));
        assert!(!allows(&rules, "get", "deployments.apps"));
        assert!(allows(&rules, "list", "jobs.batch"));
        assert!(allows(&rules, "list", "cronjobs.batch"));
        assert!(!allows(&rules, "list", "pods"));
    }
}
