//! Agent rules as code: `~/.config/recouvrance/rules.yaml`, in the spirit of Claude Code's
//! `permissions.allow / ask / deny`, with kubectl-like rule strings:
//!
//! ```yaml
//! agents:
//!   Claude:
//!     allow:
//!       - get,list,logs *                            # read everything
//!       - restart,scale deployments/web-* -n team-*  # names after the slash
//!     ask:
//!       - patch,apply,delete * --context staging
//!     deny:
//!       - "* secrets"
//!       - "* * --context *prod*"
//!       - exec pods until 2026-11-01                 # expires
//!     limits: { calls_per_minute: 120 }
//! ```
//!
//! A rule is `VERBS RESOURCES[/NAMES] [-n NAMESPACES] [--context CONTEXTS] [until TIME]`;
//! lists are comma-separated, `*` globs work everywhere, an omitted part means `*`, and `-`
//! is an empty list (the rule matches nothing until you fill it in).
//! When the file exists it is the source of truth: Recouvrance reloads it when it changes,
//! and edits on the Agents page are written back to it (its comments are not kept).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};

use super::{Mode, Policy, Rule};

pub fn path() -> PathBuf {
    crate::paths::config_dir().join("rules.yaml")
}

// Unknown keys are refused: a misspelt `deny:` would otherwise drop its rules in silence.
#[derive(Serialize, Deserialize, Default, Debug)]
#[serde(deny_unknown_fields)]
pub struct RulesFile {
    #[serde(default)]
    pub agents: BTreeMap<String, AgentRules>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct AgentRules {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ask: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<String>,
    /// Requests you approved in the app ("Always allow this"): allowed even where an ask rule matches.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approved: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<Limits>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub calls_per_minute: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_log_lines: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec_timeout: Option<u32>,
}

/// Written for an empty list, which matches nothing (never read back as `*`).
const NONE: &str = "-";

fn list(s: &str) -> Vec<String> {
    if s.trim() == NONE {
        return Vec::new();
    }
    s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect()
}

/// The patterns that count (blank ones never match a real name).
fn patterns(v: &[String]) -> Vec<&str> {
    v.iter().map(|x| x.trim()).filter(|x| !x.is_empty()).collect()
}

/// A list as written: its patterns, `-` when there are none.
fn join(v: &[String]) -> String {
    let items = patterns(v);
    if items.is_empty() { NONE.into() } else { items.join(",") }
}

/// Lists that an omitted part stands for.
fn star(v: &[String]) -> bool {
    !v.is_empty() && v.iter().all(|x| x.trim() == "*")
}

/// Parse one rule string.
pub fn parse_rule(effect: Mode, text: &str) -> Result<Rule> {
    let mut words = text.split_whitespace();
    let verbs = list(words.next().context("empty rule")?);
    let target = words.next().unwrap_or("*");
    let (res, names) = match target.split_once('/') {
        Some((r, n)) => (list(r), list(n)),
        None => (list(target), vec!["*".into()]),
    };
    let mut rule = Rule {
        effect,
        verbs,
        resources: res,
        namespaces: vec!["*".into()],
        contexts: vec!["*".into()],
        names,
        note: String::new(),
        expires: None,
        approved: false,
    };
    while let Some(w) = words.next() {
        let value = || words.clone().next().map(String::from).with_context(|| format!("`{w}` needs a value"));
        match w {
            "-n" | "--namespace" | "--namespaces" => {
                rule.namespaces = list(&value()?);
                words.next();
            }
            "--context" | "--contexts" | "--ctx" => {
                rule.contexts = list(&value()?);
                words.next();
            }
            "until" | "--until" => {
                let v = value()?;
                words.next();
                rule.expires = Some(parse_time(&v)?);
            }
            other => bail!("unexpected `{other}` (expected -n, --context or until)"),
        }
    }
    for v in &rule.verbs {
        if v != "*" && !super::VERBS.iter().any(|(name, _)| name == v) {
            bail!(
                "unknown verb `{v}` (known: {})",
                super::VERBS.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
            );
        }
    }
    Ok(rule)
}

fn parse_time(v: &str) -> Result<jiff::Timestamp> {
    if let Ok(t) = v.parse::<jiff::Timestamp>() {
        return Ok(t);
    }
    let date: jiff::civil::Date =
        v.parse().with_context(|| format!("`{v}` is not a date (2026-11-01) or time (2026-11-01T12:00Z)"))?;
    Ok(date.to_zoned(jiff::tz::TimeZone::UTC)?.timestamp())
}

/// The rule as a string (inverse of `parse_rule`).
pub fn format_rule(r: &Rule) -> String {
    let mut s = format!("{} {}", join(&r.verbs), join(&r.resources));
    if !star(&r.names) {
        s.push('/');
        s.push_str(&join(&r.names));
    }
    if !star(&r.namespaces) {
        s.push_str(&format!(" -n {}", join(&r.namespaces)));
    }
    if !star(&r.contexts) {
        s.push_str(&format!(" --context {}", join(&r.contexts)));
    }
    if let Some(t) = r.expires {
        s.push_str(&format!(" until {t}"));
    }
    s
}

/// `format_rule`, refusing a rule whose text would read back as a different rule
/// (patterns with spaces, commas or slashes), so an export never widens access.
fn format_exact(r: &Rule) -> Result<String> {
    let text = format_rule(r);
    let back = parse_rule(r.effect, &text).with_context(|| format!("rule `{text}`"))?;
    // A blank pattern is not written (it never matches a name, but makes a list non-empty).
    let same = |a: &[String], b: &[String]| {
        (star(a) && star(b)) || (a.iter().all(|x| !x.trim().is_empty()) && patterns(a) == patterns(b))
    };
    let exact = same(&r.verbs, &back.verbs)
        && same(&r.resources, &back.resources)
        && same(&r.namespaces, &back.namespaces)
        && same(&r.contexts, &back.contexts)
        && same(&r.names, &back.names)
        && r.expires == back.expires;
    if !exact {
        bail!(
            "rule `{text}` cannot be written exactly in rules.yaml (a pattern is blank or has a space, comma or slash)"
        );
    }
    Ok(text)
}

/// The agent's rules and limits as they would appear in the file.
pub fn from_policy(p: &Policy) -> Result<AgentRules> {
    let pick = |m: Mode, approved: bool| {
        p.rules
            .iter()
            .filter(|r| r.effect == m && (r.effect == Mode::Allow && r.approved) == approved)
            .map(format_exact)
            .collect::<Result<Vec<_>>>()
    };
    Ok(AgentRules {
        allow: pick(Mode::Allow, false)?,
        ask: pick(Mode::Ask, false)?,
        deny: pick(Mode::Deny, false)?,
        approved: pick(Mode::Allow, true)?,
        limits: Some(Limits {
            calls_per_minute: Some(p.max_calls_per_minute),
            max_log_lines: Some(p.max_log_lines),
            max_rows: Some(p.max_rows),
            exec_timeout: Some(p.exec_timeout),
        }),
    })
}

/// Apply an agent's section to its policy (rules replaced, limits set when given).
pub fn apply(rules: &AgentRules, p: &mut Policy) -> Result<()> {
    let mut out = Vec::new();
    let sections = [
        (Mode::Deny, false, &rules.deny),
        (Mode::Ask, false, &rules.ask),
        (Mode::Allow, false, &rules.allow),
        (Mode::Allow, true, &rules.approved),
    ];
    for (mode, approved, items) in sections {
        for text in items {
            let rule = parse_rule(mode, text).with_context(|| format!("{mode:?} rule `{text}`"))?;
            out.push(Rule { approved, ..rule });
        }
    }
    p.rules = out;
    if let Some(l) = &rules.limits {
        if let Some(v) = l.calls_per_minute {
            p.max_calls_per_minute = v;
        }
        if let Some(v) = l.max_log_lines {
            p.max_log_lines = v;
        }
        if let Some(v) = l.max_rows {
            p.max_rows = v;
        }
        if let Some(v) = l.exec_timeout {
            p.exec_timeout = v;
        }
    }
    Ok(())
}

/// Apply the whole file to the agents: each section to the agent with its name (created
/// when missing, with no token yet), and no rules at all for an agent the file doesn't
/// name, so removing its section takes its access away. With `keep_unnamed` (agents.json
/// from before 0.3.0, which created agents without a section) those keep their rules.
pub fn apply_file(file: &RulesFile, agents: &mut Vec<super::Agent>, keep_unnamed: bool) -> Result<()> {
    for (name, rules) in &file.agents {
        let agent = if let Some(i) = agents.iter().position(|a| a.name == *name) {
            &mut agents[i]
        } else {
            // Its token is issued in the app ("New token").
            agents.push(super::Agent {
                id: uuid::Uuid::new_v4().simple().to_string()[..12].to_string(),
                name: name.clone(),
                token_sha256: String::new(),
                enabled: true,
                policy: Policy::default(),
            });
            agents.last_mut().context("agent just added")?
        };
        apply(rules, &mut agent.policy).with_context(|| format!("agent `{name}`"))?;
    }
    if !keep_unnamed {
        for agent in agents.iter_mut().filter(|a| !file.agents.contains_key(&a.name)) {
            agent.policy.rules.clear();
        }
    }
    Ok(())
}

/// The file's text; None when it does not exist.
pub fn read(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        r => Ok(Some(r.with_context(|| format!("reading {}", path.display()))?)),
    }
}

/// Parse and validate the file's text: every rule is checked before anything is applied.
pub fn check(text: &str) -> Result<RulesFile> {
    let file: RulesFile = crate::yaml::from_str(text).context("rules.yaml")?;
    for (name, rules) in &file.agents {
        apply(rules, &mut Policy::default()).with_context(|| format!("agent `{name}`"))?;
    }
    Ok(file)
}

pub const HEADER: &str = "# Recouvrance agent rules. Deny wins, then ask, then allow; anything no rule matches is denied.\n\
# `approved` holds requests you approved in the app: they win over ask, not over deny.\n\
# A rule: VERBS RESOURCES[/NAMES] [-n NAMESPACES] [--context CONTEXTS] [until 2026-11-01]\n\
# Lists are comma-separated and * globs work everywhere; an omitted part means *, and - means none.\n\
# Verbs: get list logs reveal exec scale restart patch apply delete drain (or *).\n\
# Recouvrance reloads this file when you save it; edits on the Agents page rewrite it, without comments.\n";

/// A name two agents share: the file keys agents by name, so it could not tell them apart.
pub fn shared_name(agents: &[super::Agent]) -> Option<&str> {
    let mut seen = std::collections::HashSet::new();
    agents.iter().map(|a| a.name.as_str()).find(|name| !seen.insert(*name))
}

/// The file's text for these agents (refused when a rule cannot be written exactly, or
/// when two agents share a name).
pub fn render(agents: &[super::Agent]) -> Result<String> {
    if let Some(name) = shared_name(agents) {
        bail!("two agents are named `{name}`: delete one");
    }
    let agents = agents
        .iter()
        .map(|a| Ok((a.name.clone(), from_policy(&a.policy).with_context(|| format!("agent `{}`", a.name))?)))
        .collect::<Result<_>>()?;
    Ok(format!("{HEADER}\n{}", crate::yaml::to_string(&RulesFile { agents })?))
}

/// Write the file at `path` for these agents, over `seen`: the text last loaded or written.
/// Refused when the file changed since (an edit of yours not loaded yet would be lost; it
/// is loaded next). Returns the text written.
pub fn export_over(path: &Path, agents: &[super::Agent], seen: Option<&str>) -> Result<String> {
    if read(path)?.as_deref() != seen {
        bail!("rules.yaml changed on disk since it was loaded: it is being reloaded, make this change again");
    }
    let text = render(agents)?;
    crate::private_files::write_atomic(path, text.as_bytes())?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rules() {
        let r = parse_rule(Mode::Allow, "restart,scale deployments/web-* -n team-* --context staging,dev").unwrap();
        assert_eq!(r.verbs, ["restart", "scale"]);
        assert_eq!(r.resources, ["deployments"]);
        assert_eq!(r.names, ["web-*"]);
        assert_eq!(r.namespaces, ["team-*"]);
        assert_eq!(r.contexts, ["staging", "dev"]);
        let r = parse_rule(Mode::Deny, "* secrets").unwrap();
        assert_eq!(
            (r.namespaces.clone(), r.contexts.clone(), r.names),
            (vec!["*".to_string()], vec!["*".to_string()], vec!["*".to_string()])
        );
        let r = parse_rule(Mode::Deny, "exec pods until 2026-11-01").unwrap();
        assert_eq!(r.expires.unwrap().to_string(), "2026-11-01T00:00:00Z");
        assert!(parse_rule(Mode::Allow, "fly pods").is_err());
        assert!(parse_rule(Mode::Allow, "get pods --oops").is_err());
        assert!(parse_rule(Mode::Allow, "get pods -n").is_err());
    }

    #[test]
    fn round_trips() {
        for s in [
            "get,list,logs *",
            "restart deployments/web-* -n team-a --context staging",
            "* secrets",
            "delete pods until 2026-11-01T00:00:00Z",
        ] {
            assert_eq!(format_rule(&parse_rule(Mode::Allow, s).unwrap()), s);
        }
    }

    fn without_notes(rules: &[Rule]) -> Vec<Rule> {
        rules.iter().map(|r| Rule { note: String::new(), ..r.clone() }).collect()
    }

    #[test]
    fn empty_lists_stay_empty() {
        // The default policy's read rule has no contexts yet: it must not become "every context".
        let mut p = Policy::default();
        p.rules.push(Rule { verbs: vec![], ..Rule::default() });
        p.rules.push(Rule { resources: vec![], names: vec![], namespaces: vec![], ..Rule::default() });
        let file = from_policy(&p).unwrap();
        assert_eq!(file.allow[0], "get,list,logs * --context -");
        assert_eq!(file.allow[1], "- * --context -");
        assert_eq!(file.allow[2], "get,list,logs -/- -n - --context -");
        let mut back = Policy { rules: vec![], ..Policy::default() };
        apply(&file, &mut back).unwrap();
        let (mut want, mut got) = (without_notes(&p.rules), without_notes(&back.rules));
        want.sort_by_key(format_rule);
        got.sort_by_key(format_rule);
        assert_eq!(got, want);
        let a = super::super::Access {
            context: "prod".into(),
            verb: "get".into(),
            resource: "pods".into(),
            resource_aliases: vec![],
            namespace: Some("x".into()),
            name: Some("y".into()),
            cluster_scoped: false,
        };
        assert_eq!(back.decide(&a).mode, Mode::Deny);
    }

    #[test]
    fn approvals_keep_their_section() {
        let mut approved = parse_rule(Mode::Allow, "scale deployments.apps/web -n team --context c1").unwrap();
        approved.approved = true;
        let p = Policy { rules: vec![approved.clone()], ..Policy::default() };
        let file = from_policy(&p).unwrap();
        assert!(file.allow.is_empty());
        assert_eq!(file.approved, ["scale deployments.apps/web -n team --context c1"]);
        let mut back = Policy::default();
        apply(&file, &mut back).unwrap();
        assert_eq!(back.rules, [approved]);
    }

    #[test]
    fn refuses_rules_it_cannot_write_exactly() {
        for bad in [vec!["a,b".to_string()], vec!["team a".to_string()]] {
            let p = Policy { rules: vec![Rule { namespaces: bad, ..Rule::default() }], ..Policy::default() };
            assert!(from_policy(&p).is_err());
        }
        let r = Rule { resources: vec!["pods/x".into()], ..Rule::default() };
        assert!(from_policy(&Policy { rules: vec![r], ..Policy::default() }).is_err());
        let r = Rule { contexts: vec!["*".into()], expires: Some(jiff::Timestamp::now()), ..Rule::default() };
        assert!(from_policy(&Policy { rules: vec![r], ..Policy::default() }).is_ok());
    }

    #[test]
    fn an_agent_the_file_does_not_name_has_no_rules() {
        let agent = |id: &str, name: &str| super::super::Agent {
            id: id.into(),
            name: name.into(),
            token_sha256: "hash".into(),
            enabled: true,
            policy: Policy { rules: vec![parse_rule(Mode::Allow, "* *").unwrap()], ..Policy::default() },
        };
        let mut agents = vec![agent("a", "Claude"), agent("b", "ci-bot")];
        let f: RulesFile =
            crate::yaml::from_str("agents:\n  Claude:\n    allow: [\"get pods\"]\n  new:\n    allow: [\"list *\"]\n")
                .unwrap();
        apply_file(&f, &mut agents, false).unwrap();
        assert_eq!(agents.len(), 3);
        assert_eq!(agents[0].policy.rules.len(), 1);
        assert_eq!(agents[0].policy.rules[0].verbs, ["get"]);
        assert!(agents[1].policy.rules.is_empty());
        assert_eq!((agents[2].name.as_str(), agents[2].token_sha256.as_str()), ("new", ""));
        assert_eq!(agents[2].policy.rules[0].verbs, ["list"]);
    }

    #[test]
    fn an_agent_from_before_the_file_named_every_agent_keeps_its_rules() {
        // 0.2.0 created `ci` without writing its section.
        let agent = |id: &str, name: &str| super::super::Agent {
            id: id.into(),
            name: name.into(),
            token_sha256: "hash".into(),
            enabled: true,
            policy: Policy::default(),
        };
        let mut agents = vec![agent("a", "Claude"), agent("b", "ci")];
        let f = check("agents:\n  Claude:\n    allow: [\"get pods\"]\n").unwrap();
        apply_file(&f, &mut agents, true).unwrap();
        assert_eq!(agents[0].policy.rules.len(), 1);
        assert_eq!(agents[1].policy.rules, Policy::default().rules);
        // Written back, the file names it from now on.
        assert!(check(&render(&agents).unwrap()).unwrap().agents.contains_key("ci"));
    }

    #[test]
    fn refuses_agents_sharing_a_name() {
        let agent = |id: &str, name: &str| super::super::Agent {
            id: id.into(),
            name: name.into(),
            token_sha256: String::new(),
            enabled: true,
            policy: Policy::default(),
        };
        let agents = [agent("a", "ci"), agent("b", "other"), agent("c", "ci")];
        assert_eq!(shared_name(&agents), Some("ci"));
        assert!(render(&agents).is_err());
        assert_eq!(shared_name(&agents[..2]), None);
        assert!(render(&agents[..2]).is_ok());
    }

    #[test]
    fn refuses_unknown_keys() {
        for text in [
            "agents:\n  Claude:\n    allow: [\"get *\"]\n    Deny: [\"* secrets\"]\n",
            "agents:\n  Claude:\n    deny_rules: [\"* secrets\"]\n",
            "agents:\n  Claude:\n    limits: { calls_a_minute: 30 }\n",
            "agent:\n  Claude:\n    allow: [\"get *\"]\n",
        ] {
            assert!(crate::yaml::from_str::<RulesFile>(text).is_err(), "{text}");
        }
        let text =
            "agents:\n  Claude:\n    allow: [\"get *\"]\n    deny: [\"* secrets\"]\n    limits: { max_rows: 10 }\n";
        assert!(crate::yaml::from_str::<RulesFile>(text).is_ok());
    }

    #[test]
    fn applies_file() {
        let text = "agents:\n  Claude:\n    allow: [\"get,list *\"]\n    deny: [\"* secrets\"]\n    limits: { calls_per_minute: 30 }\n";
        let f: RulesFile = crate::yaml::from_str(text).unwrap();
        let mut p = Policy::default();
        apply(&f.agents["Claude"], &mut p).unwrap();
        assert_eq!(p.rules.len(), 2);
        assert_eq!(p.max_calls_per_minute, 30);
        // The deny rule wins over the allow rule.
        let a = super::super::Access {
            context: "eu".into(),
            verb: "get".into(),
            resource: "secrets".into(),
            resource_aliases: vec![],
            namespace: Some("x".into()),
            name: Some("y".into()),
            cluster_scoped: false,
        };
        assert_eq!(p.decide(&a).mode, Mode::Deny);
    }
}

/// rules.yaml over many generated agents, rules and edits: the file must never decide a
/// request differently from the rules it was written from, and a section it can't read
/// exactly must fail to load rather than lose rules (agent rule loading broke in three
/// audit rounds).
#[cfg(test)]
mod properties {
    use super::super::{Access, Agent};
    use super::*;
    use crate::mcp::policy::properties::{access, policy, rule};
    use proptest::prelude::*;

    fn agent(name: &str, policy: Policy) -> Agent {
        Agent { id: format!("id-{name}"), name: name.into(), token_sha256: String::new(), enabled: true, policy }
    }

    /// Agent names as people type them, YAML's special words and characters included.
    fn name() -> impl Strategy<Value = String> {
        prop_oneof![
            "[ -~]{1,12}",
            Just("yes".to_string()),
            Just("null".to_string()),
            Just("~".to_string()),
            Just("- x".to_string()),
            Just("a: b".to_string()),
            Just("#ci".to_string()),
            Just("1e3".to_string()),
        ]
    }

    fn same_decisions(a: &Policy, b: &Policy, requests: &[Access]) -> Result<(), TestCaseError> {
        for r in requests {
            prop_assert_eq!(a.decide(r).mode, b.decide(r).mode, "{:?}", r);
            prop_assert_eq!(a.decide_any_namespace(r).mode, b.decide_any_namespace(r).mode, "{:?}", r);
        }
        Ok(())
    }

    proptest! {
        /// Agents written to rules.yaml and loaded back decide every request the same, with
        /// the same limits, whatever their names.
        #[test]
        fn the_file_round_trips(
            agents in prop::collection::btree_map(name(), policy(), 1..4),
            limits in (0..1000u32, 0..1000u32, 0..1000u32, 0..1000u32),
            requests in prop::collection::vec(access(), 1..16),
        ) {
            let agents: Vec<Agent> = agents
                .into_iter()
                .map(|(n, p)| {
                    let (calls, lines, rows, exec) = limits;
                    agent(&n, Policy { max_calls_per_minute: calls, max_log_lines: lines, max_rows: rows, exec_timeout: exec, ..p })
                })
                .collect();
            let text = render(&agents).unwrap();
            let file = check(&text).map_err(|e| TestCaseError::fail(format!("{e:#}\n{text}")))?;
            let mut back: Vec<Agent> = agents.iter().map(|a| agent(&a.name, Policy::default())).collect();
            apply_file(&file, &mut back, false).unwrap();
            prop_assert_eq!(back.len(), agents.len(), "{}", text);
            for (a, b) in agents.iter().zip(&back) {
                same_decisions(&a.policy, &b.policy, &requests)?;
                let l = |p: &Policy| (p.max_calls_per_minute, p.max_log_lines, p.max_rows, p.exec_timeout);
                prop_assert_eq!(l(&a.policy), l(&b.policy));
            }
        }

        /// A rule is written only when it reads back as the same rule: blank patterns and
        /// ones with spaces, commas, slashes or a lone `-` are refused, never widened or narrowed.
        #[test]
        fn a_written_rule_never_changes_meaning(
            r in rule(),
            odd in prop::collection::vec("[a* ,/-]{0,4}", 0..3),
            field in 0..4usize,
            requests in prop::collection::vec(access(), 1..16),
        ) {
            let mut r = r;
            match field {
                0 => r.resources = odd,
                1 => r.namespaces = odd,
                2 => r.contexts = odd,
                _ => r.names = odd,
            }
            if let Ok(text) = format_exact(&r) {
                let back = Rule { approved: r.approved, ..parse_rule(r.effect, &text).unwrap() };
                let one = |rule: &Rule| Policy { rules: vec![rule.clone()], ..Policy::default() };
                same_decisions(&one(&r), &one(&back), &requests)?;
            }
        }

        /// A key the file doesn't know, in a section or its limits, fails the load: a typo
        /// such as `denny:` must not drop the rules under it in silence.
        #[test]
        fn an_unknown_key_fails_the_load(key in "[a-z_]{1,10}", in_limits in any::<bool>()) {
            let known = ["allow", "ask", "deny", "approved", "limits", "calls_per_minute", "max_log_lines", "max_rows", "exec_timeout"];
            prop_assume!(!known.contains(&key.as_str()));
            let text = if in_limits {
                format!("agents:\n  ci:\n    allow: [\"get *\"]\n    limits: {{ {key}: 1 }}\n")
            } else {
                format!("agents:\n  ci:\n    allow: [\"get *\"]\n    {key}: [\"* secrets\"]\n")
            };
            prop_assert!(check(&text).is_err(), "{}", text);
        }

        /// A section with nothing in it, or no section at all, lets nothing through.
        #[test]
        fn a_missing_rule_list_denies(present in any::<bool>(), before in policy(), a in access()) {
            let text = if present { "agents:\n  ci: {}\n" } else { "agents: {}\n" };
            let mut agents = vec![agent("ci", before)];
            apply_file(&check(text).unwrap(), &mut agents, false).unwrap();
            prop_assert_eq!(agents[0].policy.decide(&a).mode, Mode::Deny);
            prop_assert_eq!(agents[0].policy.decide_any_namespace(&a).mode, Mode::Deny);
        }
    }

    /// An edit saved in an editor and not loaded yet is never overwritten by an edit made
    /// in the app at the same time: the app's is refused, the file's is loaded next.
    #[test]
    fn an_edit_on_disk_wins_over_the_app() {
        let dir = std::env::temp_dir().join(format!("recouvrance-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rules.yaml");
        let agents = [agent("ci", Policy::default())];
        let written = export_over(&path, &agents, None).unwrap();
        let yours = format!("{written}# yours\n");
        std::fs::write(&path, &yours).unwrap();
        assert!(export_over(&path, &agents, Some(&written)).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), yours);
        // Once loaded, the app writes over it again.
        assert!(export_over(&path, &agents, Some(&yours)).is_ok());
        // A file deleted meanwhile is not brought back.
        std::fs::remove_file(&path).unwrap();
        assert!(export_over(&path, &agents, Some(&yours)).is_err());
        assert!(!path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
