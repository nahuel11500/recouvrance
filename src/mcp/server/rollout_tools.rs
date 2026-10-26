//! MCP tools for rollouts: history, undo and status, like `kubectl rollout`.

use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use super::{KubeMcp, access_for, check_identity};
use crate::k8s::{self, ObjectRef, rollouts};
use crate::mcp::Verb;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RolloutParams {
    pub context: String,
    #[schemars(description = "deployments, statefulsets or daemonsets")]
    pub kind: String,
    pub namespace: String,
    pub name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct UndoParams {
    pub context: String,
    #[schemars(description = "deployments, statefulsets or daemonsets")]
    pub kind: String,
    pub namespace: String,
    pub name: String,
    #[schemars(description = "the revision to go back to (see rollout_history); omit for the previous one")]
    pub to_revision: Option<i64>,
    pub dry_run: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct StatusParams {
    pub context: String,
    #[schemars(description = "deployments, statefulsets or daemonsets")]
    pub kind: String,
    pub namespace: String,
    pub name: String,
    #[schemars(description = "wait up to this many seconds for the rollout to finish (default 0, at most 300)")]
    pub wait_seconds: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PauseParams {
    pub context: String,
    #[schemars(description = "the Deployment")]
    pub name: String,
    pub namespace: String,
    pub dry_run: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SetImageParams {
    pub context: String,
    #[schemars(description = "deployments, statefulsets, daemonsets, replicasets, jobs, cronjobs or pods")]
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub container: String,
    pub image: String,
    pub dry_run: Option<bool>,
}

const ROLLOUT_KINDS: [&str; 3] = ["Deployment", "StatefulSet", "DaemonSet"];

#[tool_router(router = rollout_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "Pause a Deployment's rollouts, like `kubectl rollout pause`: changes to its template wait until resumed. Needs patch. Supports dry_run."
    )]
    async fn rollout_pause(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<PauseParams>,
    ) -> Result<String, String> {
        self.set_paused(&parts, "rollout_pause", p, true).await
    }

    #[tool(
        description = "Resume a paused Deployment's rollouts, like `kubectl rollout resume`. Needs patch. Supports dry_run."
    )]
    async fn rollout_resume(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<PauseParams>,
    ) -> Result<String, String> {
        self.set_paused(&parts, "rollout_resume", p, false).await
    }

    #[tool(
        description = "Set one container's image, like `kubectl set image` (a workload starts a rollout). Needs patch. Supports dry_run."
    )]
    async fn set_image(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SetImageParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name, "container": p.container, "image": p.image, "dry_run": p.dry_run});
        let mut call = self.begin(&parts, "set_image", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        let access = access_for(&kind, &p.context, Verb::Patch, Some(p.namespace.clone()), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        let obj = ObjectRef {
            kind: kind.clone(),
            name: p.name.clone(),
            namespace: Some(p.namespace.clone()),
            uid: String::new(),
        };
        let value = match k8s::get_object(client.clone(), obj).await {
            Ok(v) => v,
            Err(e) => return call.finish(Err(e)),
        };
        if let Err(e) = check_identity(&kind, Some(&p.namespace), &p.name, &value) {
            return call.finish(Err(e));
        }
        let patch = match rollouts::image_patch(&kind, &value, &p.container, &p.image) {
            Ok(patch) => patch,
            Err(e) => return call.finish(Err(anyhow::anyhow!(e))),
        };
        self.recheck(&mut call, &access)?;
        let done =
            k8s::strategic_patch(client, kind, Some(p.namespace), p.name, patch, p.dry_run.unwrap_or(false)).await;
        call.finish(done.map(|text| format!("{text}: {} → {}", p.container, p.image)))
    }

    #[tool(
        description = "A workload's revisions, newest first, with their images and change cause, like `kubectl rollout history`.",
        annotations(read_only_hint = true)
    )]
    async fn rollout_history(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<RolloutParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name});
        let mut call = self.begin(&parts, "rollout_history", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        let access = access_for(&kind, &p.context, Verb::Get, Some(p.namespace.clone()), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        let obj = match self.workload(&client, &kind, &p.namespace, &p.name).await {
            Ok(o) => o,
            Err(e) => return call.finish(Err(e)),
        };
        let mut revs = match rollouts::revisions(client, &kind, p.namespace, &obj).await {
            Ok(r) => r,
            Err(e) => return call.finish(Err(e)),
        };
        revs.sort_by_key(|r| std::cmp::Reverse(rollouts::number(r)));
        let current = revs.first().map(rollouts::number);
        let lines: Vec<String> = revs
            .iter()
            .map(|r| {
                let n = rollouts::number(r);
                let created = r.pointer("/metadata/creationTimestamp").and_then(|v| v.as_str()).unwrap_or("?");
                let cause = r
                    .pointer("/metadata/annotations/kubernetes.io~1change-cause")
                    .and_then(|v| v.as_str())
                    .map(|c| format!(" — {c}"))
                    .unwrap_or_default();
                let mark = if Some(n) == current { " (current)" } else { "" };
                format!("revision {n}{mark}, {created}: {}{cause}", rollouts::images(r).join(", "))
            })
            .collect();
        call.finish(Ok(if lines.is_empty() { "No revisions.".into() } else { lines.join("\n") }))
    }

    #[tool(
        description = "Roll a Deployment, StatefulSet or DaemonSet back to an earlier revision (the previous one by default), like `kubectl rollout undo`. Supports dry_run."
    )]
    async fn rollout_undo(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UndoParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name, "to_revision": p.to_revision, "dry_run": p.dry_run});
        let mut call = self.begin(&parts, "rollout_undo", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        let access = access_for(&kind, &p.context, Verb::Patch, Some(p.namespace.clone()), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        let obj = match self.workload(&client, &kind, &p.namespace, &p.name).await {
            Ok(o) => o,
            Err(e) => return call.finish(Err(e)),
        };
        let revs = match rollouts::revisions(client.clone(), &kind, p.namespace.clone(), &obj).await {
            Ok(r) => r,
            Err(e) => return call.finish(Err(e)),
        };
        let target = match rollouts::undo_target(&revs, p.to_revision) {
            Ok(t) => t,
            Err(e) => return call.finish(Err(anyhow::anyhow!(e))),
        };
        let (number, template) =
            (rollouts::number(target), target.pointer("/spec/template").cloned().unwrap_or_default());
        self.recheck(&mut call, &access)?;
        let done = k8s::rollback(client, kind, Some(p.namespace), p.name, template, p.dry_run.unwrap_or(false)).await;
        call.finish(done.map(|text| format!("{text} to revision {number}")))
    }

    #[tool(
        description = "Whether a workload's rollout is done, like `kubectl rollout status`; can wait for it.",
        annotations(read_only_hint = true)
    )]
    async fn rollout_status(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<StatusParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name, "wait_seconds": p.wait_seconds});
        let mut call = self.begin(&parts, "rollout_status", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        let access = access_for(&kind, &p.context, Verb::Get, Some(p.namespace.clone()), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        let deadline = tokio::time::Instant::now()
            + std::time::Duration::from_secs(u64::from(p.wait_seconds.unwrap_or(0).min(300)));
        loop {
            let obj = match self.workload(&client, &kind, &p.namespace, &p.name).await {
                Ok(o) => o,
                Err(e) => return call.finish(Err(e)),
            };
            let (done, text) = rollouts::status(&kind, &obj);
            if done || tokio::time::Instant::now() >= deadline {
                return call.finish(Ok(text));
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            // Switched off, or the rules changed, while waiting: stop.
            self.recheck(&mut call, &access)?;
        }
    }
}

impl KubeMcp {
    async fn set_paused(
        &self,
        parts: &Parts,
        tool: &'static str,
        p: PauseParams,
        paused: bool,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "name": p.name, "dry_run": p.dry_run});
        let mut call = self.begin(parts, tool, args)?;
        let (client, kind) = self.kind(&call, &p.context, "deployments").await?;
        let access = access_for(&kind, &p.context, Verb::Patch, Some(p.namespace.clone()), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        self.recheck(&mut call, &access)?;
        let patch = json!({"spec": {"paused": paused}});
        let done =
            k8s::merge_patch(client, kind, Some(p.namespace), p.name.clone(), patch, p.dry_run.unwrap_or(false)).await;
        call.finish(done.map(|text| format!("{text}: {}", if paused { "paused" } else { "resumed" })))
    }

    /// The workload asked for, checked to be the one asked for, of a kind that rolls out.
    async fn workload(
        &self,
        client: &kube::Client,
        kind: &k8s::Kind,
        ns: &str,
        name: &str,
    ) -> anyhow::Result<serde_json::Value> {
        anyhow::ensure!(
            kind.ar.group == "apps" && ROLLOUT_KINDS.contains(&kind.ar.kind.as_str()),
            "{} has no rollouts (Deployments, StatefulSets and DaemonSets do)",
            kind.ar.kind
        );
        let obj = ObjectRef { kind: kind.clone(), name: name.into(), namespace: Some(ns.into()), uid: String::new() };
        let value = k8s::get_object(client.clone(), obj).await?;
        check_identity(kind, Some(ns), name, &value)?;
        Ok(value)
    }
}
