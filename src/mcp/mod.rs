//! Local MCP server for AI agents, and the shared state the UI uses to
//! manage it: agents, their access rules, approvals and the activity log.

pub mod rules_file;
pub mod server;

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use futures::channel::{mpsc, oneshot};
use kube::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::k8s::{self, Kind};

pub const DEFAULT_PORT: u16 = 47070;
const MAX_AUDIT_IN_MEMORY: usize = 2000;
/// audit.jsonl moves to audit.jsonl.1 (replacing the older one) past this size.
const MAX_AUDIT_BYTES: u64 = 10 * 1024 * 1024;
/// Discovery runs again for an unknown kind (a CRD installed since) at most this often.
const REDISCOVER_EVERY: Duration = Duration::from_secs(60);

// A panic in one call must not lock every agent out: a poisoned lock is still used.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn read<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(PoisonError::into_inner)
}

fn write<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(PoisonError::into_inner)
}

// MARK: Policy

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Allow,
    Ask,
    Deny,
}

impl Mode {
    pub const ALL: [Mode; 3] = [Mode::Allow, Mode::Ask, Mode::Deny];

    pub fn label(self) -> &'static str {
        match self {
            Mode::Allow => "Allow",
            Mode::Ask => "Ask me",
            Mode::Deny => "Deny",
        }
    }
}

/// Verbs an agent can be granted, kubectl style.
pub const VERBS: [(&str, &str); 12] = [
    ("get", "Read one object (YAML)"),
    ("list", "List objects, events"),
    ("logs", "Read container logs"),
    ("reveal", "See Secret values"),
    ("exec", "Run a command in a container"),
    ("scale", "Change replicas"),
    ("restart", "Rollout restart"),
    ("patch", "Merge-patch an object"),
    ("apply", "Create or update from YAML"),
    ("delete", "Delete an object"),
    ("drain", "Cordon and drain a node"),
    ("*", "Every verb"),
];

pub const READ_VERBS: [&str; 3] = ["get", "list", "logs"];

pub fn is_write(verb: &str) -> bool {
    matches!(verb, "exec" | "scale" | "restart" | "patch" | "apply" | "delete" | "drain")
}

/// One access rule, like a Kubernetes RBAC rule with an effect.
/// Every field is a list of `*` patterns; an empty list matches nothing. A field missing
/// when read never grants (see `RuleFields`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(from = "RuleFields")]
pub struct Rule {
    pub effect: Mode,
    pub verbs: Vec<String>,
    /// kubectl names: `pods`, `deployments`, `certificates.cert-manager.io`, `*`.
    pub resources: Vec<String>,
    /// Namespace patterns; `*` also covers cluster-scoped objects.
    pub namespaces: Vec<String>,
    pub contexts: Vec<String>,
    /// Object name patterns.
    pub names: Vec<String>,
    pub note: String,
    /// The rule stops applying after this time (temporary grants).
    pub expires: Option<jiff::Timestamp>,
    /// Made by approving a request ("Always allow this", "Allow for 1h"): an Allow that
    /// wins over the Ask rule that asked, so the same request is not asked again.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub approved: bool,
}

impl Default for Rule {
    fn default() -> Self {
        Self {
            effect: Mode::Allow,
            verbs: READ_VERBS.iter().map(|v| v.to_string()).collect(),
            resources: vec!["*".into()],
            namespaces: vec!["*".into()],
            contexts: Vec::new(),
            names: vec!["*".into()],
            note: String::new(),
            expires: None,
            approved: false,
        }
    }
}

fn deny() -> Mode {
    Mode::Deny
}

/// A rule as read (agents.json, pasted rules), where any field may be missing.
#[derive(Deserialize)]
struct RuleFields {
    /// A rule written without one (by hand, pasted) denies.
    #[serde(default = "deny")]
    effect: Mode,
    verbs: Option<Vec<String>>,
    resources: Option<Vec<String>>,
    namespaces: Option<Vec<String>>,
    contexts: Option<Vec<String>>,
    names: Option<Vec<String>>,
    #[serde(default)]
    note: String,
    #[serde(default)]
    expires: Option<jiff::Timestamp>,
    #[serde(default)]
    approved: bool,
}

impl From<RuleFields> for Rule {
    /// A missing list matches nothing in an Allow, and everything in a Deny or an Ask, so a
    /// rule missing a field never grants more than it says.
    fn from(f: RuleFields) -> Self {
        let effect = f.effect;
        let list = |v: Option<Vec<String>>| {
            v.unwrap_or_else(|| if effect == Mode::Allow { Vec::new() } else { vec!["*".into()] })
        };
        Self {
            effect,
            verbs: list(f.verbs),
            resources: list(f.resources),
            namespaces: list(f.namespaces),
            contexts: list(f.contexts),
            names: list(f.names),
            note: f.note,
            expires: f.expires,
            approved: f.approved,
        }
    }
}

/// What an agent is trying to do.
#[derive(Clone, Debug, Default)]
pub struct Access {
    pub context: String,
    pub verb: String,
    /// kubectl name of the resource (`deployments`, `certificates.cert-manager.io`).
    pub resource: String,
    /// Also accepted: the bare plural and the Kind (`deployments`, `Deployment`).
    pub resource_aliases: Vec<String>,
    /// None for cluster-scoped objects, or for "any namespace" checks.
    pub namespace: Option<String>,
    pub name: Option<String>,
    /// The kind is cluster-scoped, so a None namespace is not "every namespace".
    pub cluster_scoped: bool,
}

/// Whether a pattern matches, the way rules match: trimmed, ignoring case.
pub fn any_glob(patterns: &[String], text: &str) -> bool {
    patterns.iter().any(|p| glob(&p.trim().to_lowercase(), &text.to_lowercase()))
}

impl Rule {
    /// Does this rule cover the request? `any_namespace` checks whether the
    /// rule could match some namespace (used before filtering list results).
    pub fn expired(&self) -> bool {
        self.expires.is_some_and(|t| t <= jiff::Timestamp::now())
    }

    pub fn matches(&self, a: &Access, any_namespace: bool) -> bool {
        if self.expired() {
            return false;
        }
        // A rule made by approving a request covers that request only: its resource as named
        // (not another group's kind with the same plural), names and contexts case included.
        let exact = self.approved && self.effect == Mode::Allow;
        let matches_any = |patterns: &[String], text: &str| {
            if exact { patterns.iter().any(|p| glob(p.trim(), text)) } else { any_glob(patterns, text) }
        };
        let verb = self.verbs.iter().any(|v| v == "*" || v.eq_ignore_ascii_case(&a.verb));
        let resource = if exact {
            matches_any(&self.resources, &a.resource)
        } else {
            std::iter::once(&a.resource).chain(&a.resource_aliases).any(|r| matches_any(&self.resources, r))
        };
        let namespace = match &a.namespace {
            Some(ns) => matches_any(&self.namespaces, ns),
            // Cluster-scoped objects and "somewhere" checks.
            None if any_namespace => !self.namespaces.is_empty(),
            None => self.namespaces.iter().any(|n| n.trim() == "*"),
        };
        let name = match &a.name {
            Some(n) => matches_any(&self.names, n),
            // Name-less checks (lists): a Deny of some names only leaves the rest, whose rows
            // are then filtered by name. An Allow of some names passes too, so whatever a
            // name-less check lets through must be filtered by name before it is shown.
            None if self.effect == Mode::Deny => self.names.iter().any(|n| n.trim() == "*"),
            None => !self.names.is_empty(),
        };
        verb && resource && namespace && matches_any(&self.contexts, &a.context) && name
    }

    /// Whether both rules cover the same requests, the same way (notes and expiry aside).
    pub fn same_request(&self, other: &Rule) -> bool {
        self.effect == other.effect
            && self.verbs == other.verbs
            && self.resources == other.resources
            && self.namespaces == other.namespaces
            && self.contexts == other.contexts
            && self.names == other.names
    }

    /// For list filtering: a Deny that blocks the whole request regardless of namespace.
    fn denies_everywhere(&self, a: &Access) -> bool {
        self.effect == Mode::Deny && self.namespaces.iter().any(|n| n.trim() == "*") && self.matches(a, true)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Policy {
    pub rules: Vec<Rule>,
    pub max_log_lines: u32,
    pub max_rows: u32,
    /// Seconds an exec command may run.
    pub exec_timeout: u32,
    /// Calls allowed per minute (0 = unlimited).
    pub max_calls_per_minute: u32,
    // Legacy fields (before rules); migrated on load.
    #[serde(skip_serializing)]
    contexts: Vec<String>,
    #[serde(skip_serializing)]
    namespaces_allow: Vec<String>,
    #[serde(skip_serializing)]
    namespaces_deny: Vec<String>,
    #[serde(skip_serializing)]
    kinds_deny: Vec<String>,
    #[serde(skip_serializing)]
    reveal_secrets: bool,
    #[serde(skip_serializing)]
    read: Option<Mode>,
    #[serde(skip_serializing)]
    logs: Option<Mode>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            rules: vec![
                Rule { note: "Read everything in the chosen contexts".into(), ..Rule::default() },
                Rule {
                    effect: Mode::Deny,
                    verbs: vec!["*".into()],
                    resources: vec!["secrets".into()],
                    contexts: vec!["*".into()],
                    note: "Never Secrets".into(),
                    ..Rule::default()
                },
            ],
            max_log_lines: 500,
            max_rows: 500,
            exec_timeout: 30,
            max_calls_per_minute: 120,
            contexts: Vec::new(),
            namespaces_allow: Vec::new(),
            namespaces_deny: Vec::new(),
            kinds_deny: Vec::new(),
            reveal_secrets: false,
            read: None,
            logs: None,
        }
    }
}

/// The outcome of evaluating the rules, with the rule that decided.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Verdict {
    pub mode: Mode,
    pub rule: Option<usize>,
}

impl Policy {
    /// Deny wins over Ask, Ask over Allow (except an approved request); nothing matching means Deny.
    pub fn decide(&self, a: &Access) -> Verdict {
        self.decide_inner(a, false)
    }

    /// Could the request succeed in some namespace? Rows are then filtered with `decide`.
    pub fn decide_any_namespace(&self, a: &Access) -> Verdict {
        if let Some(i) = self.rules.iter().position(|r| r.denies_everywhere(a)) {
            return Verdict { mode: Mode::Deny, rule: Some(i) };
        }
        let mut probe = a.clone();
        probe.namespace = None;
        let candidates =
            self.rules.iter().enumerate().filter(|(_, r)| r.effect != Mode::Deny && r.matches(&probe, true));
        best(candidates)
    }

    /// The Ask rules an all-namespaces request meets in some namespace: what approving it covers.
    pub fn asks_any_namespace(&self, a: &Access) -> Vec<usize> {
        let mut probe = a.clone();
        probe.namespace = None;
        (0..self.rules.len())
            .filter(|&i| self.rules[i].effect == Mode::Ask && self.rules[i].matches(&probe, true))
            .collect()
    }

    /// The Ask rules a request in one namespace (or on one object) meets: what approving it
    /// covers.
    pub fn asks(&self, a: &Access) -> Vec<usize> {
        (0..self.rules.len())
            .filter(|&i| self.rules[i].effect == Mode::Ask && self.rules[i].matches(a, false))
            .collect()
    }

    /// For a list request that asks, in one namespace or across all of them: the Allow rule
    /// that decides some rows, so the request can still go ahead unapproved, showing only the
    /// allowed rows. An Allow that an Ask or a Deny covers everywhere gives nothing.
    pub fn allow_somewhere(&self, a: &Access) -> Option<usize> {
        let mut probe = a.clone();
        probe.namespace = None;
        let one_namespace = a.namespace.clone();
        let covers = |r: &Rule| match &one_namespace {
            Some(_) => r.matches(a, false),
            None => r.matches(&probe, true),
        };
        // An Allow rule's own patterns, taken as a namespace and a name (`*` then literal),
        // are matched by the rules that cover them; no namespace for cluster-scoped rows.
        let rows = self.rules.iter().filter(|r| r.effect == Mode::Allow && covers(r)).flat_map(|r| {
            let namespaces: Vec<Option<String>> = match &one_namespace {
                Some(ns) => vec![Some(ns.clone())],
                None => r.namespaces.iter().map(|n| Some(n.trim().to_string())).chain([None]).collect(),
            };
            namespaces.into_iter().flat_map(|ns| r.names.iter().map(move |n| (ns.clone(), n.trim().to_string())))
        });
        rows.into_iter().find_map(|(namespace, name)| {
            let v = self.decide(&Access { namespace, name: Some(name), ..probe.clone() });
            if v.mode == Mode::Allow { v.rule } else { None }
        })
    }

    fn decide_inner(&self, a: &Access, any_ns: bool) -> Verdict {
        let inside = namespace_contents(a);
        best(self.rules.iter().enumerate().filter(|(_, r)| {
            r.matches(a, any_ns) || inside.as_ref().is_some_and(|i| r.effect != Mode::Allow && r.matches(i, false))
        }))
    }

    /// Rules pasted as JSON ("Copy rules" on another agent), None when there are none.
    /// No pasted rule counts as approved: it would win over every Ask rule while looking
    /// like any other Allow.
    pub fn pasted(text: &str) -> Option<Policy> {
        let mut p: Policy = serde_json::from_str(text).ok()?;
        if p.rules.is_empty() {
            return None;
        }
        for r in &mut p.rules {
            r.approved = false;
        }
        Some(p)
    }

    /// The context patterns of the non-expired Allow and Ask rules.
    pub fn context_patterns(&self) -> Vec<String> {
        self.rules.iter().filter(|r| r.effect != Mode::Deny && !r.expired()).flat_map(|r| r.contexts.clone()).collect()
    }

    /// Whether some non-expired Allow or Ask rule names this context, and no Deny of
    /// everything covers it: checked before connecting.
    pub fn context_allowed(&self, context: &str) -> bool {
        let all = |patterns: &[String]| patterns.iter().any(|p| p.trim() == "*");
        let denied = self.rules.iter().any(|r| {
            r.effect == Mode::Deny
                && !r.expired()
                && all(&r.verbs)
                && all(&r.resources)
                && all(&r.namespaces)
                && all(&r.names)
                && any_glob(&r.contexts, context)
        });
        !denied && self.rules.iter().any(|r| r.effect != Mode::Deny && !r.expired() && any_glob(&r.contexts, context))
    }

    /// Turn pre-rules settings into rules, once.
    fn migrate(&mut self) {
        if self.contexts.is_empty() && self.read.is_none() {
            return;
        }
        let ctx = std::mem::take(&mut self.contexts);
        let namespaces = if self.namespaces_allow.is_empty() {
            vec!["*".into()]
        } else {
            std::mem::take(&mut self.namespaces_allow)
        };
        let mut rules = vec![
            Rule {
                effect: self.read.unwrap_or(Mode::Allow),
                verbs: vec!["get".into(), "list".into()],
                contexts: ctx.clone(),
                namespaces: namespaces.clone(),
                note: "Migrated: read".into(),
                ..Rule::default()
            },
            Rule {
                effect: self.logs.unwrap_or(Mode::Allow),
                verbs: vec!["logs".into()],
                resources: vec!["pods".into()],
                contexts: ctx.clone(),
                namespaces,
                note: "Migrated: logs".into(),
                ..Rule::default()
            },
        ];
        if !self.namespaces_deny.is_empty() {
            rules.push(Rule {
                effect: Mode::Deny,
                verbs: vec!["*".into()],
                namespaces: std::mem::take(&mut self.namespaces_deny),
                contexts: vec!["*".into()],
                note: "Migrated: denied namespaces".into(),
                ..Rule::default()
            });
        }
        if !self.kinds_deny.is_empty() {
            rules.push(Rule {
                effect: Mode::Deny,
                verbs: vec!["*".into()],
                resources: std::mem::take(&mut self.kinds_deny),
                contexts: vec!["*".into()],
                note: "Migrated: denied resources".into(),
                ..Rule::default()
            });
        }
        if self.reveal_secrets {
            rules.push(Rule {
                verbs: vec!["reveal".into()],
                resources: vec!["secrets".into()],
                contexts: ctx,
                note: "Migrated: reveal Secrets".into(),
                ..Rule::default()
            });
        }
        self.rules = rules;
        self.read = None;
        self.logs = None;
    }
}

/// A write to a Namespace object changes (or deletes) everything in it, so it also meets
/// the Deny and Ask rules of that namespace: the same request, inside it.
fn namespace_contents(a: &Access) -> Option<Access> {
    if !is_write(&a.verb) || a.resource != "namespaces" || a.namespace.is_some() {
        return None;
    }
    Some(Access { namespace: Some(a.name.clone()?), ..a.clone() })
}

fn best<'a>(matching: impl Iterator<Item = (usize, &'a Rule)>) -> Verdict {
    let mut verdict = Verdict { mode: Mode::Deny, rule: None };
    let mut rank = 0;
    for (i, r) in matching {
        let r_rank = match r.effect {
            Mode::Deny => 4,
            Mode::Allow if r.approved => 3,
            Mode::Ask => 2,
            Mode::Allow => 1,
        };
        if r_rank > rank {
            rank = r_rank;
            verdict = Verdict { mode: r.effect, rule: Some(i) };
        }
    }
    verdict
}

pub use crate::glob::glob;

// MARK: Agents and config

/// Whether two lists of agents give the same rules.yaml, which holds each agent's name
/// and rules, not whether it is enabled.
fn same_rules(a: &[Agent], b: &[Agent]) -> bool {
    let rules = |agents: &[Agent]| {
        agents.iter().map(|a| Some((a.name.clone(), serde_json::to_value(&a.policy).ok()?))).collect::<Option<Vec<_>>>()
    };
    matches!((rules(a), rules(b)), (Some(x), Some(y)) if x == y)
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Agent {
    pub id: String,
    pub name: String,
    /// SHA-256 of the bearer token; the token itself is only shown once.
    pub token_sha256: String,
    pub enabled: bool,
    pub policy: Policy,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub port: u16,
    /// Global kill switch: when false every agent call is refused.
    pub enabled: bool,
    pub agents: Vec<Agent>,
}

impl Default for Config {
    fn default() -> Self {
        Self { port: DEFAULT_PORT, enabled: true, agents: Vec::new() }
    }
}

pub(crate) fn config_dir() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("recouvrance")
}

fn data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    base.join("recouvrance")
}

/// Create a directory (and its parents) readable by the user only.
pub(crate) fn create_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// Write a file only the user can read, atomically: a fresh temporary file (never one
/// someone else prepared, or left over with looser permissions), then a rename.
pub(crate) fn write_private(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    create_private_dir(dir)?;
    let file_name = path.file_name().context("no file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let written = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        // The umask can only remove bits; set them anyway so the result never depends on it.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(contents)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(written?)
}

pub fn hash_token(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn new_token() -> String {
    format!("kv_{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

// MARK: Audit

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    Allowed,
    Approved,
    Denied,
    Rejected,
    Failed,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuditEntry {
    pub time: jiff::Timestamp,
    pub agent_id: String,
    pub agent: String,
    pub tool: String,
    pub args: serde_json::Value,
    pub decision: Decision,
    pub detail: String,
    pub duration_ms: u64,
    pub result_bytes: usize,
    /// MCP session (one agent conversation), when the client sent one.
    #[serde(default)]
    pub session: Option<String>,
}

/// See [`Hub::audit_stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AuditStats {
    pub matching: usize,
    pub oldest: Option<jiff::Timestamp>,
    pub newest: Option<jiff::Timestamp>,
    pub full: bool,
}

// MARK: Hub

/// How a request for approval ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    Approved,
    Rejected,
    TimedOut,
    /// The kill switch was turned on while it waited.
    SwitchedOff,
}

impl Answer {
    /// Why the call didn't go ahead, for the agent and the activity log.
    pub fn reason(self) -> &'static str {
        match self {
            Answer::Approved => "approved",
            Answer::Rejected => "the user did not approve this request",
            Answer::TimedOut => "the user did not answer within 2 minutes",
            Answer::SwitchedOff => "agent access was switched off while the request waited",
        }
    }
}

pub struct Approval {
    pub id: u64,
    pub agent_id: String,
    pub agent: String,
    pub tool: String,
    pub summary: String,
    /// The exact request, to turn into a rule ("always allow this").
    pub access: Access,
    /// A dry run: approving it must not turn into a rule for real writes.
    pub dry_run: bool,
}

impl Approval {
    /// A narrow Allow rule for exactly this request, when a rule can be that narrow.
    /// None for exec (a rule cannot pin the command), patches and GitOps actions (the rule
    /// would allow any patch), scale (it would allow any replica count, 0 too), apply (the
    /// rule could not pin the content), a request other than a list with no object name
    /// (the rule would cover every name), a request across all namespaces (the rule would
    /// also cover namespaces no rule allows) and dry runs (a rule cannot be limited to dry
    /// runs, so it would allow the real write).
    pub fn as_rule(&self) -> Option<Rule> {
        let a = &self.access;
        if matches!(a.verb.as_str(), "exec" | "patch" | "scale")
            || self.tool == "gitops"
            || self.tool == "apply_manifest"
            || self.dry_run
            || (a.verb != "list" && a.name.is_none())
            || (a.namespace.is_none() && !a.cluster_scoped)
        {
            return None;
        }
        Some(Rule {
            effect: Mode::Allow,
            verbs: vec![a.verb.clone()],
            resources: vec![a.resource.clone()],
            namespaces: vec![a.namespace.clone().unwrap_or_else(|| "*".into())],
            contexts: vec![a.context.clone()],
            names: vec![a.name.clone().unwrap_or_else(|| "*".into())],
            note: format!("Always allowed from approval ({})", jiff::Timestamp::now().strftime("%Y-%m-%d")),
            expires: None,
            approved: true,
        })
    }

    /// Same as `as_rule`, valid for the next hour only.
    pub fn as_rule_for_an_hour(&self) -> Option<Rule> {
        Some(Rule {
            note: "Allowed for 1 hour from approval".into(),
            expires: Some(jiff::Timestamp::now() + jiff::SignedDuration::from_hours(1)),
            ..self.as_rule()?
        })
    }
}

pub enum HubEvent {
    Audit(Box<AuditEntry>),
    ApprovalRequested(Arc<Approval>),
    ApprovalResolved(u64),
    AgentSeen,
    ServerStatus(Result<u16, String>),
}

/// One context's connection, made once. Each context has its own, so a slow or
/// unreachable cluster only delays calls to that cluster.
#[derive(Default)]
struct Cluster {
    /// What the context pointed to when this was made (`Contexts::fingerprints`).
    fingerprint: Option<u64>,
    client: tokio::sync::OnceCell<Client>,
    /// Discovered kinds and when; discovery runs one at a time per context.
    kinds: Mutex<Option<(Arc<Vec<Kind>>, Instant)>>,
    discovering: tokio::sync::Mutex<()>,
}

/// Shared between the MCP server (tokio) and the UI (gpui).
pub struct Hub {
    config: RwLock<Config>,
    kill_switch: AtomicBool,
    /// `recouvrance serve`: no window, so nobody can answer an approval.
    headless: AtomicBool,
    audit: Mutex<VecDeque<AuditEntry>>,
    /// The last failure writing audit.jsonl, shown on the Agents page.
    audit_error: Mutex<Option<String>>,
    last_seen: Mutex<HashMap<String, jiff::Timestamp>>,
    approvals: Mutex<HashMap<u64, oneshot::Sender<Answer>>>,
    next_approval: AtomicU64,
    subscribers: Mutex<Vec<mpsc::UnboundedSender<Arc<HubEvent>>>>,
    clients: Mutex<HashMap<String, Arc<Cluster>>>,
    /// Whether rules.yaml exists, kept by the watcher so rendering never stats the file.
    rules_file_present: AtomicBool,
    /// Recent call times per agent, for the rate limit.
    recent_calls: Mutex<HashMap<String, VecDeque<Instant>>>,
    /// Last MCP server status, for views created after it was reported.
    server_status: Mutex<Option<Result<u16, String>>>,
    /// rules.yaml: its last load error, and the text last loaded or written (None: no file).
    rules_error: Mutex<Option<String>>,
    rules_text: Mutex<Option<String>>,
    /// The lock on agents.lock while the app or `serve` runs (see [`Hub::claim_config`]).
    config_lock: Mutex<Option<std::fs::File>>,
    /// Why agents.json could not be read at start (see [`load_config`]).
    config_error: Option<String>,
}

/// agents.json; missing means the defaults. One that can't be read (corrupt, or from a
/// newer version) is kept aside so the next save does not destroy its agents and rules,
/// and agent access starts off, with the reason to show.
fn load_config(path: &std::path::Path) -> (Config, Option<String>) {
    let read = match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Config::default(), None),
        r => r.map_err(anyhow::Error::from),
    };
    match read.and_then(|bytes| Ok(serde_json::from_slice::<Config>(&bytes)?)) {
        Ok(config) => (config, None),
        Err(e) => {
            let kept = crate::settings::keep_aside(path);
            let error = format!("agents.json could not be read ({e:#}; {kept}): agent access is off");
            (Config { enabled: false, ..Config::default() }, Some(error))
        }
    }
}

fn config_lock_path() -> PathBuf {
    config_dir().join("agents.lock")
}

/// Whether the app or `serve` is running: it keeps agents.json in memory and saves it
/// whole, so a change written by another process would be lost.
pub fn config_in_use() -> bool {
    locked(&config_lock_path())
}

/// Whether another open file holds the lock on `path`.
fn locked(path: &std::path::Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else { return false };
    matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// Lock `path` (created if missing) for as long as the returned file is open.
fn take_lock(path: &std::path::Path) -> Option<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).mode(0o600).open(path).ok()?;
    file.try_lock().ok()?;
    Some(file)
}

impl Hub {
    /// Mark agents.json as in use by this process until it exits (see [`config_in_use`]).
    pub fn claim_config(&self) {
        let _ = create_private_dir(&config_dir());
        *lock(&self.config_lock) = take_lock(&config_lock_path());
    }

    pub fn load() -> Arc<Self> {
        let (mut config, config_error) = load_config(&config_dir().join("agents.json"));
        if let Some(e) = &config_error {
            eprintln!("{e}");
        }
        for agent in &mut config.agents {
            agent.policy.migrate();
        }
        let audit = load_audit_tail(&audit_path());
        Arc::new(Self {
            kill_switch: AtomicBool::new(config.enabled),
            headless: AtomicBool::new(false),
            config: RwLock::new(config),
            audit: Mutex::new(audit),
            audit_error: Mutex::default(),
            last_seen: Mutex::default(),
            approvals: Mutex::default(),
            next_approval: AtomicU64::new(1),
            subscribers: Mutex::default(),
            server_status: Mutex::default(),
            clients: Mutex::default(),
            rules_file_present: AtomicBool::new(rules_file::path().exists()),
            recent_calls: Mutex::default(),
            rules_error: Mutex::default(),
            rules_text: Mutex::default(),
            config_lock: Mutex::default(),
            config_error,
        })
    }

    /// Why agents.json could not be read at start, if it couldn't.
    pub fn config_error(&self) -> Option<String> {
        self.config_error.clone()
    }

    /// No window to approve in: calls that need approval are refused at once.
    pub fn set_headless(&self, headless: bool) {
        self.headless.store(headless, Ordering::SeqCst);
    }

    pub fn headless(&self) -> bool {
        self.headless.load(Ordering::SeqCst)
    }

    /// Whether rules.yaml exists (rules as code).
    pub fn rules_file_active(&self) -> bool {
        self.rules_file_present.load(Ordering::Relaxed)
    }

    /// Why the last audit log write failed, if it did.
    pub fn audit_error(&self) -> Option<String> {
        lock(&self.audit_error).clone()
    }

    pub fn rules_error(&self) -> Option<String> {
        lock(&self.rules_error).clone()
    }

    /// Load rules.yaml into the agents (creating agents it names). A broken file
    /// changes nothing: the error is kept for the Agents page.
    pub fn apply_rules_file(&self) -> Result<bool> {
        let path = rules_file::path();
        self.rules_file_present.store(path.exists(), Ordering::Relaxed);
        let loaded = rules_file::read(&path).and_then(|text| {
            *lock(&self.rules_text) = text.clone();
            text.map(|t| rules_file::check(&t)).transpose()
        });
        let file = match loaded {
            Ok(Some(f)) => f,
            Ok(None) => {
                *lock(&self.rules_error) = None;
                return Ok(false);
            }
            Err(e) => {
                *lock(&self.rules_error) = Some(format!("{e:#}"));
                self.emit(HubEvent::AgentSeen);
                return Err(e);
            }
        };
        {
            let mut cfg = write(&self.config);
            // A section could not say which of two agents with its name it is for.
            if let Some(name) = rules_file::shared_name(&cfg.agents) {
                let e = anyhow::anyhow!("two agents are named `{name}`: delete one");
                *lock(&self.rules_error) = Some(format!("{e:#}"));
                drop(cfg);
                self.emit(HubEvent::AgentSeen);
                return Err(e);
            }
            let mut agents = cfg.agents.clone();
            rules_file::apply_file(&file, &mut agents)?;
            cfg.agents = agents;
        }
        *lock(&self.rules_error) = None;
        self.save()?;
        self.emit(HubEvent::AgentSeen);
        Ok(true)
    }

    /// Save new rules.yaml text from the app: validated first, then written and applied.
    pub fn save_rules_text(&self, text: &str) -> Result<()> {
        rules_file::check(text)?;
        write_private(&rules_file::path(), text.as_bytes())?;
        self.apply_rules_file().map(|_| ())
    }

    /// Reload rules.yaml when it changed on disk (your editor saved it).
    pub fn watch_rules_file(self: &Arc<Self>) {
        let hub = Arc::downgrade(self);
        k8s::spawn_detached(async move {
            // Why the file could not be read last time, so the same error isn't reloaded again.
            let mut failed: Option<String> = None;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let Some(hub) = hub.upgrade() else { break };
                let now = rules_file::read(&rules_file::path()).map_err(|e| format!("{e:#}"));
                if reload_due(&now, failed.as_deref(), &lock(&hub.rules_text)) {
                    let _ = hub.apply_rules_file();
                }
                failed = now.err();
            }
        });
    }

    pub fn subscribe(&self) -> mpsc::UnboundedReceiver<Arc<HubEvent>> {
        let (tx, rx) = mpsc::unbounded();
        lock(&self.subscribers).push(tx);
        rx
    }

    pub fn server_status(&self) -> Option<Result<u16, String>> {
        lock(&self.server_status).clone()
    }

    pub(crate) fn emit(&self, event: HubEvent) {
        if let HubEvent::ServerStatus(s) = &event {
            *lock(&self.server_status) = Some(s.clone());
        }
        let event = Arc::new(event);
        lock(&self.subscribers).retain(|tx| tx.unbounded_send(event.clone()).is_ok());
    }

    // Config

    pub fn config(&self) -> Config {
        read(&self.config).clone()
    }

    pub fn port(&self) -> u16 {
        read(&self.config).port
    }

    /// Takes effect on the next start.
    pub fn set_port(&self, port: u16) -> Result<()> {
        write(&self.config).port = port;
        self.save()
    }

    /// Visual edits keep rules.yaml in step when you use it (and it loaded fine), unless it
    /// changed on disk since it was loaded: then your edit there wins and is loaded next.
    fn export_rules(&self, agents: &[Agent]) -> Result<()> {
        if !self.rules_file_active() || self.rules_error().is_some() {
            return Ok(());
        }
        let mut seen = lock(&self.rules_text);
        let text =
            rules_file::export_over(&rules_file::path(), agents, seen.as_deref()).context("rules.yaml not updated")?;
        *seen = Some(text);
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let json = serde_json::to_vec_pretty(&*read(&self.config))?;
        write_private(&config_dir().join("agents.json"), &json).context("saving agents.json")
    }

    pub fn agents_enabled(&self) -> bool {
        self.kill_switch.load(Ordering::SeqCst)
    }

    pub fn set_agents_enabled(&self, enabled: bool) -> Result<()> {
        self.kill_switch.store(enabled, Ordering::SeqCst);
        write(&self.config).enabled = enabled;
        if !enabled {
            // Refuse anything still waiting for approval.
            for (_, tx) in lock(&self.approvals).drain() {
                let _ = tx.send(Answer::SwitchedOff);
            }
        }
        self.save()
    }

    /// Creates an agent and returns it with its one-time token. Names are unique: rules.yaml
    /// finds an agent by its name. With the file in use, the agent gets its section there
    /// first (an agent the file doesn't name has no rules).
    pub fn create_agent(&self, name: String) -> Result<(Agent, String)> {
        let token = new_token();
        let agent = Agent {
            id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
            name,
            token_sha256: hash_token(&token),
            enabled: true,
            policy: Policy::default(),
        };
        {
            let mut cfg = write(&self.config);
            if cfg.agents.iter().any(|a| a.name == agent.name) {
                anyhow::bail!("an agent named `{}` already exists", agent.name);
            }
            let mut agents = cfg.agents.clone();
            agents.push(agent.clone());
            self.export_rules(&agents)?;
            cfg.agents = agents;
        }
        self.save()?;
        Ok((agent, token))
    }

    pub fn rotate_token(&self, id: &str) -> Result<String> {
        let token = new_token();
        {
            let mut cfg = write(&self.config);
            let agent = cfg.agents.iter_mut().find(|a| a.id == id).context("unknown agent")?;
            agent.token_sha256 = hash_token(&token);
        }
        self.save()?;
        Ok(token)
    }

    /// Change an agent. With rules.yaml in use, a change to what the file holds is written
    /// there first: if it can't be (a rule the file can't hold, a disk that can't be
    /// written), nothing changes. Switching an agent on or off doesn't touch the file.
    pub fn update_agent(&self, id: &str, f: impl FnOnce(&mut Agent)) -> Result<()> {
        {
            let mut cfg = write(&self.config);
            let mut agents = cfg.agents.clone();
            let agent = agents.iter_mut().find(|a| a.id == id).context("unknown agent")?;
            f(agent);
            // rules.yaml doesn't hold the Enabled switch: only write it when names or rules change.
            if !same_rules(&cfg.agents, &agents) {
                self.export_rules(&agents)?;
            }
            cfg.agents = agents;
        }
        self.save()
    }

    /// Delete an agent, and its section of rules.yaml when that is in use (the next load of
    /// the file would bring it back otherwise). The agent is deleted even if the file can't
    /// be written; the error says so.
    pub fn delete_agent(&self, id: &str) -> Result<()> {
        let exported = {
            let mut cfg = write(&self.config);
            cfg.agents.retain(|a| a.id != id);
            self.export_rules(&cfg.agents)
        };
        self.save()?;
        exported
    }

    pub fn agent_by_token(&self, token: &str) -> Option<Agent> {
        let hash = hash_token(token);
        read(&self.config).agents.iter().find(|a| a.token_sha256 == hash).cloned()
    }

    /// Fresh copy of an agent, so rule edits apply to the very next call.
    pub fn agent(&self, id: &str) -> Option<Agent> {
        read(&self.config).agents.iter().find(|a| a.id == id).cloned()
    }

    /// Count a call; false when the agent is over its per-minute limit.
    pub fn take_call(&self, id: &str, per_minute: u32) -> bool {
        if per_minute == 0 {
            return true;
        }
        let now = Instant::now();
        let mut map = lock(&self.recent_calls);
        let q = map.entry(id.to_string()).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t).as_secs() >= 60) {
            q.pop_front();
        }
        if q.len() as u32 >= per_minute {
            return false;
        }
        q.push_back(now);
        true
    }

    pub fn mark_seen(&self, id: &str) {
        lock(&self.last_seen).insert(id.to_string(), jiff::Timestamp::now());
        self.emit(HubEvent::AgentSeen);
    }

    pub fn last_seen(&self, id: &str) -> Option<jiff::Timestamp> {
        lock(&self.last_seen).get(id).copied()
    }

    // Audit

    /// The newest entries first, up to `limit`, cloning only those.
    pub fn recent_audit(&self, limit: usize, keep: impl Fn(&AuditEntry) -> bool) -> Vec<AuditEntry> {
        lock(&self.audit).iter().rev().filter(|e| keep(e)).take(limit).cloned().collect()
    }

    /// How many entries in memory match, the time of the oldest one in memory, whether
    /// memory is full (older entries are then only on disk), and the newest entry's time.
    pub fn audit_stats(&self, keep: impl Fn(&AuditEntry) -> bool) -> AuditStats {
        let audit = lock(&self.audit);
        AuditStats {
            matching: audit.iter().filter(|e| keep(e)).count(),
            oldest: audit.front().map(|e| e.time),
            newest: audit.back().map(|e| e.time),
            full: audit.len() >= MAX_AUDIT_IN_MEMORY,
        }
    }

    /// Every entry still on disk (the log and its rotated copy), oldest first.
    /// Reads up to 20 MB: call it off the UI thread.
    pub fn audit_on_disk() -> Vec<AuditEntry> {
        let path = audit_path();
        let rotated = path.with_extension("jsonl.1");
        [rotated, path].iter().flat_map(|p| read_audit(p)).collect()
    }

    pub fn record(&self, entry: AuditEntry) {
        {
            // One writer at a time, so rotation and appends never interleave.
            let mut audit = lock(&self.audit);
            let written = serde_json::to_string(&entry)
                .map_err(std::io::Error::other)
                .and_then(|line| append_audit(&audit_path(), &line));
            match written {
                Ok(()) => *lock(&self.audit_error) = None,
                Err(e) => {
                    let msg = format!("cannot write {}: {e}", audit_path().display());
                    eprintln!("audit log: {msg}");
                    *lock(&self.audit_error) = Some(msg);
                }
            }
            audit.push_back(entry.clone());
            while audit.len() > MAX_AUDIT_IN_MEMORY {
                audit.pop_front();
            }
        }
        self.emit(HubEvent::Audit(Box::new(entry)));
    }

    // Approvals

    /// Ask the user in the UI. Anything but [`Answer::Approved`] refuses the call; with
    /// no window to ask in, the request is refused at once.
    pub async fn request_approval(
        &self,
        agent: &Agent,
        tool: &str,
        summary: String,
        access: Access,
        dry_run: bool,
    ) -> Answer {
        if self.headless() {
            return Answer::Rejected;
        }
        let id = self.next_approval.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        lock(&self.approvals).insert(id, tx);
        self.emit(HubEvent::ApprovalRequested(Arc::new(Approval {
            id,
            agent_id: agent.id.clone(),
            agent: agent.name.clone(),
            tool: tool.to_string(),
            summary,
            access,
            dry_run,
        })));
        let result = tokio::time::timeout(Duration::from_secs(120), rx).await;
        lock(&self.approvals).remove(&id);
        self.emit(HubEvent::ApprovalResolved(id));
        match result {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Answer::Rejected,
            Err(_) => Answer::TimedOut,
        }
    }

    pub fn resolve_approval(&self, id: u64, approved: bool) {
        if let Some(tx) = lock(&self.approvals).remove(&id) {
            let _ = tx.send(if approved { Answer::Approved } else { Answer::Rejected });
        }
    }

    // Kubernetes access shared by all agents

    /// The context's connection, made again when the kubeconfig changed the server or
    /// user it points to. A context no longer in the kubeconfig is refused.
    fn cluster(&self, context: &str) -> Result<Arc<Cluster>> {
        let contexts = k8s::load_contexts()?;
        if !contexts.names.iter().any(|n| n == context) {
            anyhow::bail!("context `{context}` is not in the kubeconfig");
        }
        let fingerprint = contexts.fingerprints.get(context).copied();
        let mut clients = lock(&self.clients);
        match clients.get(context) {
            Some(cluster) if fingerprint.is_some() && cluster.fingerprint == fingerprint => Ok(cluster.clone()),
            _ => {
                let cluster = Arc::new(Cluster { fingerprint, ..Cluster::default() });
                clients.insert(context.to_string(), cluster.clone());
                Ok(cluster)
            }
        }
    }

    pub async fn client(&self, context: &str) -> Result<(Client, Arc<Vec<Kind>>)> {
        let cluster = self.cluster(context)?;
        let client = cluster.client.get_or_try_init(|| k8s::connect_for_agents(context.to_string())).await?.clone();
        if let Some((kinds, _)) = &*lock(&cluster.kinds) {
            return Ok((client, kinds.clone()));
        }
        let _one = cluster.discovering.lock().await;
        // Another call may have discovered while this one waited.
        if let Some((kinds, _)) = &*lock(&cluster.kinds) {
            return Ok((client, kinds.clone()));
        }
        let kinds = Arc::new(k8s::discover(client.clone()).await?);
        *lock(&cluster.kinds) = Some((kinds.clone(), Instant::now()));
        Ok((client, kinds))
    }

    /// Discover again (a CRD may have been installed since), at most once a minute per
    /// context. None when it ran too recently or failed.
    pub async fn rediscover(&self, context: &str) -> Option<Arc<Vec<Kind>>> {
        let cluster = self.cluster(context).ok()?;
        let client = cluster.client.get()?.clone();
        let _one = cluster.discovering.lock().await;
        let (previous, last) = match &*lock(&cluster.kinds) {
            Some((kinds, at)) => (Some(kinds.clone()), Some(*at)),
            None => (None, None),
        };
        if !rediscovery_due(last, Instant::now()) {
            return None;
        }
        match k8s::discover(client).await {
            Ok(kinds) => {
                let kinds = Arc::new(kinds);
                *lock(&cluster.kinds) = Some((kinds.clone(), Instant::now()));
                Some(kinds)
            }
            Err(_) => {
                // A failure counts too, so a broken API isn't asked again on every call.
                if let Some(previous) = previous {
                    *lock(&cluster.kinds) = Some((previous, Instant::now()));
                }
                None
            }
        }
    }
}

/// Whether rules.yaml must be reloaded, from what reading it gave now (its text, None when
/// missing, or why it can't be read), why it failed last time and the text last loaded.
/// By content: an edit within the same second as the last one still counts. A read that
/// fails again the same way changes nothing; one that works again always reloads, to clear
/// the error.
fn reload_due(now: &Result<Option<String>, String>, failed: Option<&str>, seen: &Option<String>) -> bool {
    match now {
        Ok(text) => failed.is_some() || text != seen,
        Err(e) => failed != Some(e.as_str()),
    }
}

fn rediscovery_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|t| now.duration_since(t) >= REDISCOVER_EVERY)
}

fn audit_path() -> PathBuf {
    data_dir().join("audit.jsonl")
}

/// Append one line, first moving a full log to `<name>.1` (replacing the older one).
fn append_audit(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(dir) = path.parent() {
        create_private_dir(dir)?;
    }
    if std::fs::metadata(path).is_ok_and(|m| m.len() >= MAX_AUDIT_BYTES) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        std::fs::rename(path, old)?;
    }
    let mut f = std::fs::OpenOptions::new().append(true).create(true).mode(0o600).open(path)?;
    writeln!(f, "{line}")
}

/// The last entries of the log, reading only its end.
/// All the entries of one log file, skipping lines that don't parse.
fn read_audit(path: &std::path::Path) -> Vec<AuditEntry> {
    let text = std::fs::read(path).unwrap_or_default();
    String::from_utf8_lossy(&text).lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn load_audit_tail(path: &std::path::Path) -> VecDeque<AuditEntry> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    // Far more than MAX_AUDIT_IN_MEMORY entries of a few hundred bytes.
    const TAIL_BYTES: u64 = 4 * 1024 * 1024;
    let Ok(mut f) = std::fs::File::open(path) else {
        return VecDeque::new();
    };
    let start = f.metadata().map(|m| m.len()).unwrap_or(0).saturating_sub(TAIL_BYTES);
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(start)).is_err() || f.read_to_end(&mut buf).is_err() {
        return VecDeque::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if start > 0 && !lines.is_empty() {
        // The first line was cut by the seek.
        lines.remove(0);
    }
    let first = lines.len().saturating_sub(MAX_AUDIT_IN_MEMORY);
    lines[first..].iter().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_an_agent_off_leaves_the_rules_file_alone() {
        let agent = Agent {
            id: "a1".into(),
            name: "claude".into(),
            token_sha256: String::new(),
            enabled: true,
            policy: Policy::default(),
        };
        let mut off = agent.clone();
        off.enabled = false;
        assert!(same_rules(std::slice::from_ref(&agent), &[off]));
        let mut renamed = agent.clone();
        renamed.name = "other".into();
        assert!(!same_rules(std::slice::from_ref(&agent), &[renamed]));
        let mut ruled = agent.clone();
        ruled.policy.max_rows += 1;
        assert!(!same_rules(std::slice::from_ref(&agent), &[ruled]));
        assert!(!same_rules(std::slice::from_ref(&agent), &[]));
    }

    #[test]
    fn an_unreadable_rules_file_is_reloaded_once() {
        let seen = Some("old".to_string());
        let denied: Result<Option<String>, String> = Err("permission denied".into());
        assert!(reload_due(&denied, None, &seen));
        // Still unreadable the same way: nothing to do.
        assert!(!reload_due(&denied, Some("permission denied"), &seen));
        assert!(reload_due(&Err("other".into()), Some("permission denied"), &seen));
        // Readable again, even with the text last loaded: reloaded to clear the error.
        assert!(reload_due(&Ok(seen.clone()), Some("permission denied"), &seen));
        assert!(!reload_due(&Ok(seen.clone()), None, &seen));
        assert!(reload_due(&Ok(None), None, &seen));
    }

    #[test]
    fn only_a_refusal_says_the_user_refused() {
        assert!(Answer::Rejected.reason().contains("did not approve"));
        for a in [Answer::TimedOut, Answer::SwitchedOff] {
            assert!(!a.reason().contains("did not approve"), "{a:?}");
        }
    }

    #[test]
    fn glob_matches() {
        assert!(glob("*", "anything"));
        assert!(glob("kube-*", "kube-system"));
        assert!(!glob("kube-*", "default"));
        assert!(glob("*-prod-*", "team-prod-eu"));
        assert!(glob("exact", "exact"));
        assert!(!glob("exact", "exactly"));
        assert!(glob("a*b*c", "aXXbYYc"));
        assert!(!glob("a*b*c", "aXXbYY"));
    }

    fn access(ctx: &str, verb: &str, res: &str, ns: Option<&str>, name: Option<&str>) -> Access {
        Access {
            context: ctx.into(),
            verb: verb.into(),
            resource: res.into(),
            resource_aliases: vec![],
            namespace: ns.map(String::from),
            name: name.map(String::from),
            cluster_scoped: false,
        }
    }

    fn rule(effect: Mode, verbs: &[&str], res: &[&str], ns: &[&str], ctx: &[&str]) -> Rule {
        let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect();
        Rule {
            effect,
            verbs: v(verbs),
            resources: v(res),
            namespaces: v(ns),
            contexts: v(ctx),
            names: vec!["*".into()],
            note: String::new(),
            expires: None,
            approved: false,
        }
    }

    #[test]
    fn rules_deny_wins_and_default_deny() {
        let p = Policy {
            rules: vec![
                rule(Mode::Allow, &["get", "list", "logs"], &["*"], &["*"], &["staging-*"]),
                rule(Mode::Ask, &["scale", "restart"], &["deployments"], &["team-a"], &["staging-*"]),
                rule(Mode::Deny, &["*"], &["secrets"], &["*"], &["*"]),
                rule(Mode::Deny, &["*"], &["*"], &["kube-*"], &["*"]),
            ],
            ..Policy::default()
        };
        let d = |a: Access| p.decide(&a).mode;
        assert_eq!(d(access("staging-eu", "list", "pods", Some("team-a"), None)), Mode::Allow);
        assert_eq!(d(access("prod-eu", "list", "pods", Some("team-a"), None)), Mode::Deny);
        assert_eq!(d(access("staging-eu", "get", "secrets", Some("team-a"), Some("x"))), Mode::Deny);
        assert_eq!(d(access("staging-eu", "list", "pods", Some("kube-system"), None)), Mode::Deny);
        assert_eq!(d(access("staging-eu", "scale", "deployments", Some("team-a"), Some("web"))), Mode::Ask);
        assert_eq!(d(access("staging-eu", "scale", "deployments", Some("team-b"), Some("web"))), Mode::Deny);
        assert_eq!(d(access("staging-eu", "delete", "pods", Some("team-a"), Some("p"))), Mode::Deny);
        // Cluster-scoped: covered by `*` namespaces.
        assert_eq!(d(access("staging-eu", "list", "nodes", None, None)), Mode::Allow);
        // Somewhere-checks see the namespaced allow, but a global deny blocks.
        assert_eq!(p.decide_any_namespace(&access("staging-eu", "list", "pods", None, None)).mode, Mode::Allow);
        assert_eq!(p.decide_any_namespace(&access("staging-eu", "list", "secrets", None, None)).mode, Mode::Deny);
    }

    #[test]
    fn allowed_names_pass_a_name_less_check_but_not_other_rows() {
        let mut web = rule(Mode::Allow, &["list"], &["events"], &["*"], &["*"]);
        web.names = vec!["web-*".into()];
        let p = Policy { rules: vec![web], ..Policy::default() };
        // The check before listing passes; each row must then be checked by name.
        assert_eq!(p.decide(&access("c", "list", "events", Some("ns"), None)).mode, Mode::Allow);
        assert_eq!(p.decide(&access("c", "list", "events", Some("ns"), Some("web-1.17f2"))).mode, Mode::Allow);
        assert_eq!(p.decide(&access("c", "list", "events", Some("ns"), Some("db-0.17f2"))).mode, Mode::Deny);
    }

    #[test]
    fn namespace_rules_cover_writes_to_the_namespace_itself() {
        let p = Policy {
            rules: vec![
                rule(Mode::Allow, &["*"], &["*"], &["*"], &["*"]),
                rule(Mode::Deny, &["*"], &["*"], &["prod"], &["*"]),
                rule(Mode::Ask, &["*"], &["*"], &["team-a"], &["*"]),
                rule(Mode::Allow, &["delete"], &["*"], &["dev"], &["*"]),
            ],
            ..Policy::default()
        };
        let d = |verb: &str, name: &str| p.decide(&access("c", verb, "namespaces", None, Some(name))).mode;
        assert_eq!(d("delete", "prod"), Mode::Deny);
        assert_eq!(d("patch", "prod"), Mode::Deny);
        assert_eq!(d("apply", "prod"), Mode::Deny);
        assert_eq!(d("delete", "team-a"), Mode::Ask);
        assert_eq!(d("delete", "other"), Mode::Allow);
        // Reading it is not a change to what it holds.
        assert_eq!(d("get", "prod"), Mode::Allow);
        // An Allow inside a namespace does not allow deleting the namespace.
        let only_dev = Policy { rules: vec![p.rules[3].clone()], ..Policy::default() };
        assert_eq!(only_dev.decide(&access("c", "delete", "namespaces", None, Some("dev"))).mode, Mode::Deny);
        // Another kind named namespaces is not the Namespace.
        let crd = access("c", "delete", "namespaces.example.io", None, Some("prod"));
        assert_eq!(p.decide(&crd).mode, Mode::Allow);
    }

    #[test]
    fn names_and_aliases() {
        let mut r = rule(Mode::Allow, &["restart"], &["deployments"], &["*"], &["*"]);
        r.names = vec!["web-*".into()];
        let p = Policy { rules: vec![r], ..Policy::default() };
        let mut a = access("c", "restart", "deployments.apps", Some("ns"), Some("web-1"));
        assert_eq!(p.decide(&a).mode, Mode::Deny);
        a.resource_aliases = vec!["deployments".into(), "deployment".into()];
        assert_eq!(p.decide(&a).mode, Mode::Allow);
        a.name = Some("api".into());
        assert_eq!(p.decide(&a).mode, Mode::Deny);
    }

    #[test]
    fn an_approved_rule_covers_only_the_kind_and_case_approved() {
        let mut pods = access("Prod", "list", "pods", Some("team-b"), None);
        pods.resource_aliases = vec!["pods".into(), "pod".into()];
        let approval = Approval {
            id: 1,
            agent_id: "a".into(),
            agent: "a".into(),
            tool: "list_resources".into(),
            summary: String::new(),
            access: pods.clone(),
            dry_run: false,
        };
        let p = Policy { rules: vec![approval.as_rule().unwrap()], ..Policy::default() };
        assert_eq!(p.decide(&pods).mode, Mode::Allow);
        // Another group's kind with the same plural.
        let mut metrics = access("Prod", "list", "pods.metrics.k8s.io", Some("team-b"), None);
        metrics.resource_aliases = vec!["pods".into(), "podmetrics".into()];
        assert_eq!(p.decide(&metrics).mode, Mode::Deny);
        // Another context, named the same but for case.
        assert_eq!(p.decide(&access("prod", "list", "pods", Some("team-b"), None)).mode, Mode::Deny);
        // A rule you write still matches aliases and ignores case.
        let mut r = p.rules[0].clone();
        r.approved = false;
        let p = Policy { rules: vec![r], ..Policy::default() };
        assert_eq!(p.decide(&metrics).mode, Mode::Allow);
    }

    #[test]
    fn context_lists_follow_the_rules() {
        let mut expired = rule(Mode::Allow, &["*"], &["*"], &["*"], &["prod"]);
        expired.expires = Some(jiff::Timestamp::now() - jiff::SignedDuration::from_hours(1));
        let p = Policy {
            rules: vec![rule(Mode::Allow, &["get"], &["*"], &["*"], &[" Staging-*"]), expired],
            ..Policy::default()
        };
        assert!(p.context_allowed("staging-eu"));
        assert!(!p.context_allowed("prod"));
        assert_eq!(p.context_patterns(), [" Staging-*"]);
        assert!(any_glob(&p.context_patterns(), "staging-eu"));
    }

    #[test]
    fn all_namespaces_fall_back_to_the_allowed_ones() {
        let p = Policy {
            rules: vec![
                rule(Mode::Allow, &["list"], &["pods"], &["team-a"], &["*"]),
                rule(Mode::Ask, &["list"], &["pods"], &["team-b"], &["*"]),
            ],
            ..Policy::default()
        };
        let all = access("c", "list", "pods", None, None);
        assert_eq!(p.decide_any_namespace(&all).mode, Mode::Ask);
        assert_eq!(p.allow_somewhere(&all), Some(0));
        // Unapproved, rows are filtered: team-b asks, so it is hidden.
        assert_eq!(p.decide(&access("c", "list", "pods", Some("team-a"), Some("x"))).mode, Mode::Allow);
        assert_eq!(p.decide(&access("c", "list", "pods", Some("team-b"), Some("x"))).mode, Mode::Ask);
        assert_eq!(p.asks_any_namespace(&all), [1]);
        let only_ask = Policy { rules: vec![p.rules[1].clone()], ..Policy::default() };
        assert_eq!(only_ask.allow_somewhere(&all), None);
        assert_eq!(p.allow_somewhere(&access("c", "list", "secrets", None, None)), None);
        // An Allow that an Ask covers everywhere decides no row: nothing to fall back to.
        let shadowed = Policy {
            rules: vec![p.rules[0].clone(), rule(Mode::Ask, &["list"], &["pods"], &["*"], &["*"])],
            ..Policy::default()
        };
        assert_eq!(shadowed.decide_any_namespace(&all).mode, Mode::Ask);
        assert_eq!(shadowed.allow_somewhere(&all), None);
        // An Ask of some names only leaves the other rows allowed.
        let mut debug = rule(Mode::Ask, &["list"], &["pods"], &["*"], &["*"]);
        debug.names = vec!["debug-*".into()];
        let names =
            Policy { rules: vec![rule(Mode::Allow, &["list"], &["pods"], &["*"], &["*"]), debug], ..Policy::default() };
        assert_eq!(names.decide_any_namespace(&all).mode, Mode::Ask);
        assert_eq!(names.allow_somewhere(&all), Some(0));
    }

    #[test]
    fn approving_all_namespaces_covers_every_ask_rule() {
        let p = Policy {
            rules: vec![
                rule(Mode::Ask, &["list"], &["pods"], &["team-b"], &["*"]),
                rule(Mode::Ask, &["list"], &["pods"], &["team-c"], &["*"]),
                rule(Mode::Ask, &["list"], &["deployments"], &["*"], &["*"]),
            ],
            ..Policy::default()
        };
        let all = access("c", "list", "pods", None, None);
        assert_eq!(p.decide_any_namespace(&all).rule, Some(0));
        // Both namespaces' rows were approved; deployments were not asked about.
        assert_eq!(p.asks_any_namespace(&all), [0, 1]);
    }

    #[test]
    fn approving_one_namespace_covers_every_ask_rule_there() {
        let mut debug = rule(Mode::Ask, &["list"], &["pods"], &["*"], &["*"]);
        debug.names = vec!["debug-*".into()];
        let p = Policy {
            rules: vec![
                rule(Mode::Allow, &["list"], &["pods"], &["*"], &["*"]),
                debug,
                rule(Mode::Ask, &["list"], &["pods"], &["team-b"], &["*"]),
            ],
            ..Policy::default()
        };
        let team_b = access("c", "list", "pods", Some("team-b"), None);
        assert_eq!(p.decide(&team_b), Verdict { mode: Mode::Ask, rule: Some(1) });
        // Approved: debug pods and the other team-b pods alike.
        assert_eq!(p.asks(&team_b), [1, 2]);
        // Unapproved: every team-b row asks, nothing to fall back to...
        assert_eq!(p.allow_somewhere(&team_b), None);
        // ...but in team-a only the debug pods ask; the others can be shown.
        let team_a = access("c", "list", "pods", Some("team-a"), None);
        assert_eq!(p.decide(&team_a).mode, Mode::Ask);
        assert_eq!(p.asks(&team_a), [1]);
        assert_eq!(p.allow_somewhere(&team_a), Some(0));
        assert_eq!(p.decide(&access("c", "list", "pods", Some("team-a"), Some("web"))).mode, Mode::Allow);
    }

    #[test]
    fn denied_names_leave_the_rest_of_a_list() {
        let mut deny = rule(Mode::Deny, &["*"], &["pods"], &["*"], &["*"]);
        deny.names = vec!["debug-*".into()];
        let p = Policy { rules: vec![rule(Mode::Allow, &["list"], &["*"], &["*"], &["*"]), deny], ..Policy::default() };
        // The list itself is allowed, in a namespace or across all of them...
        assert_eq!(p.decide(&access("c", "list", "pods", Some("ns"), None)).mode, Mode::Allow);
        assert_eq!(p.decide_any_namespace(&access("c", "list", "pods", None, None)).mode, Mode::Allow);
        // ...and its rows are filtered by name.
        assert_eq!(p.decide(&access("c", "list", "pods", Some("ns"), Some("debug-1"))).mode, Mode::Deny);
        assert_eq!(p.decide(&access("c", "list", "pods", Some("ns"), Some("web-1"))).mode, Mode::Allow);
        // A Deny of every name still denies the whole list.
        let all = Policy {
            rules: vec![p.rules[0].clone(), rule(Mode::Deny, &["*"], &["pods"], &["*"], &["*"])],
            ..Policy::default()
        };
        assert_eq!(all.decide_any_namespace(&access("c", "list", "pods", None, None)).mode, Mode::Deny);
    }

    #[test]
    fn approval_becomes_a_narrow_rule() {
        let approval = Approval {
            id: 1,
            agent_id: "a".into(),
            agent: "a".into(),
            tool: "rollout_restart".into(),
            summary: String::new(),
            access: access("c1", "restart", "deployments.apps", Some("team"), Some("web")),
            dry_run: false,
        };
        // It wins over the Ask rule that asked, for exactly this request.
        let ask = rule(Mode::Ask, &["restart"], &["*"], &["*"], &["*"]);
        let p = Policy { rules: vec![ask, approval.as_rule().unwrap()], ..Policy::default() };
        assert_eq!(p.decide(&access("c1", "restart", "deployments.apps", Some("team"), Some("web"))).mode, Mode::Allow);
        assert_eq!(p.decide(&access("c1", "restart", "deployments.apps", Some("team"), Some("api"))).mode, Mode::Ask);
        let hour =
            Policy { rules: vec![p.rules[0].clone(), approval.as_rule_for_an_hour().unwrap()], ..Policy::default() };
        assert_eq!(
            hour.decide(&access("c1", "restart", "deployments.apps", Some("team"), Some("web"))).mode,
            Mode::Allow
        );
        // A Deny still wins, and a plain Allow still loses to the Ask.
        let deny = rule(Mode::Deny, &["*"], &["*"], &["team"], &["*"]);
        let denied = Policy { rules: vec![deny, approval.as_rule().unwrap()], ..Policy::default() };
        assert_eq!(
            denied.decide(&access("c1", "restart", "deployments.apps", Some("team"), Some("web"))).mode,
            Mode::Deny
        );
        let plain = Rule { approved: false, ..approval.as_rule().unwrap() };
        let asks = Policy { rules: vec![p.rules[0].clone(), plain], ..Policy::default() };
        assert_eq!(
            asks.decide(&access("c1", "restart", "deployments.apps", Some("team"), Some("web"))).mode,
            Mode::Ask
        );
        let p = Policy { rules: vec![approval.as_rule().unwrap()], ..Policy::default() };
        assert_eq!(p.decide(&access("c1", "restart", "deployments.apps", Some("team"), Some("web"))).mode, Mode::Allow);
        assert_eq!(p.decide(&access("c1", "restart", "deployments.apps", Some("team"), Some("api"))).mode, Mode::Deny);
        assert_eq!(p.decide(&access("c1", "delete", "deployments.apps", Some("team"), Some("web"))).mode, Mode::Deny);
        assert_eq!(p.decide(&access("c2", "restart", "deployments.apps", Some("team"), Some("web"))).mode, Mode::Deny);
    }

    #[test]
    fn pasted_rules_are_not_approved() {
        let approved = Rule { approved: true, ..rule(Mode::Allow, &["get"], &["*"], &["*"], &["*"]) };
        let text = serde_json::to_string(&Policy { rules: vec![approved], ..Policy::default() }).unwrap();
        assert!(text.contains("\"approved\":true"));
        let p = Policy::pasted(&text).unwrap();
        assert!(!p.rules[0].approved);
        assert!(Policy::pasted(r#"{"rules": []}"#).is_none() && Policy::pasted("not json").is_none());
    }

    #[test]
    fn approvals_that_cannot_be_narrow_make_no_rule() {
        let approval = |tool: &str, verb: &str, name: Option<&str>| Approval {
            id: 1,
            agent_id: "a".into(),
            agent: "a".into(),
            tool: tool.into(),
            summary: String::new(),
            access: access("c1", verb, "pods", Some("team"), name),
            dry_run: false,
        };
        assert!(approval("exec_command", "exec", Some("web-1")).as_rule().is_none());
        assert!(approval("exec_command", "exec", Some("web-1")).as_rule_for_an_hour().is_none());
        assert!(approval("gitops", "patch", Some("app")).as_rule().is_none());
        // A rule could not pin the patch body or the replica count.
        assert!(approval("patch_resource", "patch", Some("web")).as_rule().is_none());
        assert!(approval("scale", "scale", Some("web")).as_rule_for_an_hour().is_none());
        assert!(approval("apply_manifest", "apply", None).as_rule().is_none());
        // A rule could not pin the manifest's content.
        assert!(approval("apply_manifest", "apply", Some("web")).as_rule().is_none());
        assert!(approval("list_resources", "list", None).as_rule().is_some());
        // A certificate scan reads every Secret: a rule would allow getting any of them.
        assert!(approval("check_certificates", "get", None).as_rule().is_none());
        assert!(approval("delete_resource", "delete", Some("web")).as_rule().is_some());
        // Across all namespaces, a rule would also cover namespaces no rule allows.
        let everywhere = |cluster_scoped| Approval {
            access: Access { cluster_scoped, ..access("c1", "list", "pods", None, None) },
            ..approval("list_resources", "list", None)
        };
        assert!(everywhere(false).as_rule().is_none() && everywhere(false).as_rule_for_an_hour().is_none());
        assert!(everywhere(true).as_rule().is_some());
        // Approving a dry run must not allow the real write.
        let dry = Approval { dry_run: true, ..approval("delete_resource", "delete", Some("web")) };
        assert!(dry.as_rule().is_none() && dry.as_rule_for_an_hour().is_none());
    }

    #[test]
    fn contexts_are_checked_before_connecting() {
        let p = Policy {
            rules: vec![
                rule(Mode::Allow, &["get"], &["*"], &["*"], &["staging-*", "prod-*"]),
                rule(Mode::Deny, &["*"], &["secrets"], &["*"], &["*"]),
                rule(Mode::Deny, &["*"], &["*"], &["kube-*"], &["*"]),
                rule(Mode::Deny, &["*"], &["*"], &["*"], &["*prod*"]),
            ],
            ..Policy::default()
        };
        assert!(p.context_allowed("staging-eu"));
        assert!(p.context_allowed("Staging-EU"));
        assert!(!p.context_allowed("dev"));
        // A Deny of everything there: never contacted.
        assert!(!p.context_allowed("prod-eu"));
        let mut expired_deny = p.rules[3].clone();
        expired_deny.expires = Some(jiff::Timestamp::now() - jiff::SignedDuration::from_secs(1));
        let lifted = Policy { rules: vec![p.rules[0].clone(), expired_deny], ..Policy::default() };
        assert!(lifted.context_allowed("prod-eu"));
        // A Deny of some names only leaves the context usable.
        let mut some_names = p.rules[3].clone();
        some_names.names = vec!["debug-*".into()];
        assert!(Policy { rules: vec![p.rules[0].clone(), some_names], ..Policy::default() }.context_allowed("prod-eu"));
        let mut expired = rule(Mode::Allow, &["get"], &["*"], &["*"], &["prod-*"]);
        expired.expires = Some(jiff::Timestamp::now() - jiff::SignedDuration::from_secs(1));
        assert!(!Policy { rules: vec![expired], ..Policy::default() }.context_allowed("prod-eu"));
    }

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("recouvrance-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_running_hub_is_seen() {
        let path = temp_dir().join("agents.lock");
        assert!(!locked(&path));
        let held = take_lock(&path).unwrap();
        assert!(locked(&path));
        // A second process (or hub) can't take it while it is held.
        assert!(take_lock(&path).is_none());
        drop(held);
        assert!(!locked(&path));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_bad_agents_file_is_kept_and_turns_agents_off() {
        let dir = temp_dir();
        let path = dir.join("agents.json");
        let (config, error) = load_config(&path);
        assert!(config.enabled && error.is_none());
        std::fs::write(&path, br#"{"enabled": true, "agents": [{"id": 1}]}"#).unwrap();
        let (config, error) = load_config(&path);
        assert!(!config.enabled);
        assert!(config.agents.is_empty());
        assert!(error.is_some());
        // The file is kept aside, so the next save can't destroy it.
        assert!(!path.exists());
        let kept = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| e.file_name().to_string_lossy().starts_with("agents.json.bad-"));
        assert!(kept);
        let _ = std::fs::remove_dir_all(dir);
    }

    fn entry(tool: &str) -> AuditEntry {
        AuditEntry {
            time: jiff::Timestamp::now(),
            agent_id: "a".into(),
            agent: "a".into(),
            tool: tool.into(),
            args: serde_json::json!({}),
            decision: Decision::Allowed,
            detail: String::new(),
            duration_ms: 0,
            result_bytes: 0,
            session: None,
        }
    }

    #[test]
    fn audit_log_rotates_and_reads_its_tail() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir();
        let path = dir.join("logs").join("audit.jsonl");
        for i in 0..3 {
            append_audit(&path, &serde_json::to_string(&entry(&format!("t{i}"))).unwrap()).unwrap();
        }
        let tail = load_audit_tail(&path);
        assert_eq!(tail.iter().map(|e| e.tool.as_str()).collect::<Vec<_>>(), ["t0", "t1", "t2"]);
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&dir.join("logs")), 0o700);
        // A full log moves aside and a new one starts.
        let f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.set_len(MAX_AUDIT_BYTES).unwrap();
        append_audit(&path, &serde_json::to_string(&entry("new")).unwrap()).unwrap();
        assert_eq!(load_audit_tail(&path).len(), 1);
        assert_eq!(std::fs::metadata(dir.join("logs/audit.jsonl.1")).unwrap().len(), MAX_AUDIT_BYTES);
        // Only the end of a big file is read; the line cut by the seek is skipped.
        let big = dir.join("big.jsonl");
        let line = serde_json::to_string(&entry("x")).unwrap();
        let text = format!("{line}\n").repeat(5 * 1024 * 1024 / line.len());
        std::fs::write(&big, text).unwrap();
        let tail = load_audit_tail(&big);
        assert_eq!(tail.len(), MAX_AUDIT_IN_MEMORY);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn private_files_are_replaced_with_a_fresh_0600_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir();
        let path = dir.join("agents.json");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        // A leftover from the old fixed temporary name is not reused.
        std::fs::write(dir.join("agents.tmp"), "planted").unwrap();
        write_private(&path, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let names: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
        assert_eq!(names.len(), 2, "no temporary file left: {names:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rediscovery_is_rate_limited() {
        let now = Instant::now();
        assert!(rediscovery_due(None, now));
        assert!(!rediscovery_due(Some(now), now));
        assert!(rediscovery_due(now.checked_sub(REDISCOVER_EVERY), now));
    }

    #[test]
    fn poisoned_locks_still_work() {
        let m = Arc::new(Mutex::new(1));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison");
        })
        .join();
        assert!(m.is_poisoned());
        *lock(&m) += 1;
        assert_eq!(*lock(&m), 2);
    }

    #[test]
    fn expired_rules_stop_applying() {
        let mut r = rule(Mode::Allow, &["get"], &["*"], &["*"], &["*"]);
        r.expires = Some(jiff::Timestamp::now() - jiff::SignedDuration::from_secs(1));
        let p = Policy { rules: vec![r.clone()], ..Policy::default() };
        assert_eq!(p.decide(&access("c", "get", "pods", Some("n"), Some("x"))).mode, Mode::Deny);
        r.expires = Some(jiff::Timestamp::now() + jiff::SignedDuration::from_hours(1));
        let p = Policy { rules: vec![r], ..Policy::default() };
        assert_eq!(p.decide(&access("c", "get", "pods", Some("n"), Some("x"))).mode, Mode::Allow);
    }

    #[test]
    fn policy_json_round_trip() {
        let mut p = Policy::default();
        p.rules[0].contexts = vec!["eu-*".into()];
        p.max_calls_per_minute = 7;
        let back: Policy = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back.rules, p.rules);
        assert_eq!(back.max_calls_per_minute, 7);
    }

    #[test]
    fn legacy_policy_migrates() {
        let mut p: Policy = serde_json::from_str(
            r#"{"contexts":["c1"],"namespaces_deny":["kube-*"],"kinds_deny":["secrets"],"read":"allow","logs":"ask"}"#,
        )
        .unwrap();
        p.migrate();
        assert_eq!(p.decide(&access("c1", "list", "pods", Some("app"), None)).mode, Mode::Allow);
        assert_eq!(p.decide(&access("c1", "logs", "pods", Some("app"), Some("x"))).mode, Mode::Ask);
        assert_eq!(p.decide(&access("c1", "list", "pods", Some("kube-system"), None)).mode, Mode::Deny);
        assert_eq!(p.decide(&access("c1", "get", "secrets", Some("app"), Some("s"))).mode, Mode::Deny);
        assert_eq!(p.decide(&access("c2", "list", "pods", Some("app"), None)).mode, Mode::Deny);
    }

    #[test]
    fn token_hash_is_stable() {
        assert_eq!(hash_token("abc"), hash_token("abc"));
        assert_ne!(hash_token("abc"), hash_token("abd"));
        assert_eq!(hash_token("abc").len(), 64);
    }
}

/// Properties of the rule engine over many generated policies and requests, rather than
/// single examples: what a reviewer would otherwise have to check case by case.
#[cfg(test)]
pub(crate) mod properties {
    use super::{Access, Approval, Mode, Policy, Rule};
    use proptest::prelude::*;

    pub(crate) fn pattern() -> impl Strategy<Value = String> {
        prop_oneof![Just("*"), Just("a"), Just("b"), Just("a*"), Just("team-*"), Just("team-a")].prop_map(String::from)
    }

    pub(crate) fn patterns() -> impl Strategy<Value = Vec<String>> {
        prop::collection::vec(pattern(), 0..3)
    }

    pub(crate) fn value() -> impl Strategy<Value = String> {
        prop_oneof![Just("a"), Just("b"), Just("ab"), Just("team-a"), Just("team-b"), Just("A"), Just("Team-a")]
            .prop_map(String::from)
    }

    /// A resource and its aliases: another group's kind may share the plural.
    fn resource() -> impl Strategy<Value = (String, Vec<String>)> {
        prop_oneof![value(), Just("a.other.io".to_string()), Just("team-a.other.io".to_string())].prop_map(|r| {
            let plural = r.split('.').next().unwrap_or_default().to_string();
            (r, vec![plural])
        })
    }

    pub(crate) fn verb() -> impl Strategy<Value = String> {
        prop_oneof![Just("get"), Just("list"), Just("delete"), Just("patch"), Just("exec")].prop_map(String::from)
    }

    pub(crate) fn rule() -> impl Strategy<Value = Rule> {
        (
            prop_oneof![Just(Mode::Allow), Just(Mode::Ask), Just(Mode::Deny)],
            prop::collection::vec(prop_oneof![verb(), Just("*".to_string())], 0..3),
            patterns(),
            patterns(),
            patterns(),
            patterns(),
            any::<bool>(),
        )
            .prop_map(|(effect, verbs, resources, namespaces, contexts, names, approved)| Rule {
                effect,
                verbs,
                resources,
                namespaces,
                contexts,
                names,
                approved: approved && effect == Mode::Allow,
                ..Rule::default()
            })
    }

    pub(crate) fn access() -> impl Strategy<Value = Access> {
        (verb(), resource(), prop::option::of(value()), value(), prop::option::of(value())).prop_map(
            |(verb, (resource, resource_aliases), namespace, context, name)| Access {
                context,
                verb,
                resource,
                resource_aliases,
                namespace,
                name,
                cluster_scoped: false,
            },
        )
    }

    pub(crate) fn policy() -> impl Strategy<Value = Policy> {
        prop::collection::vec(rule(), 0..6).prop_map(|rules| Policy { rules, ..Policy::default() })
    }

    proptest! {
        /// A Deny rule that covers a request always decides it, whatever else allows it.
        #[test]
        fn deny_always_wins(policy in policy(), a in access()) {
            if policy.rules.iter().any(|r| r.effect == Mode::Deny && r.matches(&a, false)) {
                prop_assert_eq!(policy.decide(&a).mode, Mode::Deny);
            }
        }

        /// Adding a rule never lets through what a Deny rule stopped.
        #[test]
        fn more_rules_never_undo_a_deny(policy in policy(), extra in rule(), a in access()) {
            let before = policy.decide(&a);
            let mut more = policy.clone();
            more.rules.push(extra);
            if before.mode == Mode::Deny && before.rule.is_some() {
                prop_assert_eq!(more.decide(&a).mode, Mode::Deny);
            }
        }

        /// Nothing matching means Deny.
        #[test]
        fn no_rule_means_deny(a in access()) {
            let empty = Policy { rules: Vec::new(), ..Policy::default() };
            prop_assert_eq!(empty.decide(&a).mode, Mode::Deny);
        }

        /// The rule an approval writes covers only requests inside the one approved:
        /// same verb, resource and context, same namespace and name unless the approved
        /// request had none.
        #[test]
        fn an_approval_never_grants_more_than_the_request(a in access(), b in access(), dry_run in any::<bool>()) {
            let approval = Approval {
                id: 1,
                agent_id: "agent".into(),
                agent: "agent".into(),
                tool: "get_resource".into(),
                summary: String::new(),
                access: a.clone(),
                dry_run,
            };
            for rule in [approval.as_rule(), approval.as_rule_for_an_hour()].into_iter().flatten() {
                if rule.matches(&b, false) {
                    prop_assert_eq!(&b.verb, &a.verb);
                    prop_assert_eq!(&b.resource, &a.resource);
                    prop_assert_eq!(&b.context, &a.context);
                    if a.namespace.is_some() {
                        prop_assert_eq!(&b.namespace, &a.namespace);
                    }
                    // A name-less check (a list) may pass: its rows are then filtered one by
                    // one, by name, against the same rules.
                    if a.name.is_some() && b.name.is_some() {
                        prop_assert_eq!(&b.name, &a.name);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod fail_closed {
    use super::{Mode, Rule};

    #[test]
    fn a_rule_without_an_effect_denies() {
        let r: Rule = serde_json::from_str(r#"{"verbs": ["*"], "resources": ["secrets"], "contexts": ["*"]}"#).unwrap();
        assert_eq!(r.effect, Mode::Deny);
    }

    #[test]
    fn a_missing_list_never_grants() {
        // A Deny without contexts or verbs denies them all.
        let r: Rule = serde_json::from_str(r#"{"effect": "deny", "resources": ["secrets"]}"#).unwrap();
        for list in [&r.verbs, &r.namespaces, &r.contexts, &r.names] {
            assert_eq!(list, &["*"]);
        }
        let all = Rule { verbs: vec!["*".into()], contexts: vec!["*".into()], ..Rule::default() };
        let p = super::Policy { rules: vec![all, r], ..super::Policy::default() };
        let a = super::Access {
            context: "prod".into(),
            verb: "delete".into(),
            resource: "secrets".into(),
            namespace: Some("app".into()),
            name: Some("db".into()),
            ..super::Access::default()
        };
        assert_eq!(p.decide(&a).mode, Mode::Deny);
        // An Ask too.
        let r: Rule = serde_json::from_str(r#"{"effect": "ask", "verbs": ["delete"]}"#).unwrap();
        assert_eq!(r.resources, ["*"]);
        // An Allow without contexts or names allows none.
        let r: Rule = serde_json::from_str(r#"{"effect": "allow", "verbs": ["get"], "resources": ["pods"]}"#).unwrap();
        assert!(r.contexts.is_empty() && r.names.is_empty() && r.namespaces.is_empty());
        // A full rule reads back as written.
        let full = Rule { contexts: vec!["eu".into()], ..Rule::default() };
        let back: Rule = serde_json::from_str(&serde_json::to_string(&full).unwrap()).unwrap();
        assert_eq!(back, full);
    }
}
