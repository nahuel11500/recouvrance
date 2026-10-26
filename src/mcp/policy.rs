//! Agent rules: what each request may do (allow, ask, deny), decided the same way for every tool.

use serde::{Deserialize, Serialize};

use crate::glob::glob;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Allow,
    Ask,
    Deny,
}

impl Mode {
    pub const ALL: [Self; 3] = [Self::Allow, Self::Ask, Self::Deny];

    pub fn label(self) -> &'static str {
        match self {
            Self::Allow => "Allow",
            Self::Ask => "Ask me",
            Self::Deny => "Deny",
        }
    }
}

/// What an agent asks to do, kubectl style. Rules name verbs as text (`get,list`, `*`),
/// matched against [`Verb::as_str`].
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum Verb {
    #[default]
    Get,
    List,
    Logs,
    Reveal,
    Exec,
    Scale,
    Restart,
    Patch,
    Apply,
    Delete,
    Drain,
    /// Open a port-forward to a pod or Service: a network path into the cluster.
    Forward,
}

impl Verb {
    pub const ALL: [Self; 12] = [
        Self::Get,
        Self::List,
        Self::Logs,
        Self::Reveal,
        Self::Exec,
        Self::Scale,
        Self::Restart,
        Self::Patch,
        Self::Apply,
        Self::Delete,
        Self::Drain,
        Self::Forward,
    ];

    /// The verbs that only read (a default policy grants these).
    pub const READ: [Self; 3] = [Self::Get, Self::List, Self::Logs];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::List => "list",
            Self::Logs => "logs",
            Self::Reveal => "reveal",
            Self::Exec => "exec",
            Self::Scale => "scale",
            Self::Restart => "restart",
            Self::Patch => "patch",
            Self::Apply => "apply",
            Self::Delete => "delete",
            Self::Drain => "drain",
            Self::Forward => "forward",
        }
    }

    /// Whether a rule granting it deserves a second look: it changes the cluster, shows
    /// Secret values or opens a network path into it.
    pub const fn is_sensitive(self) -> bool {
        self.is_write() || matches!(self, Self::Reveal | Self::Forward)
    }

    /// Whether it changes the cluster (or runs something in it).
    pub const fn is_write(self) -> bool {
        matches!(
            self,
            Self::Exec | Self::Scale | Self::Restart | Self::Patch | Self::Apply | Self::Delete | Self::Drain
        )
    }
}

impl std::fmt::Display for Verb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Verb {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL.into_iter().find(|v| v.as_str() == s).ok_or_else(|| format!("unknown verb `{s}`"))
    }
}

/// Whether a rule's verb pattern (a verb or `*`) grants a write.
pub fn grants_write(pattern: &str) -> bool {
    pattern == "*" || pattern.parse::<Verb>().is_ok_and(Verb::is_write)
}

/// One access rule, like a Kubernetes RBAC rule with an effect.
/// Every field is a list of `*` patterns; an empty list matches nothing. A field missing
/// when read never grants (see `RuleFields`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
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
            verbs: Verb::READ.iter().map(ToString::to_string).collect(),
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
    pub verb: Verb,
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
        let verb = self.verbs.iter().any(|v| v == "*" || v.eq_ignore_ascii_case(a.verb.as_str()));
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
    pub fn same_request(&self, other: &Self) -> bool {
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
    pub(super) contexts: Vec<String>,
    #[serde(skip_serializing)]
    pub(super) namespaces_allow: Vec<String>,
    #[serde(skip_serializing)]
    pub(super) namespaces_deny: Vec<String>,
    #[serde(skip_serializing)]
    pub(super) kinds_deny: Vec<String>,
    #[serde(skip_serializing)]
    pub(super) reveal_secrets: bool,
    #[serde(skip_serializing)]
    pub(super) read: Option<Mode>,
    #[serde(skip_serializing)]
    pub(super) logs: Option<Mode>,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    pub fn pasted(text: &str) -> Option<Self> {
        let mut p: Self = serde_json::from_str(text).ok()?;
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
    pub(super) fn migrate(&mut self) {
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
    if !a.verb.is_write() || a.resource != "namespaces" || a.namespace.is_some() {
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::mcp::Approval;

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

    pub(super) fn access(ctx: &str, verb: &str, res: &str, ns: Option<&str>, name: Option<&str>) -> Access {
        Access {
            context: ctx.into(),
            verb: verb.parse().unwrap(),
            resource: res.into(),
            resource_aliases: vec![],
            namespace: ns.map(String::from),
            name: name.map(String::from),
            cluster_scoped: false,
        }
    }

    pub(super) fn rule(effect: Mode, verbs: &[&str], res: &[&str], ns: &[&str], ctx: &[&str]) -> Rule {
        let v = |x: &[&str]| x.iter().map(ToString::to_string).collect();
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
}

/// Properties of the rule engine over many generated policies and requests, rather than
/// single examples: what a reviewer would otherwise have to check case by case.
#[cfg(test)]
pub(crate) mod properties {
    use super::{Access, Mode, Policy, Rule, Verb};
    use crate::mcp::Approval;
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

    pub(crate) fn verb() -> impl Strategy<Value = Verb> {
        prop_oneof![Just(Verb::Get), Just(Verb::List), Just(Verb::Delete), Just(Verb::Patch), Just(Verb::Exec)]
    }

    pub(crate) fn rule() -> impl Strategy<Value = Rule> {
        (
            prop_oneof![Just(Mode::Allow), Just(Mode::Ask), Just(Mode::Deny)],
            prop::collection::vec(prop_oneof![verb().prop_map(|v| v.to_string()), Just("*".to_string())], 0..3),
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
            let mut more = policy;
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
    use super::{Mode, Rule, Verb};

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
            verb: Verb::Delete,
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
