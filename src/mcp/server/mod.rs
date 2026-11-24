//! The MCP server. Every tool call is turned into an access request
//! (context, verb, resource, namespace, name), checked against the calling
//! agent's rules, possibly sent to the user for approval, and logged.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use http::request::Parts;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolRequestParams, CallToolResponse, PromptMessage, Role, Tool};
use rmcp::model::{ServerCapabilities, ServerConfig};

use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ServerHandler, prompt, prompt_handler, prompt_router, schemars, tool_handler};
use serde::{Deserialize, Serialize};

use super::preview::{self, Plan};
use super::{
    Access, Agent, Answer, Approval, AuditEntry, Decision, Hub, HubEvent, Mode, Policy, Preview, Verb, rules_file,
};
use crate::k8s::{self, Kind, TaskGuard};

pub(crate) use gone::Gone;

mod argo_tools;
mod cluster_tools;
mod diff_tools;
mod file_tools;
mod gone;
mod helm_tools;
mod history_tools;
mod job_tools;
mod logs_tools;
mod metrics_tools;
mod node_tools;
mod ops_tools;
mod read_tools;
mod rollout_tools;
mod sessions;
mod write_tools;

/// Set by the auth middleware for every authenticated request.
#[derive(Clone)]
struct AgentId(String);

/// The MCP client a tool call comes from, as [`client_label`] names it: set by `call_tool`
/// for the tools to read in their request's HTTP parts.
#[derive(Clone)]
struct ClientName(String);

/// The header the in-app assistant adds to the MCP config it gives Claude Code, so its calls
/// are told from those of your own Claude Code (same client, same agent). A label only.
pub(crate) const CLIENT_HEADER: &str = "x-recouvrance-client";

/// Longest client name or version kept: they come from the client, and end up in a log line
/// and on the Activity page.
const MAX_CLIENT_CHARS: usize = 64;

/// How the activity log names a call's client: its `clientInfo` name and version (from
/// `initialize`, or from each request's metadata in a stateless session), with the in-app
/// assistant's own label when it adds one (`assistant via claude-code 2.1.3`). Control
/// characters are dropped and each part is capped. None when the client gave no name.
fn client_label(info: Option<&rmcp::model::Implementation>, header: Option<&str>) -> Option<String> {
    let clean = |s: &str| -> String {
        let s: String = s.chars().filter(|c| !c.is_control()).collect();
        let s = s.trim();
        match s.char_indices().nth(MAX_CLIENT_CHARS) {
            Some((i, _)) => format!("{}…", &s[..i]),
            None => s.to_string(),
        }
    };
    let name = info.map(|i| clean(&i.name)).filter(|n| !n.is_empty());
    let version = info.map_or_default(|i| clean(&i.version));
    let client = name.map(|n| if version.is_empty() { n } else { format!("{n} {version}") });
    let label = header.map(clean).filter(|l| !l.is_empty());
    match (label, client) {
        (Some(label), Some(client)) => Some(format!("{label} via {client}")),
        (label, client) => label.or(client),
    }
}

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
    /// Whether its agent still waits for it.
    gone: Gone,
    start: Instant,
    approved: bool,
    /// The Ask rules the approval covered: every Ask rule the request meets, in its
    /// namespace or, across all namespaces, in any of them.
    approved_rules: Vec<usize>,
    /// The rule that allowed the call, named when it was decided, for the activity log
    /// (the rules may be read again, and renumbered, before the call ends).
    rule: Option<String>,
    session: Option<String>,
    /// The MCP client it came from (`client_label`).
    client: Option<String>,
    /// What the call's answer shows Secret values of (`revealing`): logged when it succeeds.
    revealed: Vec<String>,
    /// More for the approval banner than the arguments show (a manifest's objects).
    approval_detail: Option<String>,
    /// How to work out what the call will change, should it wait for an approval.
    plan: Option<Plan>,
    /// What the call will change, worked out once, before its first approval: the banner
    /// shows it and the activity log keeps it.
    preview: Option<Preview>,
}

impl Call {
    fn policy(&self) -> &Policy {
        &self.agent.policy
    }

    fn rule_label(&self, rule: Option<usize>) -> String {
        rule_label(self.policy(), rule)
    }

    /// Remember the rule that allowed the call.
    fn allowed_by(&mut self, rule: Option<usize>) {
        self.rule = rule.map(|i| self.rule_label(Some(i)));
    }

    /// Note that the call's answer shows `what`'s Secret values (unmasked, as a `reveal` rule
    /// allows), for the activity log.
    fn revealing(&mut self, what: String) {
        self.revealed.push(what);
    }

    fn record(&self, decision: Decision, detail: String, result_bytes: usize) {
        // A refused or failed call returned no values, whatever it was about to show.
        let returned = matches!(decision, Decision::Allowed | Decision::Approved);
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
            change: self.preview.as_ref().map(Preview::text),
            client: self.client.clone(),
            revealed: if returned { self.revealed.clone() } else { Vec::new() },
        });
    }

    fn deny(&self, reason: impl Into<String>) -> String {
        self.deny_hinted(reason, "")
    }

    /// [`deny`](Self::deny), followed by what the agent can do about it: told to the agent,
    /// while the activity log keeps the reason alone.
    fn deny_hinted(&self, reason: impl Into<String>, hint: &str) -> String {
        let reason = reason.into();
        self.record(Decision::Denied, reason.clone(), 0);
        if hint.is_empty() {
            format!("Denied by Recouvrance policy: {reason}")
        } else {
            format!("Denied by Recouvrance policy: {reason}. {hint}")
        }
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
        visible(self.policy(), self.approved_rules(), a)
    }

    /// The Ask rules the user approved for this call (none when not approved).
    fn approved_rules(&self) -> &[usize] {
        if self.approved { &self.approved_rules } else { &[] }
    }

    fn finish(&self, result: anyhow::Result<String>) -> Result<String, String> {
        match result {
            Ok(text) => {
                let decision = if self.approved { Decision::Approved } else { Decision::Allowed };
                // Calls that aren't rule-checked (list_contexts) have no rule to show.
                let detail = self.rule.clone().unwrap_or_default();
                self.record(decision, detail, text.len());
                Ok(text)
            }
            Err(e) => {
                let msg = k8s::error_chain(&e);
                self.record(Decision::Failed, msg.clone(), 0);
                Err(msg)
            }
        }
    }
}

/// Whether a row may be shown: allowed by a rule, or gated by one of the `approved` Ask rules.
fn visible(policy: &Policy, approved: &[usize], a: &Access) -> bool {
    let v = policy.decide(a);
    match v.mode {
        Mode::Allow => true,
        Mode::Ask => v.rule.is_some_and(|r| approved.contains(&r)),
        Mode::Deny => false,
    }
}

/// Whether a `find_problems` finding may be shown. The approval named pods: findings of
/// other kinds need an Allow rule.
fn problem_visible(policy: &Policy, approved: &[usize], pod: bool, a: &Access) -> bool {
    visible(policy, if pod { approved } else { &[] }, a)
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

/// A rule as the agent sees it named: `rule #3 (Never prod)`, or `no matching rule`.
fn rule_label(policy: &Policy, rule: Option<usize>) -> String {
    match rule {
        Some(i) => {
            let note = policy.rules.get(i).map_or_default(|r| r.note.clone());
            if note.is_empty() { format!("rule #{}", i + 1) } else { format!("rule #{} ({note})", i + 1) }
        }
        None => "no matching rule".into(),
    }
}

/// Where the user changes an agent's rules.
const RULES_ARE_SET: &str = "in Recouvrance (Agents page, or rules.yaml)";

/// Said after a refusal that comes from a state the user set (agent access off, the agent
/// disabled): no call of the agent's changes it.
const USER_STATE: &str = "Final: only the user can change this; don't retry until they do.";

/// Said after a refusal a Deny rule decided.
const DENIED_BY_RULE: &str =
    "Final: a Deny rule decided; don't retry or work around it, and tell the user if the task needs it.";

/// What an agent can do about a request the rules deny, so it neither retries nor gives up
/// without telling the user what to grant. Final when a Deny rule decided: no rule the user
/// adds undoes it. Grantable when no rule covers the request, with the narrowest rule that
/// would allow it, in rules.yaml's words.
fn deny_hint(access: &Access, rule: Option<usize>) -> String {
    if rule.is_some() {
        return DENIED_BY_RULE.into();
    }
    let example = rules_file::rule_for(access)
        .map_or_default(|r| format!(", e.g. `{r}` under allow (or ask, to approve each call)"));
    format!("Grantable: no rule allows this; the user can add one {RULES_ARE_SET}{example}.")
}

/// What an agent can do about a request an Ask rule covers while nobody can approve it
/// (`recouvrance serve`). An Allow rule doesn't win over the Ask rule; an approved one does.
fn approval_hint(access: &Access) -> String {
    let approved = rules_file::rule_for(access).map_or_default(|r| {
        format!(", or add `{r}` under approved in rules.yaml (an Allow rule doesn't win over the Ask rule)")
    });
    format!("Grantable: the user can open the Recouvrance window and approve it when you call again{approved}.")
}

/// What a call making this request would get, as `check_access` says it: allowed, waiting
/// for the user, or denied, with the rule that decides and what the user could grant. The
/// same decision as the call's (`authorize_inner`): across every namespace for a namespaced
/// kind with none given, and a list an Ask rule covers shows the rows an Allow rule gives
/// when it isn't approved.
fn access_verdict(policy: &Policy, access: &Access, any_namespace: bool, headless: bool) -> (Mode, String) {
    let v = if any_namespace { policy.decide_any_namespace(access) } else { policy.decide(access) };
    let label = rule_label(policy, v.rule);
    let text = match v.mode {
        Mode::Allow => format!("allowed by {label}"),
        Mode::Deny if v.rule.is_some() => format!("denied by {label}. {}", deny_hint(access, v.rule)),
        Mode::Deny => format!("denied. {}", deny_hint(access, None)),
        Mode::Ask => {
            let rows = if access.verb == Verb::List { policy.allow_somewhere(access) } else { None };
            let rows = rows.map(|r| rule_label(policy, Some(r)));
            match (headless, rows) {
                (false, None) => {
                    format!(
                        "needs the user's approval ({label}): the call waits for their answer in the Recouvrance window"
                    )
                }
                (false, Some(rows)) => format!(
                    "needs the user's approval ({label}) for every row: unapproved, the list shows only the rows {rows} allows"
                ),
                (true, None) => format!(
                    "refused for now: it needs approval ({label}) and the Recouvrance window isn't open. {}",
                    approval_hint(access)
                ),
                (true, Some(rows)) => format!(
                    "allowed in part: the Recouvrance window isn't open to approve {label}, so the list shows only the rows {rows} allows"
                ),
            }
        }
    };
    (v.mode, text)
}

/// Why a call to a context the rules don't open is refused, and what the agent can do.
fn context_refusal(policy: &Policy, context: &str) -> (String, String) {
    let reason = format!("context {context:?} is not allowed for this agent");
    match policy.context_denied_by(context) {
        Some(i) => {
            (format!("{reason} ({} denies everything there)", rule_label(policy, Some(i))), DENIED_BY_RULE.into())
        }
        None => (
            format!("{reason} (no Allow or Ask rule names it)"),
            format!("Grantable: the user can add a rule with `--context {context}` {RULES_ARE_SET}."),
        ),
    }
}

/// Arguments that change what a call does, for the approval banner: what the user
/// approves is what runs. Values come from the (redacted) logged arguments.
const KEY_ARGS: [&str; 15] = [
    "command",
    "path",
    "patch",
    "replicas",
    "action",
    "resources",
    "prune",
    "deployment_id",
    "container",
    "to_revision",
    "image",
    "port",
    "minutes",
    "debug",
    "dry_run",
];
/// Most log bytes one `get_logs` call returns.
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
        Some((i, _)) => {
            let more = text[i..].chars().count();
            format!("{}… ({more} more {})", &text[..i], crate::plural::pick(more, "char", "chars"))
        }
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

/// The object an agent named, read for it (with its managedFields when asked): no namespace
/// for a cluster-scoped kind, and what came back must be that object (`check_identity`).
pub(super) async fn get_named(
    client: &kube::Client,
    kind: &Kind,
    ns: Option<&str>,
    name: &str,
    managed_fields: bool,
) -> anyhow::Result<serde_json::Value> {
    let namespace = if kind.namespaced { ns.map(String::from) } else { None };
    let obj = k8s::ObjectRef { kind: kind.clone(), name: name.to_string(), namespace, uid: String::new() };
    let ns = obj.namespace.clone();
    let value = k8s::get_object_opts(client.clone(), obj, managed_fields).await?;
    check_identity(kind, ns.as_deref(), name, &value)?;
    Ok(value)
}

/// A manifest for the approval banner: each object's kind and name, then the YAML
/// that will be applied (a Secret's values masked).
fn manifest_summary(planned: &[(Kind, Option<String>, String, serde_json::Value)]) -> String {
    let mut out = String::from("Objects:\n");
    for (kind, ns, name, _) in planned {
        let ns = ns.as_ref().map_or_default(|n| format!("{n}/"));
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
            let hash = crate::hex::lower(&sha2::Sha256::digest(yaml.as_bytes())[..6]);
            let ns = ns.as_ref().map_or_default(|n| format!("{n}/"));
            format!("{} {ns}{name} sha256:{hash}", kind.ar.kind)
        })
        .collect()
}

/// Whether the rules keep a drain from evicting a pod: an explicit Deny rule on deleting
/// it, not merely the absence of a delete rule.
fn denies_eviction(policy: &Policy, pod: &Access) -> bool {
    let v = policy.decide(pod);
    v.mode == Mode::Deny && v.rule.is_some()
}

/// Namespaces, sorted, of the pods a drain would evict (`k8s::drain` leaves DaemonSet,
/// static, finished and unmanaged pods alone) for which `denied(namespace, name)` holds.
fn denied_namespaces(pods: &[serde_json::Value], denied: impl Fn(&str, &str) -> bool) -> Vec<String> {
    let mut out: Vec<String> = pods
        .iter()
        .filter(|p| {
            let owners = p.pointer("/metadata/ownerReferences").and_then(|o| o.as_array());
            let owners = owners.map_or_default(Vec::as_slice);
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
        let agent = self.hub.agent_for_calls(&id).ok_or("unknown agent")?;
        let session = parts.headers.get("mcp-session-id").and_then(|v| v.to_str().ok()).map(String::from);
        let client = parts.extensions.get::<ClientName>().map(|c| c.0.clone());
        let invalid = validate_args(&args).err();
        let context = args.get("context").and_then(|v| v.as_str()).map(String::from);
        let call = Call {
            hub: self.hub.clone(),
            agent,
            tool,
            args: redact(args),
            gone: parts.extensions.get::<Gone>().cloned().unwrap_or_default(),
            start: Instant::now(),
            approved: false,
            approved_rules: Vec::new(),
            rule: None,
            session,
            client,
            revealed: Vec::new(),
            approval_detail: None,
            plan: None,
            preview: None,
        };
        self.ensure_active(&call)?;
        if !self.hub.take_call(&call.agent.id, call.agent.policy.max_calls_per_minute) {
            return Err(call.deny(format!(
                "rate limit: {} per minute",
                crate::plural::count(call.agent.policy.max_calls_per_minute, "call")
            )));
        }
        if let Some(e) = invalid {
            return Err(call.fail(e));
        }
        // Before connecting: a context no rule names is never contacted.
        if let Some(ctx) = context
            && !call.policy().context_allowed(&ctx)
        {
            let (reason, hint) = context_refusal(call.policy(), &ctx);
            return Err(call.deny_hinted(reason, &hint));
        }
        Ok(call)
    }

    /// Agent access is on, this agent still enabled and still waiting for the call: checked
    /// when a call starts, after waiting for approval, and before each change.
    fn ensure_active(&self, call: &Call) -> Result<(), String> {
        match self.inactive(call) {
            Some(reason) => Err(call.deny_hinted(reason, USER_STATE)),
            None => Ok(()),
        }
    }

    /// Why this call is refused now, if it is.
    fn inactive(&self, call: &Call) -> Option<&'static str> {
        inactive(&self.hub, &call.agent.id, &call.gone)
    }

    /// Evaluate one access request; Ask waits for the user.
    async fn authorize(&self, call: &mut Call, access: &Access) -> Result<(), String> {
        self.authorize_inner(call, access, false, false).await
    }

    /// Evaluate a list request, whose rows are then filtered with [`Call::may_see`]: in one
    /// namespace, or across all of them (`any_namespace`).
    async fn authorize_list(&self, call: &mut Call, access: &Access, any_namespace: bool) -> Result<(), String> {
        self.authorize_inner(call, access, any_namespace, true).await
    }

    async fn authorize_inner(
        &self,
        call: &mut Call,
        access: &Access,
        any_namespace: bool,
        list: bool,
    ) -> Result<(), String> {
        let decide = |call: &Call| {
            if any_namespace { call.policy().decide_any_namespace(access) } else { call.policy().decide(access) }
        };
        // A list can show its allowed rows without approval (Ask rows hidden).
        let fallback = |call: &Call| if list { call.policy().allow_somewhere(access) } else { None };
        loop {
            // The rules may have changed since the call started (or since its last check, or
            // while what it will change was worked out).
            self.reload_rules(call);
            self.ensure_active(call)?;
            let verdict = decide(call);
            match verdict.mode {
                Mode::Allow => {
                    call.allowed_by(verdict.rule);
                    return Ok(());
                }
                Mode::Deny => {
                    return Err(call.deny_hinted(
                        format!("{} — {}", describe(access), call.rule_label(verdict.rule)),
                        &deny_hint(access, verdict.rule),
                    ));
                }
                Mode::Ask => {}
            }
            if self.hub.headless() {
                if let Some(rule) = fallback(call) {
                    call.allowed_by(Some(rule));
                    return Ok(());
                }
                return Err(call.deny_hinted(
                    format!(
                        "{} needs approval in the Recouvrance window ({}), which isn't open",
                        describe(access),
                        call.rule_label(verdict.rule)
                    ),
                    &approval_hint(access),
                ));
            }
            // What the call will change, worked out once, before its first approval
            // (`apply_manifest` asks for each object): allowed calls never get here and pay
            // nothing for it. It takes `preview::TIMEOUT` at most, then the call is decided
            // again: agent access, the agent or its rules may have changed meanwhile.
            if call.preview.is_none()
                && let Some(plan) = &call.plan
            {
                let gone = call.gone.clone();
                let worked_out = tokio::select! {
                    p = preview::preview(plan) => p,
                    // Nobody is left to ask for.
                    () = gone.wait() => return Err(call.deny(Answer::Abandoned.reason())),
                };
                call.preview = Some(worked_out);
                continue;
            }
            let summary =
                format!("{}{}  [{}]", describe(access), arg_details(&call.args), call.rule_label(verdict.rule));
            let request = Approval {
                id: 0,
                agent_id: call.agent.id.clone(),
                agent: call.agent.name.clone(),
                tool: call.tool.to_string(),
                summary,
                detail: call.approval_detail.clone(),
                preview: call.preview.clone(),
                access: access.clone(),
                dry_run: call.args.get("dry_run").and_then(serde_json::Value::as_bool).unwrap_or(false),
                once_only: Approval::only_once(call.tool, &call.args),
                deadline: Instant::now(),
            };
            let answer = self.hub.request_approval(request, call.gone.wait()).await;
            // Access may have been switched off, or the rules changed, while the request waited.
            self.reload_rules(call);
            return if answer == Answer::Approved {
                self.ensure_active(call)?;
                let now = decide(call);
                if now.mode == Mode::Deny {
                    return Err(call.deny_hinted(
                        format!("{} — {}", describe(access), call.rule_label(now.rule)),
                        &deny_hint(access, now.rule),
                    ));
                }
                call.approved = true;
                call.allowed_by(now.rule);
                call.approved_rules =
                    if any_namespace { call.policy().asks_any_namespace(access) } else { call.policy().asks(access) };
                Ok(())
            } else if !matches!(answer, Answer::SwitchedOff | Answer::Abandoned)
                && let Some(rule) = fallback(call)
            {
                call.record(Decision::Rejected, format!("{}; showing the allowed rows only", answer.reason()), 0);
                call.allowed_by(Some(rule));
                self.ensure_active(call)
            } else {
                call.record(Decision::Rejected, answer.reason().into(), 0);
                Err(format!("Rejected: {}.{}", answer.reason(), rejected_hint(answer)))
            };
        }
    }

    /// Read the agent's rules again. A removed agent keeps its old rules, and
    /// [`Self::ensure_active`] then refuses the call.
    fn reload_rules(&self, call: &mut Call) {
        if let Some(agent) = self.hub.agent_for_calls(&call.agent.id) {
            call.agent = agent;
        }
    }

    /// Right before a change reaches the cluster: decide again on the agent's current rules,
    /// so a Deny added, or an Allow removed, while the call waited (for an approval, a dry
    /// run, a lookup) still stops it, as does the agent no longer waiting for the call. An
    /// approved request stays approved while an Ask rule still covers it.
    fn recheck(&self, call: &mut Call, access: &Access) -> Result<(), String> {
        match self.stale_reason(call, access) {
            Some(reason) => Err(call.deny_hinted(reason, &self.stale_hint(call, access))),
            None => Ok(()),
        }
    }

    /// What the agent can do about a change [`stale_reason`](Self::stale_reason) stopped,
    /// on the rules as they are now.
    fn stale_hint(&self, call: &Call, access: &Access) -> String {
        if self.inactive(call).is_some() {
            return USER_STATE.into();
        }
        let v = call.policy().decide(access);
        match v.mode {
            Mode::Deny => deny_hint(access, v.rule),
            Mode::Ask => "An Ask rule covers it now: call again to ask the user.".into(),
            Mode::Allow => "Call again.".into(),
        }
    }

    /// Why a change authorized earlier must not go ahead now, if it must not.
    fn stale_reason(&self, call: &mut Call, access: &Access) -> Option<String> {
        self.reload_rules(call);
        if let Some(reason) = self.inactive(call) {
            return Some(reason.to_string());
        }
        let rule = stopped_by(call.policy(), access, call.approved)?;
        Some(format!("{} — the rules changed while the call waited ({})", describe(access), call.rule_label(rule)))
    }

    /// [`stale_reason`](Self::stale_reason) for a change made of many requests (a drain),
    /// asked again before each one: agent access or the agent turned off, its rules changed,
    /// or the agent no longer waiting for the call, stop the rest.
    fn still_allowed(&self, call: &Call, access: &Access) -> impl Fn() -> Option<String> + Send + 'static {
        stop_check(self.hub.clone(), call.agent.id.clone(), call.approved, access.clone(), call.gone.clone())
    }

    async fn kind(&self, call: &Call, context: &str, query: &str) -> Result<(kube::Client, Kind), String> {
        self.find_kind(context, query).await.map_err(|e| call.finish(Err(e)).unwrap_err())
    }

    /// The kind a query names in a context, from its discovery, with the context's client.
    async fn find_kind(&self, context: &str, query: &str) -> anyhow::Result<(kube::Client, Kind)> {
        let (client, kinds) = self.hub.client(context).await?;
        if let Some(kind) = k8s::find_kind(&kinds, query).cloned() {
            return Ok((client, kind));
        }
        // A CRD an agent just applied is looked up in its own group at once.
        if let Some(kind) = self.hub.new_kind_named(context, query).await {
            return Ok((client, kind));
        }
        // Maybe a CRD installed since the first call.
        match self.hub.rediscover(context).await.and_then(|kinds| k8s::find_kind(&kinds, query).cloned()) {
            Some(kind) => Ok((client, kind)),
            None => Err(anyhow::anyhow!("unknown resource type {query:?}")),
        }
    }
}

/// What the agent can do once the user's answer didn't let a call go ahead.
fn rejected_hint(answer: Answer) -> &'static str {
    match answer {
        Answer::Rejected => " The user said no: don't retry unless they ask you to.",
        Answer::TimedOut => " Call again when the user can answer.",
        Answer::Approved | Answer::SwitchedOff | Answer::Abandoned => "",
    }
}

/// What stops a change authorized earlier under the rules as they are now (the rule that
/// decided, if one did): anything but an Allow, or an Ask the user approved for this very
/// change (`approved`).
fn stopped_by(policy: &Policy, access: &Access, approved: bool) -> Option<Option<usize>> {
    let verdict = policy.decide(access);
    match verdict.mode {
        Mode::Allow => None,
        Mode::Ask if approved => None,
        _ => Some(verdict.rule),
    }
}

pub(crate) fn access_for(
    kind: &Kind,
    context: &str,
    verb: Verb,
    namespace: Option<String>,
    name: Option<String>,
) -> Access {
    Access {
        context: context.to_string(),
        verb,
        resource: kind.kubectl_name(),
        resource_aliases: vec![kind.ar.plural.clone(), kind.ar.kind.to_lowercase()],
        namespace: if kind.namespaced { namespace } else { None },
        name,
        cluster_scoped: !kind.namespaced,
    }
}

/// Whether listing a kind could show something: in some namespace, or for a cluster-scoped
/// kind, outside any (only rules for every namespace cover it).
fn list_verdict(policy: &Policy, kind: &Kind, context: &str) -> Mode {
    let access = access_for(kind, context, Verb::List, None, None);
    if kind.namespaced { policy.decide_any_namespace(&access).mode } else { policy.decide(&access).mode }
}

/// Namespaced kinds need a namespace for single-object verbs (a refused call is logged).
fn require_ns(call: &Call, kind: &Kind, ns: Option<&str>) -> Result<(), String> {
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
pub struct CheckAccessParams {
    #[schemars(description = "the calls you plan, at most 20")]
    pub requests: Vec<PlannedRequest>,
}

/// One call an agent plans, as its rules see it.
#[derive(Deserialize, Serialize, schemars::JsonSchema)]
pub struct PlannedRequest {
    #[schemars(description = "kubeconfig context name, from list_contexts")]
    pub context: String,
    #[schemars(
        description = "the rules' verb: get, list, logs, reveal, exec, scale, restart, patch, refresh, sync, suspend, apply, delete, drain or forward"
    )]
    pub verb: String,
    #[schemars(description = "resource type like kubectl: pods, deploy, certificates.cert-manager.io")]
    pub kind: String,
    #[schemars(description = "namespace; omit for a cluster-scoped kind, or for every namespace")]
    pub namespace: Option<String>,
    #[schemars(description = "object name; omit for a list")]
    pub name: Option<String>,
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
    #[schemars(
        description = "container name; defaults to the pod's default container (its `kubectl.kubernetes.io/default-container` annotation, else its first), as kubectl does"
    )]
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
pub struct ReachParams {
    pub context: String,
    #[schemars(description = "services, ingresses or httproutes")]
    pub kind: String,
    pub namespace: String,
    pub name: String,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ProblemsParams {
    pub context: String,
    #[schemars(description = "Only this namespace; all namespaces you may list when omitted")]
    pub namespace: Option<String>,
    #[schemars(description = "Include minor findings (restarts, cordoned nodes). Default false")]
    pub include_minor: Option<bool>,
    #[schemars(
        description = "Oldest findings to accept, in seconds: 0 scans now (after a fix, to check it). By default the app's latest scan while it is recent (2 minutes or more on a big cluster)"
    )]
    pub max_age_seconds: Option<u64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct HistoryParams {
    pub context: String,
    #[schemars(description = "pods, deployments, statefulsets, daemonsets, replicasets, jobs, services or nodes")]
    pub kind: String,
    #[schemars(description = "required for namespaced kinds (all but nodes)")]
    pub namespace: Option<String>,
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
    #[schemars(
        description = "reconcile (Flux) · refresh, hard_refresh, sync, terminate (the operation running) or rollback (Argo CD) · suspend or resume (Flux) · diagnose (Argo CD, read only)"
    )]
    pub action: String,
    #[schemars(
        description = "sync only: sync these resources of the Application alone (hooks don't run then); omit to sync it whole"
    )]
    pub resources: Option<Vec<ArgoResource>>,
    #[schemars(
        description = "sync only: also delete what is no longer in git (of the resources named, when some are); the reply names what it deletes"
    )]
    pub prune: Option<bool>,
    #[schemars(
        description = "sync or rollback: Argo CD applies nothing and reports what it would do (the reply waits up to 30 s for it)"
    )]
    pub dry_run: Option<bool>,
    #[schemars(
        description = "rollback only: the id of the deployment in status.history to go back to; omit for the one before the newest. Argo CD refuses a rollback while auto-sync is on"
    )]
    pub deployment_id: Option<i64>,
}

/// A resource of an Argo CD Application, as its `status.resources` names it.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct ArgoResource {
    #[schemars(description = "API group; empty or omitted for the core group")]
    pub group: Option<String>,
    pub kind: String,
    #[schemars(description = "omit for a cluster-scoped resource")]
    pub namespace: Option<String>,
    pub name: String,
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
                a.namespace.as_ref().map_or_default(|n| format!(", namespace `{n}`"))
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

impl KubeMcp {
    /// Every tool, as `tools/list` gives them to agents. What the Activity page and
    /// `my_permissions` say about the tools is read from these definitions: lists of names
    /// kept by hand drifted from them.
    fn tools() -> ToolRouter<Self> {
        Self::read_tools()
            + Self::write_tools()
            + Self::rollout_tools()
            + Self::job_tools()
            + Self::ops_tools()
            + Self::node_tools()
            + Self::helm_tools()
            + Self::history_tools()
            + Self::logs_tools()
            + Self::diff_tools()
            + Self::cluster_tools()
            + Self::file_tools()
            + Self::metrics_tools()
            + Self::argo_tools()
    }
}

#[tool_handler(router = Self::tools())]
#[prompt_handler]
impl ServerHandler for KubeMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_prompts().build()).with_instructions(
            "Access to the user's Kubernetes clusters through Recouvrance. Start with my_permissions and list_contexts. \
             Every call is checked against rules the user set for you (by context, verb, resource, \
             namespace and name) and shown to them; some calls wait for their approval. \
             Before a task of several calls, check_access tells which need approval or a rule the user must add. \
             A refusal says whether it is final (a Deny rule) or what the user could grant. \
             Prefer dry_run for writes first, and explain what you are about to change.",
        )
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        mut context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, rmcp::ErrorData> {
        // The tools read the request's HTTP parts (`begin`): rmcp's token for this request
        // joins its connection's there, so the call knows when its agent stops waiting. The
        // client is read there too: rmcp knows it from the session's `initialize`, or from the
        // request itself in a stateless session, where no session id groups the calls.
        let info = context.client_info();
        if let Some(parts) = context.extensions.get_mut::<Parts>() {
            let gone = parts.extensions.remove::<Gone>().unwrap_or_default();
            parts.extensions.insert(Gone { request: Some(context.ct.clone()), ..gone });
            let header = parts.headers.get(CLIENT_HEADER).and_then(|v| v.to_str().ok());
            if let Some(client) = client_label(info.as_ref(), header) {
                parts.extensions.insert(ClientName(client));
            }
        }
        Self::tools().call(ToolCallContext::new(self, request, context)).await
    }
}

/// What the tool definitions say, read once: the tools that only read, and those that take
/// `dry_run` (by name).
struct ToolFacts {
    reads: HashSet<String>,
    dry_run: Vec<String>,
}

fn tool_facts() -> &'static ToolFacts {
    static FACTS: LazyLock<ToolFacts> = LazyLock::new(|| {
        let tools = KubeMcp::tools().list_all();
        ToolFacts {
            reads: tools.iter().filter(|t| only_reads(t)).map(|t| t.name.to_string()).collect(),
            dry_run: tools.iter().filter(|t| takes_dry_run(t)).map(|t| t.name.to_string()).collect(),
        }
    });
    &FACTS
}

/// Whether a tool's definition says it changes nothing (`readOnlyHint`).
fn only_reads(tool: &Tool) -> bool {
    tool.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true)
}

/// Whether a tool's input has a `dry_run` argument.
fn takes_dry_run(tool: &Tool) -> bool {
    tool.input_schema.get("properties").and_then(|p| p.get("dry_run")).is_some()
}

/// Whether a tool may change something: its definition doesn't say it only reads. A tool
/// this version doesn't know (in a log another version wrote) may have changed something too.
fn tool_writes(tool: &str) -> bool {
    !tool_facts().reads.contains(tool)
}

/// The tools whose input takes `dry_run`, by name.
fn dry_run_tools() -> &'static [String] {
    &tool_facts().dry_run
}

/// Whether a logged call may have changed something. The Activity page shows these calls
/// under Writes, counts them in each task and colors their tool. A call of a tool that may
/// change something counts, unless it was a dry run, `gitops`'s `diagnose`, or `browse_files`
/// without `debug` (the debug container it may add is the only change it makes). A `gitops`
/// dry run counts: it writes the Application's operation, which Argo CD then runs (applying
/// nothing) and which blocks any other sync, terminate or rollback until it ends.
pub fn call_writes(tool: &str, args: &serde_json::Value) -> bool {
    let on = |key: &str| args.get(key).and_then(serde_json::Value::as_bool) == Some(true);
    let reads = match tool {
        "gitops" => args.get("action").and_then(serde_json::Value::as_str) == Some("diagnose"),
        "browse_files" => !on("debug"),
        _ => on("dry_run"),
    };
    tool_writes(tool) && !reads
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

/// Why an agent's call is refused now, if it is: agent access or the agent turned off, or
/// the agent no longer waiting for the call (it is not there to see what a change did).
fn inactive(hub: &Hub, agent_id: &str, gone: &Gone) -> Option<&'static str> {
    if !hub.agents_enabled() {
        return Some("agent access is switched off in Recouvrance");
    }
    if !hub.agent(agent_id).is_some_and(|a| a.enabled) {
        return Some("this agent is disabled");
    }
    gone.is_gone().then_some(Answer::Abandoned.reason())
}

/// Why an agent's change, authorized at first, must stop now (see `still_allowed`).
pub(crate) fn stop_check(
    hub: Arc<Hub>,
    agent_id: String,
    approved: bool,
    access: Access,
    gone: Gone,
) -> impl Fn() -> Option<String> + Send + 'static {
    move || {
        if let Some(reason) = inactive(&hub, &agent_id, &gone) {
            return Some(reason.to_string());
        }
        let mode = hub.agent_for_calls(&agent_id).map_or(Mode::Deny, |a| a.policy.decide(&access).mode);
        match mode {
            Mode::Allow => None,
            Mode::Ask if approved => None,
            _ => Some(format!("{} — the rules changed while it ran", describe(&access))),
        }
    }
}

/// How long to wait before trying a busy port again.
const BIND_RETRY: std::time::Duration = std::time::Duration::from_secs(5);

/// Listens on `127.0.0.1` at `port()`. While the address is in use (another Recouvrance, or one
/// that is quitting), says so through `busy` and tries again every `every`, reading `port()`
/// again each time, so a port changed in Settings meanwhile is the one tried. Any other
/// error ends it.
async fn bind(
    port: impl Fn() -> u16,
    every: std::time::Duration,
    busy: impl Fn(String),
) -> Result<tokio::net::TcpListener, String> {
    loop {
        let port = port();
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => return Ok(l),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => busy(format!(
                "cannot listen on 127.0.0.1:{port}: {e} (another Recouvrance may be running; \
                 trying again every {}s, or change the port in Settings → Agents)",
                every.as_secs()
            )),
            Err(e) => return Err(format!("cannot listen on 127.0.0.1:{port}: {e}")),
        }
        tokio::time::sleep(every).await;
    }
}

/// Serve MCP on 127.0.0.1 until the guard is dropped.
pub fn start(hub: Arc<Hub>) -> TaskGuard {
    k8s::spawn_guarded(async move {
        let port = hub.port();
        let mcp = {
            let hub = hub.clone();
            StreamableHttpService::new(
                move || Ok(KubeMcp::new(hub.clone())),
                Arc::new(sessions::Sessions::new(sessions::IDLE)),
                StreamableHttpServerConfig::default(),
            )
        };
        let app = axum::Router::new()
            .nest_service("/mcp", mcp)
            .layer(axum::middleware::from_fn(gone::hangup))
            .layer(axum::middleware::from_fn_with_state(hub.clone(), auth));
        let listener = match bind(|| hub.port(), BIND_RETRY, |e| hub.emit(HubEvent::ServerStatus(Err(e)))).await {
            Ok(l) => l,
            Err(e) => {
                hub.emit(HubEvent::ServerStatus(Err(e)));
                return;
            }
        };
        let port = listener.local_addr().map_or(port, |a| a.port());
        hub.emit(HubEvent::ServerStatus(Ok(port)));
        if let Err(e) = axum::serve(listener, app).await {
            hub.emit(HubEvent::ServerStatus(Err(e.to_string())));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    /// A cluster answering any GET with a Node named after the last segment of the path,
    /// or `other` for `/n2`; records the paths asked.
    #[derive(Clone, Default)]
    struct Nodes(Arc<std::sync::Mutex<Vec<String>>>);

    impl tower::Service<http::Request<kube::client::Body>> for Nodes {
        type Response = http::Response<http_body_util::Full<bytes::Bytes>>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<kube::client::Body>) -> Self::Future {
            let path = req.uri().path().to_string();
            let name = match path.rsplit('/').next() {
                Some("n2") => "other",
                Some(n) => n,
                None => "",
            };
            let body = json!({"apiVersion": "v1", "kind": "Node", "metadata": {"name": name}}).to_string();
            self.0.lock().unwrap().push(path);
            std::future::ready(Ok(http::Response::new(http_body_util::Full::new(bytes::Bytes::from(body)))))
        }
    }

    #[tokio::test]
    async fn a_named_object_is_read_where_its_kind_lives_and_must_be_the_one_named() {
        let node = Kind {
            ar: kube::discovery::ApiResource {
                group: String::new(),
                version: "v1".into(),
                api_version: "v1".into(),
                kind: "Node".into(),
                plural: "nodes".into(),
            },
            namespaced: false,
            section: "",
            label: String::new(),
        };
        let cluster = Nodes::default();
        let client = kube::Client::new(cluster.clone(), "default");
        // A namespace given for a cluster-scoped kind is left out of the path.
        let v = get_named(&client, &node, Some("default"), "n1", false).await.unwrap();
        assert_eq!(v["metadata"]["name"], "n1");
        assert_eq!(cluster.0.lock().unwrap().as_slice(), ["/api/v1/nodes/n1"]);
        // Another object than the one named is refused.
        assert!(get_named(&client, &node, None, "n2", false).await.is_err());
    }

    #[test]
    fn approving_find_problems_shows_only_pod_findings() {
        let ask = super::super::Rule {
            effect: Mode::Ask,
            verbs: vec!["list".into()],
            resources: vec!["*".into()],
            namespaces: vec!["team-b".into()],
            contexts: vec!["*".into()],
            names: vec!["*".into()],
            ..Default::default()
        };
        let policy = Policy { rules: vec![ask], ..Default::default() };
        let access = |resource: &str| Access {
            context: "c".into(),
            verb: Verb::List,
            resource: resource.into(),
            resource_aliases: vec![],
            namespace: Some("team-b".into()),
            name: Some("web".into()),
            cluster_scoped: false,
        };
        let approved = policy.asks(&access("pods"));
        assert!(!approved.is_empty());
        assert!(problem_visible(&policy, &approved, true, &access("pods")));
        assert!(!problem_visible(&policy, &approved, false, &access("deployments")));
        assert!(!problem_visible(&policy, &[], true, &access("pods")));
    }

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
    fn a_change_turned_ask_needs_its_own_approval() {
        let rule = |effect: Mode, name: &str| crate::mcp::Rule {
            effect,
            verbs: vec!["apply".into()],
            names: vec![name.into()],
            contexts: vec!["*".into()],
            ..crate::mcp::Rule::default()
        };
        let cms = kind("", "v1", "ConfigMap", "configmaps", true);
        let a = access_for(&cms, "c", Verb::Apply, Some("app".into()), Some("a".into()));
        let b = access_for(&cms, "c", Verb::Apply, Some("app".into()), Some("b".into()));
        // B's rule turned into Ask while A was applied: approving A does not cover B.
        let p = Policy { rules: vec![rule(Mode::Allow, "a"), rule(Mode::Ask, "b")], ..Policy::default() };
        assert_eq!(stopped_by(&p, &a, false), None);
        assert_eq!(stopped_by(&p, &b, false), Some(Some(1)));
        assert_eq!(stopped_by(&p, &b, true), None);
        // A Deny stops it, approved or not.
        let p = Policy { rules: vec![rule(Mode::Allow, "*"), rule(Mode::Deny, "b")], ..Policy::default() };
        assert_eq!(stopped_by(&p, &b, true), Some(Some(1)));
        // So does a removed Allow.
        let p = Policy { rules: vec![], ..Policy::default() };
        assert_eq!(stopped_by(&p, &a, false), Some(None));
    }

    /// A refusal tells a Deny rule (final) from a missing rule (grantable, with the rule that
    /// would allow the call), and `check_access` says what the call would get.
    #[test]
    fn a_refusal_says_whether_the_user_could_allow_it() {
        use crate::mcp::Rule;
        let deploys = kind("apps", "v1", "Deployment", "deployments", true);
        let read = Rule { contexts: vec!["*".into()], ..Rule::default() };
        let never_prod = Rule {
            effect: Mode::Deny,
            verbs: vec!["*".into()],
            contexts: vec!["prod".into()],
            note: "Never prod".into(),
            ..Rule::default()
        };
        let ask =
            Rule { effect: Mode::Ask, verbs: vec!["scale".into()], contexts: vec!["*".into()], ..Rule::default() };
        let policy = Policy { rules: vec![read, never_prod, ask], ..Policy::default() };
        let web = |ctx: &str, verb| access_for(&deploys, ctx, verb, Some("team-a".into()), Some("web".into()));

        let (mode, text) = access_verdict(&policy, &web("prod", Verb::Restart), false, false);
        assert_eq!(mode, Mode::Deny);
        assert_eq!(text, format!("denied by rule #2 (Never prod). {DENIED_BY_RULE}"));
        let (mode, text) = access_verdict(&policy, &web("staging", Verb::Restart), false, false);
        assert_eq!(mode, Mode::Deny);
        assert!(text.starts_with("denied. Grantable: no rule allows this"), "{text}");
        assert!(text.contains("`restart deployments.apps/web -n team-a --context staging` under allow"), "{text}");

        let allowed = access_verdict(&policy, &web("staging", Verb::Get), false, false);
        assert_eq!(allowed, (Mode::Allow, "allowed by rule #1".to_string()));
        let (mode, text) = access_verdict(&policy, &web("staging", Verb::Scale), false, false);
        assert_eq!(mode, Mode::Ask);
        assert!(text.starts_with("needs the user's approval (rule #3)"), "{text}");
        // Nobody can approve without the window, and an Allow rule doesn't win over the Ask
        // rule: the rule offered is an approved one.
        let (_, text) = access_verdict(&policy, &web("staging", Verb::Scale), false, true);
        assert!(text.contains("`scale deployments.apps/web -n team-a --context staging` under approved"), "{text}");

        // A list across namespaces that asks in some and is allowed in others still shows
        // the allowed rows when nobody can approve, as the call does.
        let list = |effect, ns: &str| Rule {
            effect,
            verbs: vec!["list".into()],
            namespaces: vec![ns.into()],
            contexts: vec!["*".into()],
            ..Rule::default()
        };
        let lists = Policy { rules: vec![list(Mode::Allow, "team-a"), list(Mode::Ask, "team-b")], ..Policy::default() };
        let all = access_for(&deploys, "c", Verb::List, None, None);
        let (mode, text) = access_verdict(&lists, &all, true, true);
        assert_eq!(mode, Mode::Ask);
        assert!(text.starts_with("allowed in part:") && text.contains("only the rows rule #1 allows"), "{text}");

        // A context no rule names can be granted; one a Deny of everything covers can't.
        let scoped = Policy {
            rules: vec![
                Rule { contexts: vec!["staging".into()], ..Rule::default() },
                Rule { effect: Mode::Deny, verbs: vec!["*".into()], contexts: vec!["prod".into()], ..Rule::default() },
            ],
            ..Policy::default()
        };
        assert_eq!(context_refusal(&scoped, "prod").1, DENIED_BY_RULE);
        let (reason, hint) = context_refusal(&scoped, "dev");
        assert!(reason.contains("no Allow or Ask rule names it"), "{reason}");
        assert!(hint.starts_with("Grantable") && hint.contains("`--context dev`"), "{hint}");
    }

    #[test]
    fn cluster_scoped_lists_are_not_all_namespace_lists() {
        let rule = |effect: Mode, ns: &str| crate::mcp::Rule {
            effect,
            verbs: vec!["list".into()],
            namespaces: vec![ns.into()],
            contexts: vec!["*".into()],
            ..crate::mcp::Rule::default()
        };
        let nodes = kind("", "v1", "Node", "nodes", false);
        let pods = kind("", "v1", "Pod", "pods", true);
        let ask = Policy { rules: vec![rule(Mode::Ask, "team-a")], ..Policy::default() };
        assert_eq!(list_verdict(&ask, &nodes, "c"), Mode::Deny);
        assert_eq!(list_verdict(&ask, &pods, "c"), Mode::Ask);
        let allow = Policy { rules: vec![rule(Mode::Allow, "team-a")], ..Policy::default() };
        assert_eq!(list_verdict(&allow, &nodes, "c"), Mode::Deny);
        assert_eq!(list_verdict(&allow, &pods, "c"), Mode::Allow);
        // Rules for every namespace cover cluster-scoped kinds.
        let all = Policy { rules: vec![rule(Mode::Allow, "*")], ..Policy::default() };
        assert_eq!(list_verdict(&all, &nodes, "c"), Mode::Allow);
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
        let mut changed = planned;
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

    #[test]
    fn a_new_deny_keeps_its_pods_in_a_running_drain() {
        let rule = |effect: Mode, verb: &str, ns: &str| crate::mcp::Rule {
            effect,
            verbs: vec![verb.into()],
            namespaces: vec![ns.into()],
            contexts: vec!["*".into()],
            ..crate::mcp::Rule::default()
        };
        let pods = kind("", "v1", "Pod", "pods", true);
        let pod = |ns: &str| access_for(&pods, "c", Verb::Delete, Some(ns.into()), Some("web".into()));
        // No delete rule at all: the drain rule is what lets it evict.
        let allowed = Policy { rules: vec![rule(Mode::Allow, "drain", "*")], ..Policy::default() };
        assert!(!denies_eviction(&allowed, &pod("app")));
        // A Deny on deleting pods added while it runs keeps that namespace's pods only.
        let mut denied = allowed;
        denied.rules.push(rule(Mode::Deny, "delete", "app"));
        assert!(denies_eviction(&denied, &pod("app")));
        assert!(!denies_eviction(&denied, &pod("other")));
    }

    /// A call as it starts, by `agent`, that `gone` tells when its agent stops waiting.
    fn started_call(hub: &Arc<Hub>, agent: &Agent, gone: Gone) -> Call {
        Call {
            hub: hub.clone(),
            agent: agent.clone(),
            tool: "patch_resource",
            args: json!({}),
            gone,
            start: Instant::now(),
            approved: false,
            approved_rules: Vec::new(),
            rule: None,
            session: None,
            client: None,
            revealed: Vec::new(),
            approval_detail: None,
            plan: None,
            preview: None,
        }
    }

    /// Once its agent stops waiting, a call changes nothing more, approved or not: the check
    /// right before a change, and a drain's before each eviction, refuse it as they do with
    /// agent access switched off.
    #[test]
    fn a_call_its_agent_stopped_waiting_for_changes_nothing() {
        let ask = crate::mcp::Rule {
            effect: Mode::Ask,
            verbs: vec!["patch".into()],
            contexts: vec!["*".into()],
            ..crate::mcp::Rule::default()
        };
        let agent = Agent {
            id: "a1".into(),
            name: "claude".into(),
            token_sha256: String::new(),
            enabled: true,
            policy: Policy { rules: vec![ask], ..Policy::default() },
        };
        let hub = Hub::for_test(vec![agent.clone()]);
        let mcp = KubeMcp::new(hub.clone());
        let cms = kind("", "v1", "ConfigMap", "configmaps", true);
        let access = access_for(&cms, "c", Verb::Patch, Some("app".into()), Some("cfg".into()));
        let gone_reason = Some(Answer::Abandoned.reason().to_string());
        for cancel_request in [true, false] {
            let (request, connection) = (CancellationToken::new(), CancellationToken::new());
            let gone = Gone { request: Some(request.clone()), connection: Some(connection.clone()) };
            let mut call = started_call(&hub, &agent, gone.clone());
            call.approved = true;
            let stop = stop_check(hub.clone(), agent.id.clone(), true, access.clone(), gone);
            // Approved, with its agent waiting: it goes ahead.
            assert_eq!(mcp.stale_reason(&mut call, &access), None);
            assert_eq!(stop(), None);
            if cancel_request {
                request.cancel();
            } else {
                connection.cancel();
            }
            assert_eq!(mcp.stale_reason(&mut call, &access), gone_reason, "cancel_request={cancel_request}");
            assert_eq!(stop(), gone_reason, "cancel_request={cancel_request}");
        }
    }

    /// A cluster holding the ConfigMap `app/cfg` (`a: "1"`): a GET reads it, a patch answers
    /// with `a: "2"`. Records each request as (method, path and query).
    #[derive(Clone, Default)]
    struct OneConfigMap(Arc<std::sync::Mutex<Vec<(String, String)>>>);

    impl OneConfigMap {
        fn requests(&self) -> Vec<(String, String)> {
            self.0.lock().unwrap().clone()
        }
    }

    impl tower::Service<http::Request<kube::client::Body>> for OneConfigMap {
        type Response = http::Response<http_body_util::Full<bytes::Bytes>>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<kube::client::Body>) -> Self::Future {
            let a = if req.method() == http::Method::PATCH { "2" } else { "1" };
            let body = json!({"apiVersion": "v1", "kind": "ConfigMap",
                "metadata": {"name": "cfg", "namespace": "app", "resourceVersion": "7"}, "data": {"a": a}});
            let target = req.uri().path_and_query().map_or_default(ToString::to_string);
            self.0.lock().unwrap().push((req.method().to_string(), target));
            std::future::ready(Ok(http::Response::new(http_body_util::Full::new(bytes::Bytes::from(body.to_string())))))
        }
    }

    /// What a patch will change is worked out before its approval is asked for, from a dry
    /// run, and the approval shows it; an allowed patch reads nothing more.
    #[tokio::test]
    async fn an_approval_shows_what_the_patch_will_change() {
        use futures::StreamExt as _;
        let cms = kind("", "v1", "ConfigMap", "configmaps", true);
        let access = access_for(&cms, "c", Verb::Patch, Some("app".into()), Some("cfg".into()));
        let cluster = OneConfigMap::default();
        let plan = || Plan::Patch {
            client: kube::Client::new(cluster.clone(), "default"),
            kind: cms.clone(),
            ns: Some("app".into()),
            name: "cfg".into(),
            patch: kube::api::Patch::Merge(json!({"data": {"a": "2"}})),
        };
        let agent = |effect: Mode| Agent {
            id: "a1".into(),
            name: "claude".into(),
            token_sha256: String::new(),
            enabled: true,
            policy: Policy {
                rules: vec![crate::mcp::Rule {
                    effect,
                    verbs: vec!["patch".into()],
                    contexts: vec!["*".into()],
                    ..crate::mcp::Rule::default()
                }],
                ..Policy::default()
            },
        };
        // Allowed: nothing is worked out, nothing more is read.
        let allowed = agent(Mode::Allow);
        let hub = Hub::for_test(vec![allowed.clone()]);
        let mut call = started_call(&hub, &allowed, Gone::default());
        call.plan = Some(plan());
        KubeMcp::new(hub).authorize(&mut call, &access).await.unwrap();
        assert_eq!(call.preview, None);
        assert!(cluster.requests().is_empty(), "{:?}", cluster.requests());
        // Asked: worked out first, and shown with the request.
        let asker = agent(Mode::Ask);
        let hub = Hub::for_test(vec![asker.clone()]);
        let mut events = hub.subscribe();
        let mut call = started_call(&hub, &asker, Gone::default());
        call.plan = Some(plan());
        let mcp = KubeMcp::new(hub.clone());
        let approve = async {
            while let Some(event) = events.next().await {
                if let HubEvent::ApprovalRequested(a) = &*event {
                    hub.resolve_approval(a.id, true);
                    return a.preview.clone();
                }
            }
            None
        };
        let (authorized, shown) = tokio::join!(mcp.authorize(&mut call, &access), approve);
        authorized.unwrap();
        let shown = shown.expect("the approval shows what will change");
        let text = shown.text();
        assert!(text.contains("ConfigMap app/cfg: changed") && text.contains("data.a: 1 → 2"), "{text}");
        assert_eq!(call.preview.as_ref(), Some(&shown));
        let requests = cluster.requests();
        assert_eq!(requests.len(), 2, "{requests:?}");
        assert!(
            requests.iter().any(|(m, t)| m == "GET" && t == "/api/v1/namespaces/app/configmaps/cfg"),
            "{requests:?}"
        );
        assert!(requests.iter().any(|(m, t)| m == "PATCH" && t.contains("dryRun=All")), "{requests:?}");
    }

    /// A port busy at start is tried again, and the server listens once it is free (#794).
    #[tokio::test]
    async fn a_busy_port_is_tried_again_until_free() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = held.local_addr().unwrap().port();
        let said = Arc::new(std::sync::Mutex::new(Vec::new()));
        let busy = {
            let said = said.clone();
            move |e: String| said.lock().unwrap().push(e)
        };
        let every = std::time::Duration::from_millis(20);
        let bound = tokio::spawn(async move { bind(|| port, every, busy).await });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(!bound.is_finished(), "it waits while the port is busy");
        drop(held);
        let listener = tokio::time::timeout(std::time::Duration::from_secs(5), bound).await.unwrap().unwrap().unwrap();
        assert_eq!(listener.local_addr().unwrap().port(), port);
        let said = said.lock().unwrap();
        assert!(!said.is_empty() && said[0].contains("trying again"), "{said:?}");
    }

    /// A port changed while waiting is the one tried next.
    #[tokio::test]
    async fn a_port_changed_while_waiting_is_used() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let busy_port = held.local_addr().unwrap().port();
        let tries = std::sync::atomic::AtomicU32::new(0);
        // The first try finds the busy port, the next ones a free one (0: any).
        let port = || if tries.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 { busy_port } else { 0 };
        let every = std::time::Duration::from_millis(10);
        let listener =
            tokio::time::timeout(std::time::Duration::from_secs(5), bind(port, every, |_| {})).await.unwrap().unwrap();
        assert_ne!(listener.local_addr().unwrap().port(), busy_port);
    }

    fn read_only(tool: &Tool) -> bool {
        tool.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true)
    }

    /// A new tool must say whether it changes anything: the Activity page counts its calls
    /// from that, and agents' clients read it.
    #[test]
    fn every_tool_says_whether_it_only_reads() {
        let tools = KubeMcp::tools().list_all();
        assert!(tools.len() >= 49, "{} tools", tools.len());
        for t in &tools {
            let hint = t.annotations.as_ref().and_then(|a| a.read_only_hint);
            assert!(hint.is_some(), "{} doesn't say whether it only reads (readOnlyHint)", t.name);
        }
    }

    /// Only the tools that just add something say `destructiveHint: false`, on which a client
    /// may soften its confirmation. `trigger_cronjob` doesn't: its Job runs the CronJob's own
    /// code, which may delete or overwrite data.
    #[test]
    fn only_tools_that_just_add_say_they_destroy_nothing() {
        let tools = KubeMcp::tools().list_all();
        let additive: HashSet<String> = tools
            .iter()
            .filter(|t| t.annotations.as_ref().and_then(|a| a.destructive_hint) == Some(false))
            .map(|t| t.name.to_string())
            .collect();
        let expected: HashSet<String> = ["browse_files", "debug_pod", "port_forward"].map(String::from).into();
        assert_eq!(additive, expected);
    }

    /// Whether a call wrote comes from the tools' definitions, not from a list of names kept
    /// by hand, which missed 10 tools that change things (#843).
    #[test]
    fn every_tool_that_may_change_something_counts_as_a_write() {
        let tools = KubeMcp::tools().list_all();
        // Called for real, with the arguments that make `gitops` and `browse_files` change something.
        let real = json!({"dry_run": false, "action": "sync", "debug": true});
        for t in &tools {
            assert_eq!(call_writes(&t.name, &real), !read_only(t), "{}", t.name);
        }
        for tool in [
            "set_image",
            "rollout_undo",
            "rollout_pause",
            "rollout_resume",
            "trigger_cronjob",
            "cordon_node",
            "uncordon_node",
            "debug_pod",
            "port_forward",
            "stop_port_forward",
        ] {
            assert!(tools.iter().any(|t| t.name == tool), "{tool} is a tool");
            assert!(call_writes(tool, &json!({"dry_run": null})), "{tool}");
        }
        // A dry run changes nothing, but `gitops`'s: Argo CD's own writes the operation.
        assert_eq!(dry_run_tools().len(), 14, "{:?}", dry_run_tools());
        for tool in dry_run_tools() {
            assert_eq!(call_writes(tool, &json!({"dry_run": true})), tool == "gitops", "{tool}");
        }
        assert!(call_writes("gitops", &json!({"action": "sync", "dry_run": true})));
        // Their arguments say when these only read.
        assert!(!call_writes("gitops", &json!({"action": "diagnose"})));
        assert!(!call_writes("browse_files", &json!({"path": "/etc", "debug": false})));
        assert!(!call_writes("get_logs", &json!({})));
        // A tool this version doesn't know (a log another version wrote) may have changed something.
        assert!(call_writes("a_tool_of_another_version", &json!({})));
    }

    /// A tool that takes `dry_run` changes something: a read has nothing to try first, and
    /// `my_permissions` would offer agents a choice that does nothing.
    #[test]
    fn only_changes_take_dry_run() {
        let tools = KubeMcp::tools().list_all();
        for t in tools.iter().filter(|t| read_only(t)) {
            let dry_run = t.input_schema.get("properties").and_then(|p| p.get("dry_run"));
            assert!(dry_run.is_none(), "{} only reads but takes dry_run", t.name);
        }
    }

    /// A call's client is named by its `clientInfo`, the assistant's label first when it
    /// adds one; what the client sent is cleaned and capped before it reaches the log.
    #[test]
    fn a_client_is_named_by_what_it_sent() {
        use rmcp::model::Implementation;
        assert_eq!(
            client_label(Some(&Implementation::new("claude-code", "2.1.3")), None).as_deref(),
            Some("claude-code 2.1.3")
        );
        assert_eq!(
            client_label(Some(&Implementation::new("claude-code", "2.1.3")), Some("assistant")).as_deref(),
            Some("assistant via claude-code 2.1.3")
        );
        assert_eq!(client_label(Some(&Implementation::new("codex", "")), None).as_deref(), Some("codex"));
        assert_eq!(client_label(None, Some("assistant")).as_deref(), Some("assistant"));
        // Nothing said, or only blanks: no client.
        assert_eq!(client_label(None, None), None);
        assert_eq!(client_label(Some(&Implementation::new(" \n", "1")), Some("")), None);
        // Control characters can't break the log's lines or the page, and a long name is cut.
        let odd = client_label(Some(&Implementation::new("evil\nname\u{1b}[31m", "1\r")), None).unwrap();
        assert_eq!(odd, "evilname[31m 1");
        let long = client_label(Some(&Implementation::new("x".repeat(500), "9".repeat(500))), None).unwrap();
        assert!(long.chars().count() <= 2 * (MAX_CLIENT_CHARS + 1) + 1, "{long}");
    }
}
