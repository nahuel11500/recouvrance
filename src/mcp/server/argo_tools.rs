//! Argo CD for agents, from the hub to the destination: `argocd_apps` lists the Applications
//! of the hubs an agent may read, each with the kubeconfig context it deploys to, and
//! `describe_resource` and `get_changes` name the Application that deployed an object
//! (`KubeMcp::managed_by`).
//!
//! The hubs are those the user picked in Settings, else those the app or the agents' calls
//! found serving Applications, and only those the agent's rules name: no other context is
//! ever asked whether it runs Argo CD.

use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Call, KubeMcp, access_for, cap_chars, format_table, get_named};
use crate::k8s::argo::{self, Destination, Summary, TrackedBy};
use crate::k8s::{self, ContextTarget, Kind};
use crate::mcp::{Access, Mode, Policy, Verb};
use crate::status::Tone;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ArgoAppsParams {
    #[schemars(
        description = "the context Argo CD runs in (its hub), from list_contexts; omit for every hub known that your rules name"
    )]
    pub context: Option<String>,
    #[schemars(
        description = "only those that need a look: a failed sync, an error, Degraded or Missing, a sync running, or out of sync with auto-sync on (out of sync with auto-sync off is how an Application synced by hand waits)"
    )]
    pub problems_only: Option<bool>,
    #[schemars(description = "only those out of sync, auto-sync on or off")]
    pub out_of_sync: Option<bool>,
    #[schemars(description = "only this Argo CD project")]
    pub project: Option<String>,
    #[schemars(description = "only those deploying to this kubeconfig context")]
    pub destination: Option<String>,
    #[schemars(description = "only those deploying to this namespace")]
    pub dest_namespace: Option<String>,
}

/// Said when no hub can be read.
const NO_HUB: &str = "No Argo CD hub is known that your rules name. Pass `context` with the context Argo CD runs \
     in (list_contexts), or the user can pick it in Recouvrance (Settings → Contexts).";

/// Longest message kept from Argo CD in a reply line.
const MAX_MESSAGE_CHARS: usize = 300;

/// The longest wait for a hub's Applications once connected (a list of thousands is slow).
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[tool_router(router = argo_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "Argo CD Applications across hubs, like the Argo CD page: for each, its hub (the context Argo CD runs in), project, sync, health, auto-sync, last operation, revision, where it deploys (the kubeconfig context that reaches its destination, and whether your rules open it) and the Application that made it (app of apps); those needing a look first, with Argo CD's message. Out of sync with auto-sync off is not a problem: it is how an Application synced by hand waits. The hubs are the one in `context`, else those the user picked in Settings or Recouvrance found, among the contexts your rules name. Needs list on applications.argoproj.io on the hub; without `context` it never asks for approval and leaves out the hubs and rows only an Ask rule covers. Then: describe_resource or get_changes on the destination, gitops on the Application (on its hub), wait_for its sync or health.",
        annotations(read_only_hint = true)
    )]
    async fn argocd_apps(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ArgoAppsParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "problems_only": p.problems_only, "out_of_sync": p.out_of_sync,
            "project": p.project, "destination": p.destination, "dest_namespace": p.dest_namespace});
        let mut call = self.begin(&parts, "argocd_apps", args)?;
        let kind = argo::application_kind();
        let mut notes = Vec::new();
        let hubs = if let Some(hub) = &p.context {
            // Named: an Ask rule waits for the user, as for list_resources.
            let access = access_for(&kind, hub, Verb::List, None, None);
            self.authorize_list(&mut call, &access, true).await?;
            vec![hub.clone()]
        } else {
            let mut readable = Vec::new();
            for hub in self.agent_hubs(call.policy()) {
                let access = access_for(&kind, &hub, Verb::List, None, None);
                match list_reach(call.policy(), &access) {
                    Mode::Allow => readable.push(hub),
                    Mode::Ask => notes.push(format!(
                        "{hub}: listing applications.argoproj.io needs the user's approval: pass `context: {hub}` to ask"
                    )),
                    Mode::Deny => notes.push(format!("{hub}: your rules don't let you list applications.argoproj.io")),
                }
            }
            readable
        };
        if hubs.is_empty() && notes.is_empty() {
            return call.finish(Ok(NO_HUB.into()));
        }
        // One list per hub, kept 30 s and shared with the window's Argo CD page.
        let reads = futures::future::join_all(hubs.iter().map(|hub| self.hub_applications(hub))).await;
        let targets = argo::targets();
        let (mut rows, mut hidden) = (Vec::new(), 0);
        for (hub, read) in hubs.iter().zip(reads) {
            let list = match read {
                Ok(Some(list)) => list,
                Ok(None) => {
                    notes.push(format!("{hub}: serves no Argo CD Applications"));
                    continue;
                }
                // One hub that can't be read doesn't hide the others.
                Err(e) => {
                    notes.push(format!("{hub}: could not be read: {}", k8s::short_error(&e)));
                    continue;
                }
            };
            for row in hub_rows(hub, &list, &targets).into_iter().filter(|r| keeps(&p, r)) {
                let a = access_for(&kind, hub, Verb::List, Some(row.app.namespace.clone()), Some(row.app.name.clone()));
                if call.may_see(&a) {
                    rows.push(row);
                } else {
                    hidden += 1;
                }
            }
        }
        let max = call.policy().max_rows.max(1) as usize;
        let text = apps_text(rows, |c| call.policy().context_allowed(c), hidden, &notes, max);
        call.finish(Ok(text))
    }
}

impl KubeMcp {
    /// The hubs an agent's Argo CD reads go to: those the user picked in Settings, else those
    /// found serving Applications (by the window's pages, or by an agent's call to them),
    /// among the contexts its rules name. No request.
    fn agent_hubs(&self, policy: &Policy) -> Vec<String> {
        let chosen = crate::settings::with(|s| s.argocd_contexts.clone());
        let mut found = argo::known_hubs();
        found.extend(self.hub.contexts_serving("argoproj.io", "Application"));
        hub_candidates(&chosen, found, |c| policy.context_allowed(c))
    }

    /// A hub's Applications, None when it serves none.
    async fn hub_applications(&self, hub: &str) -> anyhow::Result<Option<std::sync::Arc<Vec<Value>>>> {
        let (client, kinds) = self.hub.client(hub).await?;
        if !kinds.iter().any(|k| k.is("argoproj.io", "Application")) {
            return Ok(None);
        }
        argo::applications(&argo::Hub { context: hub.to_string(), client }).await.map(Some)
    }

    /// The Application that deployed `obj` (read on the context `here`), as `describe_resource`
    /// and `get_changes` say it: by the object's tracking id or instance label, read on the
    /// agent's hubs where an Allow rule lets it. None for an object that names no Application:
    /// nothing is asked of any hub then.
    pub(super) async fn managed_by(&self, call: &Call, here: &str, kind: &Kind, obj: &Value) -> Option<Managed> {
        let by = argo::tracked_by(&kind.ar.group, &kind.ar.kind, obj)?;
        let key = ObjectKey::of(kind, obj);
        let hubs = self.agent_hubs(call.policy());
        let targets = argo::targets();
        let here = argo::target(&targets, here);
        // All hubs at once, each bounded in time: one that is down doesn't hold the others.
        let reads = futures::future::join_all(hubs.iter().map(|hub| self.read_hub(call.policy(), hub, &by))).await;
        let mut notes = Vec::new();
        let mut found = Vec::new();
        for (hub, read) in hubs.iter().zip(reads) {
            let apps = match read {
                None => continue,
                Some(Ok(apps)) => apps,
                Some(Err(note)) => {
                    notes.push(note);
                    continue;
                }
            };
            let hub_target = argo::target(&targets, hub);
            for app in apps {
                let here = argo::targets_cluster(&app, &hub_target, &here);
                let tracks = argo::resource_of(&app, &key.group, &key.kind, &key.namespace, &key.name).is_some();
                found.push(Candidate { hub: hub.clone(), app, here, tracks });
            }
        }
        let picked = pick(&by, found);
        let text = managed_text(&by, &picked, &key, &hubs, &notes)?;
        let app = match picked {
            Pick::One(_, app) => Some(app),
            _ => None,
        };
        Some(Managed { text, app })
    }

    /// What one hub says of `by`: None when it is not asked (a label alone never connects to a
    /// hub, nor asks one that failed recently), Err with a line saying why it said nothing.
    async fn read_hub(&self, policy: &Policy, hub: &str, by: &TrackedBy) -> Option<Result<Vec<Value>, String>> {
        let ready = self.hub.ready(hub);
        // A hub connected for agents is judged by the agents' reads alone: the window's probe
        // may have failed for its own reasons.
        let failed = argo::recent_failure(hub, !ready, std::time::Instant::now());
        match plan(by, ready, failed) {
            Plan::Quiet => None,
            Plan::Skip(why) => {
                Some(Err(format!("{hub}: not asked again yet, it failed less than 5 minutes ago: {why}")))
            }
            Plan::Read => Some(self.manager_on(policy, hub, by).await),
        }
    }

    /// The Applications on `hub` that `by` may name, as far as the agent's Allow rules let it
    /// read them; Err with a line saying why the hub said nothing. The connection and the read
    /// are each bounded in time; a hub that can't be reached is not asked again for a while.
    async fn manager_on(&self, policy: &Policy, hub: &str, by: &TrackedBy) -> Result<Vec<Value>, String> {
        let unreached = |why: String| {
            argo::note_unreached(hub, why.clone());
            format!("{hub}: {why}")
        };
        let (client, kinds) = match tokio::time::timeout(argo::PROBE_TIMEOUT, self.hub.client(hub)).await {
            Ok(Ok(connected)) => connected,
            Ok(Err(e)) => return Err(unreached(k8s::short_error(&e))),
            Err(_) => return Err(unreached("did not answer".into())),
        };
        argo::note_reached(hub);
        if !kinds.iter().any(|k| k.is("argoproj.io", "Application")) {
            return Ok(Vec::new());
        }
        match tokio::time::timeout(READ_TIMEOUT, managers_on(policy, hub, client, by)).await {
            Ok(read) => read,
            Err(_) => Err(unreached("did not answer".into())),
        }
    }
}

/// `manager_on`'s reads, once connected to the hub.
async fn managers_on(policy: &Policy, hub: &str, client: kube::Client, by: &TrackedBy) -> Result<Vec<Value>, String> {
    let kind = argo::application_kind();
    let allowed = |verb: Verb, ns: &str, name: &str| {
        policy.decide(&access_for(&kind, hub, verb, Some(ns.into()), Some(name.into()))).mode == Mode::Allow
    };
    let name = match by {
        TrackedBy::Id(t) => {
            let ns = t.app_ns.as_deref().unwrap_or(argo::DEFAULT_NAMESPACE);
            if allowed(Verb::Get, ns, &t.app) {
                match get_named(&client, &kind, Some(ns), &t.app, false).await {
                    Ok(app) => return Ok(vec![app]),
                    // Argo CD may keep its Applications in another namespace: list them.
                    Err(e) if t.app_ns.is_none() && matches!(k8s::api_status(&e), Some(403 | 404)) => {}
                    Err(e) if k8s::is_not_found(&e) => return Ok(Vec::new()),
                    Err(e) => return Err(format!("{hub}: {}", k8s::short_error(&e))),
                }
            } else if t.app_ns.is_some() {
                return Err(format!("{hub}: your rules don't let you read Application {ns}/{}", t.app));
            }
            t.app.as_str()
        }
        TrackedBy::Label(name) => name.as_str(),
    };
    let list_access = access_for(&kind, hub, Verb::List, None, None);
    if list_reach(policy, &list_access) != Mode::Allow {
        return Err(format!("{hub}: your rules don't let you list applications.argoproj.io"));
    }
    let list = argo::applications(&argo::Hub { context: hub.to_string(), client })
        .await
        .map_err(|e| format!("{hub}: {}", k8s::short_error(&e)))?;
    Ok(list
        .iter()
        .filter(|a| a.pointer("/metadata/name").and_then(Value::as_str) == Some(name))
        .filter(|a| {
            let ns = a.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default();
            allowed(Verb::List, ns, name)
        })
        .cloned()
        .collect())
}

/// What `managed_by` found: the line to say, and the Application when it was read.
pub(super) struct Managed {
    pub text: String,
    pub app: Option<Value>,
}

/// Whether `managed_by` asks a hub.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Read,
    /// Not asked, and said why.
    Skip(String),
    /// Not asked, and nothing said.
    Quiet,
}

/// A hub that failed less than `RETRY` ago is not asked again: said for a tracking id, quiet
/// for a label. A label alone reads only hubs already connected: Helm sets the same label, so
/// it never justifies a connection or a login.
fn plan(by: &TrackedBy, ready: bool, failed: Option<String>) -> Plan {
    match (by, failed) {
        (TrackedBy::Id(_), Some(why)) => Plan::Skip(why),
        (TrackedBy::Label(_), Some(_)) => Plan::Quiet,
        (TrackedBy::Label(_), None) if !ready => Plan::Quiet,
        _ => Plan::Read,
    }
}

/// An Application a hub named for the object.
struct Candidate {
    hub: String,
    app: Value,
    /// It deploys to the object's cluster.
    here: bool,
    /// Its `status.resources` lists the object.
    tracks: bool,
}

/// Which Application deployed the object.
enum Pick {
    One(String, Value),
    /// Several fit as well: the hub and name of each.
    Several(Vec<String>),
    /// Found by name, but none deploys to this cluster or tracks the object: the hub and
    /// name of each.
    Elsewhere(Vec<String>),
    NotFound,
}

/// The Application that deployed the object among those named for it. A tracking id names an
/// Application by name only, and another hub may have one of the same name deploying
/// elsewhere: it must deploy to this cluster or track the object. A label is only a hint: it
/// must do both. One that does both wins over one that does one.
fn pick(by: &TrackedBy, found: Vec<Candidate>) -> Pick {
    let name = |c: &Candidate| {
        let field = |p: &str| c.app.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string();
        format!("{} {}/{}", c.hub, field("/metadata/namespace"), field("/metadata/name"))
    };
    let fits = |c: &Candidate| match by {
        TrackedBy::Id(_) => c.here || c.tracks,
        TrackedBy::Label(_) => c.here && c.tracks,
    };
    let (mut fit, rest): (Vec<Candidate>, Vec<Candidate>) = found.into_iter().partition(fits);
    let rank = |c: &Candidate| u8::from(c.here) + u8::from(c.tracks);
    let Some(best) = fit.iter().map(rank).max() else {
        return if rest.is_empty() { Pick::NotFound } else { Pick::Elsewhere(rest.iter().map(name).collect()) };
    };
    fit.retain(|c| rank(c) == best);
    if fit.len() == 1 {
        let c = fit.remove(0);
        return Pick::One(c.hub, c.app);
    }
    Pick::Several(fit.iter().map(name).collect())
}

/// The object an Application may track, as its `status.resources` names it.
struct ObjectKey {
    group: String,
    kind: String,
    namespace: String,
    name: String,
}

impl ObjectKey {
    fn of(kind: &Kind, obj: &Value) -> Self {
        let meta = |field: &str| obj.pointer(field).and_then(Value::as_str).unwrap_or_default().to_string();
        Self {
            group: kind.ar.group.clone(),
            kind: kind.ar.kind.clone(),
            namespace: meta("/metadata/namespace"),
            name: meta("/metadata/name"),
        }
    }
}

/// Whether a list could show rows without asking: an Allow rule decides some of them. Ask
/// when only an Ask rule could give some.
fn list_reach(policy: &Policy, access: &Access) -> Mode {
    let mode = policy.decide_any_namespace(access).mode;
    if mode == Mode::Ask && policy.allow_somewhere(access).is_some() { Mode::Allow } else { mode }
}

/// The hubs to read: those the user picked, else those found, among the contexts allowed.
fn hub_candidates(chosen: &[String], found: Vec<String>, allowed: impl Fn(&str) -> bool) -> Vec<String> {
    let mut hubs = if chosen.is_empty() { found } else { chosen.to_vec() };
    hubs.retain(|c| allowed(c));
    hubs.sort();
    hubs.dedup();
    hubs
}

/// One Application as `argocd_apps` shows it.
struct AppRow {
    hub: String,
    app: Summary,
    dest: Destination,
    /// The Application that made it (app of apps), on the same hub.
    parent: Option<String>,
}

/// A hub's Applications, each with where it deploys among the kubeconfig's contexts.
fn hub_rows(hub: &str, list: &[Value], targets: &[ContextTarget]) -> Vec<AppRow> {
    let hub_target = argo::target(targets, hub);
    let parents = argo::parents(list);
    list.iter()
        .zip(parents)
        .map(|(app, parent)| AppRow {
            hub: hub.to_string(),
            app: Summary::of(app),
            dest: argo::destination(app, &hub_target, targets),
            parent: parent
                .and_then(|i| list.get(i))
                .and_then(|p| p.pointer("/metadata/name"))
                .and_then(Value::as_str)
                .map(String::from),
        })
        .collect()
}

/// What `problems_only` keeps: what the Argo CD page shows red or amber, and Missing health
/// with auto-sync off too (what it should deploy isn't there). Out of sync alone with
/// auto-sync off is not kept: an Application synced by hand waits for its sync so.
fn needs_a_look(app: &Summary) -> bool {
    matches!(argo::attention(app), Tone::Bad | Tone::Busy) || app.health == "Missing"
}

fn keeps(p: &ArgoAppsParams, row: &AppRow) -> bool {
    let on = |flag: Option<bool>| flag == Some(true);
    (!on(p.problems_only) || needs_a_look(&row.app))
        && (!on(p.out_of_sync) || row.app.sync == "OutOfSync")
        && p.project.as_ref().is_none_or(|project| row.app.project == *project)
        && p.destination.as_ref().is_none_or(|c| row.dest.contexts.contains(c))
        && p.dest_namespace.as_ref().is_none_or(|ns| row.dest.namespace == *ns)
}

/// Where it deploys: the first context the agent may use, else one it may not (said), else
/// the server or cluster name the Application gives.
fn destination_cell(dest: &Destination, allowed: impl Fn(&str) -> bool) -> String {
    let ns = if dest.namespace.is_empty() { String::new() } else { format!(" / {}", dest.namespace) };
    match (dest.contexts.iter().find(|c| allowed(c)), dest.contexts.first()) {
        (Some(c), _) => format!("{c}{ns}"),
        (None, Some(c)) => format!("{c}{ns} (not open to you)"),
        (None, None) if dest.cluster.is_empty() => format!("?{ns}"),
        (None, None) => format!("{}{ns} (no context)", dest.cluster),
    }
}

/// What Argo CD says of an Application that needs a look: its failed operation, an error, or
/// its health.
fn why(app: &Summary) -> Option<String> {
    let failed = app.operation.as_ref().filter(|_| argo::sync_failed(app)).map(|(phase, message, _)| {
        format!("last operation {phase}{}", message.as_ref().map_or_default(|m| format!(": {m}")))
    });
    let error = || app.conditions.iter().find(|(ty, _)| ty.ends_with("Error")).map(|(ty, m)| format!("{ty}: {m}"));
    let health = || app.health_message.as_ref().map(|m| format!("{}: {m}", app.health));
    let text = failed.or_else(error).or_else(health)?;
    Some(cap_chars(&text, MAX_MESSAGE_CHARS))
}

/// `argocd_apps`'s reply: those needing a look first, then the others, at most `max`; what
/// Argo CD says of each one needing a look; then what was left out and why.
fn apps_text(
    mut rows: Vec<AppRow>,
    allowed: impl Fn(&str) -> bool,
    hidden: usize,
    notes: &[String],
    max: usize,
) -> String {
    let rank = |a: &Summary| match argo::attention(a) {
        Tone::Bad => 0,
        _ if needs_a_look(a) => 1,
        _ => 2,
    };
    rows.sort_by(|a, b| {
        (rank(&a.app), &a.hub, &a.app.namespace, &a.app.name).cmp(&(
            rank(&b.app),
            &b.hub,
            &b.app.namespace,
            &b.app.name,
        ))
    });
    let total = rows.len();
    rows.truncate(max);
    let mut out = String::new();
    if rows.is_empty() {
        out.push_str("No Applications found.\n");
    } else {
        let table: Vec<Vec<String>> = rows
            .iter()
            .map(|r| {
                let a = &r.app;
                let operation = if a.in_progress {
                    "Running".to_string()
                } else {
                    a.operation.as_ref().map_or_else(|| "-".into(), |(phase, _, _)| phase.clone())
                };
                vec![
                    r.hub.clone(),
                    format!("{}/{}", a.namespace, a.name),
                    a.project.clone(),
                    a.sync.clone(),
                    a.health.clone(),
                    if a.automated { "on" } else { "off" }.into(),
                    operation,
                    a.revision.clone().unwrap_or_else(|| "-".into()),
                    destination_cell(&r.dest, &allowed),
                    r.parent.clone().unwrap_or_else(|| "-".into()),
                ]
            })
            .collect();
        let headers = [
            "HUB",
            "APPLICATION",
            "PROJECT",
            "SYNC",
            "HEALTH",
            "AUTO-SYNC",
            "OPERATION",
            "REVISION",
            "DESTINATION",
            "PARENT",
        ];
        out.push_str(&format_table(&headers, &table));
        out.push('\n');
        let looks: Vec<String> = rows
            .iter()
            .filter(|r| needs_a_look(&r.app))
            .filter_map(|r| why(&r.app).map(|w| format!("  {} {}/{}: {w}", r.hub, r.app.namespace, r.app.name)))
            .collect();
        if !looks.is_empty() {
            out.push_str("\nNeeding a look:\n");
            out.push_str(&looks.join("\n"));
            out.push('\n');
        }
    }
    if total > rows.len() {
        out.push_str(&format!(
            "… {} not shown (your limit is {}; filter by project, destination or problems_only)\n",
            crate::plural::count(total - rows.len(), "more Application"),
            crate::plural::count(max, "row")
        ));
    }
    if hidden > 0 {
        out.push_str(&format!(
            "{} your rules don't let you list left out.\n",
            crate::plural::count(hidden, "Application")
        ));
    }
    for note in notes {
        out.push_str(&format!("{note}\n"));
    }
    out
}

/// What `describe_resource` and `get_changes` say of the Application that deployed an object.
/// Nothing for an instance label that names no Application found: Helm sets the same label,
/// so most objects carrying it have nothing to do with Argo CD (and no hub may be known).
fn managed_text(by: &TrackedBy, found: &Pick, key: &ObjectKey, hubs: &[String], notes: &[String]) -> Option<String> {
    let (named, how) = match by {
        TrackedBy::Id(t) => (
            t.app_ns.as_ref().map_or_else(|| t.app.clone(), |ns| format!("{ns}/{}", t.app)),
            "its argocd.argoproj.io/tracking-id annotation",
        ),
        TrackedBy::Label(l) => (l.clone(), "its app.kubernetes.io/instance label, Argo CD's label tracking"),
    };
    let lead = format!("Argo CD: managed by Application {named} ({how})");
    let not_read = if notes.is_empty() { String::new() } else { format!(" Not read: {}.", notes.join("; ")) };
    let (hub, app) = match (found, by) {
        (Pick::One(hub, app), _) => (hub, app),
        (Pick::Several(apps), _) => {
            return Some(format!(
                "{lead}: several fit, which one is unclear: {}.{not_read} Read them with argocd_apps.\n",
                apps.join(", ")
            ));
        }
        (_, TrackedBy::Label(_)) => return None,
        (Pick::Elsewhere(apps), _) => {
            return Some(format!(
                "{lead}: found ({}), but none deploys to this cluster or tracks this object.{not_read}\n",
                apps.join(", ")
            ));
        }
        (Pick::NotFound, _) if hubs.is_empty() => return Some(format!("{lead}. {NO_HUB}\n")),
        (Pick::NotFound, _) if notes.is_empty() => {
            return Some(format!("{lead}, not found on {}.\n", hubs.join(", ")));
        }
        (Pick::NotFound, _) => return Some(format!("{lead}, not found on the hubs read.{not_read}\n")),
    };
    let s = Summary::of(app).with_own(app, &key.group, &key.kind, &key.namespace, &key.name);
    let mut out = format!(
        "Argo CD: managed by Application {}/{} on hub {hub} ({how}): {}, {}, auto-sync {}",
        s.namespace,
        s.name,
        s.sync,
        s.health,
        if s.automated { "on" } else { "off" }
    );
    if s.in_progress {
        out.push_str(", an operation running");
    } else if let Some((phase, message, _)) = &s.operation {
        let message = message.as_ref().filter(|_| argo::sync_failed(&s)).map_or_default(|m| format!(": {m}"));
        out.push_str(&cap_chars(&format!(", last operation {phase}{message}"), MAX_MESSAGE_CHARS));
    }
    if let Some(rev) = &s.revision {
        out.push_str(&format!(", revision {rev}"));
    }
    out.push('.');
    if let Some((sync, health, _)) = &s.own {
        let health = if health.is_empty() { String::new() } else { format!(", {health}") };
        out.push_str(&format!(" This object in it: {sync}{health}."));
    }
    out.push_str(&format!(
        " Act on it with gitops or wait_for: context {hub}, kind applications, namespace {}, name {}.\n",
        s.namespace, s.name
    ));
    Some(out)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;

    fn ctx(name: &str, server: &str) -> ContextTarget {
        ContextTarget { name: name.into(), cluster: format!("{name}-cluster"), server: Some(server.into()), exec: None }
    }

    fn app(name: &str, sync: &str, health: &str, automated: bool, dest: Value) -> Value {
        let mut app = json!({
            "metadata": {"name": name, "namespace": "argocd"},
            "spec": {"project": "shop", "destination": dest},
            "status": {"sync": {"status": sync, "revision": "a".repeat(40)}, "health": {"status": health}},
        });
        if automated {
            app["spec"]["syncPolicy"] = json!({"automated": {}});
        }
        app
    }

    fn params() -> ArgoAppsParams {
        ArgoAppsParams {
            context: None,
            problems_only: None,
            out_of_sync: None,
            project: None,
            destination: None,
            dest_namespace: None,
        }
    }

    /// The rows of a hub: where each deploys, mapped to a context, and its parent.
    fn fleet() -> Vec<AppRow> {
        let targets = [ctx("hub", "https://hub.example"), ctx("prod", "https://prod.example")];
        let mut root = app("root", "Synced", "Healthy", true, json!({"server": "https://kubernetes.default.svc"}));
        root["status"]["resources"] =
            json!([{"group": "argoproj.io", "kind": "Application", "namespace": "argocd", "name": "web-workload"}]);
        let mut failed =
            app("api", "OutOfSync", "Healthy", false, json!({"server": "https://prod.example", "namespace": "api"}));
        failed["status"]["operationState"] =
            json!({"phase": "Failed", "message": "one or more objects failed to apply"});
        let list = [
            root,
            app(
                "web-workload",
                "OutOfSync",
                "Healthy",
                false,
                json!({"server": "https://prod.example", "namespace": "web"}),
            ),
            app("db", "OutOfSync", "Missing", false, json!({"server": "https://prod.example", "namespace": "db"})),
            app(
                "cache",
                "OutOfSync",
                "Healthy",
                true,
                json!({"server": "https://other.example", "namespace": "cache"}),
            ),
            failed,
        ];
        hub_rows("hub", &list, &targets)
    }

    #[test]
    fn rows_map_the_destination_to_a_context_and_name_the_parent() {
        let rows = fleet();
        let row = |name: &str| rows.iter().find(|r| r.app.name == name).unwrap();
        assert_eq!(row("root").dest.contexts, ["hub"]);
        assert_eq!(row("web-workload").dest.contexts, ["prod"]);
        assert_eq!(row("web-workload").dest.namespace, "web");
        assert_eq!(row("web-workload").parent.as_deref(), Some("root"));
        assert_eq!(row("root").parent, None);
        assert!(row("cache").dest.contexts.is_empty());
        // The context the agent may use, else one it may not, else the server.
        let all = |_: &str| true;
        let none = |_: &str| false;
        assert_eq!(destination_cell(&row("web-workload").dest, all), "prod / web");
        assert_eq!(destination_cell(&row("web-workload").dest, none), "prod / web (not open to you)");
        assert_eq!(destination_cell(&row("cache").dest, all), "https://other.example / cache (no context)");
    }

    /// Out of sync with auto-sync off is how an Application synced by hand waits: not a
    /// problem. Missing is, auto-sync on or off; so are a failed sync and out of sync with
    /// auto-sync on.
    #[test]
    fn problems_leave_out_what_waits_for_a_sync_by_hand() {
        let rows = fleet();
        let problems: Vec<&str> = rows
            .iter()
            .filter(|r| keeps(&ArgoAppsParams { problems_only: Some(true), ..params() }, r))
            .map(|r| r.app.name.as_str())
            .collect();
        assert_eq!(problems, ["db", "cache", "api"]);
        let out_of_sync = rows.iter().filter(|r| keeps(&ArgoAppsParams { out_of_sync: Some(true), ..params() }, r));
        assert_eq!(out_of_sync.count(), 4);
        let to_prod =
            ArgoAppsParams { destination: Some("prod".into()), dest_namespace: Some("web".into()), ..params() };
        let names: Vec<&str> = rows.iter().filter(|r| keeps(&to_prod, r)).map(|r| r.app.name.as_str()).collect();
        assert_eq!(names, ["web-workload"]);
        assert_eq!(
            rows.iter().filter(|r| keeps(&ArgoAppsParams { project: Some("x".into()), ..params() }, r)).count(),
            0
        );
    }

    #[test]
    fn the_reply_puts_failures_first_with_argo_cds_message() {
        let text = apps_text(fleet(), |c| c != "prod", 2, &["other: could not be read: timed out".into()], 4);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("HUB") && lines[0].contains("DESTINATION"), "{text}");
        // The failed sync first, then the others needing a look, then the rest (one cut by the limit).
        assert!(lines[1].contains("argocd/api"), "{text}");
        assert!(lines[2].contains("argocd/cache"), "{text}");
        assert!(lines[3].contains("argocd/db"), "{text}");
        assert!(lines[4].contains("argocd/root"), "{text}");
        assert!(text.contains("prod / api (not open to you)"), "{text}");
        assert!(text.contains("hub argocd/api: last operation Failed: one or more objects failed to apply"), "{text}");
        assert!(text.contains("1 more Application not shown"), "{text}");
        assert!(text.contains("2 Applications your rules don't let you list left out."), "{text}");
        assert!(text.contains("other: could not be read: timed out"), "{text}");
        assert_eq!(apps_text(Vec::new(), |_| true, 0, &[], 10), "No Applications found.\n");
    }

    fn key() -> ObjectKey {
        ObjectKey { group: "apps".into(), kind: "Deployment".into(), namespace: "web".into(), name: "web".into() }
    }

    #[test]
    fn an_object_names_the_application_that_deployed_it() {
        let by = TrackedBy::Id(argo::parse_tracking("web-workload:apps/Deployment:web/web").unwrap());
        let mut found = app("web-workload", "OutOfSync", "Healthy", false, json!({"server": "https://prod.example"}));
        found["status"]["resources"] = json!([{"group": "apps", "kind": "Deployment", "namespace": "web", "name": "web", "status": "OutOfSync", "health": {"status": "Healthy"}}]);
        let one = Pick::One("hub".into(), found);
        let text = managed_text(&by, &one, &key(), &["hub".into()], &[]).unwrap();
        assert!(text.starts_with("Argo CD: managed by Application argocd/web-workload on hub hub"), "{text}");
        assert!(text.contains("OutOfSync, Healthy, auto-sync off, revision aaaaaaaa."), "{text}");
        assert!(text.contains("This object in it: OutOfSync, Healthy."), "{text}");
        assert!(text.contains("context hub, kind applications, namespace argocd, name web-workload"), "{text}");
        // Not read: why, and what the agent can do.
        let text = managed_text(&by, &Pick::NotFound, &key(), &[], &[]).unwrap();
        assert!(text.contains("managed by Application web-workload (its argocd.argoproj.io/tracking-id"), "{text}");
        assert!(text.contains("Pass `context`"), "{text}");
        let denied = ["hub: your rules don't let you list it".to_string()];
        let text = managed_text(&by, &Pick::NotFound, &key(), &["hub".into()], &denied).unwrap();
        assert!(text.contains("Not read: hub: your rules don't let you list it."), "{text}");
        // Found on several hubs, none of them clearly this one: said so, not "not found".
        let several = Pick::Several(vec!["staging argocd/web-workload".into(), "prod argocd/web-workload".into()]);
        let text = managed_text(&by, &several, &key(), &["prod".into(), "staging".into()], &[]).unwrap();
        assert!(text.contains("several fit, which one is unclear: staging argocd/web-workload, prod"), "{text}");
        assert!(!text.contains("not found"), "{text}");
        let elsewhere = Pick::Elsewhere(vec!["other argocd/web-workload".into()]);
        let text = managed_text(&by, &elsewhere, &key(), &["other".into()], &[]).unwrap();
        assert!(text.contains("found (other argocd/web-workload), but none deploys to this cluster"), "{text}");
        // An instance label is Helm's too: said only once the Application is found tracking
        // the object, and never with no hub (a cluster without Argo CD says nothing).
        let label = TrackedBy::Label("web-workload".into());
        assert_eq!(managed_text(&label, &Pick::NotFound, &key(), &["hub".into()], &[]), None);
        assert_eq!(managed_text(&label, &Pick::NotFound, &key(), &[], &[]), None);
        assert_eq!(managed_text(&label, &elsewhere, &key(), &["other".into()], &[]), None);
        let text = managed_text(&label, &one, &key(), &["hub".into()], &[]).unwrap();
        assert!(text.contains("argocd/web-workload on hub hub (its app.kubernetes.io/instance label"), "{text}");
    }

    fn candidate(hub: &str, here: bool, tracks: bool) -> Candidate {
        Candidate { hub: hub.into(), app: app("web", "Synced", "Healthy", true, json!({})), here, tracks }
    }

    /// A tracking id names an Application by name only: one of the same name on another hub
    /// that neither deploys here nor tracks the object is not taken for its manager, and two
    /// that fit as well are said to be unclear.
    #[test]
    fn the_manager_must_deploy_here_or_track_the_object() {
        let id = TrackedBy::Id(argo::parse_tracking("web:apps/Deployment:web/web").unwrap());
        let hub_of = |p: Pick| match p {
            Pick::One(hub, _) => hub,
            _ => String::new(),
        };
        assert!(matches!(pick(&id, vec![]), Pick::NotFound));
        assert!(
            matches!(pick(&id, vec![candidate("other", false, false)]), Pick::Elsewhere(a) if a == ["other argocd/web"])
        );
        assert_eq!(hub_of(pick(&id, vec![candidate("prod", false, true)])), "prod");
        assert_eq!(hub_of(pick(&id, vec![candidate("staging", true, false), candidate("prod", true, true)])), "prod");
        let several = pick(&id, vec![candidate("staging", false, true), candidate("prod", false, true)]);
        assert!(matches!(several, Pick::Several(a) if a == ["staging argocd/web", "prod argocd/web"]));
        // A label must do both.
        let label = TrackedBy::Label("web".into());
        assert!(matches!(pick(&label, vec![candidate("prod", false, true)]), Pick::Elsewhere(_)));
        assert_eq!(hub_of(pick(&label, vec![candidate("prod", true, true)])), "prod");
    }

    /// A hub that failed recently is not asked again (said for a tracking id); a label alone
    /// reads only hubs already connected.
    #[test]
    fn a_hub_is_asked_only_when_it_may_answer_without_a_login() {
        let id = TrackedBy::Id(argo::parse_tracking("web:apps/Deployment:web/web").unwrap());
        let label = TrackedBy::Label("web".into());
        assert_eq!(plan(&id, false, None), Plan::Read);
        assert_eq!(plan(&id, true, None), Plan::Read);
        assert_eq!(plan(&id, true, Some("did not answer".into())), Plan::Skip("did not answer".into()));
        assert_eq!(plan(&label, true, None), Plan::Read);
        assert_eq!(plan(&label, false, None), Plan::Quiet);
        assert_eq!(plan(&label, true, Some("did not answer".into())), Plan::Quiet);
    }

    proptest! {
        /// Whatever the user picked or the app found, an agent's Argo CD reads never go to a
        /// context its rules don't name; what the user picked wins over what was found.
        #[test]
        fn hubs_are_only_contexts_the_rules_name(
            chosen in proptest::collection::vec("[a-d]", 0..4),
            found in proptest::collection::vec("[a-d]", 0..4),
            open in proptest::collection::vec("[a-d]", 0..4),
        ) {
            let hubs = hub_candidates(&chosen, found.clone(), |c| open.iter().any(|o| o == c));
            prop_assert!(hubs.iter().all(|h| open.contains(h)));
            let from = if chosen.is_empty() { &found } else { &chosen };
            prop_assert!(hubs.iter().all(|h| from.contains(h)));
            prop_assert!(from.iter().filter(|c| open.contains(c)).all(|c| hubs.contains(c)));
            prop_assert!(hubs.windows(2).all(|w| w[0] < w[1]));
        }
    }
}
