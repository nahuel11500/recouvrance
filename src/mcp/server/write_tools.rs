//! MCP tools that change things: exec, scale, restart, patch, apply, drain, GitOps, delete.

use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router};
use serde_json::json;

use super::{
    ApplyParams, DeleteParams, DrainParams, ExecParams, GitOpsParams, KubeMcp, PatchParams, RestartParams, ScaleParams,
    access_for, denied_namespaces, denies_eviction, manifest_objects, manifest_summary, require_ns,
};
use crate::k8s;
use crate::k8s::Kind;
use crate::mcp::Mode;

#[tool_router(router = write_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "Run a command in a container and return its output, like `kubectl exec -- cmd` (no tty, time-limited). This can change the container."
    )]
    async fn exec_command(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ExecParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "pod": p.pod, "container": p.container, "command": p.command});
        let mut call = self.begin(&parts, "exec_command", args)?;
        let (client, kind) = self.kind(&call, &p.context, "pods").await?;
        let access = access_for(&kind, &p.context, "exec", Some(p.namespace.clone()), Some(p.pod.clone()));
        self.authorize(&mut call, &access).await?;
        let timeout = u64::from(call.policy().exec_timeout.max(1));
        self.recheck(&mut call, &access)?;
        call.finish(k8s::exec_capture(client, p.namespace, p.pod, p.container, p.command, timeout, 64 * 1024).await)
    }

    #[tool(description = "Scale a workload to a number of replicas, like `kubectl scale`. Supports dry_run.")]
    pub(super) async fn scale(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ScaleParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name, "replicas": p.replicas, "dry_run": p.dry_run});
        let mut call = self.begin(&parts, "scale", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        let access = access_for(&kind, &p.context, "scale", Some(p.namespace.clone()), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        call.finish(k8s::scale(client, kind, Some(p.namespace), p.name, p.replicas, p.dry_run.unwrap_or(false)).await)
    }

    #[tool(
        description = "Restart a Deployment, StatefulSet or DaemonSet, like `kubectl rollout restart`. Supports dry_run."
    )]
    async fn rollout_restart(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<RestartParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name, "dry_run": p.dry_run});
        let mut call = self.begin(&parts, "rollout_restart", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        let access = access_for(&kind, &p.context, "restart", Some(p.namespace.clone()), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        call.finish(k8s::rollout_restart(client, kind, Some(p.namespace), p.name, p.dry_run.unwrap_or(false)).await)
    }

    #[tool(
        description = "Apply a JSON merge patch to one object, like `kubectl patch --type merge`. Supports dry_run."
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
        let access = access_for(&kind, &p.context, "patch", p.namespace.clone(), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        call.finish(k8s::merge_patch(client, kind, p.namespace, p.name, p.patch, p.dry_run.unwrap_or(false)).await)
    }

    #[tool(
        description = "Create or update objects from YAML with server-side apply, like `kubectl apply --server-side`. Each object is authorized separately. Supports dry_run."
    )]
    async fn apply_manifest(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ApplyParams>,
    ) -> Result<String, String> {
        let args =
            json!({"context": p.context, "namespace": p.namespace, "dry_run": p.dry_run, "bytes": p.manifest.len()});
        let mut call = self.begin(&parts, "apply_manifest", args)?;
        let (client, kinds) = match self.hub.client(&p.context).await {
            Ok(c) => c,
            Err(e) => return call.finish(Err(e)),
        };
        let docs: Vec<serde_json::Value> = match crate::yaml::documents(&p.manifest) {
            Ok(d) => d.into_iter().filter(|v| !v.is_null()).collect(),
            Err(e) => return call.finish(Err(anyhow::anyhow!("invalid YAML: {e}"))),
        };
        if docs.is_empty() {
            return call.finish(Err(anyhow::anyhow!("no objects in manifest")));
        }
        // Resolve and check every object before asking or changing anything.
        let mut kinds = kinds;
        let mut planned = Vec::new();
        for doc in docs {
            let api_version = doc["apiVersion"].as_str().unwrap_or_default().to_string();
            let kind_name = doc["kind"].as_str().unwrap_or_default().to_string();
            let find = |kinds: &[Kind]| {
                kinds.iter().find(|k| k.ar.api_version == api_version && k.ar.kind == kind_name).cloned()
            };
            let mut kind = find(&kinds);
            if kind.is_none()
                && let Some(fresh) = self.hub.rediscover(&p.context).await
            {
                kind = find(&fresh);
                kinds = fresh;
            }
            let Some(kind) = kind else {
                return call.finish(Err(anyhow::anyhow!("unknown type {api_version} {kind_name}")));
            };
            let ns =
                doc.pointer("/metadata/namespace").and_then(|v| v.as_str()).map(String::from).or(p.namespace.clone());
            let ns = if kind.namespaced { ns } else { None };
            require_ns(&call, &kind, ns.as_deref())?;
            let Some(name) = doc.pointer("/metadata/name").and_then(|v| v.as_str()).map(String::from) else {
                return Err(call.fail(format!("a {kind_name} has no metadata.name")));
            };
            if let Some(ns) = &ns {
                k8s::names::validate_namespace(ns).map_err(|e| call.fail(e))?;
            }
            k8s::names::validate_name(&name).map_err(|e| call.fail(e))?;
            planned.push((kind, ns, name, doc));
        }
        // What the user approves is the content, not only the names; the log keeps what each object was.
        call.approval_detail = Some(manifest_summary(&planned));
        if let Some(args) = call.args.as_object_mut() {
            args.insert("objects".into(), json!(manifest_objects(&planned)));
        }
        // Each object is approved on its own: approving one does not cover another.
        let mut approved = Vec::new();
        for (kind, ns, name, _) in &planned {
            let access = access_for(kind, &p.context, "apply", ns.clone(), Some(name.clone()));
            call.approved = false;
            self.authorize(&mut call, &access).await?;
            approved.push(call.approved);
        }
        let any_approved = approved.contains(&true);
        let dry = p.dry_run.unwrap_or(false);
        let mut out = Vec::new();
        for ((kind, ns, name, doc), approved) in planned.into_iter().zip(approved) {
            // Approvals for later objects, and applying earlier ones, can take minutes:
            // access may be off, or the rules changed, by now.
            let access = access_for(&kind, &p.context, "apply", ns.clone(), Some(name));
            call.approved = approved;
            let refused = self.stale_reason(&mut call, &access);
            call.approved = any_approved;
            if let Some(reason) = refused {
                // The log says what was applied before.
                let detail =
                    if out.is_empty() { reason } else { format!("{reason}; applied before: {}", out.join("; ")) };
                out.push(call.deny(detail));
                return Err(out.join("\n"));
            }
            match k8s::apply(client.clone(), kind, ns, doc, dry).await {
                Ok(line) => out.push(line),
                Err(e) => {
                    out.push(format!("error: {e:#}"));
                    return call.finish(Err(anyhow::anyhow!(out.join("\n"))));
                }
            }
        }
        call.finish(Ok(out.join("\n")))
    }

    #[tool(
        description = "Cordon a node and evict its pods, like `kubectl drain --ignore-daemonsets`. PodDisruptionBudgets are respected; refused evictions are reported. Refused (nothing is changed) when the node runs pods in namespaces your rules deny `delete` on pods for."
    )]
    async fn drain_node(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<DrainParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "node": p.node});
        let mut call = self.begin(&parts, "drain_node", args)?;
        let (client, kind) = self.kind(&call, &p.context, "nodes").await?;
        let access = access_for(&kind, &p.context, "drain", None, Some(p.node.clone()));
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
            let a = access_for(&pods_kind, &p.context, "delete", Some(ns.into()), Some(name.into()));
            denies_eviction(call.policy(), &a)
        });
        if !denied.is_empty() {
            return Err(call.deny(format!(
                "node {} runs pods in namespaces your rules deny (delete pods): {}; nothing was changed",
                p.node,
                denied.join(", ")
            )));
        }
        self.recheck(&mut call, &access)?;
        let stop = self.still_allowed(&call, &access);
        // The drain lists the pods again and takes minutes on a big node: a Deny on deleting
        // a pod, added while it runs, keeps that pod in place.
        let (hub, agent_id, context, kind) =
            (self.hub.clone(), call.agent.id.clone(), p.context.clone(), pods_kind.clone());
        let keep = move |ns: &str, name: &str| {
            let a = access_for(&kind, &context, "delete", Some(ns.into()), Some(name.into()));
            match hub.agent(&agent_id) {
                Some(agent) if !denies_eviction(&agent.policy, &a) => None,
                Some(_) => Some("your rules deny deleting it".to_string()),
                None => Some("this agent is disabled".to_string()),
            }
        };
        // The reply names only the pods the agent may list.
        let shown = |ns: &str, name: &str| {
            let a = access_for(&pods_kind, &p.context, "list", Some(ns.into()), Some(name.into()));
            call.policy().decide(&a).mode == Mode::Allow
        };
        call.finish(k8s::drain_report(client, p.node, stop, keep).await.map(|d| d.summary(shown)))
    }

    #[tool(
        description = "GitOps actions: Flux `reconcile` (like `flux reconcile`), `suspend`, `resume`; Argo CD Application `refresh` or `sync`. Authorized as `patch` on the object."
    )]
    async fn gitops(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<GitOpsParams>,
    ) -> Result<String, String> {
        let args =
            json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name, "action": p.action});
        let mut call = self.begin(&parts, "gitops", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        let flux = kind.ar.group.ends_with(".toolkit.fluxcd.io");
        let argo = kind.is("argoproj.io", "Application");
        let ok = match p.action.as_str() {
            "reconcile" | "suspend" | "resume" => flux,
            "refresh" | "sync" => argo,
            _ => false,
        };
        if !ok {
            return call.finish(Err(anyhow::anyhow!(
                "`{}` does not apply to {} (Flux: reconcile/suspend/resume; Argo CD applications: refresh/sync)",
                p.action,
                kind.ar.kind
            )));
        }
        let access = access_for(&kind, &p.context, "patch", Some(p.namespace.clone()), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        let (ns, name) = (Some(p.namespace), p.name);
        call.finish(match p.action.as_str() {
            "reconcile" => k8s::flux_reconcile(client, kind, ns, name).await,
            "suspend" => k8s::merge_patch(client, kind, ns, name, json!({"spec": {"suspend": true}}), false).await,
            "resume" => k8s::merge_patch(client, kind, ns, name, json!({"spec": {"suspend": false}}), false).await,
            "refresh" => k8s::argo_refresh(client, kind, ns, name).await,
            _ => k8s::argo_sync(client, kind, ns, name).await,
        })
    }

    #[tool(description = "Delete one object, like `kubectl delete`. Supports dry_run.")]
    pub(super) async fn delete_resource(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<DeleteParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name, "dry_run": p.dry_run});
        let mut call = self.begin(&parts, "delete_resource", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        require_ns(&call, &kind, p.namespace.as_deref())?;
        let access = access_for(&kind, &p.context, "delete", p.namespace.clone(), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        call.finish(k8s::delete(client, kind, p.namespace, p.name, None, p.dry_run.unwrap_or(false)).await)
    }
}
