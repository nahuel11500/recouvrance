//! The action buttons in a detail page's header.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::DetailPanel;
use crate::k8s;

impl DetailPanel {
    pub(super) fn render_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        // Nothing left to act on.
        if self.deleted {
            return div()
                .px_2()
                .py_0p5()
                .rounded_sm()
                .text_sm()
                .bg(cx.theme().danger.opacity(0.12))
                .text_color(cx.theme().danger)
                .child("Deleted from the cluster")
                .into_any_element();
        }
        let k = &self.obj.kind;
        let scalable =
            k.ar.group == "apps" && matches!(k.ar.kind.as_str(), "Deployment" | "StatefulSet" | "ReplicaSet");
        let restartable =
            k.ar.group == "apps" && matches!(k.ar.kind.as_str(), "Deployment" | "StatefulSet" | "DaemonSet");
        let flag = |p: &str| {
            self.object.as_ref().and_then(|o| o.pointer(p)).and_then(serde_json::Value::as_bool).unwrap_or(false)
        };
        let is_node = k.is("", "Node");
        let cordoned = flag("/spec/unschedulable");
        let is_cron = k.is("batch", "CronJob");
        let suspended = flag("/spec/suspend");
        let is_flux = k.ar.group.ends_with(".toolkit.fluxcd.io")
            && self.object.as_ref().is_some_and(|o| o.pointer("/spec/interval").is_some());
        // Flux objects, Argo CronWorkflows and others with a boolean `spec.suspend`.
        let suspendable = !is_cron
            && (is_flux
                || self
                    .object
                    .as_ref()
                    .is_some_and(|o| o.pointer("/spec/suspend").is_some_and(serde_json::Value::is_boolean)));
        let is_argo = k.is("argoproj.io", "Application");
        let is_pod = k.is("", "Pod");
        let plugins = crate::plugins::for_kind(&k.kubectl_name(), &k.ar.plural, cx);
        // Actions your RBAC does not allow are disabled (checked like `kubectl auth can-i`).
        let denied = self.denied.clone();
        // With an identity to run as (Settings → Contexts), what its confirmation can elevate
        // stays usable: the check is yours, the server decides for that identity.
        let run_as = crate::settings::run_as(&self.context);
        let gate = move |b: Button, verb: &'static str| -> Button {
            if let Some(id) = run_as.as_ref().filter(|_| denied.contains(verb) && can_run_as(verb)) {
                b.tooltip(format!("Your RBAC does not allow this: its confirmation can run it as {}", id.label()))
            } else if denied.contains(verb) {
                b.disabled(true).tooltip(match verb {
                    "run" => "Your RBAC does not allow creating Jobs here",
                    "drain" => "Your RBAC does not allow evicting pods",
                    "debug" => "Your RBAC does not allow adding debug containers",
                    "nodeshell" => "Your RBAC does not allow creating pods in kube-system",
                    "delete" => "Your RBAC does not allow deleting this",
                    "token" => "Your RBAC does not allow requesting tokens for this ServiceAccount",
                    "create" => "Your RBAC does not allow creating RoleBindings here",
                    _ => "Your RBAC does not allow changing this",
                })
            } else {
                b
            }
        };
        h_flex()
            .gap_1()
            .children(plugins.into_iter().enumerate().map(|(i, p)| {
                let tip = if p.description.is_empty() { p.command.clone() } else { p.description.clone() };
                Button::new(("plugin", i)).small().ghost().icon(Lucide::Puzzle).label(p.name.clone()).tooltip(tip).on_click(cx.listener(move |this, _, window, cx| {
                    this.run_plugin(p.clone(), window, cx);
                }))
            }))
            .when(k.is("", "ConfigMap") || k.is("", "Secret"), |d| {
                d.child(
                    gate(Button::new("act-edit-data"), "patch")
                        .small()
                        .ghost()
                        .icon(Lucide::PencilLine)
                        .label(if self.data_editor.is_some() { "Close editor" } else { "Edit values" })
                        .on_click(cx.listener(|this, _, w, cx| this.toggle_data_editor(w, cx))),
                )
            })
            .when(k.is("rbac.authorization.k8s.io", "Role") || k.is("rbac.authorization.k8s.io", "ClusterRole"), |d| {
                d.child(gate(Button::new("act-bind"), "create").small().ghost().icon(Lucide::Link).label("Bind…").tooltip("Grant this role to a user, group or ServiceAccount").on_click(cx.listener(|this, _, w, cx| this.bind_dialog(None, None, w, cx))))
            })
            .when(k.is("rbac.authorization.k8s.io", "RoleBinding") || k.is("rbac.authorization.k8s.io", "ClusterRoleBinding"), |d| {
                d.child(gate(Button::new("act-add-subject"), "patch").small().ghost().icon(Lucide::UserPlus).label("Add subject…").on_click(cx.listener(|this, _, w, cx| {
                    let b = this.object.clone();
                    this.bind_dialog(b, None, w, cx);
                })))
            })
            .when(k.is("", "ServiceAccount"), |d| {
                d.child(gate(Button::new("act-sa-kubeconfig"), "token").small().ghost().icon(Lucide::KeyRound).label("Kubeconfig…").tooltip("A kubeconfig that acts as this ServiceAccount (short-lived token)").on_click(cx.listener(|this, _, w, cx| this.sa_kubeconfig_dialog(w, cx))))
            })
            .when(is_pod, |d| {
                d.child(gate(Button::new("act-debug"), "debug").small().ghost().icon(Lucide::Bug).label("Debug").on_click(cx.listener(|this, _, w, cx| this.confirm_debug(w, cx))))
            })
            .when(is_flux, |d| {
                d.child(
                    gate(Button::new("act-flux-reconcile"), "patch").small().ghost().icon(Lucide::RefreshCw).label("Reconcile").on_click(cx.listener(|this, _, w, cx| {
                        let title = format!("Reconcile {} {} now?", this.obj.kind.ar.kind, this.obj.name);
                        this.confirm_action(title, "Asks Flux to reconcile now instead of waiting for its interval, like `flux reconcile`.", "Reconcile", |p, client| {
                            Box::pin(k8s::flux_reconcile(client, p.obj.kind.clone(), p.obj.namespace.clone(), p.obj.name.clone()))
                        }, w, cx);
                    })),
                )
            })
            .when(suspendable, |d| {
                d.child(
                    gate(Button::new("act-suspend-generic"), "patch")
                        .small()
                        .ghost()
                        .icon(if suspended { Lucide::CirclePlay } else { Lucide::CirclePause })
                        .label(if suspended { "Resume" } else { "Suspend" })
                        .on_click(cx.listener(move |this, _, w, cx| {
                            let title = format!("{} {} {}?", if suspended { "Resume" } else { "Suspend" }, this.obj.kind.ar.kind, this.obj.name);
                            if suspended {
                                this.confirm_action(title, "It runs or reconciles again.", "Resume", |p, client| {
                                    Box::pin(k8s::merge_patch(client, p.obj.kind.clone(), p.obj.namespace.clone(), p.obj.name.clone(), serde_json::json!({"spec": {"suspend": false}}), false))
                                }, w, cx);
                            } else {
                                // Suspending a Job is not a pause: Kubernetes deletes its running pods.
                                let what = if this.obj.kind.is("batch", "Job") {
                                    "Its running pods are deleted (their work is lost) and no new ones start until resumed."
                                } else {
                                    "It stops running or reconciling until resumed; what is deployed stays."
                                };
                                this.confirm_action(title, what, "Suspend", |p, client| {
                                    Box::pin(k8s::merge_patch(client, p.obj.kind.clone(), p.obj.namespace.clone(), p.obj.name.clone(), serde_json::json!({"spec": {"suspend": true}}), false))
                                }, w, cx);
                            }
                        })),
                )
            })
            .when(is_argo, |d| {
                d.child(
                    gate(Button::new("act-argo-refresh"), "patch").small().ghost().icon(Lucide::RefreshCw).label("Refresh").on_click(cx.listener(|this, _, w, cx| {
                        let title = format!("Refresh Application {}?", this.obj.name);
                        this.confirm_action(title, super::argo::REFRESH_NOTE, "Refresh", |p, client| {
                            Box::pin(k8s::argo_refresh(client, p.obj.kind.clone(), p.obj.namespace.clone(), p.obj.name.clone()))
                        }, w, cx);
                    })),
                )
                .child(gate(Button::new("act-argo-sync"), "patch").small().ghost().icon(Lucide::GitBranch).label("Sync").on_click(cx.listener(|this, _, w, cx| {
                    let title = format!("Sync Application {}?", this.obj.name);
                    this.confirm_action(title, super::argo::SYNC_NOTE, "Sync", |p, client| {
                        Box::pin(k8s::argo_sync(client, p.obj.kind.clone(), p.obj.namespace.clone(), p.obj.name.clone()))
                    }, w, cx);
                })))
            })
            .when(is_node, |d| {
                d.child(
                    gate(Button::new("act-node-shell"), "nodeshell")
                        .small()
                        .ghost()
                        .icon(Lucide::SquareTerminal)
                        .label("Shell")
                        .on_click(cx.listener(|this, _, w, cx| this.confirm_node_shell(w, cx))),
                )
                .child(
                    gate(Button::new("act-cordon"), "patch")
                        .small()
                        .ghost()
                        .icon(Lucide::Ban)
                        .label(if cordoned { "Uncordon" } else { "Cordon" })
                        .on_click(cx.listener(move |this, _, w, cx| {
                            let title = format!("{} node {}?", if cordoned { "Uncordon" } else { "Cordon" }, this.obj.name);
                            if cordoned {
                                this.confirm_action(title, "New pods can be scheduled on it again.", "Uncordon", |p, client| {
                                    Box::pin(k8s::merge_patch(client, p.obj.kind.clone(), None, p.obj.name.clone(), serde_json::json!({"spec": {"unschedulable": false}}), false))
                                }, w, cx);
                            } else {
                                this.confirm_action(title, "No new pods will be scheduled on it; running pods stay.", "Cordon", |p, client| {
                                    Box::pin(k8s::merge_patch(client, p.obj.kind.clone(), None, p.obj.name.clone(), serde_json::json!({"spec": {"unschedulable": true}}), false))
                                }, w, cx);
                            }
                        })),
                )
                .child(
                    gate(Button::new("act-drain"), "drain")
                        .small()
                        .ghost()
                        .icon(Lucide::ArrowDownToLine)
                        .label("Drain")
                        .on_click(cx.listener(|this, _, w, cx| {
                            let title = format!("Drain node {}?", this.obj.name);
                            this.confirm_action(
                                title,
                                "Cordons the node, then evicts its pods (DaemonSet and static pods stay). PodDisruptionBudgets are respected; refused evictions are reported.",
                                "Drain",
                                |p, client| {
                                    // The gate refuses each eviction once write mode is off; stop at the first.
                                    let off = || (!k8s::gate::ui_writes()).then(|| k8s::gate::WRITES_OFF.to_string());
                                    Box::pin(k8s::drain(client, p.obj.name.clone(), off))
                                },
                                w,
                                cx,
                            );
                        })),
                )
            })
            .when(is_cron, |d| {
                d.child(
                    gate(Button::new("act-run-cron"), "run")
                        .small()
                        .ghost()
                        .icon(Lucide::Play)
                        .label("Run now")
                        .on_click(cx.listener(|this, _, w, cx| {
                            let title = format!("Run CronJob {} now?", this.obj.name);
                            this.confirm_action(title, "Creates a Job from its template, like `kubectl create job --from=cronjob/…`.", "Run", |p, client| {
                                Box::pin(k8s::trigger_cronjob(client, p.obj.namespace.clone().unwrap_or_default(), p.obj.name.clone()))
                            }, w, cx);
                        })),
                )
                .child(
                    gate(Button::new("act-suspend"), "patch")
                        .small()
                        .ghost()
                        .icon(if suspended { Lucide::CirclePlay } else { Lucide::CirclePause })
                        .label(if suspended { "Resume" } else { "Suspend" })
                        .on_click(cx.listener(move |this, _, w, cx| {
                            let title = format!("{} CronJob {}?", if suspended { "Resume" } else { "Suspend" }, this.obj.name);
                            if suspended {
                                this.confirm_action(title, "It runs on its schedule again.", "Resume", |p, client| {
                                    Box::pin(k8s::merge_patch(client, p.obj.kind.clone(), p.obj.namespace.clone(), p.obj.name.clone(), serde_json::json!({"spec": {"suspend": false}}), false))
                                }, w, cx);
                            } else {
                                this.confirm_action(title, "No new runs until resumed; running jobs continue.", "Suspend", |p, client| {
                                    Box::pin(k8s::merge_patch(client, p.obj.kind.clone(), p.obj.namespace.clone(), p.obj.name.clone(), serde_json::json!({"spec": {"suspend": true}}), false))
                                }, w, cx);
                            }
                        })),
                )
            })
            .when(scalable, |d| {
                d.child(gate(Button::new("act-scale"), "patch").small().ghost().icon(Lucide::ArrowUpDown).label("Scale").on_click(cx.listener(|this, _, w, cx| this.confirm_scale(w, cx))))
            })
            .when(restartable, |d| {
                d.child(gate(Button::new("act-restart"), "patch").small().ghost().icon(Lucide::RotateCw).label("Restart").on_click(cx.listener(|this, _, w, cx| this.confirm_restart(w, cx))))
            })
            .child(
                gate(Button::new("act-delete"), "delete")
                    .small()
                    .ghost()
                    .icon(Icon::new(Lucide::Trash).text_color(cx.theme().danger))
                    .label("Delete")
                    .on_click(cx.listener(|this, _, w, cx| this.confirm_delete(w, cx))),
            )
            .into_any_element()
    }
}

/// Whether the confirmation of the actions checked with `verb` offers to run as another
/// identity. Shells (debug, node shell) never do, nor a ServiceAccount's kubeconfig.
fn can_run_as(verb: &str) -> bool {
    matches!(verb, "patch" | "delete" | "run" | "drain" | "create")
}
