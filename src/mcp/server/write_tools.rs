//! MCP tools that change things: exec, scale, restart, patch, apply, drain, GitOps, delete.

use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde_json::json;

use super::{
    ApplyParams, DENIED_BY_RULE, DeleteParams, DrainParams, EvictParams, ExecParams, GitOpsParams, KubeMcp,
    PatchParams, RefreshExternalSecretParams, RestartParams, ScaleParams, access_for, denied_namespaces,
    denies_eviction, manifest_objects, manifest_summary, require_ns,
};
use std::sync::Arc;

use crate::k8s;
use crate::k8s::Kind;
use crate::k8s::argo::diagnosis::{self, Diagnosis, DriftRead, MayRead, Res};
use crate::k8s::argo::ops::{self, Op, SyncRequest};
use crate::mcp::preview::Plan;
use crate::mcp::{Mode, Policy, Verb};

#[tool_router(router = write_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "Run a command in a container and return its output, like `kubectl exec -- cmd` (no tty, time-limited). This can change the container.",
        annotations(read_only_hint = false)
    )]
    async fn exec_command(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ExecParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "pod": p.pod, "container": p.container, "command": p.command});
        let mut call = self.begin(&parts, "exec_command", args)?;
        let (client, kind) = self.kind(&call, &p.context, "pods").await?;
        let access = access_for(&kind, &p.context, Verb::Exec, Some(p.namespace.clone()), Some(p.pod.clone()));
        self.authorize(&mut call, &access).await?;
        let timeout = u64::from(call.policy().exec_timeout.max(1));
        self.recheck(&mut call, &access)?;
        call.finish(k8s::exec_capture(client, p.namespace, p.pod, p.container, p.command, timeout, 64 * 1024).await)
    }

    #[tool(
        description = "Scale a workload to a number of replicas, like `kubectl scale`. Supports dry_run.",
        annotations(read_only_hint = false)
    )]
    pub(super) async fn scale(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ScaleParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name, "replicas": p.replicas, "dry_run": p.dry_run});
        let mut call = self.begin(&parts, "scale", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        let access = access_for(&kind, &p.context, Verb::Scale, Some(p.namespace.clone()), Some(p.name.clone()));
        call.plan = Some(Plan::Scale {
            client: client.clone(),
            kind: kind.clone(),
            ns: p.namespace.clone(),
            name: p.name.clone(),
            replicas: p.replicas,
        });
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        call.finish(k8s::scale(client, kind, Some(p.namespace), p.name, p.replicas, p.dry_run.unwrap_or(false)).await)
    }

    #[tool(
        description = "Restart a Deployment, StatefulSet or DaemonSet, like `kubectl rollout restart`. Supports dry_run.",
        annotations(read_only_hint = false)
    )]
    async fn rollout_restart(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<RestartParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name, "dry_run": p.dry_run});
        let mut call = self.begin(&parts, "rollout_restart", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        let access = access_for(&kind, &p.context, Verb::Restart, Some(p.namespace.clone()), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        call.finish(k8s::rollout_restart(client, kind, Some(p.namespace), p.name, p.dry_run.unwrap_or(false)).await)
    }

    #[tool(
        description = "Apply a JSON merge patch to one object, like `kubectl patch --type merge`. Supports dry_run.",
        annotations(read_only_hint = false)
    )]
    async fn patch_resource(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<PatchParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name, "patch": p.patch, "dry_run": p.dry_run});
        let mut call = self.begin(&parts, "patch_resource", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        require_ns(&call, &kind, p.namespace.as_deref())?;
        let access = access_for(&kind, &p.context, Verb::Patch, p.namespace.clone(), Some(p.name.clone()));
        call.plan = Some(Plan::Patch {
            client: client.clone(),
            kind: kind.clone(),
            ns: p.namespace.clone().filter(|_| kind.namespaced),
            name: p.name.clone(),
            patch: kube::api::Patch::Merge(p.patch.clone()),
        });
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        call.finish(k8s::merge_patch(client, kind, p.namespace, p.name, p.patch, p.dry_run.unwrap_or(false)).await)
    }

    #[tool(
        description = "Create or update objects from YAML with server-side apply, like `kubectl apply --server-side`. Each object is authorized separately. CRDs are applied first, and objects of the types they define once the API server serves them. Supports dry_run.",
        annotations(read_only_hint = false)
    )]
    async fn apply_manifest(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ApplyParams>,
    ) -> Result<String, String> {
        let args =
            json!({"context": p.context, "namespace": p.namespace, "dry_run": p.dry_run, "bytes": p.manifest.len()});
        let mut call = self.begin(&parts, "apply_manifest", args)?;
        let (client, planned) = self.plan_manifest(&call, &p.context, p.namespace.as_ref(), &p.manifest).await?;
        // What the user approves is the content, not only the names; the log keeps what each object was.
        call.approval_detail = Some(manifest_summary(&planned));
        if let Some(args) = call.args.as_object_mut() {
            args.insert("objects".into(), json!(manifest_objects(&planned)));
        }
        call.plan = Some(Plan::Apply { client: client.clone(), objects: planned.clone() });
        // Each object is approved on its own: approving one does not cover another.
        let mut approved = Vec::new();
        for (kind, ns, name, _) in &planned {
            let access = access_for(kind, &p.context, Verb::Apply, ns.clone(), Some(name.clone()));
            call.approved = false;
            self.authorize(&mut call, &access).await?;
            approved.push(call.approved);
        }
        let any_approved = approved.contains(&true);
        let dry = p.dry_run.unwrap_or(false);
        let mut out = Vec::new();
        for ((kind, ns, name, doc), approved) in planned.into_iter().zip(approved) {
            // A type a CRD of this manifest (or of the call before) defines: applied once
            // the API server serves it. A dry run created no CRD, so it can't check these.
            let kind = match self.served(&p.context, &kind).await {
                Ok(Some(k)) => k,
                Ok(None) if dry => {
                    out.push(format!(
                        "[dry run] {} {name} not checked: its CRD is not on the cluster yet",
                        kind.ar.kind
                    ));
                    continue;
                }
                Ok(None) => {
                    let (secs, ar) = (crate::mcp::hub::SERVE_WAIT.as_secs(), &kind.ar);
                    out.push(format!(
                        "error: {} {name}: the API server does not serve {} {} {secs} s after its CRD was applied (see the CRD's conditions)",
                        ar.kind, ar.api_version, ar.kind
                    ));
                    return call.finish(Err(anyhow::anyhow!(out.join("\n"))));
                }
                Err(e) => {
                    out.push(format!("error: {} {name}: {e}", kind.ar.kind));
                    return call.finish(Err(anyhow::anyhow!(out.join("\n"))));
                }
            };
            // Approvals for later objects, and applying earlier ones, can take minutes:
            // access may be off, or the rules changed, by now.
            let access = access_for(&kind, &p.context, Verb::Apply, ns.clone(), Some(name));
            call.approved = approved;
            let refused = self.stale_reason(&mut call, &access);
            call.approved = any_approved;
            if let Some(reason) = refused {
                // The log says what was applied before.
                let detail =
                    if out.is_empty() { reason } else { format!("{reason}; applied before: {}", out.join("; ")) };
                out.push(call.deny_hinted(detail, &self.stale_hint(&call, &access)));
                return Err(out.join("\n"));
            }
            let defines = if dry { Vec::new() } else { k8s::crd_kinds(&doc) };
            match k8s::apply(client.clone(), kind, ns, doc, dry).await {
                Ok(line) => {
                    // The next objects, and the agent's next calls, find its types at once.
                    self.hub.crds_applied(&p.context, defines);
                    out.push(line);
                }
                Err(e) => {
                    out.push(format!("error: {e:#}"));
                    return call.finish(Err(anyhow::anyhow!(out.join("\n"))));
                }
            }
        }
        call.finish(Ok(out.join("\n")))
    }

    #[tool(
        description = "Cordon a node and evict its pods, like `kubectl drain --ignore-daemonsets`. PodDisruptionBudgets are respected; refused evictions are reported. Refused (nothing is changed) when the node runs pods in namespaces your rules deny `delete` on pods for.",
        annotations(read_only_hint = false)
    )]
    async fn drain_node(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<DrainParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "node": p.node});
        let mut call = self.begin(&parts, "drain_node", args)?;
        let (client, kind) = self.kind(&call, &p.context, "nodes").await?;
        let access = access_for(&kind, &p.context, Verb::Drain, None, Some(p.node.clone()));
        self.authorize(&mut call, &access).await?;
        // Evicting is deleting pods: a drain must not reach namespaces the rules keep the agent out of.
        let pods_kind = self.kind(&call, &p.context, "pods").await?.1;
        let on_node = match k8s::list_values_fields(
            client.clone(),
            pods_kind.ar.clone(),
            None,
            None,
            Some(format!("spec.nodeName={}", p.node)),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => return call.finish(Err(e)),
        };
        // The rules may have changed while the pods were listed.
        self.reload_rules(&mut call);
        let denied = denied_namespaces(&on_node, |ns, name| {
            let a = access_for(&pods_kind, &p.context, Verb::Delete, Some(ns.into()), Some(name.into()));
            denies_eviction(call.policy(), &a)
        });
        if !denied.is_empty() {
            return Err(call.deny_hinted(
                format!(
                    "node {} runs pods in namespaces your rules deny (delete pods): {}; nothing was changed",
                    p.node,
                    denied.join(", ")
                ),
                DENIED_BY_RULE,
            ));
        }
        self.recheck(&mut call, &access)?;
        let stop = self.still_allowed(&call, &access);
        // The drain lists the pods again and takes minutes on a big node: a Deny on deleting
        // a pod, added while it runs, keeps that pod in place.
        let (hub, agent_id, context, kind) =
            (self.hub.clone(), call.agent.id.clone(), p.context.clone(), pods_kind.clone());
        let keep = move |ns: &str, name: &str| {
            let a = access_for(&kind, &context, Verb::Delete, Some(ns.into()), Some(name.into()));
            match hub.agent_for_calls(&agent_id) {
                Some(agent) if !denies_eviction(&agent.policy, &a) => None,
                Some(_) => Some("your rules deny deleting it".to_string()),
                None => Some("this agent is disabled".to_string()),
            }
        };
        // The reply names only the pods the agent may list.
        let shown = |ns: &str, name: &str| {
            let a = access_for(&pods_kind, &p.context, Verb::List, Some(ns.into()), Some(name.into()));
            call.policy().decide(&a).mode == Mode::Allow
        };
        call.finish(k8s::drain_report(client, p.node, stop, keep).await.map(|d| d.summary(shown)))
    }

    #[tool(
        description = "GitOps actions: Flux `reconcile` (like `flux reconcile`), `suspend`, `resume`; Argo CD Application `refresh`, `hard_refresh` (renders its manifests again, without Argo CD's cache), `sync` (optionally of some `resources` only, with `prune`, or a `dry_run` whose result is returned), `terminate` (stops the operation running, like `argocd app terminate-op`) or `rollback` (to a deployment of its history, like `argocd app rollback`; refused while auto-sync is on), each authorized by its own verb on the object, not `patch`: `refresh` (refresh, hard_refresh), `sync` (Argo CD sync, terminate and rollback, Flux reconcile) or `suspend` (Flux suspend and resume). Like argocd-server, nothing starts while another operation runs. Argo CD `diagnose` changes nothing (authorized as `get`): why the Application drifts, keeps syncing (a sync loop, with the `ignoreDifferences` that would stop it), failed (hook, admission, missing CRD...) or is stuck (a long sync, a finalizer holding its deletion), and for each out-of-sync resource the fields that differ between what was last applied and the live object on its destination, with who wrote them since the last sync. The destination's objects are read only where your rules allow `get`.",
        annotations(read_only_hint = false)
    )]
    async fn gitops(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<GitOpsParams>,
    ) -> Result<String, String> {
        let resources: Option<Vec<serde_json::Value>> = p.resources.as_ref().map(|rs| {
            rs.iter()
                .map(|r| json!({"group": r.group, "kind": r.kind, "namespace": r.namespace, "name": r.name}))
                .collect()
        });
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name,
            "action": p.action, "resources": resources, "prune": p.prune, "dry_run": p.dry_run,
            "deployment_id": p.deployment_id});
        let mut call = self.begin(&parts, "gitops", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        let flux = kind.ar.group.ends_with(".toolkit.fluxcd.io");
        let argo = kind.is("argoproj.io", "Application");
        let ok = match p.action.as_str() {
            "reconcile" | "suspend" | "resume" => flux,
            "refresh" | "hard_refresh" | "sync" | "terminate" | "rollback" | "diagnose" => argo,
            _ => false,
        };
        if !ok {
            return call.finish(Err(anyhow::anyhow!(
                "`{}` does not apply to {} (Flux: reconcile/suspend/resume; Argo CD applications: refresh/hard_refresh/sync/terminate/rollback/diagnose)",
                p.action,
                kind.ar.kind
            )));
        }
        if let Some(why) = gitops_option_misuse(&p) {
            return Err(call.fail(why));
        }
        if p.action == "diagnose" {
            return self.argo_diagnose(call, client, kind, &p.context, &p.namespace, &p.name).await;
        }
        let verb =
            gitops_verb(&p.action).ok_or_else(|| call.fail(format!("`{}` has no verb in the rules", p.action)))?;
        let mut op = argo_op(&p);
        let access = access_for(&kind, &p.context, verb, Some(p.namespace.clone()), Some(p.name.clone()));
        // What a prune deletes, and the deployment a rollback goes to, are part of what is
        // approved: read before asking, and what is sent is held to what was shown. Never
        // shown to the agent before its rules allow the call (a failed read says nothing).
        // A prune is read only when the user will be asked; a rollback is pinned unless denied.
        self.reload_rules(&mut call);
        let mode = call.policy().decide(&access).mode;
        let asks = mode == Mode::Ask && !self.hub.headless();
        let prunes = matches!(&op, Some(Op::Sync(req)) if req.prune && asks);
        let rollback = matches!(op, Some(Op::Rollback { id: None, .. })) && mode != Mode::Deny;
        let mut read = None;
        if (prunes || rollback)
            && let Ok(app) = super::get_named(&client, &kind, Some(p.namespace.as_str()), &p.name, false).await
            && let Some(o) = op.as_mut()
        {
            call.approval_detail = approval_detail(&app, o);
            read = Some(app);
        }
        // The banner's preview of a sync: of the Application just read, when it was.
        if let Some(Op::Sync(req)) = &op {
            call.plan = Some(Plan::Sync {
                client: client.clone(),
                kind: kind.clone(),
                ns: p.namespace.clone(),
                name: p.name.clone(),
                req: req.clone(),
                app: read,
            });
        }
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        let (ns, name) = (Some(p.namespace), p.name);
        call.finish(match (p.action.as_str(), op) {
            (_, Some(op)) => k8s::argo_operation(client, kind, ns, name, op).await,
            ("reconcile", _) => k8s::flux_reconcile(client, kind, ns, name).await,
            ("suspend", _) => k8s::merge_patch(client, kind, ns, name, json!({"spec": {"suspend": true}}), false).await,
            ("resume", _) => k8s::merge_patch(client, kind, ns, name, json!({"spec": {"suspend": false}}), false).await,
            ("hard_refresh", _) => k8s::argo_refresh(client, kind, ns, name, true).await,
            _ => k8s::argo_refresh(client, kind, ns, name, false).await,
        })
    }

    #[tool(
        description = "Delete one object, like `kubectl delete`. `cascade` is kubectl's `--cascade`: `orphan` keeps what it owns (a workload's pods), `foreground` deletes that first. Supports dry_run.",
        annotations(read_only_hint = false)
    )]
    pub(super) async fn delete_resource(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<DeleteParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name, "dry_run": p.dry_run, "cascade": p.cascade});
        let mut call = self.begin(&parts, "delete_resource", args)?;
        // A cascade it can't read is refused, never sent as the default: that would delete
        // the pods the agent meant to keep.
        let cascade = p
            .cascade
            .as_deref()
            .map(|c| {
                k8s::Cascade::parse(c)
                    .ok_or_else(|| call.fail(format!("unknown cascade `{c}`: use background, foreground or orphan")))
            })
            .transpose()?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        require_ns(&call, &kind, p.namespace.as_deref())?;
        let access = access_for(&kind, &p.context, Verb::Delete, p.namespace.clone(), Some(p.name.clone()));
        call.plan = Some(Plan::Delete {
            client: client.clone(),
            kind: kind.clone(),
            ns: p.namespace.clone().filter(|_| kind.namespaced),
            name: p.name.clone(),
            cascade,
        });
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        call.finish(k8s::delete(client, kind, p.namespace, p.name, None, p.dry_run.unwrap_or(false), cascade).await)
    }

    #[tool(
        description = "Evict one pod through the Eviction API, as `kubectl drain` does for each pod: unlike `delete_resource`, refused when a PodDisruptionBudget would be broken, and the refusal names that budget. Needs `delete` on the pod. Supports dry_run.",
        annotations(read_only_hint = false)
    )]
    async fn evict_pod(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<EvictParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "pod": p.pod, "dry_run": p.dry_run});
        let mut call = self.begin(&parts, "evict_pod", args)?;
        let (client, kind) = self.kind(&call, &p.context, "pods").await?;
        // Evicting is deleting the pod, as a drain's evictions are checked.
        let access = access_for(&kind, &p.context, Verb::Delete, Some(p.namespace.clone()), Some(p.pod.clone()));
        call.plan = Some(Plan::Delete {
            client: client.clone(),
            kind,
            ns: Some(p.namespace.clone()),
            name: p.pod.clone(),
            cascade: None,
        });
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        call.finish(k8s::evict(client, p.namespace, p.pod, None, p.dry_run.unwrap_or(false)).await)
    }

    #[tool(
        description = "Make the External Secrets Operator fetch an ExternalSecret from its store now instead of at its next refreshInterval (after a secret was rotated), like `kubectl annotate externalsecret <name> force-sync=$(date +%s) --overwrite`. Authorized as `refresh` on the ExternalSecret. Supports dry_run.",
        annotations(read_only_hint = false)
    )]
    async fn refresh_external_secret(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<RefreshExternalSecretParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "name": p.name, "dry_run": p.dry_run});
        let mut call = self.begin(&parts, "refresh_external_secret", args)?;
        let query = format!("externalsecrets.{}", crate::external_secrets::GROUP);
        let (client, kind) = self.kind(&call, &p.context, &query).await?;
        let access = access_for(&kind, &p.context, Verb::Refresh, Some(p.namespace.clone()), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        let dry_run = p.dry_run.unwrap_or(false);
        call.finish(k8s::force_sync_external_secret(client, kind, Some(p.namespace), p.name, dry_run).await)
    }
}

/// The rules' verb for a GitOps action: a refresh deploys nothing, a sync (or a Flux
/// reconcile) deploys what the repository holds (a rollback an earlier deployment of it, and
/// a terminate stops one), and suspend and resume stop or restart that.
/// None for any other action, so one added later is refused until it is given its verb.
fn gitops_verb(action: &str) -> Option<Verb> {
    match action {
        "refresh" | "hard_refresh" => Some(Verb::Refresh),
        "sync" | "reconcile" | "terminate" | "rollback" => Some(Verb::Sync),
        "suspend" | "resume" => Some(Verb::Suspend),
        _ => None,
    }
}

/// An option given to a `gitops` action that doesn't take it: said, not ignored, so an agent
/// never believes it got a dry run or a partial sync it did not.
fn gitops_option_misuse(p: &GitOpsParams) -> Option<String> {
    let action = p.action.as_str();
    if p.resources.is_some() && action != "sync" {
        return Some("`resources` applies to `sync` only".into());
    }
    if p.resources.as_ref().is_some_and(Vec::is_empty) {
        return Some("`resources` is empty: omit it to sync the whole Application".into());
    }
    if p.prune == Some(true) && action != "sync" {
        return Some("`prune` applies to `sync` only (a rollback never prunes)".into());
    }
    if p.dry_run == Some(true) && !matches!(action, "sync" | "rollback") {
        return Some(format!("`{action}` has no dry run: `dry_run` applies to `sync` and `rollback`"));
    }
    if p.deployment_id.is_some() && action != "rollback" {
        return Some("`deployment_id` applies to `rollback` only".into());
    }
    None
}

/// What the approval banner adds for `op` on the Application `app` as read before asking,
/// and `op` held to it: a prune's list (it may delete no more), or the deployment a rollback
/// to the one before the newest goes to (named by its id from then on).
fn approval_detail(app: &serde_json::Value, op: &mut Op) -> Option<String> {
    match op {
        Op::Sync(req) if req.prune => {
            let gone = ops::pruned(app, &req.resources);
            let verb = if req.dry_run { "would delete" } else { "deletes" };
            let detail = if gone.is_empty() {
                "prune: nothing to delete".to_string()
            } else {
                format!("prune {verb} {}: {}", gone.len(), ops::named(&gone))
            };
            req.prune_shown = Some(gone);
            Some(detail)
        }
        Op::Rollback { .. } => {
            // Nothing to go back to: left as it is, refused once authorized.
            *op = ops::pinned(app, op).ok()?;
            let Op::Rollback { id, .. } = op else { return None };
            let entry = ops::rollback_target(app, *id).ok()?;
            Some(format!("rollback to {}", ops::deployment_label(entry)))
        }
        _ => None,
    }
}

/// The Argo CD operation a `gitops` action writes, for those that write one.
fn argo_op(p: &GitOpsParams) -> Option<Op> {
    let dry_run = p.dry_run.unwrap_or(false);
    match p.action.as_str() {
        "sync" => {
            let resources = p
                .resources
                .iter()
                .flatten()
                .map(|r| Res {
                    group: r.group.clone().unwrap_or_default(),
                    kind: r.kind.clone(),
                    namespace: r.namespace.clone().unwrap_or_default(),
                    name: r.name.clone(),
                    version: String::new(),
                })
                .collect();
            Some(Op::Sync(SyncRequest { resources, prune: p.prune.unwrap_or(false), dry_run, prune_shown: None }))
        }
        "rollback" => Some(Op::Rollback { id: p.deployment_id, dry_run }),
        "terminate" => Some(Op::Terminate),
        _ => None,
    }
}

/// One object of a manifest, resolved: its kind, namespace, name and content.
pub(super) type Planned = (Kind, Option<String>, String, serde_json::Value);

impl KubeMcp {
    /// `gitops` `diagnose`: what is wrong with an Argo CD Application, read only. The
    /// Application needs `get`; each object of its destination is read only where the rules
    /// allow `get` on it without asking (the others are named as not read).
    async fn argo_diagnose(
        &self,
        mut call: super::Call,
        client: kube::Client,
        kind: Kind,
        context: &str,
        namespace: &str,
        name: &str,
    ) -> Result<String, String> {
        let access = access_for(&kind, context, Verb::Get, Some(namespace.to_string()), Some(name.to_string()));
        self.authorize(&mut call, &access).await?;
        let app = match super::get_named(&client, &kind, Some(namespace), name, false).await {
            Ok(v) => v,
            Err(e) => return call.finish(Err(e)),
        };
        let d = Diagnosis::of(&app, jiff::Timestamp::now());
        let (dest, _) = k8s::argo::destination_of(app.clone(), context.to_string(), Vec::new()).await;
        let s = k8s::argo::Summary::of(&app);
        let head = format!(
            "Application {namespace}/{name} on {context}: {}, {}, auto-sync {}; deploys to {}",
            s.sync,
            s.health,
            if s.automated { "on" } else { "off" },
            k8s::argo::destination_text(&dest.cluster, &dest.namespace)
        );
        let (policy, approved) = (call.policy().clone(), call.approved_rules().to_vec());
        let read = self.argo_drift(policy, approved, &dest.contexts, &d).await;
        // Told again with what was read live: whether an object is there.
        let d = Diagnosis::of_live(&app, jiff::Timestamp::now(), &read.drifts);
        call.finish(Ok(diagnosis::report(&head, &d, &read)))
    }

    /// The drift of what `d` compares, read on the first context reaching the destination
    /// that the agent's rules (`policy`, with the rules it approved) name.
    async fn argo_drift(&self, policy: Policy, approved: Vec<usize>, contexts: &[String], d: &Diagnosis) -> DriftRead {
        let Some(on) = contexts.iter().find(|c| policy.context_allowed(c)) else {
            let note = if contexts.is_empty() {
                "no context of the kubeconfig reaches its destination, so its objects were not read".to_string()
            } else {
                format!("your rules allow none of the contexts that reach its destination ({})", contexts.join(", "))
            };
            return DriftRead { note: Some(note), ..Default::default() };
        };
        let mut read = DriftRead { context: Some(on.clone()), ..Default::default() };
        let targets = d.to_read();
        if targets.is_empty() {
            return read;
        }
        let (client, kinds) = match self.hub.client(on).await {
            Ok(c) => c,
            Err(e) => {
                read.note = Some(format!("{on} could not be reached: {}", k8s::short_error(&e)));
                return read;
            }
        };
        let context = on.clone();
        let may_read: MayRead = Arc::new(move |kind: &Kind, res: &Res| {
            let ns = (!res.namespace.is_empty()).then(|| res.namespace.clone());
            super::visible(&policy, &approved, &access_for(kind, &context, Verb::Get, ns, Some(res.name.clone())))
        });
        read.drifts = diagnosis::read_drift(client, on.clone(), Some(kinds), targets, d.since, may_read).await;
        read
    }

    /// Parse a manifest and resolve every object (kind, namespace, a valid name) before
    /// anything is asked or changed.
    pub(super) async fn plan_manifest(
        &self,
        call: &super::Call,
        context: &str,
        namespace: Option<&String>,
        manifest: &str,
    ) -> Result<(kube::Client, Vec<Planned>), String> {
        let (client, kinds) = match self.hub.client(context).await {
            Ok(c) => c,
            Err(e) => return Err(call.finish(Err(e)).unwrap_err()),
        };
        let mut docs: Vec<serde_json::Value> = match crate::yaml::documents(manifest) {
            Ok(d) => d.into_iter().filter(|v| !v.is_null()).collect(),
            Err(e) => return Err(call.finish(Err(anyhow::anyhow!("invalid YAML: {e}"))).unwrap_err()),
        };
        if docs.is_empty() {
            return Err(call.finish(Err(anyhow::anyhow!("no objects in manifest"))).unwrap_err());
        }
        crds_first(&mut docs);
        // The types this manifest's CRDs define: its objects of those types are applied once
        // the API server serves them (see `served`).
        let defined: Vec<Kind> = docs.iter().flat_map(k8s::crd_kinds).collect();
        let mut planned = Vec::new();
        for doc in docs {
            let api_version = doc["apiVersion"].as_str().unwrap_or_default().to_string();
            let kind_name = doc["kind"].as_str().unwrap_or_default().to_string();
            let find = |kinds: &[Kind]| k8s::manifest_kind(kinds, &api_version, &kind_name).cloned();
            let mut kind = find(&kinds).or_else(|| find(&defined));
            if kind.is_none() {
                // A CRD applied by the call before, or installed since discovery ran.
                kind = self.hub.new_kind(context, &api_version, &kind_name).await;
            }
            let Some(kind) = kind else {
                let hint = unknown_hint(&kinds, &api_version, &kind_name);
                return Err(call
                    .finish(Err(anyhow::anyhow!("unknown type {api_version} {kind_name}{hint}")))
                    .unwrap_err());
            };
            let ns =
                doc.pointer("/metadata/namespace").and_then(|v| v.as_str()).map(String::from).or(namespace.cloned());
            let ns = if kind.namespaced { ns } else { None };
            require_ns(call, &kind, ns.as_deref())?;
            let Some(name) = doc.pointer("/metadata/name").and_then(|v| v.as_str()).map(String::from) else {
                return Err(call.fail(format!("a {kind_name} has no metadata.name")));
            };
            if let Some(ns) = &ns {
                k8s::names::validate_namespace(ns).map_err(|e| call.fail(e))?;
            }
            k8s::names::validate_name(&name).map_err(|e| call.fail(e))?;
            planned.push((kind, ns, name, doc));
        }
        Ok((client, planned))
    }

    /// `kind` as the API server serves it now. A type discovery doesn't have is one a CRD
    /// of the same manifest defines (or one applied just before): it is asked of its group
    /// version, waiting for a CRD applied through the hub to be served. `Ok(None)` when it
    /// isn't served; an error when it is served as another resource or scope than the one
    /// the object was authorized as.
    pub(super) async fn served(&self, context: &str, kind: &Kind) -> Result<Option<Kind>, String> {
        let (api_version, kind_name) = (&kind.ar.api_version, &kind.ar.kind);
        let served = match self.hub.known(context, api_version, kind_name) {
            Some(k) => k,
            None => match self.hub.new_kind(context, api_version, kind_name).await {
                Some(k) => k,
                None => return Ok(None),
            },
        };
        if served.ar.plural != kind.ar.plural || served.namespaced != kind.namespaced {
            let scope = |k: &Kind| if k.namespaced { "namespaced" } else { "cluster-scoped" };
            return Err(format!(
                "the API server serves {api_version} {kind_name} as {} ({}), not as {} ({}) as planned",
                served.ar.plural,
                scope(&served),
                kind.ar.plural,
                scope(kind)
            ));
        }
        Ok(Some(served))
    }
}

/// CRDs first, the rest in manifest order, as Helm installs them: an object of a type the
/// manifest defines can only be applied once its CRD is.
fn crds_first(docs: &mut [serde_json::Value]) {
    docs.sort_by_key(|d| !k8s::is_crd(d));
}

/// What to do about a type discovery doesn't have: the `apiVersion` the cluster serves that
/// kind at, if it does. Otherwise, in a group version discovered, when discovery runs again;
/// a group version it hadn't was just asked of the API server (`Hub::new_kind`).
fn unknown_hint(kinds: &[Kind], api_version: &str, kind_name: &str) -> String {
    if let Some(k) = kinds.iter().find(|k| k.ar.kind == kind_name) {
        format!(" (did you mean apiVersion: {}?)", k.ar.api_version)
    } else if kinds.iter().any(|k| k.ar.api_version == api_version) {
        " (a CRD installed outside apply_manifest in the last minute is found when discovery runs again, at most a minute later)".to_string()
    } else {
        " (the API server serves no such type)".to_string()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Op, Res, SyncRequest, approval_detail, crds_first, gitops_verb, ops, unknown_hint};
    use crate::k8s::Kind;
    use crate::mcp::Verb;

    /// A custom resource listed before its CRD is applied after it; the rest keep their order.
    #[test]
    fn crds_are_applied_first() {
        let crd = |name: &str| json!({"apiVersion": "apiextensions.k8s.io/v1", "kind": "CustomResourceDefinition", "metadata": {"name": name}});
        let obj =
            |name: &str| json!({"apiVersion": "kwok.x-k8s.io/v1alpha1", "kind": "Stage", "metadata": {"name": name}});
        let mut docs = vec![obj("fast"), crd("stages.kwok.x-k8s.io"), obj("slow"), crd("attaches.kwok.x-k8s.io")];
        crds_first(&mut docs);
        let order: Vec<&str> = docs.iter().filter_map(|d| d["metadata"]["name"].as_str()).collect();
        assert_eq!(order, ["stages.kwok.x-k8s.io", "attaches.kwok.x-k8s.io", "fast", "slow"]);
    }

    #[test]
    fn an_unknown_type_says_what_to_do() {
        let kinds = vec![Kind::fake("apps", "Deployment")];
        assert_eq!(unknown_hint(&kinds, "apps/v2", "Deployment"), " (did you mean apiVersion: apps/v1?)");
        // A new kind in a group version discovered waits for the next discovery.
        assert!(unknown_hint(&kinds, "apps/v1", "Stage").contains("discovery runs again"));
        // A group version discovery didn't have was asked of the API server just now.
        assert_eq!(unknown_hint(&kinds, "kwok.x-k8s.io/v1alpha1", "Stage"), " (the API server serves no such type)");
    }

    #[test]
    fn each_gitops_action_has_its_verb() {
        let cases = [
            ("refresh", Verb::Refresh),
            ("hard_refresh", Verb::Refresh),
            ("sync", Verb::Sync),
            ("reconcile", Verb::Sync),
            ("suspend", Verb::Suspend),
            ("resume", Verb::Suspend),
            ("terminate", Verb::Sync),
            ("rollback", Verb::Sync),
        ];
        for (action, verb) in cases {
            assert_eq!(gitops_verb(action), Some(verb), "{action}");
        }
        // Never `patch`, and nothing for an action without its own verb.
        assert_eq!(gitops_verb("diagnose"), None);
        assert_eq!(gitops_verb("delete"), None);
    }

    fn app() -> serde_json::Value {
        let source = json!({"repoURL": "https://git.example/r", "path": "web"});
        json!({
            "metadata": {"name": "web", "namespace": "argocd", "resourceVersion": "7"},
            "status": {
                "resources": [
                    {"kind": "ConfigMap", "namespace": "shop", "name": "old", "status": "OutOfSync", "requiresPruning": true}
                ],
                "history": [
                    {"id": 1, "revision": "a".repeat(40), "source": source},
                    {"id": 2, "revision": "b".repeat(40), "source": source}
                ]
            }
        })
    }

    #[test]
    fn the_rollback_approved_is_the_one_sent() {
        let mut op = Op::Rollback { id: None, dry_run: false };
        let detail = approval_detail(&app(), &mut op).unwrap();
        assert_eq!(detail, format!("rollback to deployment 1 ({})", "a".repeat(8)));
        // Sent by its id: a deployment landing after the approval changes nothing.
        assert_eq!(op, Op::Rollback { id: Some(1), dry_run: false });
        let mut later = app();
        later["status"]["history"].as_array_mut().unwrap().push(json!({"id": 3, "revision": "c".repeat(40),
            "source": {"repoURL": "https://git.example/r", "path": "web"}}));
        assert_eq!(ops::patch(&later, &op).unwrap()["operation"]["sync"]["revision"], "a".repeat(40));
    }

    #[test]
    fn the_prune_approved_bounds_what_is_deleted() {
        let mut op = Op::Sync(SyncRequest { prune: true, ..Default::default() });
        assert_eq!(approval_detail(&app(), &mut op).unwrap(), "prune deletes 1: ConfigMap shop/old");
        let Op::Sync(req) = &op else { unreachable!() };
        let shown: Vec<String> = req.prune_shown.iter().flatten().map(Res::label).collect();
        assert_eq!(shown, ["ConfigMap shop/old"]);
        // Git drops another resource after the approval: the sync is refused.
        let mut later = app();
        later["status"]["resources"].as_array_mut().unwrap().push(
            json!({"kind": "ConfigMap", "namespace": "shop", "name": "more", "status": "OutOfSync", "requiresPruning": true}),
        );
        assert!(ops::patch(&later, &op).unwrap_err().contains("ConfigMap shop/more"));
        // A dry run deletes nothing: said so.
        let mut dry = Op::Sync(SyncRequest { prune: true, dry_run: true, ..Default::default() });
        assert!(approval_detail(&app(), &mut dry).unwrap().starts_with("prune would delete 1"));
    }
}
