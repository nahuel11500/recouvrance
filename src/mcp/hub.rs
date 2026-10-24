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
    Agent, CONFIG_VERSION, Config, config_lock_path, hash_token, load_config, new_token, same_rules, take_lock,
};
use super::policy::{Access, Mode, Policy, Rule, Verb};
use super::{lock, read, rules_file, write};
use crate::k8s::{self, Kind};

/// Discovery runs again for an unknown kind (a CRD installed since) at most this often.
const REDISCOVER_EVERY: Duration = Duration::from_secs(60);

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
            Self::Approved => "approved",
            Self::Rejected => "the user did not approve this request",
            Self::TimedOut => "the user did not answer within 2 minutes",
            Self::SwitchedOff => "agent access was switched off while the request waited",
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
        if matches!(a.verb, Verb::Exec | Verb::Patch | Verb::Scale)
            || self.tool == "gitops"
            || self.tool == "apply_manifest"
            || self.dry_run
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
        {
            // Read and apply under the config lock: an edit in the app, which writes the file
            // under it, can't land between the two and leave the file and the rules in force
            // different.
            let mut cfg = write(&self.config);
            let loaded = rules_file::read(&path).and_then(|text| {
                *lock(&self.rules_text) = text.clone();
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
            // agents.json from before 0.3.0: an agent created then had no section until its
            // rules were edited. It keeps its rules and its section is written, once.
            let upgrade = cfg.version < CONFIG_VERSION;
            let mut agents = cfg.agents.clone();
            rules_file::apply_file(&file, &mut agents, upgrade)?;
            if upgrade && agents.iter().any(|a| !file.agents.contains_key(&a.name)) {
                let mut seen = lock(&self.rules_text);
                match rules_file::export_over(&path, &agents, seen.as_deref()) {
                    Ok(text) => *seen = Some(text),
                    Err(e) => {
                        let e = e.context("rules.yaml could not be given the agents it doesn't name");
                        *lock(&self.rules_error) = Some(format!("{e:#}"));
                        drop((seen, cfg));
                        self.emit(HubEvent::AgentSeen);
                        return Err(e);
                    }
                }
            }
            cfg.agents = agents;
            cfg.version = CONFIG_VERSION;
        }
        *lock(&self.rules_error) = None;
        self.save()?;
        self.emit(HubEvent::AgentSeen);
        Ok(true)
    }

    /// Save new rules.yaml text from the app: validated first, then written and applied.
    pub fn save_rules_text(&self, text: &str) -> Result<()> {
        rules_file::check(text)?;
        crate::private_files::write_atomic(&rules_file::path(), text.as_bytes())?;
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
        crate::private_files::write_atomic(&crate::paths::config_dir().join("agents.json"), &json)
            .context("saving agents.json")
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
        let waiting = lock(&self.approvals).remove(&id);
        if let Some(tx) = waiting {
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
        for a in [Answer::TimedOut, Answer::SwitchedOff] {
            assert!(!a.reason().contains("did not approve"), "{a:?}");
        }
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
}
