//! The hub: the state the MCP server and the UI share (agents, approvals, clients, events).

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use futures::channel::{mpsc, oneshot};
use kube::Client;

use super::audit::{
    AuditEntry, AuditStats, MAX_AUDIT_IN_MEMORY, append_audit, audit_path, load_audit_tail, read_audit,
};
use super::config::{
    Agent, CONFIG_VERSION, Config, GITOPS_VERBS_VERSION, SECTIONS_VERSION, config_lock_path, hash_token, load_config,
    new_token, same_rules, take_lock,
};
use super::policy::{Access, Mode, Policy, Rule, Verb};
use super::preview::Preview;
use super::{lock, read, rules_file, write};
use crate::k8s::{self, Kind};

/// Discovery runs again for an unknown kind (a CRD installed since) at most this often.
const REDISCOVER_EVERY: Duration = Duration::from_secs(60);

/// How long the kinds of a CRD applied through the hub count as new: until then a lookup
/// that misses them asks their own group version, and waits for it to serve them.
const NEW_CRD_FOR: Duration = Duration::from_secs(300);

/// The most new CRD kinds kept per context.
const MAX_NEW_KINDS: usize = 256;

/// How long a lookup waits for the API server to serve the kind of a CRD just applied. It
/// takes about a second once the CRD is accepted; one that never is fails after this.
pub(crate) const SERVE_WAIT: Duration = Duration::from_secs(20);

/// How often a lookup asks again while it waits.
const SERVE_POLL: Duration = Duration::from_millis(500);

/// How long a request waits for the user's answer. The configs the Agents page copies
/// give clients a tool timeout longer than this.
pub const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);

/// How a request for approval ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Answer {
    Approved,
    Rejected,
    TimedOut,
    /// The kill switch was turned on while it waited.
    SwitchedOff,
    /// The agent stopped waiting for the answer: it cancelled the call, or hung up.
    Abandoned,
}

impl Answer {
    /// Why the call didn't go ahead, for the agent and the activity log.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Rejected => "the user did not approve this request",
            Self::TimedOut => "the user did not answer within 2 minutes",
            Self::SwitchedOff => "agent access was switched off while the request waited",
            Self::Abandoned => "the agent stopped waiting",
        }
    }
}

pub struct Approval {
    /// Set by [`Hub::request_approval`].
    pub id: u64,
    pub agent_id: String,
    pub agent: String,
    pub tool: String,
    /// The request, its key arguments and the rule that asks.
    pub summary: String,
    /// More than the arguments show, said after the preview: a manifest's objects and YAML.
    pub detail: Option<String>,
    /// What the call will change, worked out before asking (none for calls without one).
    pub preview: Option<Preview>,
    /// The exact request, to turn into a rule ("always allow this").
    pub access: Access,
    /// A dry run: approving it must not turn into a rule for real writes.
    pub dry_run: bool,
    /// What was approved is narrower than any rule for it: an Argo CD sync, terminate or
    /// rollback (see [`Approval::only_once`]).
    pub once_only: bool,
    /// When the request is refused unanswered. Set by [`Hub::request_approval`]; the banner
    /// counts down to it.
    pub deadline: Instant,
}

impl Approval {
    /// How long the request still waits for an answer.
    pub fn time_left(&self, now: Instant) -> Duration {
        self.deadline.saturating_duration_since(now)
    }

    /// Whether approving the `gitops` call with these arguments must not become a rule: an
    /// Argo CD sync, terminate or rollback. One `sync` verb covers them all, and a rule
    /// cannot pin a prune, the `resources` synced or a rollback's deployment. Approving a
    /// plain sync would then let a prune delete objects nobody was shown, or a rollback go
    /// anywhere, without asking. A refresh and a Flux reconcile deploy nothing more than
    /// themselves, so they keep their rule.
    pub fn only_once(tool: &str, args: &serde_json::Value) -> bool {
        tool == "gitops" && matches!(args["action"].as_str(), Some("sync" | "terminate" | "rollback"))
    }

    /// A narrow Allow rule for exactly this request, when a rule can be that narrow.
    /// None for exec (a rule cannot pin the command), patches (the rule would allow any
    /// patch), scale (it would allow any replica count, 0 too), Flux suspend and resume (one
    /// verb for both: approving a resume would allow suspending), apply (the
    /// rule could not pin the content), a request other than a list with no object name
    /// (the rule would cover every name), a request across all namespaces (the rule would
    /// also cover namespaces no rule allows) and dry runs (a rule cannot be limited to dry
    /// runs, so it would allow the real write), and an Argo CD sync, rollback or terminate
    /// (the `sync` verb would allow every prune and rollback of the object): `sync` on an
    /// Application makes no rule whatever the tool or [`once_only`](Self::once_only) say.
    pub fn as_rule(&self) -> Option<Rule> {
        let a = &self.access;
        let argo_app = |r: &String| r.eq_ignore_ascii_case("applications.argoproj.io");
        // A refresh or a Flux reconcile has its own verb, so its rule allows that action on
        // that object only.
        if matches!(a.verb, Verb::Exec | Verb::Patch | Verb::Scale | Verb::Suspend)
            || (a.verb == Verb::Sync && (argo_app(&a.resource) || a.resource_aliases.iter().any(argo_app)))
            || self.tool == "apply_manifest"
            || self.dry_run
            || self.once_only
            || (a.verb != Verb::List && a.name.is_none())
            || (a.namespace.is_none() && !a.cluster_scoped)
        {
            return None;
        }
        Some(Rule {
            effect: Mode::Allow,
            verbs: vec![a.verb.to_string()],
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
    /// An agent's port-forward started, changed or stopped.
    Forwards,
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
    /// The kinds of CRDs applied through the hub, and when, that a lookup has not found yet.
    new_kinds: Mutex<Vec<(Kind, Instant)>>,
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
    /// Why rules.yaml, loaded and in force, could not be written with its upgrade. Kept apart
    /// from `rules_error`: edits on the Agents page still try to write the file (and say why
    /// they can't), rather than being skipped and undone at the next load.
    rules_warning: Mutex<Option<String>>,
    rules_text: Mutex<Option<String>>,
    /// The lock on agents.lock while the app or `serve` runs (see [`Hub::claim_config`]).
    config_lock: Mutex<Option<std::fs::File>>,
    /// Why agents.json could not be read at start (see [`load_config`]).
    config_error: Option<String>,
    /// Port-forwards agents opened.
    forwards: super::forwards::AgentForwards,
    /// The folder of agents.json and rules.yaml (another one in tests).
    dir: std::path::PathBuf,
}

impl Hub {
    /// Mark agents.json as in use by this process until it exits (see [`config_in_use`](super::config::config_in_use)).
    pub fn claim_config(&self) {
        let _ = crate::private_files::create_dir(&crate::paths::config_dir());
        *lock(&self.config_lock) = take_lock(&config_lock_path());
    }

    pub fn load() -> Arc<Self> {
        let (mut config, config_error) = load_config(&crate::paths::config_dir().join("agents.json"));
        if let Some(e) = &config_error {
            eprintln!("{e}");
        }
        for agent in &mut config.agents {
            agent.policy.migrate();
            // Saved before the GitOps verbs. rules.yaml, when it exists, says itself whether
            // its rules are that old (`rules_file::apply_upgrading`).
            if config.version < GITOPS_VERBS_VERSION {
                agent.policy.add_gitops_verbs();
            }
        }
        let audit = load_audit_tail(&audit_path());
        Arc::new(Self::new(config, audit, config_error, rules_file::path().exists()))
    }

    fn new(
        config: Config,
        audit: VecDeque<AuditEntry>,
        config_error: Option<String>,
        rules_file_present: bool,
    ) -> Self {
        Self {
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
            rules_file_present: AtomicBool::new(rules_file_present),
            recent_calls: Mutex::default(),
            rules_error: Mutex::default(),
            rules_warning: Mutex::default(),
            rules_text: Mutex::default(),
            config_lock: Mutex::default(),
            config_error,
            forwards: super::forwards::AgentForwards::default(),
            dir: crate::paths::config_dir(),
        }
    }

    /// A hub with these agents that reads no file (and writes none, as long as nothing
    /// saves the config or records a call).
    #[cfg(test)]
    pub(crate) fn for_test(agents: Vec<Agent>) -> Arc<Self> {
        Arc::new(Self::new(Config { agents, ..Config::default() }, VecDeque::new(), None, false))
    }

    /// [`for_test`](Self::for_test) with agents.json and rules.yaml in `dir`.
    #[cfg(test)]
    fn for_test_in(agents: Vec<Agent>, dir: &std::path::Path) -> Arc<Self> {
        let mut hub = Self::new(Config { agents, ..Config::default() }, VecDeque::new(), None, false);
        hub.dir = dir.to_path_buf();
        Arc::new(hub)
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

    /// Why rules.yaml, whose rules are in force, could not be written with its upgrade.
    pub fn rules_warning(&self) -> Option<String> {
        lock(&self.rules_warning).clone()
    }

    /// Load rules.yaml into the agents (creating agents it names). A broken file
    /// changes nothing: the error is kept for the Agents page.
    pub fn apply_rules_file(&self) -> Result<bool> {
        let path = self.dir.join("rules.yaml");
        self.rules_file_present.store(path.exists(), Ordering::Relaxed);
        // Said on the Agents page when the rules loaded but the file couldn't be upgraded.
        let mut warning = None;
        *lock(&self.rules_warning) = None;
        {
            // Read and apply under the config lock: an edit in the app, which writes the file
            // under it, can't land between the two and leave the file and the rules in force
            // different.
            let mut cfg = write(&self.config);
            let loaded = rules_file::read(&path).and_then(|text| {
                (*lock(&self.rules_text)).clone_from(&text);
                text.map(|t| rules_file::check(&t)).transpose()
            });
            let file = match loaded {
                Ok(Some(f)) => f,
                Ok(None) => {
                    *lock(&self.rules_error) = None;
                    // No file, so no agent missing from it: a file made later is read as it is.
                    cfg.version = CONFIG_VERSION;
                    return Ok(false);
                }
                Err(e) => {
                    *lock(&self.rules_error) = Some(format!("{e:#}"));
                    drop(cfg);
                    self.emit(HubEvent::AgentSeen);
                    return Err(e);
                }
            };
            // A section could not say which of two agents with its name it is for.
            if let Some(name) = rules_file::shared_name(&cfg.agents) {
                let e = anyhow::anyhow!("two agents are named `{name}`: delete one");
                *lock(&self.rules_error) = Some(format!("{e:#}"));
                drop(cfg);
                self.emit(HubEvent::AgentSeen);
                return Err(e);
            }
            // Rules saved by an older version are upgraded, and the file written, once: see
            // `rules_file::apply_upgrading`.
            let mut agents = cfg.agents.clone();
            // The file's own `version` decides whether its rules are upgraded, so agents.json's
            // is the current one from now on, but while sections it should have gained (agents
            // from before 0.3.0) are not written yet, it stays below that.
            let mut version = CONFIG_VERSION;
            if rules_file::apply_upgrading(&file, &mut agents, cfg.version)? {
                let mut seen = lock(&self.rules_text);
                match rules_file::export_over(&path, &agents, seen.as_deref()) {
                    Ok(text) => *seen = Some(text),
                    // Your edit is loaded next, and upgraded then.
                    Err(e) if rules_file::changed_on_disk(&e) => {
                        let e = e.context("rules.yaml could not be upgraded for this version");
                        *lock(&self.rules_error) = Some(format!("{e:#}"));
                        drop((seen, cfg));
                        self.emit(HubEvent::AgentSeen);
                        return Err(e);
                    }
                    // A file in a folder that can't be written (managed by another tool) still
                    // holds the rules: they are applied upgraded, as the file means them, and
                    // the file, still without its `version`, is upgraded again at each load
                    // until it is written. Refusing the load would keep older rules in force,
                    // new denies left out. Edits on the Agents page try the write and fail.
                    Err(e) => {
                        let e = e.context("rules.yaml could not be written for this version; its rules are in force");
                        warning = Some(format!("{e:#}"));
                        if cfg.version < SECTIONS_VERSION {
                            version = cfg.version;
                        }
                    }
                }
            }
            cfg.agents = agents;
            cfg.version = version;
        }
        *lock(&self.rules_error) = None;
        *lock(&self.rules_warning) = warning;
        self.save()?;
        self.emit(HubEvent::AgentSeen);
        Ok(true)
    }

    /// Save new rules.yaml text from the app: validated first, then written and applied.
    pub fn save_rules_text(&self, text: &str) -> Result<()> {
        rules_file::check(text)?;
        crate::private_files::write_atomic(&self.dir.join("rules.yaml"), text.as_bytes())?;
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
                if reload_due(&now, failed.as_deref(), lock(&hub.rules_text).as_deref()) {
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

    /// Takes effect on the next start (or on the next try, while the server waits for a busy port).
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
        let text = rules_file::export_over(&self.dir.join("rules.yaml"), agents, seen.as_deref())
            .context("rules.yaml not updated")?;
        *seen = Some(text);
        Ok(())
    }

    fn save(&self) -> Result<()> {
        let json = serde_json::to_vec_pretty(&*read(&self.config))?;
        crate::private_files::write_atomic(&self.dir.join("agents.json"), &json).context("saving agents.json")
    }

    /// The port-forwards agents have open.
    pub fn forwards(&self) -> Vec<super::forwards::ForwardInfo> {
        self.forwards.list()
    }

    pub(super) fn agent_forwards(&self) -> &super::forwards::AgentForwards {
        &self.forwards
    }

    /// Stop the agents' forwards `which` picks (the user's stop button, the agent's own
    /// call, the kill switch, an expiry); returns how many stopped.
    pub fn stop_forwards(&self, which: impl Fn(&super::forwards::ForwardInfo) -> bool) -> usize {
        let n = self.forwards.stop(which);
        if n > 0 {
            self.emit(HubEvent::Forwards);
        }
        n
    }

    pub(super) fn forwards_changed(&self) {
        self.emit(HubEvent::Forwards);
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
            self.stop_forwards(|_| true);
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
            let allowed_protected = agent.policy.allow_names_protected();
            f(agent);
            // rules.yaml could not hold it, and it would allow nothing. One already in a
            // hand-edited agents.json doesn't block other edits.
            if agent.policy.allow_names_protected() && !allowed_protected {
                anyhow::bail!(
                    "`{}` works in Ask and Deny rules only: in an Allow rule it would allow nothing",
                    super::PROTECTED
                );
            }
            // rules.yaml doesn't hold the Enabled switch: only write it when names or rules change.
            if !same_rules(&cfg.agents, &agents) {
                self.export_rules(&agents)?;
            }
            cfg.agents = agents;
        }
        // A disabled agent's forwards stop with its calls.
        let enabled: Vec<String> =
            read(&self.config).agents.iter().filter(|a| a.enabled).map(|a| a.id.clone()).collect();
        self.stop_forwards(|f| !enabled.contains(&f.agent_id));
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
        self.stop_forwards(|f| f.agent_id == id);
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

    /// [`agent`](Self::agent), with its rules ready to decide its calls: when one names
    /// `@protected`, the protection of contexts is read now (Settings → Contexts and the
    /// kubeconfig), so a context protected since the last call counts. Unreadable, every
    /// context counts as protected.
    pub fn agent_for_calls(&self, id: &str) -> Option<Agent> {
        let mut agent = self.agent(id)?;
        if agent.policy.names_protected() {
            agent.policy.set_unprotected(crate::settings::unprotected_contexts());
        }
        Some(agent)
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

    /// Ask the user in the UI, giving the request its id. Anything but [`Answer::Approved`]
    /// refuses the call; with no window to ask in, the request is refused at once. The
    /// request ends when `gone` does (the agent stopped waiting for the answer). However it
    /// ends, even with this future dropped, it leaves the hub and its banner goes: an answer
    /// given after that changes nothing.
    pub async fn request_approval(&self, mut request: Approval, gone: impl Future<Output = ()>) -> Answer {
        if self.headless() {
            return Answer::Rejected;
        }
        let id = self.next_approval.fetch_add(1, Ordering::SeqCst);
        request.id = id;
        // One deadline for the banner's countdown and the refusal, so they agree.
        let deadline = Instant::now() + APPROVAL_TIMEOUT;
        request.deadline = deadline;
        let (tx, rx) = oneshot::channel();
        lock(&self.approvals).insert(id, tx);
        let _pending = Pending { hub: self, id };
        self.emit(HubEvent::ApprovalRequested(Arc::new(request)));
        tokio::select! {
            // The agent gone first: an Approve clicked as it hung up must not run the call.
            biased;
            () = gone => Answer::Abandoned,
            answer = rx => answer.unwrap_or(Answer::Rejected),
            () = tokio::time::sleep_until(deadline.into()) => Answer::TimedOut,
        }
    }

    /// Answer a request; a request that no longer waits (answered, timed out, its agent
    /// gone) is left as it ended.
    pub fn resolve_approval(&self, id: u64, approved: bool) {
        let waiting = lock(&self.approvals).remove(&id);
        if let Some(tx) = waiting {
            let _ = tx.send(if approved { Answer::Approved } else { Answer::Rejected });
        }
    }

    /// Whether a request still waits for an answer.
    pub fn approval_pending(&self, id: u64) -> bool {
        lock(&self.approvals).contains_key(&id)
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

    /// The contexts connected for agents whose discovered kinds hold `group`/`kind`, sorted.
    /// No request: a context not connected yet is not among them.
    pub fn contexts_serving(&self, group: &str, kind: &str) -> Vec<String> {
        let clusters: Vec<(String, Arc<Cluster>)> =
            lock(&self.clients).iter().map(|(n, c)| (n.clone(), c.clone())).collect();
        let mut out: Vec<String> = clusters
            .into_iter()
            .filter(|(_, c)| lock(&c.kinds).as_ref().is_some_and(|(kinds, _)| kinds.iter().any(|k| k.is(group, kind))))
            .map(|(n, _)| n)
            .collect();
        out.sort();
        out
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
        if let Ok(kinds) = k8s::discover(client).await {
            let kinds = Arc::new(kinds);
            *lock(&cluster.kinds) = Some((kinds.clone(), Instant::now()));
            Some(kinds)
        } else {
            // A failure counts too, so a broken API isn't asked again on every call.
            if let Some(previous) = previous {
                *lock(&cluster.kinds) = Some((previous, Instant::now()));
            }
            None
        }
    }

    /// Note the kinds of CRDs just applied on `context` (`k8s::crd_kinds`). Until a lookup
    /// finds them, one that misses them asks their group version at once, and waits for the
    /// API server to serve them, instead of waiting for the next full discovery.
    pub fn crds_applied(&self, context: &str, kinds: Vec<Kind>) {
        if kinds.is_empty() {
            return;
        }
        let Ok(cluster) = self.cluster(context) else { return };
        note_new(&mut lock(&cluster.new_kinds), kinds, Instant::now());
    }

    /// The kind of `api_version` `kind` the kinds discovered on `context` hold. No request.
    pub fn known(&self, context: &str, api_version: &str, kind: &str) -> Option<Kind> {
        let cluster = self.cluster(context).ok()?;
        let kinds = lock(&cluster.kinds);
        kinds.as_ref().and_then(|(kinds, _)| k8s::manifest_kind(kinds, api_version, kind).cloned())
    }

    /// A kind missing from the kinds discovered on `context`, looked up again. The kind of a
    /// CRD applied through the hub lately, or one of a group version discovery doesn't
    /// have, is asked of its group version at once (one request); for a CRD applied lately
    /// it waits up to `SERVE_WAIT` for the API server to serve it. Anything else waits for
    /// the next full discovery (`rediscover`, once a minute).
    pub async fn new_kind(&self, context: &str, api_version: &str, kind: &str) -> Option<Kind> {
        let cluster = self.cluster(context).ok()?;
        let client = cluster.client.get()?.clone();
        let cached = lock(&cluster.kinds).as_ref().map_or_default(|(k, _)| k.clone());
        let (group, version, wait) = match lookup(&cached, &new_kinds(&cluster), api_version, kind) {
            Lookup::Known => return k8s::manifest_kind(&cached, api_version, kind).cloned(),
            Lookup::Everything => {
                let kinds = self.rediscover(context).await?;
                return k8s::manifest_kind(&kinds, api_version, kind).cloned();
            }
            Lookup::Group { group, version, wait } => (group, version, wait),
        };
        let deadline = Instant::now() + if wait { SERVE_WAIT } else { Duration::ZERO };
        loop {
            if let Ok(fresh) = k8s::discover_version(client.clone(), group.clone(), version.clone()).await {
                let found = k8s::manifest_kind(&fresh, api_version, kind).cloned();
                {
                    let mut cache = lock(&cluster.kinds);
                    if let Some((kinds, _)) = cache.as_mut() {
                        *kinds = Arc::new(k8s::with_version(kinds, &group, &version, fresh));
                    }
                }
                if let Some(found) = found {
                    lock(&cluster.new_kinds).retain(|(k, _)| !same_type(k, &found));
                    return Some(found);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(SERVE_POLL).await;
        }
    }

    /// The kind named `query` (`stages`, `stages.kwok.x-k8s.io`) of a CRD applied lately
    /// that discovery doesn't have yet, looked up in its group version (see `new_kind`).
    pub async fn new_kind_named(&self, context: &str, query: &str) -> Option<Kind> {
        let cluster = self.cluster(context).ok()?;
        let new = k8s::find_kind(&new_kinds(&cluster), query).cloned()?;
        self.new_kind(context, &new.ar.api_version, &new.ar.kind).await
    }
}

/// Where a kind missing from discovery is looked for.
#[derive(Debug, PartialEq, Eq)]
enum Lookup {
    /// It is there now: another call found it meanwhile.
    Known,
    /// In its own group version, asked now; `wait`: for the API server to serve it, as the
    /// kind of a CRD applied lately.
    Group { group: String, version: String, wait: bool },
    /// In the next full discovery.
    Everything,
}

/// Where to look for `api_version` `kind`, from the kinds discovered (`cached`) and those of
/// CRDs applied lately (`new`). A kind of a CRD applied lately, or of a group version
/// discovery doesn't have (its CRD may be new), is asked of its group version: one cheap
/// request. A kind missing from a group version discovered waits for the next full
/// discovery, as does anything that can't be an `apiVersion`.
fn lookup(cached: &[Kind], new: &[Kind], api_version: &str, kind: &str) -> Lookup {
    if k8s::manifest_kind(cached, api_version, kind).is_some() {
        return Lookup::Known;
    }
    let Some((group, version)) = group_version(api_version) else { return Lookup::Everything };
    let wait = k8s::manifest_kind(new, api_version, kind).is_some();
    if wait || !cached.iter().any(|k| k.ar.api_version == api_version) {
        Lookup::Group { group: group.into(), version: version.into(), wait }
    } else {
        Lookup::Everything
    }
}

/// The group and version of an `apiVersion` (`v1`, `apps/v1`), when it can be one: they go
/// into the request's path.
fn group_version(api_version: &str) -> Option<(&str, &str)> {
    let (group, version) = match api_version.split_once('/') {
        Some(("", _)) => return None,
        Some(gv) => gv,
        None => ("", api_version),
    };
    let name = |s: &str, dots: bool| {
        s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || (dots && c == '.'))
    };
    (!version.is_empty() && name(version, false) && name(group, true) && !group.starts_with('.'))
        .then_some((group, version))
}

/// The same type: the same `apiVersion` and `kind`.
fn same_type(a: &Kind, b: &Kind) -> bool {
    a.ar.api_version == b.ar.api_version && a.ar.kind == b.ar.kind
}

/// `kinds` noted in `new` as applied at `now`: each type once, the expired dropped, and the
/// oldest past `MAX_NEW_KINDS`.
fn note_new(new: &mut Vec<(Kind, Instant)>, kinds: Vec<Kind>, now: Instant) {
    new.retain(|(k, at)| now.duration_since(*at) < NEW_CRD_FOR && !kinds.iter().any(|n| same_type(n, k)));
    new.extend(kinds.into_iter().map(|k| (k, now)));
    let excess = new.len().saturating_sub(MAX_NEW_KINDS);
    new.drain(..excess);
}

/// The kinds of CRDs applied lately that no lookup has found yet.
fn new_kinds(cluster: &Cluster) -> Vec<Kind> {
    let now = Instant::now();
    let new = lock(&cluster.new_kinds);
    new.iter().filter(|(_, at)| now.duration_since(*at) < NEW_CRD_FOR).map(|(k, _)| k.clone()).collect()
}

/// A request waiting for the user's answer. Dropped when the wait ends, however it ends, and
/// with the call if the call is dropped while it waits: the request leaves the hub, and the
/// UI takes its banner down.
struct Pending<'a> {
    hub: &'a Hub,
    id: u64,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        lock(&self.hub.approvals).remove(&self.id);
        self.hub.emit(HubEvent::ApprovalResolved(self.id));
    }
}

/// Whether rules.yaml must be reloaded, from what reading it gave now (its text, None when
/// missing, or why it can't be read), why it failed last time and the text last loaded.
/// By content: an edit within the same second as the last one still counts. A read that
/// fails again the same way changes nothing; one that works again always reloads, to clear
/// the error.
fn reload_due(now: &Result<Option<String>, String>, failed: Option<&str>, seen: Option<&str>) -> bool {
    match now {
        Ok(text) => failed.is_some() || text.as_deref() != seen,
        Err(e) => failed != Some(e.as_str()),
    }
}

fn rediscovery_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|t| now.duration_since(t) >= REDISCOVER_EVERY)
}

#[cfg(test)]
mod tests {
    use tokio_util::sync::CancellationToken;

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

    /// rules.yaml refuses `@protected` in an Allow rule, so the app never holds one it
    /// could not write back. Refused before anything is saved: no file is written here.
    #[test]
    fn an_allow_rule_naming_protected_contexts_is_not_saved() {
        let protected = |effect| Rule {
            effect,
            verbs: vec!["delete".into()],
            resources: vec!["pods".into()],
            namespaces: vec!["*".into()],
            contexts: vec!["staging".into(), super::super::PROTECTED.into()],
            names: vec!["*".into()],
            ..Rule::default()
        };
        let hub = Hub::for_test(vec![agent()]);
        assert!(hub.update_agent("a1", |a| a.policy.rules.push(protected(Mode::Allow))).is_err());
        assert_eq!(hub.agent("a1").unwrap().policy.rules, Policy::default().rules);
        // Ask and Deny rules may name it.
        for effect in [Mode::Ask, Mode::Deny] {
            let p = Policy { rules: vec![protected(effect)], ..Policy::default() };
            assert!(!p.allow_names_protected());
        }
    }

    /// A rules.yaml from before the GitOps verbs, given with no agents.json (so the config is
    /// the default one, at the current version), is upgraded all the same: an Argo CD sync
    /// is asked as the `patch` it was checked as, not allowed by a wider Allow. Written
    /// again with its `version`, it then loads as written.
    #[test]
    fn an_old_rules_file_without_agents_json_is_upgraded() {
        let dir = crate::mcp::test_dir();
        let old = "agents:\n  ci:\n    allow: [\"* * -n team-a\"]\n    ask: [\"patch,delete *\"]\n";
        std::fs::write(dir.join("rules.yaml"), old).unwrap();
        let hub = Hub::for_test_in(Vec::new(), &dir);
        assert_eq!(hub.config().version, CONFIG_VERSION);
        assert!(hub.apply_rules_file().unwrap());
        let sync = |verb| Access {
            context: "hub".into(),
            verb,
            resource: "applications.argoproj.io".into(),
            namespace: Some("team-a".into()),
            name: Some("web".into()),
            ..Access::default()
        };
        let ci = || hub.config().agents.into_iter().find(|a| a.name == "ci").unwrap();
        for verb in [Verb::Patch, Verb::Refresh, Verb::Sync, Verb::Suspend] {
            assert_eq!(ci().policy.decide(&sync(verb)).mode, Mode::Ask, "{verb}");
        }
        let written = std::fs::read_to_string(dir.join("rules.yaml")).unwrap();
        assert!(written.contains(&format!("\nversion: {}\n", rules_file::FILE_VERSION)), "{written}");
        // Loaded again, as written: a verb taken out by hand stays out.
        let edited = written.replace("patch,delete,refresh,sync,suspend", "patch,delete,sync,suspend");
        assert_ne!(edited, written);
        std::fs::write(dir.join("rules.yaml"), &edited).unwrap();
        assert!(hub.apply_rules_file().unwrap());
        assert_eq!(std::fs::read_to_string(dir.join("rules.yaml")).unwrap(), edited);
        assert_eq!(ci().policy.decide(&sync(Verb::Refresh)).mode, Mode::Allow);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A rules.yaml whose upgrade can't be written (its folder is read-only) is in force,
    /// upgraded, with a warning that doesn't stop edits from trying the file: a Deny added
    /// on the Agents page then fails with the write error, rather than being kept in
    /// agents.json only and undone at the next load.
    #[cfg(unix)]
    #[test]
    fn an_edit_never_vanishes_when_the_upgrade_could_not_be_written() {
        use std::os::unix::fs::PermissionsExt as _;
        /// Gives the folder its permissions back, whatever the test does.
        struct Writable(std::path::PathBuf);
        impl Drop for Writable {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let dir = crate::mcp::test_dir();
        let guard = Writable(dir.clone());
        let old = "agents:\n  ci:\n    ask: [\"patch *\"]\n";
        std::fs::write(dir.join("rules.yaml"), old).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        // Root writes anyway: nothing to check then.
        if std::fs::write(dir.join("probe"), "").is_ok() {
            return;
        }
        let hub = Hub::for_test_in(Vec::new(), &dir);
        // agents.json can't be saved in that folder either; the rules are in force all the same.
        let _ = hub.apply_rules_file();
        assert_eq!(hub.rules_error(), None);
        assert!(hub.rules_warning().is_some_and(|w| w.contains("could not be written")));
        let ci = || hub.config().agents.into_iter().find(|a| a.name == "ci").unwrap();
        assert_eq!(ci().policy.rules[0].verbs, ["patch", "refresh", "sync", "suspend"]);
        let deny = rules_file::parse_rule(Mode::Deny, "delete pods").unwrap();
        let edit = hub.update_agent(&ci().id, |a| a.policy.rules.push(deny.clone()));
        assert!(edit.is_err());
        assert!(!ci().policy.rules.contains(&deny));
        assert_eq!(std::fs::read_to_string(dir.join("rules.yaml")).unwrap(), old);
        // Writable again: the next load writes the upgrade, and the edit is written too.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(hub.apply_rules_file().unwrap());
        assert_eq!(hub.rules_warning(), None);
        hub.update_agent(&ci().id, |a| a.policy.rules.push(deny.clone())).unwrap();
        assert!(hub.apply_rules_file().unwrap());
        assert!(ci().policy.rules.contains(&deny));
        drop(guard);
    }

    #[test]
    fn an_unreadable_rules_file_is_reloaded_once() {
        let seen = Some("old".to_string());
        let denied: Result<Option<String>, String> = Err("permission denied".into());
        assert!(reload_due(&denied, None, seen.as_deref()));
        // Still unreadable the same way: nothing to do.
        assert!(!reload_due(&denied, Some("permission denied"), seen.as_deref()));
        assert!(reload_due(&Err("other".into()), Some("permission denied"), seen.as_deref()));
        // Readable again, even with the text last loaded: reloaded to clear the error.
        assert!(reload_due(&Ok(seen.clone()), Some("permission denied"), seen.as_deref()));
        assert!(!reload_due(&Ok(seen.clone()), None, seen.as_deref()));
        assert!(reload_due(&Ok(None), None, seen.as_deref()));
    }

    #[test]
    fn only_a_refusal_says_the_user_refused() {
        assert!(Answer::Rejected.reason().contains("did not approve"));
        for a in [Answer::TimedOut, Answer::SwitchedOff, Answer::Abandoned] {
            assert!(!a.reason().contains("did not approve"), "{a:?}");
        }
    }

    fn agent() -> Agent {
        Agent {
            id: "a1".into(),
            name: "claude".into(),
            token_sha256: String::new(),
            enabled: true,
            policy: Policy::default(),
        }
    }

    /// A request for approval of a patch, waiting on its own task until `gone` ends.
    fn ask(hub: &Arc<Hub>, gone: impl Future<Output = ()> + Send + 'static) -> tokio::task::JoinHandle<Answer> {
        let hub = hub.clone();
        let access = Access {
            context: "c".into(),
            verb: Verb::Patch,
            resource: "configmaps".into(),
            resource_aliases: vec![],
            namespace: Some("app".into()),
            name: Some("cfg".into()),
            cluster_scoped: false,
        };
        let request = Approval {
            id: 0,
            agent_id: agent().id,
            agent: agent().name,
            tool: "patch_resource".into(),
            summary: "patch".into(),
            detail: None,
            preview: None,
            access,
            dry_run: false,
            once_only: false,
            deadline: Instant::now(),
        };
        tokio::spawn(async move { hub.request_approval(request, gone).await })
    }

    /// Lets the request's task run until request `id` waits in the hub.
    async fn until_pending(hub: &Hub, id: u64) {
        while !hub.approval_pending(id) {
            tokio::task::yield_now().await;
        }
    }

    /// The approval events sent so far, as (asked, id): `true` when the request was made,
    /// `false` when it ended (the UI then takes its banner down).
    fn approval_events(events: &mut mpsc::UnboundedReceiver<Arc<HubEvent>>) -> Vec<(bool, u64)> {
        std::iter::from_fn(|| events.try_recv().ok())
            .filter_map(|e| match &*e {
                HubEvent::ApprovalRequested(a) => Some((true, a.id)),
                HubEvent::ApprovalResolved(id) => Some((false, *id)),
                _ => None,
            })
            .collect()
    }

    /// The agent stopping waiting ends its request at once: the banner goes, the call is
    /// told it was abandoned, and an Approve clicked after that reaches nothing.
    #[tokio::test]
    async fn a_request_ends_when_its_agent_stops_waiting() {
        let hub = Hub::for_test(vec![agent()]);
        let mut events = hub.subscribe();
        let gone = CancellationToken::new();
        let wait = ask(&hub, gone.clone().cancelled_owned());
        until_pending(&hub, 1).await;
        gone.cancel();
        let answer = tokio::time::timeout(Duration::from_secs(1), wait).await.unwrap().unwrap();
        assert_eq!(answer, Answer::Abandoned);
        assert!(!hub.approval_pending(1));
        assert_eq!(approval_events(&mut events), [(true, 1), (false, 1)]);
        // A late Approve: no request waits for it, so nothing is sent and nothing changes.
        hub.resolve_approval(1, true);
        assert!(lock(&hub.approvals).is_empty());
        assert!(approval_events(&mut events).is_empty());
    }

    /// The banner counts down to the moment the hub refuses the request: the deadline it
    /// carries is the timeout's, not the placeholder its caller gave.
    #[tokio::test]
    async fn a_request_carries_the_deadline_it_is_refused_at() {
        let hub = Hub::for_test(vec![agent()]);
        let mut events = hub.subscribe();
        let before = Instant::now();
        let wait = ask(&hub, std::future::pending());
        until_pending(&hub, 1).await;
        let after = Instant::now();
        let asked = std::iter::from_fn(|| events.try_recv().ok())
            .find_map(|e| match &*e {
                HubEvent::ApprovalRequested(a) => Some(a.clone()),
                _ => None,
            })
            .unwrap();
        assert!(asked.deadline >= before + APPROVAL_TIMEOUT && asked.deadline <= after + APPROVAL_TIMEOUT);
        assert!(asked.time_left(after) <= APPROVAL_TIMEOUT);
        assert_eq!(asked.time_left(asked.deadline + Duration::from_secs(5)), Duration::ZERO);
        wait.abort();
    }

    /// A call dropped while it waits takes its request with it: no banner is left to approve.
    #[tokio::test]
    async fn a_dropped_call_takes_its_request_with_it() {
        let hub = Hub::for_test(vec![agent()]);
        let mut events = hub.subscribe();
        let wait = ask(&hub, std::future::pending());
        until_pending(&hub, 1).await;
        wait.abort();
        assert!(wait.await.unwrap_err().is_cancelled());
        assert!(!hub.approval_pending(1));
        assert_eq!(approval_events(&mut events), [(true, 1), (false, 1)]);
        hub.resolve_approval(1, true);
        assert!(lock(&hub.approvals).is_empty());
        assert!(approval_events(&mut events).is_empty());
    }

    /// An answer reaches a request whose agent still waits; given as the agent goes, the
    /// request is abandoned all the same.
    #[tokio::test]
    async fn an_answer_reaches_only_a_request_still_waited_for() {
        let hub = Hub::for_test(vec![agent()]);
        let wait = ask(&hub, std::future::pending());
        until_pending(&hub, 1).await;
        hub.resolve_approval(1, true);
        assert_eq!(wait.await.unwrap(), Answer::Approved);

        let gone = CancellationToken::new();
        let wait = ask(&hub, gone.clone().cancelled_owned());
        until_pending(&hub, 2).await;
        hub.resolve_approval(2, true);
        gone.cancel();
        assert_eq!(wait.await.unwrap(), Answer::Abandoned);
        assert!(lock(&hub.approvals).is_empty());
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

    fn group(group: &str, version: &str, wait: bool) -> Lookup {
        Lookup::Group { group: group.into(), version: version.into(), wait }
    }

    /// A CR applied right after its CRD (#839): its type is asked of its own group version
    /// at once, and waited for, instead of being unknown until the next full discovery.
    #[test]
    fn a_type_of_a_crd_just_applied_is_asked_of_its_group() {
        let cached = vec![Kind::fake("", "Pod"), Kind::fake("apps", "Deployment"), Kind::fake("example.com", "Widget")];
        let stage = Kind::fake("kwok.x-k8s.io", "Stage");
        let gadget = Kind::fake("example.com", "Gadget");
        let new = vec![stage, gadget];
        assert_eq!(lookup(&cached, &new, "apps/v1", "Deployment"), Lookup::Known);
        // Announced by a CRD just applied: asked of its group, waiting for it to be served,
        // whether or not its group version was discovered.
        assert_eq!(lookup(&cached, &new, "kwok.x-k8s.io/v1", "Stage"), group("kwok.x-k8s.io", "v1", true));
        assert_eq!(lookup(&cached, &new, "example.com/v1", "Gadget"), group("example.com", "v1", true));
        // A group version discovery doesn't have: its CRD may be new, asked once.
        assert_eq!(lookup(&cached, &[], "kwok.x-k8s.io/v1alpha1", "Stage"), group("kwok.x-k8s.io", "v1alpha1", false));
        assert_eq!(lookup(&cached, &[], "v2", "Pod"), group("", "v2", false));
        // A kind missing from a group version discovered, or no `apiVersion` at all: the next
        // full discovery, as before.
        assert_eq!(lookup(&cached, &[], "example.com/v1", "Gadget"), Lookup::Everything);
        assert_eq!(lookup(&cached, &[], "v1", "Pdo"), Lookup::Everything);
        for bad in ["", "/v1", "a/b/c", "Example.com/v1", "example.com/", "../v1", "example.com/v1?x=1"] {
            assert_eq!(lookup(&cached, &new, bad, "Stage"), Lookup::Everything, "{bad}");
        }
    }

    #[test]
    fn only_a_plain_api_version_goes_into_a_request() {
        assert_eq!(group_version("v1"), Some(("", "v1")));
        assert_eq!(group_version("kwok.x-k8s.io/v1alpha1"), Some(("kwok.x-k8s.io", "v1alpha1")));
        assert_eq!(group_version("a/b/c"), None);
        assert_eq!(group_version("../v1"), None);
        assert_eq!(group_version("/v1"), None);
        assert_eq!(group_version("x/v 1"), None);
    }

    /// The kinds of CRDs applied are kept once each, for a while, and only so many.
    #[test]
    fn new_crd_kinds_are_kept_once_for_a_while_and_bounded() {
        let names = |new: &[(Kind, Instant)]| new.iter().map(|(k, _)| k.ar.kind.clone()).collect::<Vec<_>>();
        let now = Instant::now();
        let mut new = Vec::new();
        note_new(&mut new, vec![Kind::fake("a.io", "Stage")], now);
        note_new(&mut new, vec![Kind::fake("a.io", "Stage"), Kind::fake("a.io", "Attach")], now);
        assert_eq!(names(&new), ["Stage", "Attach"]);
        // Applied again later: kept from then.
        let later = now + NEW_CRD_FOR / 2;
        note_new(&mut new, vec![Kind::fake("a.io", "Stage")], later);
        assert_eq!(names(&new), ["Attach", "Stage"]);
        note_new(&mut new, vec![Kind::fake("b.io", "Other")], now + NEW_CRD_FOR);
        assert_eq!(names(&new), ["Stage", "Other"]);
        let many: Vec<Kind> = (0..MAX_NEW_KINDS + 10).map(|i| Kind::fake("c.io", &format!("K{i}"))).collect();
        note_new(&mut new, many, now + NEW_CRD_FOR);
        assert_eq!(new.len(), MAX_NEW_KINDS);
        assert_eq!(new.first().map(|(k, _)| k.ar.kind.clone()), Some("K10".to_string()));
    }
}
