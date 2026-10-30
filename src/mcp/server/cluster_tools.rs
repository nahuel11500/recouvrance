//! MCP tools that answer questions about the cluster itself, like `kubectl explain`,
//! `kubectl auth can-i`, `kubectl version` and `kubectl wait`.

use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{ContextParam, KubeMcp, access_for, check_identity, problem_visible, require_ns};
use crate::k8s::{self, ObjectRef};
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
        description = "a condition type that must be True (Ready, Available, Complete…), `phase=Running`, or `deleted`"
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
        description = "Wait until an object meets a condition (Ready, Available, Complete, `phase=Running`) or is deleted, like `kubectl wait`.",
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
        let access = access_for(&kind, &p.context, Verb::Get, p.namespace.clone(), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        let ns = if kind.namespaced { p.namespace.clone() } else { None };
        let secs = u64::from(p.timeout_seconds.unwrap_or(60).min(300));
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
        let deleted = p.condition == "deleted";
        loop {
            let obj = ObjectRef { kind: kind.clone(), name: p.name.clone(), namespace: ns.clone(), uid: String::new() };
            let state = match k8s::get_object(client.clone(), obj).await {
                Ok(v) => {
                    if let Err(e) = check_identity(&kind, ns.as_deref(), &p.name, &v) {
                        return call.finish(Err(e));
                    }
                    (!deleted).then(|| condition_met(&v, &p.condition)).flatten()
                }
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

/// Whether `obj` meets `condition`: `phase=X`, or a condition type whose status is True.
/// None while the object doesn't report it yet.
fn condition_met(obj: &Value, condition: &str) -> Option<bool> {
    if let Some(phase) = condition.strip_prefix("phase=") {
        return obj.pointer("/status/phase").and_then(Value::as_str).map(|p| p.eq_ignore_ascii_case(phase));
    }
    obj.pointer("/status/conditions")?
        .as_array()?
        .iter()
        .find(|c| c["type"].as_str().is_some_and(|t| t.eq_ignore_ascii_case(condition)))
        .map(|c| c["status"] == "True")
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

    use super::condition_met;

    #[test]
    fn conditions_and_phases() {
        let pod = json!({"status": {"phase": "Running", "conditions": [
            {"type": "Ready", "status": "True"}, {"type": "PodScheduled", "status": "True"},
            {"type": "ContainersReady", "status": "False"},
        ]}});
        assert_eq!(condition_met(&pod, "Ready"), Some(true));
        assert_eq!(condition_met(&pod, "ready"), Some(true));
        assert_eq!(condition_met(&pod, "ContainersReady"), Some(false));
        assert_eq!(condition_met(&pod, "Available"), None, "not reported yet");
        assert_eq!(condition_met(&pod, "phase=Running"), Some(true));
        assert_eq!(condition_met(&pod, "phase=Succeeded"), Some(false));
        assert_eq!(condition_met(&json!({}), "Ready"), None);
    }
}
