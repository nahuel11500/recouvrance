//! `get_changes`: what changed on an object, from what the cluster keeps (see `crate::history`).
//! `recent_changes`: what changed in a cluster or a namespace, as the Timeline page shows it
//! (see `crate::timeline`).

use std::sync::Arc;

use http::request::Parts;
use k8s_openapi::api::core::v1::{Event, Secret};
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use super::{KubeMcp, access_for, get_named, require_ns, visible};
use crate::history::{self, Allowed, Change, Read, Source};
use crate::k8s::{self, ClusterEvent, Kind, ObjectRef};
use crate::mcp::{Access, Mode, Policy, Verb};
use crate::timeline::Record;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ChangesParams {
    pub context: String,
    #[schemars(description = "resource type like kubectl: deployment, pod, application, ...")]
    pub kind: String,
    pub name: String,
    #[schemars(description = "required for namespaced resources")]
    pub namespace: Option<String>,
    #[schemars(description = "the most changes returned, newest first (default 30, at most 200)")]
    pub limit: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RecentParams {
    #[schemars(description = "kubeconfig context name, from list_contexts")]
    pub context: String,
    #[schemars(description = "only this namespace (cluster-scoped objects left out); all you may read when omitted")]
    pub namespace: Option<String>,
    #[schemars(
        description = "how far back: a duration like 30m, 2h or 3d, or a time like 2026-11-22T02:40:00Z (default 1h)"
    )]
    pub since: Option<String>,
    #[schemars(description = "events mixed in, as on the Timeline page: \"warnings\" (default), \"all\" or \"none\"")]
    pub events: Option<String>,
    #[schemars(description = "add each change's changed lines (at most 12) when its record kept them (default false)")]
    pub diffs: Option<bool>,
    #[schemars(description = "the most rows returned, newest first (default 50, at most 200)")]
    pub limit: Option<u32>,
}

#[tool_router(router = history_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "What changed on an object, newest first, each with its source, what changed and a short diff: the revisions of the workload that rolls it out (a pod's: its owner's), of the Helm release that installed it (values masked), an Argo CD Application's deployments (of the Application that deployed the object too, read through Argo CD's API where your Allow rules let you get or list it where it deploys, and named with its sync and health), which field managers last wrote which fields and when (managedFields), its events, and the changes Recouvrance recorded while it watched the object (source Recorded: ConfigMaps, Services, scale changes... that the cluster keeps no history of; a Secret's values never, only which keys changed). Needs get on the object; a pod's owner needs get, the Helm revisions list on secrets, the events list on events.",
        annotations(read_only_hint = true)
    )]
    async fn get_changes(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ChangesParams>,
    ) -> Result<String, String> {
        let args =
            json!({"context": p.context, "kind": p.kind, "name": p.name, "namespace": p.namespace, "limit": p.limit});
        let mut call = self.begin(&parts, "get_changes", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        require_ns(&call, &kind, p.namespace.as_deref())?;
        let access = access_for(&kind, &p.context, Verb::Get, p.namespace.clone(), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        let ns = if kind.namespaced { p.namespace.clone() } else { None };
        // With its managedFields: who wrote it, and when.
        let value = match get_named(&client, &kind, ns.as_deref(), &p.name, true).await {
            Ok(v) => v,
            Err(e) => return call.finish(Err(e)),
        };
        let allowed = by_rules(call.policy().clone(), call.approved_rules().to_vec(), p.context.clone());
        let fetched = history::fetch(client.clone(), kind.clone(), ns.clone(), value.clone(), allowed).await;
        let mut errors = fetched.errors;
        let subject = format!("{} {}", kind.ar.kind, p.name);
        let mut lists = vec![fetched.changes, history::own_changes(&kind, &value)];
        // The deployments of the Application that deployed it, as the History tab shows them
        // (an Application's own are in `own_changes`).
        let managed = self.managed_by(&call, &p.context, &kind, &value).await;
        if let Some(app) = managed.as_ref().and_then(|m| m.app.as_ref())
            && !kind.is("argoproj.io", "Application")
        {
            lists.push(history::argo_changes(app));
        }
        // What the app recorded while it watched this object (this one: by its uid).
        crate::timeline::opened(&p.context).await;
        let uid = value.pointer("/metadata/uid").and_then(|v| v.as_str()).unwrap_or_default();
        lists.push(crate::timeline::changes_for(&p.context, &kind.kubectl_name(), ns.as_deref(), &p.name, uid));
        let events_kind = self.kind(&call, &p.context, "events").await?.1;
        // Events the rules keep it from reading are said, as a failure is: the history is incomplete.
        if call.may_see(&access_for(&events_kind, &p.context, Verb::List, ns.clone(), None)) {
            let uid = value.pointer("/metadata/uid").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let obj = ObjectRef { kind: kind.clone(), name: p.name.clone(), namespace: ns.clone(), uid };
            match k8s::events_for(client, obj).await {
                Ok(mut events) => {
                    // The check above has no name: an Allow of some event names passes it.
                    let total = events.len();
                    events.retain(|e| {
                        let a =
                            access_for(&events_kind, &p.context, Verb::List, e.namespace.clone(), Some(e.name.clone()));
                        call.may_see(&a)
                    });
                    if let Some(note) = events_hidden(total - events.len()) {
                        errors.push(note);
                    }
                    lists.push(history::event_changes(&events, &subject));
                }
                Err(e) => errors.push(format!("events: {}", k8s::short_error(&e))),
            }
        } else {
            errors.push(EVENTS_DENIED.to_string());
        }
        let changes = history::merge(lists);
        let limit = p.limit.unwrap_or(30).clamp(1, 200) as usize;
        let mut text = history::report(&subject, &changes, &errors, limit);
        if let Some(managed) = managed {
            text.push_str(&managed.text);
        }
        call.finish(Ok(text))
    }

    #[tool(
        description = "What changed lately in a cluster or a namespace, newest first, as Recouvrance's Timeline page shows it: the changes it recorded while its window watched the cluster (ConfigMaps, which keys of a Secret, Services, Ingresses, scale and spec changes, Argo CD Applications, custom resources...: who changed what, from managedFields), with the cluster's Warning events mixed in. Only what the window watched is recorded (`recouvrance serve` records nothing); the reply says how far back the records go. It never asks for approval: it shows what your Allow rules let you read (get on each object, list on events) and counts the rest. One object's whole history (rollout revisions, Helm, Argo CD deployments): get_changes.",
        annotations(read_only_hint = true)
    )]
    async fn recent_changes(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<RecentParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "since": p.since, "events": p.events,
            "diffs": p.diffs, "limit": p.limit});
        // Never asks: a record or an event is shown only where an Allow rule covers it, and
        // what an Ask rule covers is counted with the rest (`get_changes` asks, one object).
        let call = self.begin(&parts, "recent_changes", args)?;
        let now = jiff::Timestamp::now();
        let since = match parse_since(p.since.as_deref(), now) {
            Ok(t) => t,
            Err(e) => return Err(call.fail(e)),
        };
        let shown = match EventsShown::parse(p.events.as_deref()) {
            Ok(s) => s,
            Err(e) => return Err(call.fail(e)),
        };
        // The kubeconfig as it is now, as for every call: a context removed from it is refused.
        match k8s::load_contexts() {
            Ok(c) if c.names.contains(&p.context) => {}
            Ok(_) => return call.finish(Err(anyhow::anyhow!("context `{}` is not in the kubeconfig", p.context))),
            Err(e) => return call.finish(Err(e.context("reading the kubeconfig"))),
        }
        let ns = p.namespace.as_deref();
        // The records are a file the app keeps: no request, and they read while the cluster
        // doesn't answer.
        crate::timeline::opened(&p.context).await;
        let recorded = crate::timeline::since(&p.context, since);
        let (records, hidden_records) = readable_records(call.policy(), &p.context, ns, recorded);
        let diffs = p.diffs.unwrap_or(false);
        let record_rows: Vec<Change> = records.iter().map(|r| record_row(r, diffs)).collect();
        let (mut event_rows, mut hidden_events, mut events_note) = (Vec::new(), 0, None);
        if shown != EventsShown::Hidden {
            let access = access_for(&events_kind(), &p.context, Verb::List, p.namespace.clone(), None);
            if may_list(call.policy(), &access) {
                let read = match self.hub.client(&p.context).await {
                    Ok((client, _)) => k8s::recent_events(client, p.namespace.clone(), shown == EventsShown::All).await,
                    Err(e) => Err(e),
                };
                match read {
                    Ok(read) => {
                        // Only the newest are held, as on the Timeline page: said when some
                        // older than them would have been in the range asked.
                        events_note = events_cut(&read, since);
                        let events = read.rows.into_iter().map(Arc::unwrap_or_clone).collect();
                        let (events, hidden) = readable_events(call.policy(), &p.context, ns, since, events);
                        event_rows = events.iter().map(event_row).collect();
                        hidden_events = hidden;
                    }
                    // The records still come: a cluster that doesn't answer fails its events alone.
                    Err(e) => events_note = Some(format!("Could not read events: {}", k8s::short_error(&e))),
                }
            } else {
                events_note = Some(format!("Could not read {EVENTS_DENIED}"));
            }
        }
        let rows = history::merge([record_rows, event_rows]);
        let recording = crate::settings::with(|s| s.timeline.record);
        let oldest = crate::timeline::oldest(&p.context);
        let covers = coverage(oldest, recording, self.hub.headless(), shown != EventsShown::Hidden);
        let notes: Vec<String> =
            left_out(hidden_records, hidden_events).into_iter().chain(events_note).chain([covers]).collect();
        let limit = p.limit.unwrap_or(50).clamp(1, 200).min(call.policy().max_rows.max(1)) as usize;
        call.finish(Ok(recent_text(&heading(&p.context, ns, since, shown), &rows, limit, &notes)))
    }
}

/// Which events `recent_changes` mixes in: the Timeline page's choices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EventsShown {
    Warnings,
    All,
    Hidden,
}

impl EventsShown {
    fn parse(text: Option<&str>) -> Result<Self, String> {
        match text.map(|t| t.trim().to_lowercase()).as_deref() {
            None | Some("" | "warnings" | "warning") => Ok(Self::Warnings),
            Some("all") => Ok(Self::All),
            Some("none") => Ok(Self::Hidden),
            Some(other) => Err(format!("events {other:?}: \"warnings\", \"all\" or \"none\"")),
        }
    }
}

/// Since when `recent_changes` reads: `text` a duration back from `now`, as kubectl writes
/// ages (`30m`, `2h`, `3d`, `1h30m`), or a time (`2026-11-22T02:40:00Z`, a space for the `T`
/// as replies write it); an hour when not given.
fn parse_since(text: Option<&str>, now: jiff::Timestamp) -> Result<jiff::Timestamp, String> {
    let text = text.map(str::trim).filter(|t| !t.is_empty()).unwrap_or("1h");
    let since = if let Ok(t) = text.parse::<jiff::Timestamp>() {
        t
    } else if let Some(secs) = k8s::age_seconds(text) {
        // Further back than time goes: from the start.
        now.checked_sub(jiff::SignedDuration::from_secs(secs)).unwrap_or(jiff::Timestamp::MIN)
    } else {
        return Err(format!("since {text:?}: a duration like 30m, 2h or 3d, or a time like 2026-11-22T02:40:00Z"));
    };
    // A wrong year, more likely than a question about the future: said rather than "None found".
    if since > now {
        return Err(format!("since {text:?} is in the future"));
    }
    Ok(since)
}

/// Core events, as the rules name them (`events`), known without asking discovery.
fn events_kind() -> Kind {
    Kind { ar: k8s::ar_of::<Event>(), namespaced: true, section: "", label: "Event".into() }
}

/// Whether listing could show any row: some Allow rule covers some of them where `access`
/// asks (one namespace, or none for all of them). Rows are then checked one by one.
fn may_list(policy: &Policy, access: &Access) -> bool {
    let verdict = if access.namespace.is_some() { policy.decide(access) } else { policy.decide_any_namespace(access) };
    verdict.mode == Mode::Allow || policy.allow_somewhere(access).is_some()
}

/// What reading a record's object asks for: `get` on it, the very request `get_changes` makes
/// for it. Built from the record alone (its kubectl name says the group), without discovery,
/// so the records read while the cluster doesn't answer.
fn record_access(context: &str, r: &Record) -> Access {
    let (plural, group) = r.resource.split_once('.').unwrap_or((r.resource.as_str(), ""));
    let ar = kube::discovery::ApiResource {
        group: group.to_string(),
        version: String::new(),
        api_version: String::new(),
        kind: r.kind.clone(),
        plural: plural.to_string(),
    };
    let kind = Kind { ar, namespaced: r.namespace.is_some(), section: "", label: String::new() };
    access_for(&kind, context, Verb::Get, r.namespace.clone(), Some(r.name.clone()))
}

/// The records an agent may read among `records`: those of `namespace` (none: every one,
/// cluster-scoped objects too) where an Allow rule gives `get` on the object. Also how many
/// others of that namespace there were, to say so.
fn readable_records(
    policy: &Policy,
    context: &str,
    namespace: Option<&str>,
    records: Vec<Arc<Record>>,
) -> (Vec<Arc<Record>>, usize) {
    let in_scope = records.into_iter().filter(|r| namespace.is_none_or(|ns| r.namespace.as_deref() == Some(ns)));
    let (shown, hidden): (Vec<_>, Vec<_>) = in_scope.partition(|r| visible(policy, &[], &record_access(context, r)));
    (shown, hidden.len())
}

/// The events an agent may list among `events`: those last seen at `since` or later, about
/// objects of `namespace` (none: every one), where an Allow rule gives `list` on the event.
/// Also how many others there were in that scope, to say so.
fn readable_events(
    policy: &Policy,
    context: &str,
    namespace: Option<&str>,
    since: jiff::Timestamp,
    events: Vec<ClusterEvent>,
) -> (Vec<ClusterEvent>, usize) {
    let kind = events_kind();
    // Placed as the Timeline page places them: by their object's namespace.
    let in_scope = events
        .into_iter()
        .filter(|e| e.line.last >= Some(since) && namespace.is_none_or(|ns| e.object_namespace.as_deref() == Some(ns)));
    // A row stands for the events of one object with one reason: their names are the object's
    // and a suffix, which a rule's names treat alike, so the latest one's decides.
    let (shown, hidden): (Vec<_>, Vec<_>) = in_scope.partition(|e| {
        let a = access_for(&kind, context, Verb::List, e.line.namespace.clone(), Some(e.line.name.clone()));
        visible(policy, &[], &a)
    });
    (shown, hidden.len())
}

/// A record as a row: its object with its namespace, its diff only when asked (a record
/// keeps up to 64 KB a side, parsed then).
fn record_row(r: &Record, diffs: bool) -> Change {
    Change {
        time: Some(r.time),
        source: Source::Recorded,
        object: object_text(&r.kind, r.namespace.as_deref(), &r.name),
        summary: r.summary.clone(),
        warning: false,
        diff: if diffs { r.diff().map(Arc::new) } else { None },
    }
}

/// An event as a row, worded as on the Timeline page.
fn event_row(e: &ClusterEvent) -> Change {
    let namespace = e.object_namespace.as_deref().filter(|ns| !ns.is_empty());
    Change {
        time: e.line.last,
        source: Source::Event,
        object: object_text(&e.object_kind, namespace, &e.object_name),
        summary: e.summary(),
        warning: e.line.kind == "Warning",
        diff: None,
    }
}

/// `ConfigMap shop/web`, `Node n1`.
fn object_text(kind: &str, namespace: Option<&str>, name: &str) -> String {
    match namespace {
        Some(ns) => format!("{kind} {ns}/{name}"),
        None => format!("{kind} {name}"),
    }
}

/// The first line of `recent_changes`'s reply: what the rows are, where, since when.
fn heading(context: &str, namespace: Option<&str>, since: jiff::Timestamp, events: EventsShown) -> String {
    let what = match events {
        EventsShown::Warnings => "Changes recorded and Warning events",
        EventsShown::All => "Changes recorded and events",
        EventsShown::Hidden => "Changes recorded",
    };
    let place = match namespace {
        Some(ns) => format!("namespace {ns} of context {context}"),
        None => format!("context {context}"),
    };
    format!("{what} in {place} since {}, newest first:", history::when(since))
}

/// What the rules left out, counted: the agent must not take a short list for all there is.
fn left_out(changes: usize, events: usize) -> Option<String> {
    let mut parts = Vec::new();
    if changes > 0 {
        parts.push(format!("{} (get on the object)", crate::plural::count(changes, "change")));
    }
    if events > 0 {
        parts.push(format!("{} (list on events)", crate::plural::count(events, "event")));
    }
    (!parts.is_empty()).then(|| {
        format!(
            "Left out, not allowed by your rules: {}. This tool never asks for approval; get_changes on one \
             object does.",
            parts.join(", ")
        )
    })
}

/// What the records cover, so that "None found" doesn't read as "nothing changed": only what
/// the window watched, from when, and where the rest of an object's history is.
fn coverage(oldest: Option<jiff::Timestamp>, recording: bool, headless: bool, events: bool) -> String {
    let mut out = String::from(
        "Covers only what Recouvrance's window watched: the lists open, the Problems scan (not a kind of more than \
         1000 objects) and the kinds Settings → Timeline names; never pods, events or ReplicaSets.",
    );
    match oldest {
        Some(t) => out.push_str(&format!(" Records of this context go back to {}.", history::when(t))),
        None => out.push_str(" Nothing is recorded for this context."),
    }
    if !recording {
        out.push_str(" Recording is off (Settings → Timeline).");
    }
    if headless {
        out.push_str(" This server runs without the window (`recouvrance serve`): it records nothing itself.");
    }
    if events {
        out.push_str(" Events are what the cluster still keeps, an hour by default.");
    }
    out.push_str(" One object's whole history (rollout revisions, Helm, Argo CD deployments): get_changes.");
    out
}

/// `recent_changes`'s reply: the heading, the rows (newest first, at most `limit`), then the
/// notes (what was left out, what could not be read, what the records cover).
fn recent_text(heading: &str, rows: &[Change], limit: usize, notes: &[String]) -> String {
    let mut out = format!("{heading}\n");
    if rows.is_empty() {
        out.push_str("None found.\n");
    }
    out.push_str(&history::change_lines(rows, limit));
    for n in notes {
        out.push_str(n);
        out.push('\n');
    }
    out
}

/// What the events read leave out of the range asked, when the cluster keeps more than are
/// held: older ones of the range may be missing. None when they reach back far enough.
fn events_cut(read: &k8s::EventRows, since: jiff::Timestamp) -> Option<String> {
    let oldest = read.since?;
    (read.kept < read.total && oldest > since).then(|| {
        format!(
            "Events: only the newest {} of the {} the cluster keeps were read, back to {}; older ones are left out.",
            read.kept,
            read.total,
            history::when(oldest)
        )
    })
}

/// The events left out when the rules allow listing none of them.
const EVENTS_DENIED: &str = "events: not allowed by your rules (list on events)";

/// The note on `hidden` events the rules don't allow listing, when there are any.
fn events_hidden(hidden: usize) -> Option<String> {
    (hidden > 0).then(|| {
        format!("{hidden} event{}: not allowed by your rules (list on events)", if hidden == 1 { "" } else { "s" })
    })
}

/// What `history::fetch` may read for an agent: an owner it may get, Helm's Secrets it may list.
fn by_rules(policy: Policy, approved: Vec<usize>, context: String) -> Allowed {
    let secrets = Kind { ar: k8s::ar_of::<Secret>(), namespaced: true, section: "", label: "Secret".into() };
    Arc::new(move |read: Read<'_>| match read {
        Read::Get { kind, ns, name } => {
            visible(&policy, &approved, &access_for(kind, &context, Verb::Get, Some(ns.into()), Some(name.into())))
        }
        // Listing them at all: some rule must allow some of them (each is then checked).
        Read::Helm { ns, secret: None } => {
            let a = access_for(&secrets, &context, Verb::List, Some(ns.into()), None);
            visible(&policy, &approved, &a) || policy.allow_somewhere(&a).is_some()
        }
        Read::Helm { ns, secret: Some(name) } => {
            visible(&policy, &approved, &access_for(&secrets, &context, Verb::List, Some(ns.into()), Some(name.into())))
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use proptest::prelude::*;
    use serde_json::json;

    use super::{
        EVENTS_DENIED, EventsShown, access_for, coverage, event_row, events_cut, events_hidden, events_kind, heading,
        left_out, may_list, parse_since, readable_events, readable_records, recent_text, record_row,
    };
    use crate::history;
    use crate::k8s::{ClusterEvent, EventLine, EventRows, Kind};
    use crate::mcp::policy::properties::{policy as any_policy, value};
    use crate::mcp::{Mode, Policy, Verb, rules_file};
    use crate::timeline::{Record, What};

    /// What the rules left out is named in the reply, not dropped without a word.
    #[test]
    fn events_left_out_are_named() {
        assert_eq!(events_hidden(0), None);
        assert_eq!(events_hidden(1).as_deref(), Some("1 event: not allowed by your rules (list on events)"));
        let out = history::report("Deployment web", &[], &[EVENTS_DENIED.into(), events_hidden(3).unwrap()], 30);
        assert!(out.contains("Could not read events: not allowed by your rules (list on events)\n"), "{out}");
        assert!(out.contains("Could not read 3 events: not allowed by your rules"), "{out}");
    }

    fn at(secs: i64) -> jiff::Timestamp {
        jiff::Timestamp::from_second(1_700_000_000 + secs).unwrap()
    }

    /// Fewer events read than the cluster keeps: the reply says so when older ones of the
    /// range asked may be missing, not when those read reach back before it.
    #[test]
    fn events_cut_short_are_said() {
        let read = |kept, total, oldest| EventRows { rows: Vec::new(), kept, total, since: Some(at(oldest)) };
        assert_eq!(events_cut(&read(5, 5, 100), at(0)), None, "all of them");
        assert_eq!(events_cut(&read(5, 9, -10), at(0)), None, "they reach back before the range");
        let note = events_cut(&read(5, 9, 100), at(0)).unwrap();
        assert!(note.starts_with("Events: only the newest 5 of the 9 the cluster keeps were read, back to "), "{note}");
        assert!(note.ends_with("; older ones are left out."), "{note}");
    }

    fn record(resource: &str, kind: &str, namespace: Option<&str>, name: &str, secs: i64) -> Arc<Record> {
        Arc::new(Record {
            time: at(secs),
            resource: resource.into(),
            kind: kind.into(),
            namespace: namespace.map(String::from),
            name: name.into(),
            uid: format!("{name}-uid"),
            what: What::Updated,
            summary: "changed data.k by kubectl-edit".into(),
            before: None,
            after: None,
        })
    }

    /// A Warning event about pod `pod` of `namespace`, last seen `secs` after `at(0)`.
    fn event(namespace: &str, pod: &str, secs: i64) -> ClusterEvent {
        ClusterEvent {
            line: EventLine {
                namespace: Some(namespace.into()),
                name: format!("{pod}.187a2b3c4d5e6f70"),
                age: String::new(),
                first: None,
                last: Some(at(secs)),
                kind: "Warning".into(),
                reason: "BackOff".into(),
                message: "restarting".into(),
                count: 3,
                source: String::new(),
            },
            object_kind: "Pod".into(),
            object_namespace: Some(namespace.into()),
            object_name: pod.into(),
            events: 1,
        }
    }

    /// Rules as rules.yaml writes them, on every context.
    fn rules(rules: &[(Mode, &str)]) -> Policy {
        let rules = rules.iter().map(|(effect, r)| rules_file::parse_rule(*effect, r).unwrap()).collect();
        Policy { rules, ..Policy::default() }
    }

    /// A record shows only where the rules let the agent `get` its object, and the others of
    /// the namespace asked are counted; a namespace asked leaves out cluster-scoped objects.
    #[test]
    fn a_record_shows_only_where_get_on_its_object_is_allowed() {
        let all = || {
            vec![
                record("configmaps", "ConfigMap", Some("shop"), "web", 0),
                record("secrets", "Secret", Some("shop"), "db", 1),
                record("deployments.apps", "Deployment", Some("team-b"), "api", 2),
                record("nodes", "Node", None, "n1", 3),
            ]
        };
        let read = |p: &Policy, ns: Option<&str>| {
            let (shown, hidden) = readable_records(p, "prod", ns, all());
            (shown.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(" "), hidden)
        };
        // Everything but Secrets: the Secret is counted, not shown.
        let everything = rules(&[(Mode::Allow, "get,list *"), (Mode::Deny, "* secrets")]);
        assert_eq!(read(&everything, None), ("web api n1".to_string(), 1));
        // One namespace: its records only, without the Node, and nothing counted from elsewhere.
        assert_eq!(read(&everything, Some("shop")), ("web".to_string(), 1));
        assert_eq!(read(&everything, Some("team-b")), ("api".to_string(), 0));
        // An Allow in one namespace: the other namespace's and the Node's records are counted.
        assert_eq!(read(&rules(&[(Mode::Allow, "get,list * -n shop")]), None), ("web db".to_string(), 2));
        // `list` is not `get`: a record tells the object's history.
        assert_eq!(read(&rules(&[(Mode::Allow, "list *")]), None), (String::new(), 4));
        // What an Ask rule covers is counted too: this tool never asks.
        let ask = rules(&[(Mode::Allow, "get,list *"), (Mode::Ask, "get deployments -n team-b")]);
        assert_eq!(read(&ask, None), ("web db n1".to_string(), 1));
        // A Deny of one name, the kind named with its group.
        let deny = rules(&[(Mode::Allow, "get *"), (Mode::Deny, "get deployments.apps/api")]);
        assert_eq!(read(&deny, None), ("web db n1".to_string(), 1));
        assert_eq!(read(&rules(&[]), None), (String::new(), 4));
    }

    fn kind(group: &str, kind: &str, plural: &str, namespaced: bool) -> Kind {
        let gvk = kube::api::GroupVersionKind::gvk(group, "v1", kind);
        let ar = kube::discovery::ApiResource::from_gvk_with_plural(&gvk, plural);
        Kind { ar, namespaced, section: "", label: kind.into() }
    }

    /// Kinds the generated rules' patterns (`a`, `a*`, `team-*`...) match or not: one plural in
    /// two groups, and a cluster-scoped kind.
    fn any_kind() -> impl Strategy<Value = Kind> {
        prop_oneof![
            Just(kind("", "A", "a", true)),
            Just(kind("other.io", "A", "a", true)),
            Just(kind("", "Team", "team-a", true)),
            Just(kind("", "B", "b", false)),
        ]
    }

    proptest! {
        /// Whatever the rules, a record shows only where an Allow rule gives `get` on its
        /// object, the very request `get_changes` makes for it (its kind and group, namespace
        /// and name), and only in the namespace asked; every other one there is counted.
        #[test]
        fn a_record_shows_only_where_get_changes_may_read_its_object(
            policy in any_policy(),
            objects in prop::collection::vec((any_kind(), value(), value()), 0..8),
            namespace in prop::option::of(value()),
        ) {
            let records: Vec<Arc<Record>> = objects
                .iter()
                .map(|(k, ns, name)| {
                    let ns = k.namespaced.then_some(ns.as_str());
                    record(&k.kubectl_name(), &k.ar.kind, ns, name, 0)
                })
                .collect();
            let (shown, hidden) = readable_records(&policy, "a", namespace.as_deref(), records.clone());
            let mut counted = 0;
            for ((k, ns, name), r) in objects.iter().zip(&records) {
                let in_scope = namespace.is_none() || r.namespace == namespace;
                let get = access_for(k, "a", Verb::Get, Some(ns.clone()), Some(name.clone()));
                let allowed = policy.decide(&get).mode == Mode::Allow;
                // Positional: `prop_assert_eq!` builds its message with `concat!`.
                prop_assert_eq!(shown.iter().any(|s| Arc::ptr_eq(s, r)), in_scope && allowed, "{:?}", get);
                counted += usize::from(in_scope && !allowed);
            }
            prop_assert_eq!(hidden, counted);
        }

        /// Whatever an agent types as `since`, it reads or says why, without panicking. A
        /// duration is that long before now (or as far back as time goes); one too long to
        /// count in seconds is refused, never wrapped around.
        #[test]
        fn since_reads_any_text(text in any::<String>(), n in 1i64.., unit in 0usize..5) {
            let now = jiff::Timestamp::now();
            let _ = parse_since(Some(&text), now);
            let (letter, secs) = [("s", 1i64), ("m", 60), ("h", 3600), ("d", 86_400), ("y", 31_536_000)][unit];
            let typed = parse_since(Some(&format!("{n}{letter}")), now);
            match n.checked_mul(secs) {
                Some(s) => {
                    let back = now.checked_sub(jiff::SignedDuration::from_secs(s)).unwrap_or(jiff::Timestamp::MIN);
                    prop_assert_eq!(typed, Ok(back));
                }
                None => prop_assert!(typed.is_err()),
            }
        }
    }

    #[test]
    fn since_is_a_duration_back_or_a_time() {
        let now = at(100_000);
        assert_eq!(parse_since(None, now), Ok(at(100_000 - 3600)), "an hour by default");
        assert_eq!(parse_since(Some(" "), now), Ok(at(100_000 - 3600)));
        assert_eq!(parse_since(Some("30m"), now), Ok(at(100_000 - 1800)));
        assert_eq!(parse_since(Some("1h30m"), now), Ok(at(100_000 - 5400)));
        assert_eq!(parse_since(Some("2023-11-14T22:13:20Z"), now), Ok(at(0)));
        assert_eq!(parse_since(Some("2023-11-15T00:13:20+02:00"), now), Ok(at(0)));
        // As replies write times: a space for the T.
        assert_eq!(parse_since(Some("2023-11-14 22:13:20Z"), now), Ok(at(0)));
        assert_eq!(parse_since(Some("9999999999y"), now), Ok(jiff::Timestamp::MIN), "further back than time goes");
        let err = parse_since(Some("yesterday"), now).unwrap_err();
        assert!(err.contains("a duration like 30m, 2h or 3d"), "{err}");
        assert!(parse_since(Some("2023-11-14T22:13:20"), now).is_err(), "a time without its offset is not guessed");
        assert!(parse_since(Some("999999999999y"), now).is_err(), "too long to count in seconds");
        let err = parse_since(Some("2030-01-01T00:00:00Z"), now).unwrap_err();
        assert!(err.contains("in the future"), "{err}");
    }

    #[test]
    fn events_are_the_page_s_choices() {
        assert_eq!(EventsShown::parse(None), Ok(EventsShown::Warnings));
        assert_eq!(EventsShown::parse(Some(" All ")), Ok(EventsShown::All));
        assert_eq!(EventsShown::parse(Some("none")), Ok(EventsShown::Hidden));
        assert!(EventsShown::parse(Some("normal")).unwrap_err().contains("\"warnings\", \"all\" or \"none\""));
    }

    /// The events are listed only when some of them could show: never on an Ask rule alone,
    /// nor with rules for other kinds, and in one namespace only where an Allow covers it.
    #[test]
    fn events_are_listed_only_when_an_allow_rule_could_show_some() {
        let access = |ns: Option<&str>| access_for(&events_kind(), "prod", Verb::List, ns.map(String::from), None);
        let shop = rules(&[(Mode::Allow, "list events -n shop")]);
        assert!(may_list(&shop, &access(None)));
        assert!(may_list(&shop, &access(Some("shop"))));
        assert!(!may_list(&shop, &access(Some("team-b"))));
        assert!(!may_list(&rules(&[(Mode::Allow, "get,list configmaps")]), &access(None)));
        assert!(!may_list(&rules(&[(Mode::Ask, "list events")]), &access(None)));
        let some = rules(&[(Mode::Ask, "list events -n prod-*"), (Mode::Allow, "list events -n shop")]);
        assert!(may_list(&some, &access(None)));
        // Some names only: listed, then filtered by name.
        assert!(may_list(&rules(&[(Mode::Allow, "list events/web-* -n shop")]), &access(Some("shop"))));
    }

    /// The events shown are those since the time asked, about objects of the namespace asked,
    /// that the rules let the agent list; the others of that scope are counted.
    #[test]
    fn events_since_the_time_in_the_namespace_that_the_rules_let_it_list() {
        let all = || vec![event("shop", "web-1", 20), event("shop", "web-2", -10), event("team-b", "api-1", 30)];
        let read = |p: &Policy, ns: Option<&str>| {
            let (shown, hidden) = readable_events(p, "prod", ns, at(0), all());
            (shown.iter().map(|e| e.object_name.clone()).collect::<Vec<_>>(), hidden)
        };
        let shop = rules(&[(Mode::Allow, "list events -n shop")]);
        // web-2 was last seen before `since`: neither shown nor counted.
        assert_eq!(read(&shop, None), (vec!["web-1".to_string()], 1));
        assert_eq!(read(&shop, Some("shop")), (vec!["web-1".to_string()], 0));
        assert_eq!(read(&shop, Some("team-b")), (vec![], 1));
        // An event about a cluster-scoped object, kept in `default`, is no namespace's.
        let mut node = event("default", "n1", 20);
        node.object_kind = "Node".into();
        node.object_namespace = None;
        let everything = rules(&[(Mode::Allow, "list events")]);
        assert!(readable_events(&everything, "prod", Some("default"), at(0), vec![node]).0.is_empty());
    }

    /// The reply: changes and events in one list, newest first, at most `limit`; then what the
    /// rules left out and what the records cover.
    #[test]
    fn changes_and_events_come_newest_first_then_what_was_left_out() {
        let records =
            [record("configmaps", "ConfigMap", Some("shop"), "web", 10), record("nodes", "Node", None, "n1", 30)];
        let rows = history::merge([
            records.iter().map(|r| record_row(r, false)).collect(),
            vec![event_row(&event("shop", "web-1", 20))],
        ]);
        let notes = [left_out(2, 1).unwrap(), coverage(Some(at(-60)), true, false, true)];
        let text = recent_text(&heading("prod", None, at(0), EventsShown::Warnings), &rows, 2, &notes);
        let lines: Vec<&str> = text.lines().collect();
        let first = "Changes recorded and Warning events in context prod since 2023-11-14 22:13:20Z (";
        assert!(lines[0].starts_with(first), "{text}");
        assert!(lines[1].starts_with("2023-11-14 22:13:50Z ("), "{text}");
        assert!(lines[1].ends_with(")  Recorded  Node n1: changed data.k by kubectl-edit"), "{text}");
        assert!(lines[2].ends_with(")  Event  Pod shop/web-1: Warning BackOff: restarting (×3)"), "{text}");
        assert_eq!(lines[3], "… 1 older change not shown (raise `limit`)");
        assert_eq!(
            lines[4],
            "Left out, not allowed by your rules: 2 changes (get on the object), 1 event (list on events). \
             This tool never asks for approval; get_changes on one object does."
        );
        assert!(lines[5].starts_with("Covers only what Recouvrance's window watched"), "{text}");
        assert!(lines[5].contains("Records of this context go back to 2023-11-14 22:12:20Z ("), "{text}");
        assert!(lines[5].contains("Events are what the cluster still keeps"), "{text}");
        assert_eq!(lines.len(), 6, "{text}");
    }

    /// "None found" comes with what the records cover: nothing recorded, recording off, a
    /// server without the window.
    #[test]
    fn an_empty_reply_says_what_the_records_cover() {
        let covers = coverage(None, false, true, false);
        let text = recent_text(&heading("prod", Some("shop"), at(0), EventsShown::Hidden), &[], 50, &[covers]);
        assert!(text.starts_with("Changes recorded in namespace shop of context prod since "), "{text}");
        assert!(text.contains("\nNone found.\n"), "{text}");
        for said in
            ["Nothing is recorded for this context.", "Recording is off", "records nothing itself", "get_changes"]
        {
            assert!(text.contains(said), "{said}: {text}");
        }
        assert!(!text.contains("Events are"), "no events asked for: {text}");
    }

    /// A change's diff comes only when asked for, worked out then.
    #[test]
    fn diffs_come_when_asked_for() {
        let mut r = Arc::unwrap_or_clone(record("configmaps", "ConfigMap", Some("shop"), "web", 0));
        r.before = Some(serde_json::value::to_raw_value(&json!({"data": {"level": "info"}})).unwrap());
        r.after = Some(serde_json::value::to_raw_value(&json!({"data": {"level": "debug"}})).unwrap());
        assert!(record_row(&r, false).diff.is_none());
        let text = recent_text("Changes", &[record_row(&r, true)], 50, &[]);
        assert!(text.contains("    - ") && text.contains("level: info"), "{text}");
        assert!(text.contains("    + ") && text.contains("level: debug"), "{text}");
    }
}
