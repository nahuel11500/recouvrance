//! The MCP server. Every tool call is turned into an access request
//! (context, verb, resource, namespace, name), checked against the calling
//! agent's rules, possibly sent to the user for approval, and logged.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{PromptMessage, Role};
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ServerHandler, prompt, prompt_handler, prompt_router, schemars, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::json;

use super::{Access, Agent, AuditEntry, Decision, Hub, HubEvent, Mode, Policy, is_write};
use crate::k8s::{self, Kind, ObjectRef, TaskGuard};

mod read_tools;
mod write_tools;
#[allow(unused_imports)]
pub(crate) use read_tools::*;
#[allow(unused_imports)]
pub(crate) use write_tools::*;

/// Set by the auth middleware for every authenticated request.
#[derive(Clone)]
struct AgentId(String);

#[derive(Clone)]
pub struct KubeMcp {
    hub: Arc<Hub>,
}

/// One tool call in flight.
struct Call {
    hub: Arc<Hub>,
    agent: Agent,
    tool: &'static str,
    args: serde_json::Value,
    start: Instant,
    approved: bool,
    /// The Ask rules the approval covered: the one that asked, or across all namespaces
    /// every Ask rule of the request.
    approved_rules: Vec<usize>,
    /// The rule that allowed the call, for the activity log.
    rule: Option<usize>,
    session: Option<String>,
    /// More for the approval banner than the arguments show (a manifest's objects).
    approval_detail: Option<String>,
}

impl Call {
    fn policy(&self) -> &Policy {
        &self.agent.policy
    }

    fn rule_label(&self, rule: Option<usize>) -> String {
        match rule {
            Some(i) => {
                let note = self.policy().rules.get(i).map(|r| r.note.clone()).unwrap_or_default();
                if note.is_empty() { format!("rule #{}", i + 1) } else { format!("rule #{} ({note})", i + 1) }
            }
            None => "no matching rule".into(),
        }
    }

    fn record(&self, decision: Decision, detail: String, result_bytes: usize) {
        self.hub.record(AuditEntry {
            time: jiff::Timestamp::now(),
            agent_id: self.agent.id.clone(),
            agent: self.agent.name.clone(),
            tool: self.tool.to_string(),
            args: self.args.clone(),
            decision,
            detail,
            duration_ms: self.start.elapsed().as_millis() as u64,
            result_bytes,
            session: self.session.clone(),
        });
    }

    fn deny(&self, reason: impl Into<String>) -> String {
        let reason = reason.into();
        self.record(Decision::Denied, reason.clone(), 0);
        format!("Denied by Recouvrance policy: {reason}")
    }

    /// Log a failed call (bad arguments) and return its message.
    fn fail(&self, msg: impl Into<String>) -> String {
        let msg = msg.into();
        self.record(Decision::Failed, msg.clone(), 0);
        msg
    }

    /// Whether one row of a list result may be shown: allowed by a rule, or gated by
    /// an Ask rule the user approved for this call.
    fn may_see(&self, a: &Access) -> bool {
        let v = self.policy().decide(a);
        match v.mode {
            Mode::Allow => true,
            Mode::Ask => self.approved && v.rule.is_some_and(|r| self.approved_rules.contains(&r)),
            Mode::Deny => false,
        }
    }

    fn finish(&self, result: anyhow::Result<String>) -> Result<String, String> {
        match result {
            Ok(text) => {
                let decision = if self.approved { Decision::Approved } else { Decision::Allowed };
                // Calls that aren't rule-checked (list_contexts) have no rule to show.
                let detail = if self.rule.is_some() { self.rule_label(self.rule) } else { String::new() };
                self.record(decision, detail, text.len());
                Ok(text)
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.record(Decision::Failed, msg.clone(), 0);
                Err(msg)
            }
        }
    }
}

fn describe(a: &Access) -> String {
    let mut s = format!("{} {}", a.verb, a.resource);
    if let Some(n) = &a.name {
        s.push_str(&format!(" {n}"));
    }
    match &a.namespace {
        Some(ns) => s.push_str(&format!(" -n {ns}")),
        None => s.push_str(" (all namespaces / cluster)"),
    }
    s.push_str(&format!(" --context {}", a.context));
    s
}

/// Arguments that change what a call does, for the approval banner: what the user
/// approves is what runs. Values come from the (redacted) logged arguments.
const KEY_ARGS: [&str; 6] = ["command", "patch", "replicas", "action", "container", "dry_run"];
/// Most log bytes one get_logs call returns.
const MAX_LOG_BYTES: i64 = 1024 * 1024;
/// The banner scrolls; this only bounds pathological inputs.
const MAX_ARG_CHARS: usize = 20_000;

fn arg_details(args: &serde_json::Value) -> String {
    let mut s = String::new();
    for key in KEY_ARGS {
        let text = match args.get(key) {
            None | Some(serde_json::Value::Null) => continue,
            Some(serde_json::Value::String(v)) => v.clone(),
            Some(v) => v.to_string(),
        };
        s.push_str(&format!(" {key}={}", cap_chars(&text, MAX_ARG_CHARS)));
    }
    s
}

fn cap_chars(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}… ({} more chars)", &text[..i], text[i..].chars().count()),
        None => text.to_string(),
    }
}

/// Whether a `kind` argument may name core Secrets (`secrets`, `Secret`, `secrets.`…).
fn may_be_secret(kind: &str) -> bool {
    let kind = kind.trim().to_lowercase();
    matches!(kind.split('.').next(), Some("secret" | "secrets"))
}

/// Arguments as logged and shown: values in a patch to a Secret are masked; patches to
/// other kinds stay readable, so the user sees what they approve.
fn redact(mut args: serde_json::Value) -> serde_json::Value {
    let secret = args.get("kind").and_then(|k| k.as_str()).is_none_or(may_be_secret);
    if secret && let Some(patch) = args.get_mut("patch") {
        k8s::mask_secret_values(patch);
    }
    args
}

/// Names that end up in API URLs, checked before any rule: a `/` or `?` in them would
/// reach another object than the one the rules were checked for.
fn validate_args(args: &serde_json::Value) -> Result<(), String> {
    use k8s::names::{validate_container, validate_kind_name, validate_name, validate_namespace};
    let text = |key: &str| args.get(key).and_then(|v| v.as_str());
    if let Some(ns) = text("namespace") {
        validate_namespace(ns)?;
    }
    for key in ["name", "pod", "node"] {
        if let Some(name) = text(key) {
            validate_name(name)?;
        }
    }
    if let Some(c) = text("container") {
        validate_container(c)?;
    }
    if let Some(k) = text("object_kind") {
        validate_kind_name(k)?;
    }
    Ok(())
}

/// The API server must have returned the object asked for (names are validated, but a
/// mismatch must never show another object under the rules checked for this one).
fn check_identity(kind: &Kind, ns: Option<&str>, name: &str, value: &serde_json::Value) -> anyhow::Result<()> {
    let text = |path: &str| value.pointer(path).and_then(|v| v.as_str());
    let kind_ok = text("/kind").is_none_or(|k| k == kind.ar.kind);
    let version_ok = text("/apiVersion").is_none_or(|v| v == kind.ar.api_version);
    let ns_ok = !kind.namespaced || text("/metadata/namespace") == ns;
    if kind_ok && version_ok && ns_ok && text("/metadata/name") == Some(name) {
        Ok(())
    } else {
        anyhow::bail!("the API server returned another object than {} {name}", kind.ar.kind)
    }
}

/// A manifest for the approval banner: each object's kind and name, then the YAML
/// that will be applied (a Secret's values masked).
fn manifest_summary(planned: &[(Kind, Option<String>, String, serde_json::Value)]) -> String {
    let mut out = String::from("Objects:\n");
    for (kind, ns, name, _) in planned {
        let ns = ns.as_ref().map(|n| format!("{n}/")).unwrap_or_default();
        out.push_str(&format!("  {} {ns}{name}\n", kind.ar.kind));
    }
    for (kind, _, _, doc) in planned {
        let secret = kind.is("", "Secret") || is_secret_object(doc);
        let yaml = k8s::to_yaml(doc, secret, false).unwrap_or_else(|e| format!("# {e}\n"));
        out.push_str("---\n");
        out.push_str(&cap_chars(&yaml, MAX_ARG_CHARS));
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// What the activity log keeps of a manifest: each object, with a hash of its content
/// as the banner shows it (a Secret's values masked).
fn manifest_objects(planned: &[(Kind, Option<String>, String, serde_json::Value)]) -> Vec<String> {
    use sha2::Digest as _;
    planned
        .iter()
        .map(|(kind, ns, name, doc)| {
            let secret = kind.is("", "Secret") || is_secret_object(doc);
            let yaml = k8s::to_yaml(doc, secret, false).unwrap_or_default();
            let hash: String = sha2::Sha256::digest(yaml.as_bytes())[..6].iter().map(|b| format!("{b:02x}")).collect();
            let ns = ns.as_ref().map(|n| format!("{n}/")).unwrap_or_default();
            format!("{} {ns}{name} sha256:{hash}", kind.ar.kind)
        })
        .collect()
}

/// Namespaces, sorted, of the pods a drain would evict (`k8s::drain` leaves DaemonSet,
/// static, finished and unmanaged pods alone) for which `denied(namespace, name)` holds.
fn denied_namespaces(pods: &[serde_json::Value], denied: impl Fn(&str, &str) -> bool) -> Vec<String> {
    let mut out: Vec<String> = pods
        .iter()
        .filter(|p| {
            let owners = p.pointer("/metadata/ownerReferences").and_then(|o| o.as_array());
            let owners = owners.map(Vec::as_slice).unwrap_or_default();
            let daemon = owners.iter().any(|o| o["kind"] == "DaemonSet");
            let managed = owners.iter().any(|o| o["controller"] == true);
            let mirror = p.pointer("/metadata/annotations/kubernetes.io~1config.mirror").is_some();
            let done = matches!(p.pointer("/status/phase").and_then(|v| v.as_str()), Some("Succeeded" | "Failed"));
            managed && !daemon && !mirror && !done
        })
        .filter_map(|p| {
            let text = |path: &str| p.pointer(path).and_then(|v| v.as_str());
            Some((text("/metadata/namespace")?, text("/metadata/name")?))
        })
        .filter(|(ns, name)| denied(ns, name))
        .map(|(ns, _)| ns.to_string())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Whether a returned object is a core Secret, whatever kind was asked for.
fn is_secret_object(value: &serde_json::Value) -> bool {
    value.get("kind").and_then(|k| k.as_str()) == Some("Secret")
}

impl KubeMcp {
    /// Identify the agent and apply the checks every tool shares.
    fn begin(&self, parts: &Parts, tool: &'static str, args: serde_json::Value) -> Result<Call, String> {
        let id = parts.extensions.get::<AgentId>().map(|a| a.0.clone()).ok_or("unauthenticated")?;
        let agent = self.hub.agent(&id).ok_or("unknown agent")?;
        let session = parts.headers.get("mcp-session-id").and_then(|v| v.to_str().ok()).map(String::from);
        let invalid = validate_args(&args).err();
        let context = args.get("context").and_then(|v| v.as_str()).map(String::from);
        let call = Call {
            hub: self.hub.clone(),
            agent,
            tool,
            args: redact(args),
            start: Instant::now(),
            approved: false,
            approved_rules: Vec::new(),
            rule: None,
            session,
            approval_detail: None,
        };
        self.ensure_active(&call)?;
        if !self.hub.take_call(&call.agent.id, call.agent.policy.max_calls_per_minute) {
            return Err(call.deny(format!("rate limit: {} calls per minute", call.agent.policy.max_calls_per_minute)));
        }
        if let Some(e) = invalid {
            return Err(call.fail(e));
        }
        // Before connecting: a context no rule names is never contacted.
        if let Some(ctx) = context
            && !call.policy().context_allowed(&ctx)
        {
            return Err(call.deny(format!("context {ctx:?} is not allowed for this agent")));
        }
        Ok(call)
    }

    /// Agent access is on and this agent still enabled: checked when a call starts,
    /// after waiting for approval, and before each change.
    fn ensure_active(&self, call: &Call) -> Result<(), String> {
        match self.inactive(call) {
            Some(reason) => Err(call.deny(reason)),
            None => Ok(()),
        }
    }

    /// Why calls are refused now, if they are.
    fn inactive(&self, call: &Call) -> Option<&'static str> {
        if !self.hub.agents_enabled() {
            return Some("agent access is switched off in Recouvrance");
        }
        if !self.hub.agent(&call.agent.id).is_some_and(|a| a.enabled) {
            return Some("this agent is disabled");
        }
        None
    }

    /// Evaluate one access request; Ask waits for the user.
    async fn authorize(&self, call: &mut Call, access: &Access, any_namespace: bool) -> Result<(), String> {
        let verdict =
            if any_namespace { call.policy().decide_any_namespace(access) } else { call.policy().decide(access) };
        match verdict.mode {
            Mode::Allow => {
                call.rule = verdict.rule;
                self.ensure_active(call)
            }
            Mode::Deny => Err(call.deny(format!("{} — {}", describe(access), call.rule_label(verdict.rule)))),
            Mode::Ask => {
                // Across namespaces, the allowed ones can be shown without approval (Ask rows hidden).
                let fallback = if any_namespace { call.policy().allow_somewhere(access) } else { None };
                if self.hub.headless() {
                    if fallback.is_some() {
                        call.rule = fallback;
                        return self.ensure_active(call);
                    }
                    return Err(call.deny(format!(
                        "{} needs approval in the Recouvrance window ({}); run the app, or add an Allow rule",
                        describe(access),
                        call.rule_label(verdict.rule)
                    )));
                }
                let mut summary =
                    format!("{}{}  [{}]", describe(access), arg_details(&call.args), call.rule_label(verdict.rule));
                if let Some(detail) = &call.approval_detail {
                    summary.push('\n');
                    summary.push_str(detail);
                }
                let dry_run = call.args.get("dry_run").and_then(|v| v.as_bool()).unwrap_or(false);
                if self.hub.request_approval(&call.agent, call.tool, summary, access.clone(), dry_run).await {
                    // Access may have been switched off while the request waited.
                    self.ensure_active(call)?;
                    call.approved = true;
                    call.rule = verdict.rule;
                    call.approved_rules = if any_namespace {
                        call.policy().asks_any_namespace(access)
                    } else {
                        verdict.rule.into_iter().collect()
                    };
                    Ok(())
                } else if fallback.is_some() {
                    call.rule = fallback;
                    self.ensure_active(call)
                } else {
                    call.record(Decision::Rejected, "not approved".into(), 0);
                    Err("Rejected: the user did not approve this request".into())
                }
            }
        }
    }

    async fn kind(&self, call: &Call, context: &str, query: &str) -> Result<(kube::Client, Kind), String> {
        let (client, kinds) = match self.hub.client(context).await {
            Ok(c) => c,
            Err(e) => return Err(call.finish(Err(e)).unwrap_err()),
        };
        if let Some(kind) = k8s::find_kind(&kinds, query).cloned() {
            return Ok((client, kind));
        }
        // Maybe a CRD installed since the first call.
        match self.hub.rediscover(context).await.and_then(|kinds| k8s::find_kind(&kinds, query).cloned()) {
            Some(kind) => Ok((client, kind)),
            None => Err(call.finish(Err(anyhow::anyhow!("unknown resource type {query:?}"))).unwrap_err()),
        }
    }
}

fn access_for(kind: &Kind, context: &str, verb: &str, namespace: Option<String>, name: Option<String>) -> Access {
    Access {
        context: context.to_string(),
        verb: verb.to_string(),
        resource: kind.kubectl_name(),
        resource_aliases: vec![kind.ar.plural.clone(), kind.ar.kind.to_lowercase()],
        namespace: if kind.namespaced { namespace } else { None },
        name,
    }
}

/// Namespaced kinds need a namespace for single-object verbs (a refused call is logged).
fn require_ns(call: &Call, kind: &Kind, ns: &Option<String>) -> Result<(), String> {
    if kind.namespaced && ns.is_none() {
        Err(call.fail(format!("{} is namespaced: pass `namespace`", kind.ar.kind)))
    } else {
        Ok(())
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ContextParam {
    #[schemars(description = "kubeconfig context name, from list_contexts")]
    pub context: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ListParams {
    #[schemars(description = "kubeconfig context name, from list_contexts")]
    pub context: String,
    #[schemars(description = "resource type like kubectl: pods, deploy, svc, certificates.cert-manager.io")]
    pub kind: String,
    #[schemars(description = "namespace; omit for all namespaces you are allowed to see")]
    pub namespace: Option<String>,
    #[schemars(description = "label selector, e.g. app=web,tier!=cache")]
    pub label_selector: Option<String>,
    #[schemars(description = "field selector, e.g. status.phase=Running")]
    pub field_selector: Option<String>,
    #[schemars(description = "include extra columns, like kubectl -o wide")]
    pub wide: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ObjectParams {
    pub context: String,
    #[schemars(description = "resource type like kubectl: pod, deployment, cm, ...")]
    pub kind: String,
    pub name: String,
    #[schemars(description = "required for namespaced resources")]
    pub namespace: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DrainParams {
    pub context: String,
    pub node: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct TopParams {
    pub context: String,
    #[schemars(description = "namespace; omit for all namespaces you may read")]
    pub namespace: Option<String>,
    #[schemars(description = "\"cpu\" (default) or \"memory\"")]
    pub sort_by: Option<String>,
    #[schemars(description = "how many pods to return (default 20)")]
    pub limit: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SearchParams {
    pub context: String,
    #[schemars(description = "text contained in the object name, case-insensitive")]
    pub query: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct EventsParams {
    pub context: String,
    #[schemars(description = "namespace; omit for all allowed namespaces")]
    pub namespace: Option<String>,
    #[schemars(description = "only events about the object with this name")]
    pub name: Option<String>,
    #[schemars(description = "only events about objects of this Kind, e.g. Pod")]
    pub object_kind: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct LogsParams {
    pub context: String,
    pub namespace: String,
    pub pod: String,
    #[schemars(description = "container name; defaults to the pod's first container")]
    pub container: Option<String>,
    #[schemars(description = "number of most recent lines (capped by policy)")]
    pub tail_lines: Option<u32>,
    #[schemars(description = "only lines from the last N seconds")]
    pub since_seconds: Option<i64>,
    #[schemars(description = "logs of the previous (crashed) container")]
    pub previous: Option<bool>,
    #[schemars(description = "only keep lines containing this text (case-insensitive)")]
    pub grep: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ScaleParams {
    pub context: String,
    #[schemars(description = "deployments, statefulsets, replicasets, …")]
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub replicas: i32,
    #[schemars(description = "validate on the server without changing anything")]
    pub dry_run: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RestartParams {
    pub context: String,
    #[schemars(description = "deployments, statefulsets or daemonsets")]
    pub kind: String,
    pub namespace: String,
    pub name: String,
    pub dry_run: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct CertParams {
    pub context: String,
    #[schemars(description = "Only this namespace; all namespaces you may read when omitted")]
    pub namespace: Option<String>,
    #[schemars(description = "Only certificates expiring within this many days (default: list all, soonest first)")]
    pub within_days: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProblemsParams {
    pub context: String,
    #[schemars(description = "Only this namespace; all namespaces you may list when omitted")]
    pub namespace: Option<String>,
    #[schemars(description = "Include minor findings (restarts, cordoned nodes). Default false")]
    pub include_minor: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct HistoryParams {
    pub context: String,
    #[schemars(description = "pods, deployments, statefulsets, daemonsets, replicasets or jobs")]
    pub kind: String,
    pub namespace: String,
    pub name: String,
    #[schemars(description = "How far back, in minutes (default 60, max 10080)")]
    pub minutes: Option<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GitOpsParams {
    pub context: String,
    #[schemars(
        description = "A Flux kind (kustomizations, helmreleases, gitrepositories, …) or applications (Argo CD)"
    )]
    pub kind: String,
    pub namespace: String,
    pub name: String,
    #[schemars(description = "reconcile (Flux) · refresh or sync (Argo CD) · suspend or resume (Flux)")]
    pub action: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct PatchParams {
    pub context: String,
    pub kind: String,
    pub name: String,
    pub namespace: Option<String>,
    #[schemars(description = "JSON merge patch, e.g. {\"metadata\":{\"labels\":{\"a\":\"b\"}}}")]
    pub patch: serde_json::Value,
    pub dry_run: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ApplyParams {
    pub context: String,
    #[schemars(description = "one or more YAML documents (server-side apply)")]
    pub manifest: String,
    #[schemars(description = "namespace for objects that don't set one")]
    pub namespace: Option<String>,
    pub dry_run: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct DeleteParams {
    pub context: String,
    pub kind: String,
    pub name: String,
    pub namespace: Option<String>,
    pub dry_run: Option<bool>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ExecParams {
    pub context: String,
    pub namespace: String,
    pub pod: String,
    pub container: Option<String>,
    #[schemars(description = "command and arguments, e.g. [\"cat\", \"/etc/hosts\"]; no shell unless you call one")]
    pub command: Vec<String>,
}

impl KubeMcp {
    pub fn new(hub: Arc<Hub>) -> Self {
        Self { hub }
    }
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ScopeArgs {
    /// Kubernetes context (cluster)
    pub context: String,
    /// Namespace (optional: the whole cluster when empty)
    pub namespace: Option<String>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WorkloadArgs {
    pub context: String,
    pub namespace: String,
    /// e.g. deployments/web or statefulsets/db
    pub workload: String,
}

/// Ready-made workflows; clients show them as slash commands (e.g. in Claude Code).
#[prompt_router]
impl KubeMcp {
    #[prompt(name = "troubleshoot", description = "Find what is broken in a cluster or namespace, and why")]
    async fn troubleshoot(&self, Parameters(a): Parameters<ScopeArgs>) -> Vec<PromptMessage> {
        let scope = match &a.namespace {
            Some(ns) if !ns.is_empty() => format!("namespace `{ns}` of context `{}`", a.context),
            _ => format!("context `{}`", a.context),
        };
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Troubleshoot {scope} using the Recouvrance tools.\n\
                 1. Call find_problems for this scope.\n\
                 2. For the most severe items (at most 5), call describe_resource, get_events about the object, and get_logs \
                    (with previous=true for crash loops or OOMKilled containers).\n\
                 3. Group findings by root cause. For each: what is wrong, the evidence (quote the event or log line), and the fix you suggest.\n\
                 Read only: do not change anything unless I ask you to."
            ),
        )]
    }

    #[prompt(
        name = "rightsize",
        description = "Suggest CPU/memory requests and limits for a workload from its real usage"
    )]
    async fn rightsize(&self, Parameters(a): Parameters<WorkloadArgs>) -> Vec<PromptMessage> {
        let (kind, name) = a.workload.split_once('/').unwrap_or(("deployments", a.workload.as_str()));
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Right-size {kind}/{name} in namespace `{ns}` of context `{ctx}`.\n\
                 1. Call usage_history for it over 10080 minutes (7 days), and also 1440 (24 hours).\n\
                 2. Call get_resource to read its containers' current requests and limits.\n\
                 3. Propose requests close to the p95 with ~20% headroom, a memory limit above the max with margin (never below), \
                    and say whether a CPU limit is worth keeping. Show a before/after table and the monthly savings direction.\n\
                 4. Give the exact patch (kubectl patch or YAML), but do not apply it unless I ask.",
                ns = a.namespace,
                ctx = a.context
            ),
        )]
    }

    #[prompt(name = "cert-audit", description = "Find TLS certificates that expire soon and what renews them")]
    async fn cert_audit(&self, Parameters(a): Parameters<ScopeArgs>) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Audit TLS certificates in context `{}`{}.\n\
                 1. Call check_certificates with within_days=30.\n\
                 2. For each one, find what should renew it (a cert-manager Certificate with the same secretName, via list_resources on certificates.cert-manager.io) \
                    and its Ready condition and events.\n\
                 3. Report: certificates at risk, why renewal is not happening, and the fix. Read only.",
                a.context,
                a.namespace.as_ref().map(|n| format!(", namespace `{n}`")).unwrap_or_default()
            ),
        )]
    }

    #[prompt(
        name = "cluster-health",
        description = "A short health report of a cluster: problems, capacity, risky workloads"
    )]
    async fn cluster_health(&self, Parameters(a): Parameters<ScopeArgs>) -> Vec<PromptMessage> {
        vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Write a short health report for context `{}`.\n\
                 Use find_problems, list_resources on nodes, top_pods (by cpu and by memory), and get_events with warnings. \
                 Cover: node health, what is broken, the heaviest workloads, capacity headroom, and the 3 things to fix first. Keep it under 30 lines.",
                a.context
            ),
        )]
    }
}

#[tool_handler(router = (Self::read_tools() + Self::write_tools()))]
#[prompt_handler]
impl ServerHandler for KubeMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_prompts().build()).with_instructions(
            "Access to the user's Kubernetes clusters through Recouvrance. Start with my_permissions and list_contexts. \
             Every call is checked against rules the user set for you (by context, verb, resource, \
             namespace and name) and shown to them; some calls wait for their approval. \
             Prefer dry_run for writes first, and explain what you are about to change.",
        )
    }
}

/// Whether a tool changes the cluster; used by the UI for labels.
pub fn tool_writes(tool: &str) -> bool {
    is_write(match tool {
        "exec_command" => "exec",
        "scale" => "scale",
        "rollout_restart" => "restart",
        "patch_resource" => "patch",
        "apply_manifest" => "apply",
        "delete_resource" => "delete",
        "drain_node" => "drain",
        "gitops" => "patch",
        _ => "",
    })
}

fn format_table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
    }
    let line = |cells: &mut dyn Iterator<Item = &str>| {
        let mut out = String::new();
        for (i, c) in cells.enumerate() {
            if i + 1 == widths.len() {
                out.push_str(c);
            } else {
                out.push_str(&format!("{c:<w$}   ", w = widths[i]));
            }
        }
        out.trim_end().to_string()
    };
    let mut out = line(&mut headers.iter().copied());
    for row in rows {
        out.push('\n');
        out.push_str(&line(&mut row.iter().map(String::as_str)));
    }
    out
}

async fn auth(State(hub): State<Arc<Hub>>, mut req: Request, next: Next) -> Result<Response, StatusCode> {
    let token = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim);
    let Some(agent) = token.and_then(|t| hub.agent_by_token(t)) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    hub.mark_seen(&agent.id);
    req.extensions_mut().insert(AgentId(agent.id));
    Ok(next.run(req).await)
}

/// Serve MCP on 127.0.0.1 until the guard is dropped.
pub fn start(hub: Arc<Hub>) -> TaskGuard {
    k8s::spawn_guarded(async move {
        let port = hub.port();
        let mcp = {
            let hub = hub.clone();
            StreamableHttpService::new(
                move || Ok(KubeMcp::new(hub.clone())),
                Arc::new(LocalSessionManager::default()),
                StreamableHttpServerConfig::default(),
            )
        };
        let app = axum::Router::new()
            .nest_service("/mcp", mcp)
            .layer(axum::middleware::from_fn_with_state(hub.clone(), auth));
        let listener = match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => l,
            Err(e) => {
                let hint = if e.kind() == std::io::ErrorKind::AddrInUse {
                    " (another Recouvrance may be running; change the port in Settings → Agents)"
                } else {
                    ""
                };
                hub.emit(HubEvent::ServerStatus(Err(format!("cannot listen on 127.0.0.1:{port}: {e}{hint}"))));
                return;
            }
        };
        hub.emit(HubEvent::ServerStatus(Ok(port)));
        if let Err(e) = axum::serve(listener, app).await {
            hub.emit(HubEvent::ServerStatus(Err(e.to_string())));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logged_patches_hide_secret_values() {
        let args = redact(json!({
            "name": "db",
            "patch": {"data": {"config": "ZXhhbXBsZQ=="}, "stringData": {"note": "example"}},
        }));
        let text = args.to_string();
        assert!(!text.contains("ZXhhbXBsZQ") && !text.contains("example"), "{text}");
        assert!(text.contains("config") && text.contains("note"));
    }

    #[test]
    fn approval_shows_what_runs() {
        let s = arg_details(&json!({"context": "c", "command": ["rm", "-rf", "/"], "dry_run": false, "replicas": 0}));
        assert_eq!(s, " command=[\"rm\",\"-rf\",\"/\"] replicas=0 dry_run=false");
        let s = arg_details(&json!({"action": "sync", "patch": {"spec": {"suspend": true}}, "dry_run": null}));
        assert_eq!(s, " patch={\"spec\":{\"suspend\":true}} action=sync");
        // Long patches are shown whole (the banner scrolls).
        let long = arg_details(&json!({"patch": {"x": "y".repeat(2000)}}));
        assert!(long.contains(&"y".repeat(2000)) && !long.contains("more chars"));
        let huge = arg_details(&json!({"patch": {"x": "y".repeat(MAX_ARG_CHARS)}}));
        assert!(huge.ends_with("… (8 more chars)"), "{}", &huge[huge.len() - 40..]);
    }

    #[test]
    fn only_secret_patches_are_masked() {
        let patch = json!({"data": {"config": "plain-value"}});
        let shown = redact(json!({"kind": "configmaps", "patch": patch}));
        assert!(shown.to_string().contains("plain-value"));
        for kind in ["secrets", "Secret", "secret", "secrets.", " Secrets "] {
            let shown = redact(json!({"kind": kind, "patch": patch}));
            assert!(!shown.to_string().contains("plain-value"), "{kind}");
        }
    }

    #[test]
    fn names_in_urls_are_validated() {
        for bad in [
            json!({"context": "c", "kind": "pods", "namespace": "prod/secrets/db?", "name": "x"}),
            json!({"namespace": "kube%2Dsystem"}),
            json!({"name": "node-1/proxy/pods"}),
            json!({"node": "node-1/proxy/pods"}),
            json!({"pod": "web?container=x"}),
            json!({"namespace": "app", "pod": "web", "container": "c&tailLines=99999"}),
            json!({"object_kind": "Pod,involvedObject.name=x"}),
            json!({"namespace": ""}),
        ] {
            assert!(validate_args(&bad).is_err(), "{bad}");
        }
        for good in [
            json!({"context": "any/thing", "kind": "configmaps", "namespace": "kube-system", "name": "kube-root-ca.crt"}),
            json!({"kind": "clusterroles", "name": "system:node", "namespace": null}),
            json!({"namespace": "app", "pod": "web-0", "container": "app", "grep": "a/b?c"}),
            json!({"node": "ip-10-0-0-1.eu-west-1.compute.internal"}),
        ] {
            assert!(validate_args(&good).is_ok(), "{good}");
        }
    }

    fn kind(group: &str, version: &str, kind: &str, plural: &str, namespaced: bool) -> Kind {
        let gvk = kube::api::GroupVersionKind::gvk(group, version, kind);
        Kind {
            ar: kube::discovery::ApiResource::from_gvk_with_plural(&gvk, plural),
            namespaced,
            section: "",
            label: kind.into(),
        }
    }

    #[test]
    fn returned_object_must_be_the_requested_one() {
        let pods = kind("", "v1", "Pod", "pods", true);
        let pod = json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": "web", "namespace": "app"}});
        assert!(check_identity(&pods, Some("app"), "web", &pod).is_ok());
        assert!(check_identity(&pods, Some("other"), "web", &pod).is_err());
        assert!(check_identity(&pods, Some("app"), "api", &pod).is_err());
        let secret = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "web", "namespace": "app"}});
        assert!(check_identity(&pods, Some("app"), "web", &secret).is_err());
        assert!(is_secret_object(&secret) && !is_secret_object(&pod));
        let nodes = kind("", "v1", "Node", "nodes", false);
        let node = json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": "n1"}});
        assert!(check_identity(&nodes, None, "n1", &node).is_ok());
        let list = json!({"apiVersion": "v1", "kind": "PodList", "metadata": {}});
        assert!(check_identity(&nodes, None, "n1", &list).is_err());
    }

    #[test]
    fn apply_approval_shows_the_content() {
        let cm = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cfg", "namespace": "app"},
            "data": {"mode": "debug"}});
        let secret = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "db", "namespace": "app"},
            "stringData": {"config": "example-value"}});
        let planned = vec![
            (kind("", "v1", "ConfigMap", "configmaps", true), Some("app".into()), "cfg".into(), cm),
            (kind("", "v1", "Secret", "secrets", true), Some("app".into()), "db".into(), secret),
        ];
        let s = manifest_summary(&planned);
        assert!(s.contains("ConfigMap app/cfg") && s.contains("Secret app/db"), "{s}");
        assert!(s.contains("mode: debug"), "{s}");
        assert!(s.contains("config:") && !s.contains("example-value"), "{s}");
        // The log keeps each object and a hash of its masked content.
        let objects = manifest_objects(&planned);
        assert_eq!(objects.len(), 2);
        assert!(objects[0].starts_with("ConfigMap app/cfg sha256:") && objects[1].starts_with("Secret app/db sha256:"));
        let mut changed = planned.clone();
        changed[0].3["data"]["mode"] = json!("info");
        assert_ne!(manifest_objects(&changed)[0], objects[0]);
        // A Secret's values are masked before hashing: they leave no trace.
        changed[1].3["stringData"]["config"] = json!("other-value");
        assert_eq!(manifest_objects(&changed)[1], objects[1]);
    }

    #[test]
    fn drain_finds_pods_in_denied_namespaces() {
        let pod = |ns: &str, owner: serde_json::Value| json!({"metadata": {"namespace": ns, "name": format!("{ns}-pod"), "ownerReferences": [owner]}});
        let rs = json!({"kind": "ReplicaSet", "controller": true});
        let ds = json!({"kind": "DaemonSet", "controller": true});
        let pods = [
            pod("app", rs.clone()),
            pod("kube-system", rs.clone()),
            pod("kube-public", rs),
            // Not evicted by a drain, so not in the way.
            pod("kube-node-lease", ds),
            pod("kube-other", json!({"kind": "Job"})),
        ];
        let denied = denied_namespaces(&pods, |ns, _| ns.starts_with("kube-"));
        assert_eq!(denied, ["kube-public", "kube-system"]);
        assert!(denied_namespaces(&pods, |_, _| false).is_empty());
    }
}
