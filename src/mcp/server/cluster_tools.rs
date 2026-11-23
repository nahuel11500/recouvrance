//! MCP tools that answer questions about the cluster itself, like `kubectl explain`,
//! `kubectl auth can-i`, `kubectl version` and `kubectl wait`.

use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ContextParam, KubeMcp, access_for, get_named, problem_visible, require_ns};
use crate::k8s;
use crate::mcp::Verb;
use crate::schema::{Explained, Schema};

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ExplainParams {
    pub context: String,
    #[schemars(description = "resource type, e.g. deployments, pods, certificates.cert-manager.io")]
    pub kind: String,
    #[schemars(description = "dotted field path, e.g. spec.template.spec.containers.resources; omit for the top")]
    pub field: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CanIParams {
    pub context: String,
    #[schemars(description = "Kubernetes verb: get, list, watch, create, update, patch, delete…")]
    pub verb: String,
    pub kind: String,
    pub namespace: Option<String>,
    pub name: Option<String>,
    #[schemars(description = "a subresource, e.g. log, exec, scale, eviction")]
    pub subresource: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WaitParams {
    pub context: String,
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
    #[schemars(
        description = "a condition type that must be True (Ready, Available, Complete…), `phase=Running`, `deleted`, or for an Argo CD Application `sync=Synced`, `health=Healthy` or `operation=Succeeded`"
    )]
    pub condition: String,
    #[schemars(description = "how long to wait, in seconds (default 60, at most 300)")]
    pub timeout_seconds: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct UpgradeParams {
    pub context: String,
    #[schemars(description = "the minor version to check against, e.g. \"1.33\"; the cluster's next one when omitted")]
    pub target: Option<String>,
}

#[tool_router(router = cluster_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "Describe a resource type's fields from the cluster's OpenAPI schema (CRDs included), like `kubectl explain`.",
        annotations(read_only_hint = true)
    )]
    async fn explain_resource(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ExplainParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "field": p.field});
        let call = self.begin(&parts, "explain_resource", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        let doc = match crate::schema::document(&p.context, client, &kind.ar.api_version).await {
            Ok(d) => d,
            Err(e) => return call.finish(Err(e)),
        };
        let Some(schema) = Schema::find(doc, &kind.ar.api_version, &kind.ar.kind) else {
            return call.finish(Err(anyhow::anyhow!("no schema for {} {}", kind.ar.api_version, kind.ar.kind)));
        };
        let path: Vec<String> =
            p.field.as_deref().unwrap_or_default().split('.').filter(|s| !s.is_empty()).map(String::from).collect();
        call.finish(match schema.explain(&path) {
            Some(e) => Ok(explained_text(&kind.ar.api_version, &kind.ar.kind, &e)),
            None => Err(anyhow::anyhow!("no field {} in {}", path.join("."), kind.ar.kind)),
        })
    }

    #[tool(
        description = "Whether the cluster's RBAC lets the kubeconfig user do something, like `kubectl auth can-i` (your own rules here are a separate matter: see my_permissions).",
        annotations(read_only_hint = true)
    )]
    async fn can_i(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<CanIParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "verb": p.verb, "kind": p.kind, "namespace": p.namespace, "name": p.name, "subresource": p.subresource});
        let call = self.begin(&parts, "can_i", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        let answer = k8s::can_i_checked(client, kind, &p.verb, p.namespace, p.name, p.subresource.as_deref()).await;
        call.finish(answer.map(|(allowed, reason)| {
            let mut text = if allowed { "yes" } else { "no" }.to_string();
            if let Some(r) = reason {
                text.push_str(&format!(" ({r})"));
            }
            text
        }))
    }

    #[tool(
        description = "The cluster's Kubernetes version and platform, like `kubectl version`.",
        annotations(read_only_hint = true)
    )]
    async fn cluster_version(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ContextParam>,
    ) -> Result<String, String> {
        let call = self.begin(&parts, "cluster_version", json!({"context": p.context}))?;
        let (client, _) = match self.hub.client(&p.context).await {
            Ok(c) => c,
            Err(e) => return call.finish(Err(e)),
        };
        let version = k8s::get_json(client, "/version".into()).await;
        call.finish(version.map_or_else(
            || Err(anyhow::anyhow!("the API server did not answer /version")),
            |v| {
                let s = |k: &str| v[k].as_str().unwrap_or("?").to_string();
                Ok(format!("{} ({}, built {})", s("gitVersion"), s("platform"), s("buildDate")))
            },
        ))
    }

    #[tool(
        description = "What needs action before upgrading the cluster to a newer minor version (the next one by default), most urgent first: API versions the target removes that are still used (objects written in them, Helm release manifests, the API server's deprecated-API metric), kubelet and kube-proxy version skew, PodDisruptionBudgets that would block drains, deprecated or alpha feature gates, and add-on CRD versions marked deprecated. Each finding says what to do and lists its objects. Asks to list pods in all namespaces; objects of other kinds show where an Allow rule covers them, and Helm releases' where listing their Secret is allowed.",
        annotations(read_only_hint = true)
    )]
    async fn upgrade_check(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<UpgradeParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "target": p.target});
        let mut call = self.begin(&parts, "upgrade_check", args)?;
        let target = p.target.as_deref().map(crate::upgrade::minor_of);
        if target == Some(None) {
            let t = p.target.as_deref().unwrap_or_default();
            return Err(call.fail(format!("target {t:?} is not a Kubernetes 1.x version like \"1.33\"")));
        }
        let target = target.flatten();
        let (client, pods) = self.kind(&call, &p.context, "pods").await?;
        let access = access_for(&pods, &p.context, Verb::List, None, None);
        self.authorize_list(&mut call, &access, true).await?;
        let kinds = match self.hub.client(&p.context).await {
            Ok((_, kinds)) => kinds,
            Err(e) => return call.finish(Err(e)),
        };
        let report = match crate::upgrade::check(client, Ok(kinds.to_vec()), target).await {
            Ok(r) => r,
            Err(e) => return call.finish(Err(e)),
        };
        let secrets = k8s::find_kind(&kinds, "secrets").cloned();
        // Like find_problems: the approval named pods; other kinds need an Allow rule.
        let visible = |o: &crate::upgrade::Object| {
            let (kind, ns, name) = match &o.helm {
                Some(h) => (secrets.as_ref(), Some(h.namespace.clone()), h.secret_name()),
                None => (k8s::find_kind(&kinds, &o.kind), o.namespace.clone(), o.name.clone()),
            };
            kind.is_some_and(|k| {
                let a = access_for(k, &p.context, Verb::List, ns, Some(name));
                problem_visible(call.policy(), call.approved_rules(), k.is("", "Pod"), &a)
            })
        };
        let text = crate::upgrade::to_text(&p.context, &report, visible);
        call.finish(Ok(text))
    }

    #[tool(
        description = "Wait until an object meets a condition (Ready, Available, Complete, `phase=Running`) or is deleted, like `kubectl wait`. For an Argo CD Application (on its hub): `sync=Synced`, `health=Healthy` or `operation=Succeeded`, met only once no operation runs; it returns at once with Argo CD's message when its last operation failed.",
        annotations(read_only_hint = true)
    )]
    async fn wait_for(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<WaitParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name, "condition": p.condition, "timeout_seconds": p.timeout_seconds});
        let mut call = self.begin(&parts, "wait_for", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        require_ns(&call, &kind, p.namespace.as_deref())?;
        if argo_condition(&p.condition).is_some() && !kind.is("argoproj.io", "Application") {
            return Err(call.fail(format!("{} is a condition of Argo CD Applications", p.condition)));
        }
        let access = access_for(&kind, &p.context, Verb::Get, p.namespace.clone(), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        let ns = if kind.namespaced { p.namespace.clone() } else { None };
        let secs = u64::from(p.timeout_seconds.unwrap_or(60).min(300));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
        let deleted = p.condition == "deleted";
        loop {
            let state = match get_named(&client, &kind, ns.as_deref(), &p.name, false).await {
                Ok(_) if deleted => None,
                Ok(v) => match condition_met(&v, &p.condition) {
                    Ok(state) => state,
                    // Argo CD won't get there on its own: say why now rather than at the timeout.
                    Err(why) => return call.finish(Err(anyhow::anyhow!("{} {}: {why}", kind.ar.kind, p.name))),
                },
                Err(e) if k8s::is_not_found(&e) => deleted.then_some(true),
                Err(e) => return call.finish(Err(e)),
            };
            if state == Some(true) {
                return call.finish(Ok(format!("{} {}: {}", kind.ar.kind, p.name, p.condition)));
            }
            if tokio::time::Instant::now() >= deadline {
                return call.finish(Err(anyhow::anyhow!("timed out after {secs}s waiting for {}", p.condition)));
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            // Switched off, or the rules changed, while waiting: stop.
            self.recheck(&mut call, &access)?;
        }
    }
}

/// Whether `obj` meets `condition`: `phase=X`, an Argo CD Application's `sync=X`,
/// `health=X` or `operation=X`, or a condition type whose status is True. Ok(None) while the
/// object doesn't report it yet; Err when it won't get there without someone acting.
fn condition_met(obj: &Value, condition: &str) -> Result<Option<bool>, String> {
    if let Some(phase) = condition.strip_prefix("phase=") {
        return Ok(obj.pointer("/status/phase").and_then(Value::as_str).map(|p| p.eq_ignore_ascii_case(phase)));
    }
    if let Some((field, want)) = argo_condition(condition) {
        return argo_met(obj, field, want);
    }
    Ok(obj
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .and_then(|conds| conds.iter().find(|c| c["type"].as_str().is_some_and(|t| t.eq_ignore_ascii_case(condition))))
        .map(|c| c["status"] == "True"))
}

/// Where an Application's status keeps `operation`.
const OPERATION_PHASE: &str = "/status/operationState/phase";

/// An Argo CD Application's condition: where its status keeps it, and the value wanted.
/// Argo CD keeps them outside `status.conditions`, which holds only its errors and warnings.
fn argo_condition(condition: &str) -> Option<(&'static str, &str)> {
    let (what, want) = condition.split_once('=')?;
    let field = match what.trim() {
        "sync" => "/status/sync/status",
        "health" => "/status/health/status",
        "operation" => OPERATION_PHASE,
        _ => return None,
    };
    Some((field, want.trim()))
}

/// Whether an Application's `field` reads `want`. While an operation runs, its sync and health
/// are those from before it, so nothing is met until it ends (but `operation=Running`). Once
/// its last operation failed, a sync won't start again on its own (auto-sync doesn't retry a
/// revision that failed): that is an Err with Argo CD's message, unless what was waited for
/// is already there.
fn argo_met(obj: &Value, field: &str, want: &str) -> Result<Option<bool>, String> {
    let text = |p: &str| obj.pointer(p).and_then(Value::as_str).filter(|s| !s.is_empty());
    let phase = text(OPERATION_PHASE);
    let running = k8s::argo::ops::in_progress(obj) || matches!(phase, Some("Running" | "Terminating"));
    let is = |v: Option<&str>| v.is_some_and(|v| v.eq_ignore_ascii_case(want));
    if running {
        return Ok(Some(field == OPERATION_PHASE && is(phase)));
    }
    let value = text(field);
    if is(value) {
        return Ok(Some(true));
    }
    if let Some(failed @ ("Failed" | "Error")) = phase {
        let at = text("/status/operationState/finishedAt").map_or_default(|t| format!(" at {t}"));
        let message = text("/status/operationState/message").map_or_default(|m| format!(": {m}"));
        return Err(format!("its last operation ended {failed}{at}{message}"));
    }
    Ok(value.map(|_| false))
}

/// `kubectl explain` style text.
fn explained_text(api_version: &str, kind: &str, e: &Explained) -> String {
    let mut out = format!("KIND: {kind}\nVERSION: {api_version}\n\n");
    if !e.path.is_empty() {
        out.push_str(&format!("FIELD: {} <{}>\n", e.path.join("."), e.field.type_name));
    }
    out.push_str(&format!("DESCRIPTION:\n  {}\n", e.field.description.replace('\n', "\n  ")));
    if !e.field.enum_values.is_empty() {
        out.push_str(&format!("ENUM: {}\n", e.field.enum_values.join(", ")));
    }
    if !e.children.is_empty() {
        out.push_str("\nFIELDS:\n");
        for f in &e.children {
            let required = if f.required { " -required-" } else { "" };
            let first = f.description.lines().next().unwrap_or_default();
            out.push_str(&format!("  {} <{}>{required}\n    {first}\n", f.name, f.type_name));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{argo_condition, condition_met};

    #[test]
    fn conditions_and_phases() {
        let pod = json!({"status": {"phase": "Running", "conditions": [
            {"type": "Ready", "status": "True"}, {"type": "PodScheduled", "status": "True"},
            {"type": "ContainersReady", "status": "False"},
        ]}});
        assert_eq!(condition_met(&pod, "Ready"), Ok(Some(true)));
        assert_eq!(condition_met(&pod, "ready"), Ok(Some(true)));
        assert_eq!(condition_met(&pod, "ContainersReady"), Ok(Some(false)));
        assert_eq!(condition_met(&pod, "Available"), Ok(None), "not reported yet");
        assert_eq!(condition_met(&pod, "phase=Running"), Ok(Some(true)));
        assert_eq!(condition_met(&pod, "phase=Succeeded"), Ok(Some(false)));
        assert_eq!(condition_met(&json!({}), "Ready"), Ok(None));
    }

    fn app(sync: &str, health: &str, phase: Option<&str>, running: bool) -> serde_json::Value {
        let mut app = json!({"status": {"sync": {"status": sync}, "health": {"status": health}}});
        if let Some(phase) = phase {
            app["status"]["operationState"] = json!({"phase": phase,
                "message": "one or more objects failed to apply", "finishedAt": "2026-11-23T10:00:00Z"});
        }
        if running {
            app["operation"] = json!({"sync": {}});
        }
        app
    }

    /// An Application's sync and health are read where Argo CD keeps them, and only once no
    /// operation runs: until then they are those from before it.
    #[test]
    fn an_application_is_synced_and_healthy_once_its_operation_ends() {
        let done = app("Synced", "Healthy", Some("Succeeded"), false);
        assert_eq!(condition_met(&done, "sync=Synced"), Ok(Some(true)));
        assert_eq!(condition_met(&done, "health=healthy"), Ok(Some(true)));
        assert_eq!(condition_met(&done, "operation=Succeeded"), Ok(Some(true)));
        assert_eq!(condition_met(&done, "health=Degraded"), Ok(Some(false)));
        // A sync asked for, not started yet, or running: what it shows is from before.
        let asked = app("Synced", "Healthy", Some("Succeeded"), true);
        assert_eq!(condition_met(&asked, "sync=Synced"), Ok(Some(false)));
        assert_eq!(condition_met(&asked, "operation=Succeeded"), Ok(Some(false)));
        let running = app("OutOfSync", "Progressing", Some("Running"), false);
        assert_eq!(condition_met(&running, "health=Progressing"), Ok(Some(false)));
        assert_eq!(condition_met(&running, "operation=Running"), Ok(Some(true)));
        // Never synced: nothing to wait on but the next sync.
        assert_eq!(condition_met(&app("OutOfSync", "Missing", None, false), "sync=Synced"), Ok(Some(false)));
        assert_eq!(condition_met(&json!({}), "sync=Synced"), Ok(None));
        // Not an Argo CD condition: a condition type, as before.
        assert_eq!(argo_condition("Ready"), None);
        assert_eq!(argo_condition("synced=yes"), None);
    }

    /// A sync that failed stops the wait at once with Argo CD's message, unless what was
    /// waited for is there.
    #[test]
    fn a_failed_sync_ends_the_wait_with_argo_cds_message() {
        let failed = app("OutOfSync", "Healthy", Some("Failed"), false);
        for condition in ["sync=Synced", "operation=Succeeded", "health=Degraded"] {
            let err = condition_met(&failed, condition).expect_err(condition);
            assert!(err.contains("ended Failed at 2026-11-23T10:00:00Z: one or more objects failed"), "{err}");
        }
        assert_eq!(condition_met(&failed, "operation=Failed"), Ok(Some(true)));
        assert_eq!(condition_met(&failed, "health=Healthy"), Ok(Some(true)));
        assert!(condition_met(&app("OutOfSync", "Healthy", Some("Error"), false), "sync=Synced").is_err());
        // A new sync running after it: wait for that one.
        assert_eq!(condition_met(&app("OutOfSync", "Healthy", Some("Failed"), true), "sync=Synced"), Ok(Some(false)));
    }
}
