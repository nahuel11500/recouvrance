//! Access actions: bind a role, add or remove subjects, a ServiceAccount's kubeconfig.

#[allow(unused_imports)]
use super::*;

impl DetailPanel {
    /// Grant this role (create a binding), or add a subject to this binding (`binding`).
    pub fn bind_dialog(
        &mut self,
        binding: Option<serde_json::Value>,
        prefill: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.ensure_writes(window, cx) {
            return;
        }
        let kind_input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("ServiceAccount, User or Group");
            s.set_value("ServiceAccount", window, cx);
            s
        });
        let name_input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("subject name");
            s.set_value(prefill.unwrap_or_default(), window, cx);
            s
        });
        let default_ns = self.obj.namespace.clone().unwrap_or_else(|| "default".into());
        // Where the ServiceAccount lives (other subjects have no namespace).
        let ns_input = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("ServiceAccount namespace");
            s.set_value(default_ns.clone(), window, cx);
            s
        });
        let panel = cx.entity().downgrade();
        let is_binding = binding.is_some();
        // Where a ClusterRole applies: a RoleBinding in a namespace, or cluster-wide.
        // A Role always binds in its own namespace.
        let scope_input = (!is_binding && self.obj.namespace.is_none()).then(|| {
            cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder("grant in namespace (empty = cluster-wide)");
                s.set_value(default_ns, window, cx);
                s
            })
        });
        let title = if is_binding {
            format!("Add a subject to {}", self.obj.name)
        } else {
            format!("Bind {} {}", self.obj.kind.ar.kind, self.obj.name)
        };
        let guard = self.protection(window, cx);
        let (context, obj_ns, obj_name, obj_kind, role_kind) = (
            self.context.clone(),
            self.obj.namespace.clone(),
            self.obj.name.clone(),
            self.obj.kind.kubectl_name(),
            self.obj.kind.ar.kind.clone(),
        );
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (panel, guard, binding) = (panel.clone(), guard.clone(), binding.clone());
            let (k, n, ns, scope_in) = (kind_input.clone(), name_input.clone(), ns_input.clone(), scope_input.clone());
            let label = |text: &'static str| div().text_xs().text_color(cx.theme().muted_foreground).child(text);
            // The kubectl equivalent of what is typed so far.
            let typed = |i: &Entity<InputState>| i.read(cx).value().trim().to_string();
            let (skind, sname, sns) = (typed(&kind_input), typed(&name_input), typed(&ns_input));
            let cmd = match (&binding, subject_of(&skind, &sname, &sns)) {
                (_, None) => String::new(),
                (Some(b), Some(subject)) => {
                    let mut subjects = b["subjects"].as_array().cloned().unwrap_or_default();
                    subjects.push(subject);
                    subjects_patch_cmd(&context, obj_ns.as_deref(), &obj_kind, &obj_name, b, subjects)
                }
                (None, Some(subject)) => {
                    let scope = obj_ns.clone().or_else(|| scope_input.as_ref().map(typed).filter(|s| !s.is_empty()));
                    let (bkind, role_flag) = match (&scope, role_kind.as_str()) {
                        (Some(_), "Role") => ("rolebinding", "role"),
                        (Some(_), _) => ("rolebinding", "clusterrole"),
                        (None, _) => ("clusterrolebinding", "clusterrole"),
                    };
                    let who = match skind.as_str() {
                        "ServiceAccount" => format!("--serviceaccount={}", shell_word(&format!("{sns}:{sname}"))),
                        "User" => format!("--user={}", shell_word(&sname)),
                        _ => format!("--group={}", shell_word(&sname)),
                    };
                    kubectl(
                        &context,
                        scope.as_deref(),
                        &format!(
                            "create {bkind} {} --{role_flag}={} {who}",
                            shell_word(&binding_name(&obj_name, &subject)),
                            shell_word(&obj_name)
                        ),
                    )
                }
            };
            alert
                .title(title.clone())
                .description(if is_binding {
                    "The subject gets what this binding grants."
                } else if scope_input.is_some() {
                    "Creates a RoleBinding in the namespace given (or a ClusterRoleBinding when it is empty), checked with a server dry run first."
                } else {
                    "Creates a RoleBinding in the role's namespace, checked with a server dry run first."
                })
                .child(
                    v_flex()
                        .gap_1()
                        .child(label("Subject kind"))
                        .child(Input::new(&kind_input))
                        .child(label("Subject name"))
                        .child(Input::new(&name_input))
                        .child(label("ServiceAccount namespace (ServiceAccount only)"))
                        .child(Input::new(&ns_input))
                        .when_some(scope_input.as_ref(), |d, scope| {
                            d.child(label("Grant in namespace (empty = cluster-wide)")).child(Input::new(scope))
                        }),
                )
                .child(confirm_extras(&cmd, guard.as_ref(), cx))
                .show_cancel(true)
                .ok_text(if is_binding { "Add" } else { "Bind" })
                .on_ok(move |_, window, cx| {
                    if !guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    let kind = k.read(cx).value().trim().to_string();
                    let name = n.read(cx).value().trim().to_string();
                    let ns = ns.read(cx).value().trim().to_string();
                    let scope_ns = scope_in.as_ref().map(|s| s.read(cx).value().trim().to_string()).unwrap_or_default();
                    let Some(subject) = subject_of(&kind, &name, &ns) else { return false };
                    let binding = binding.clone();
                    let _ = panel.update(cx, |this, cx| {
                        let client = this.client.clone();
                        match binding {
                            Some(b) => {
                                let mut subjects = b["subjects"].as_array().cloned().unwrap_or_default();
                                subjects.push(subject);
                                // Fails with a conflict if the binding changed since it was read.
                                let patch = subjects_patch(&b, subjects);
                                let fut = k8s::merge_patch(client, this.obj.kind.clone(), this.obj.namespace.clone(), this.obj.name.clone(), patch, false);
                                this.run_write("Add subject", async move { fut.await.map(|_| "Subject added".to_string()) }, window, cx);
                            }
                            None => {
                                let role_kind = this.obj.kind.ar.kind.clone();
                                let role = this.obj.name.clone();
                                // A Role binds in its namespace; a ClusterRole where asked, or cluster-wide.
                                let scope = this.obj.namespace.clone().or_else(|| (!scope_ns.is_empty()).then(|| scope_ns.clone()));
                                let bk = binding_kind(scope.is_some());
                                let bkind = bk.ar.kind.clone();
                                let bname = binding_name(&role, &subject);
                                let mut obj = serde_json::json!({
                                    "apiVersion": "rbac.authorization.k8s.io/v1", "kind": bkind,
                                    "metadata": {"name": bname},
                                    "roleRef": {"apiGroup": "rbac.authorization.k8s.io", "kind": role_kind, "name": role},
                                    "subjects": [subject],
                                });
                                if let Some(s) = &scope {
                                    obj["metadata"]["namespace"] = s.clone().into();
                                }
                                let fut = async move {
                                    k8s::create(client.clone(), bk.clone(), scope.clone(), obj.clone(), true).await?;
                                    k8s::create(client, bk, scope, obj, false).await
                                };
                                this.run_write("Bind", fut, window, cx);
                            }
                        }
                    });
                    true
                })
        });
    }

    pub fn confirm_remove_subject(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(obj) = self.object.clone() else { return };
        let mut subjects = obj["subjects"].as_array().cloned().unwrap_or_default();
        if ix >= subjects.len() {
            return;
        }
        let gone = subjects.remove(ix);
        let label = format!("{} {}", gone["kind"].as_str().unwrap_or("?"), gone["name"].as_str().unwrap_or("?"));
        if !self.ensure_writes(window, cx) {
            return;
        }
        let panel = cx.entity().downgrade();
        let guard = self.protection(window, cx);
        let title = format!("Remove {label} from {}?", self.obj.name);
        let cmd = subjects_patch_cmd(
            &self.context,
            self.obj.namespace.as_deref(),
            &self.obj.kind.kubectl_name(),
            &self.obj.name,
            &obj,
            subjects.clone(),
        );
        // Fails with a conflict if the binding changed since it was read.
        let patch = subjects_patch(&obj, subjects);
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (panel, guard, patch) = (panel.clone(), guard.clone(), patch.clone());
            alert
                .title(title.clone())
                .description("It loses what this binding grants.")
                .child(confirm_extras(&cmd, guard.as_ref(), cx))
                .show_cancel(true)
                .ok_text("Remove")
                .on_ok(move |_, window, cx| {
                    if !guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    let patch = patch.clone();
                    let _ = panel.update(cx, |this, cx| {
                        let fut = k8s::merge_patch(
                            this.client.clone(),
                            this.obj.kind.clone(),
                            this.obj.namespace.clone(),
                            this.obj.name.clone(),
                            patch,
                            false,
                        );
                        this.run_write(
                            "Remove subject",
                            async move { fut.await.map(|_| "Subject removed".to_string()) },
                            window,
                            cx,
                        );
                    });
                    true
                })
        });
    }

    /// Save a kubeconfig acting as this ServiceAccount (token valid for the chosen hours).
    pub fn sa_kubeconfig_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.ensure_writes(window, cx) {
            return;
        }
        let hours = cx.new(|cx| {
            let mut s = InputState::new(window, cx).placeholder("hours");
            s.set_value("1", window, cx);
            s
        });
        let panel = cx.entity().downgrade();
        let guard = self.protection(window, cx);
        let (context, ns, name) =
            (self.context.clone(), self.obj.namespace.clone().unwrap_or_default(), self.obj.name.clone());
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (panel, hours, guard) = (panel.clone(), hours.clone(), guard.clone());
            let parsed = token_hours(&hours.read(cx).value());
            let cmd = kubectl(
                &context,
                Some(&ns),
                &format!("create token {} --duration={}h", shell_word(&name), parsed.as_ref().map_or("…".to_string(), |h| h.to_string())),
            );
            alert
                .title("Kubeconfig for this ServiceAccount")
                .description(format!(
                    "Requests a short-lived token (TokenRequest API) and saves a kubeconfig that uses it to ~/Downloads (readable only by you). Token lifetime in hours (1 to {}):",
                    k8s::MAX_TOKEN_HOURS
                ))
                .child(Input::new(&hours))
                .when_some(parsed.as_ref().err(), |a, e| {
                    a.child(div().text_sm().text_color(cx.theme().danger).child(e.clone()))
                })
                .child(confirm_extras(&cmd, guard.as_ref(), cx))
                .show_cancel(true)
                .ok_text("Save kubeconfig")
                .on_ok(move |_, window, cx| {
                    if !guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    let Ok(h) = token_hours(&hours.read(cx).value()) else { return false };
                    let _ = panel.update(cx, |this, cx| this.save_sa_kubeconfig(h, window, cx));
                    true
                })
        });
    }

    pub fn save_sa_kubeconfig(&mut self, hours: i64, window: &mut Window, cx: &mut Context<Self>) {
        let fut = k8s::service_account_kubeconfig(
            self.client.clone(),
            self.context.clone(),
            self.obj.namespace.clone().unwrap_or_default(),
            self.obj.name.clone(),
            hours,
        );
        self.run_write(
            "Kubeconfig",
            async move { fut.await.map(|p| format!("Saved {} (token valid {hours}h)", p.display())) },
            window,
            cx,
        );
    }
}

/// A token lifetime typed in hours: a whole number from 1 to `MAX_TOKEN_HOURS`.
pub(crate) fn token_hours(text: &str) -> Result<i64, String> {
    let max = k8s::MAX_TOKEN_HOURS;
    match text.trim().parse::<i64>() {
        Ok(h) if (1..=max).contains(&h) => Ok(h),
        _ => Err(format!("Enter a whole number of hours from 1 to {max}.")),
    }
}

/// A binding subject from what was typed, or None when it is incomplete.
fn subject_of(kind: &str, name: &str, ns: &str) -> Option<serde_json::Value> {
    if name.is_empty()
        || !matches!(kind, "ServiceAccount" | "User" | "Group")
        || (kind == "ServiceAccount" && ns.is_empty())
    {
        return None;
    }
    let mut subject = serde_json::json!({"kind": kind, "name": name});
    if kind == "ServiceAccount" {
        subject["namespace"] = ns.into();
    } else {
        subject["apiGroup"] = "rbac.authorization.k8s.io".into();
    }
    Some(subject)
}

/// A RoleBinding (in a namespace) or a ClusterRoleBinding, the kind `k8s::create` posts.
fn binding_kind(namespaced: bool) -> k8s::Kind {
    let (kind, plural, label) = if namespaced {
        ("RoleBinding", "rolebindings", "Role Bindings")
    } else {
        ("ClusterRoleBinding", "clusterrolebindings", "Cluster Role Bindings")
    };
    k8s::Kind {
        ar: kube::discovery::ApiResource {
            group: "rbac.authorization.k8s.io".into(),
            version: "v1".into(),
            api_version: "rbac.authorization.k8s.io/v1".into(),
            kind: kind.into(),
            plural: plural.into(),
        },
        namespaced,
        section: "Access Control",
        label: label.into(),
    }
}

/// A binding's name for a role and a subject; a ServiceAccount's namespace is part of it,
/// so two ServiceAccounts with the same name don't get the same binding.
fn binding_name(role: &str, subject: &serde_json::Value) -> String {
    let name = subject["name"].as_str().unwrap_or_default();
    let who = match subject["namespace"].as_str() {
        Some(ns) => format!("{ns}-{name}"),
        None => name.to_string(),
    };
    format!("{role}-{}", who.replace(':', "-")).chars().take(63).collect()
}

/// The patch that sets a binding's subjects, with the resourceVersion they were read at:
/// if someone changed the binding since, the API server refuses it (a conflict) instead
/// of overwriting their change.
pub(crate) fn subjects_patch(binding: &serde_json::Value, subjects: Vec<serde_json::Value>) -> serde_json::Value {
    let mut patch = serde_json::json!({"subjects": subjects});
    if let Some(rv) = binding.pointer("/metadata/resourceVersion") {
        patch["metadata"] = serde_json::json!({"resourceVersion": rv});
    }
    patch
}

fn subjects_patch_cmd(
    context: &str,
    ns: Option<&str>,
    kind: &str,
    name: &str,
    binding: &serde_json::Value,
    subjects: Vec<serde_json::Value>,
) -> String {
    let patch = subjects_patch(binding, subjects).to_string();
    kubectl(context, ns, &format!("patch {kind} {} --type merge -p {}", shell_word(name), k8s::shell_quote(&patch)))
}

#[cfg(test)]
mod tests {
    use super::{binding_kind, binding_name, subject_of, subjects_patch, token_hours};

    #[test]
    fn bindings_take_their_own_scope() {
        // A ClusterRole granted in a namespace: a namespaced RoleBinding.
        let rb = binding_kind(true);
        assert!(rb.namespaced);
        assert_eq!((rb.ar.kind.as_str(), rb.ar.plural.as_str()), ("RoleBinding", "rolebindings"));
        let crb = binding_kind(false);
        assert!(!crb.namespaced);
        assert_eq!(crb.ar.kind, "ClusterRoleBinding");
    }

    #[test]
    fn token_lifetime_is_short() {
        assert_eq!(token_hours("1"), Ok(1));
        assert_eq!(token_hours(" 24 "), Ok(24));
        assert!(token_hours("25").is_err());
        assert!(token_hours("0").is_err());
        assert!(token_hours("").is_err());
        assert!(token_hours("8h").is_err());
    }

    #[test]
    fn subject_patches_carry_the_resource_version() {
        let b = serde_json::json!({"metadata": {"name": "x", "resourceVersion": "42"}, "subjects": []});
        let s = subject_of("User", "alice", "").unwrap();
        let p = subjects_patch(&b, vec![s]);
        assert_eq!(p["metadata"]["resourceVersion"], "42");
        assert_eq!(p["subjects"][0]["name"], "alice");
        assert_eq!(p["subjects"][0]["apiGroup"], "rbac.authorization.k8s.io");
        assert!(subject_of("ServiceAccount", "ci", "").is_none());
        assert!(subject_of("Robot", "x", "").is_none());
        assert_eq!(subject_of("ServiceAccount", "ci", "team").unwrap()["namespace"], "team");
        let user = subject_of("User", "system:serviceaccount:a:b", "").unwrap();
        assert_eq!(binding_name("view", &user), "view-system-serviceaccount-a-b");
        let sa = |ns: &str| binding_name("view", &subject_of("ServiceAccount", "default", ns).unwrap());
        assert_eq!(sa("a"), "view-a-default");
        assert_ne!(sa("a"), sa("b"));
    }
}
