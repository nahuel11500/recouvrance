//! Argo CD for agents, from the hub to the destination: `argocd_apps` lists the Applications
//! of the hubs an agent may read, each with the kubeconfig context it deploys to, and
//! `describe_resource` and `get_changes` name the Application that deployed an object
//! (`KubeMcp::managed_by`), found first in what Argo CD's API feeds hold, on the servers the
//! agent's rules name (a server is a rule's context, by its name), then on hubs.
//!
//! The hubs are those the user picked in Settings, else those the app or the agents' calls
//! found serving Applications, and only those the agent's rules name: no other context is
//! ever asked whether it runs Argo CD.

use std::time::{Duration, Instant};

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

/// Said when no Argo CD server nor hub can be read for an object's Application.
const NO_ARGO: &str = "No Argo CD is known that your rules name: no server set in Recouvrance (Settings → Argo CD) \
     or logged in to with the argocd CLI, and no hub. Pass `context` with the context Argo CD runs in \
     (list_contexts), or the user can pick it in Recouvrance (Settings → Contexts).";

/// Said when no hub can be read.
const NO_HUB: &str = "No Argo CD hub is known that your rules name. Pass `context` with the context Argo CD runs \
     in (list_contexts), or the user can pick it in Recouvrance (Settings → Contexts).";

/// Longest message kept from Argo CD in a reply line.
const MAX_MESSAGE_CHARS: usize = 300;

/// The longest wait for a hub's Applications once connected (a list of thousands is slow).
const READ_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// and `get_changes` say it: by the object's tracking id or instance label. Found first in
    /// what Argo CD's API feeds hold, on the servers the agent's rules name, among the
    /// Applications an Allow rule lets it get or list there (`argo::api_candidates`: no list
    /// of its own, a server down answers at once); a label alone reads only feeds running
    /// already. Then, when no server has one of that name, on the agent's hubs where an Allow
    /// rule lets it (hubs go with #1054). None for an object that names no Application:
    /// nothing is asked of any server or hub then.
    pub(super) async fn managed_by(&self, call: &Call, here: &str, kind: &Kind, obj: &Value) -> Option<Managed> {
        let by = argo::tracked_by(&kind.ar.group, &kind.ar.kind, obj)?;
        let key = ObjectKey::of(kind, obj);
        let policy = call.policy();
        let app_kind = argo::application_kind();
        let may_read = |server: &str, app: &Summary| {
            let allowed = |verb: Verb| {
                let a = access_for(&app_kind, server, verb, Some(app.namespace.clone()), Some(app.name.clone()));
                policy.decide(&a).mode == Mode::Allow
            };
            allowed(Verb::Get) || allowed(Verb::List)
        };
        let start = matches!(by, TrackedBy::Id(_));
        let api = argo::api_candidates(
            here,
            &by,
            (key.group.as_str(), key.kind.as_str(), obj),
            |server| policy.context_allowed(server),
            start,
            may_read,
        )
        .await;
        let (mut read, mut notes, mut found) = (Vec::new(), Vec::new(), Vec::new());
        if let Some((candidates, said, servers)) = api {
            let tied = crate::settings::with(|s| s.argocd_servers.clone());
            found.extend(candidates.into_iter().map(|c| {
                // gitops and wait_for act on a hub context: the one tied to this server, if open to it.
                let hub = argo::server::contexts_of(&tied, &c.endpoint.server.base)
                    .find(|h| policy.context_allowed(h))
                    .map(String::from);
                Candidate { hub: c.endpoint.name(), app: c.app, here: c.here, tracks: c.tracks, via: Via::Server(hub) }
            }));
            notes.extend(said);
            read.extend(servers);
        }
        if found.is_empty() {
            let (hubs, said, on_hubs) = self.managed_on_hubs(policy, here, &by, &key).await;
            read.extend(hubs);
            notes.extend(said);
            found = on_hubs;
        }
        let picked = pick(&by, found);
        let text = managed_text(&by, &picked, &key, &read, &notes)?;
        let app = match picked {
            Pick::One { app, .. } => Some(app),
            _ => None,
        };
        Some(Managed { text, app })
    }

    /// The Applications `by` names on the agent's hubs: the hubs, why some said nothing, and
    /// what the others named.
    async fn managed_on_hubs(
        &self,
        policy: &Policy,
        here: &str,
        by: &TrackedBy,
        key: &ObjectKey,
    ) -> (Vec<String>, Vec<String>, Vec<Candidate>) {
        let hubs = self.agent_hubs(policy);
        let targets = argo::targets();
        let here = argo::target(&targets, here);
        let reads = read_hubs(
            &hubs,
            by,
            |hub| self.hub.ready(hub),
            HUB_BOUND,
            |hub: String| async move { self.manager_on(policy, &hub, by).await },
        )
        .await;
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
                let here = argo::app_deploys_to(&app, &hub_target, &here);
                let tracks = argo::resource_of(&app, &key.group, &key.kind, &key.namespace, &key.name).is_some();
                found.push(Candidate { hub: hub.clone(), app, here, tracks, via: Via::Hub });
            }
        }
        (hubs, notes, found)
    }

    /// The Applications on `hub` that `by` may name, as far as the agent's Allow rules let it
    /// read them; Err with a line saying why the hub said nothing. The connection and the read
    /// are each bounded in time. A hub that can't be reached, or whose login fails, is not
    /// asked again for `RETRY`: its login command doesn't run again on every call.
    async fn manager_on(&self, policy: &Policy, hub: &str, by: &TrackedBy) -> Result<Vec<Value>, String> {
        let said = |failed: NotRead| {
            if failed.unreached {
                argo::note_unreached(hub, failed.why.clone());
            }
            format!("{hub}: {}", failed.why)
        };
        let (client, kinds) = self.connect(hub).await.map_err(said)?;
        if !kinds.iter().any(|k| k.is("argoproj.io", "Application")) {
            return Ok(Vec::new());
        }
        match tokio::time::timeout(READ_TIMEOUT, managers_on(policy, hub, client, by)).await {
            Ok(Ok(apps)) => {
                argo::note_reached(hub);
                Ok(apps)
            }
            Ok(Err(failed)) => Err(said(failed)),
            // Connected, so reached: slow, not down. Its list goes on and is kept for the next call.
            Err(_) => Err(format!(
                "{hub}: slow, it did not answer within {} s (asked again on the next call)",
                READ_TIMEOUT.as_secs()
            )),
        }
    }

    /// The hub's client and kinds for agents, waited for at most `PROBE_TIMEOUT`. The
    /// connection and discovery run in a task of their own: one that ends later still fills
    /// the hub's connection for the next call (and says the hub reached).
    async fn connect(&self, hub: &str) -> Result<(kube::Client, std::sync::Arc<Vec<Kind>>), NotRead> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (shared, context) = (self.hub.clone(), hub.to_string());
        k8s::spawn_detached(async move {
            // A hub connected already makes no request here: that says nothing of it.
            let was_ready = shared.ready(&context);
            let connected = shared.client(&context).await;
            if connected.is_ok() && !was_ready {
                argo::note_reached(&context);
            }
            let _ = tx.send(connected);
        });
        match tokio::time::timeout(argo::PROBE_TIMEOUT, rx).await {
            Ok(Ok(Ok(connected))) => Ok(connected),
            Ok(Ok(Err(e))) => Err(NotRead::of(&e)),
            Ok(Err(_)) | Err(_) => Err(NotRead { why: "did not answer".into(), unreached: true }),
        }
    }
}

/// The longest wait for one hub in `managed_by`, connection and read: beyond what
/// `manager_on` bounds itself, so that no hub holds the others' answers longer.
const HUB_BOUND: Duration = Duration::from_secs(argo::PROBE_TIMEOUT.as_secs() + READ_TIMEOUT.as_secs() + 5);

/// What each hub says of `by`, all read at once: None when it is not asked (`plan`), Err with a
/// line saying why it said nothing. Each read is cut at `bound`, so a hub that hangs holds the
/// others' answers no longer than that.
async fn read_hubs<F, Fut>(
    hubs: &[String],
    by: &TrackedBy,
    ready: impl Fn(&str) -> bool,
    bound: Duration,
    read: F,
) -> Vec<Option<Result<Vec<Value>, String>>>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<Vec<Value>, String>>,
{
    let now = Instant::now();
    let reads = hubs.iter().map(|hub| {
        let ready = ready(hub);
        // A hub connected for agents is judged by the agents' reads alone: the window's probe
        // may have failed for its own reasons.
        let planned = plan(by, ready, argo::recent_failure(hub, !ready, now));
        let read = &read;
        async move {
            match planned {
                Plan::Quiet => None,
                Plan::Skip(why) => Some(Err(skipped_note(hub, &why))),
                Plan::Read => Some(
                    tokio::time::timeout(bound, read(hub.clone()))
                        .await
                        .unwrap_or_else(|_| Err(format!("{hub}: did not answer"))),
                ),
            }
        }
    });
    futures::future::join_all(reads).await
}

/// What is said of a hub left alone since it failed.
fn skipped_note(hub: &str, why: &str) -> String {
    let retry = crate::plural::count(argo::RETRY.as_secs().div_ceil(60), "minute");
    format!("{hub}: not asked again yet, it failed less than {retry} ago: {why}")
}

/// Why a hub said nothing, and whether it is because it can't be reached now.
#[derive(Debug, PartialEq, Eq)]
struct NotRead {
    why: String,
    /// Not reached: no answer, or its login refused or failing. It is left alone for `RETRY`.
    unreached: bool,
}

impl NotRead {
    fn of(e: &anyhow::Error) -> Self {
        Self { why: k8s::short_error(e), unreached: unreachable_by(e) }
    }

    /// A refusal of the agent's own rules: the hub answered nothing because it wasn't asked.
    fn refused(why: String) -> Self {
        Self { why, unreached: false }
    }
}

/// Whether a read error says the hub can't be reached now: no answer at all (a connection,
/// TLS or timeout failure: no API status), its credentials refused (401) or its login command
/// failing. Asking it again would wait again, or run the login again (a browser may open). A
/// refusal (403), an object not found (404) or another answer of the API server is not.
fn unreachable_by(e: &anyhow::Error) -> bool {
    k8s::login_failed(e) || matches!(k8s::api_status(e), None | Some(401))
}

/// `manager_on`'s reads, once connected to the hub.
async fn managers_on(policy: &Policy, hub: &str, client: kube::Client, by: &TrackedBy) -> Result<Vec<Value>, NotRead> {
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
                    Err(e) => return Err(NotRead::of(&e)),
                }
            } else if t.app_ns.is_some() {
                return Err(NotRead::refused(format!("your rules don't let you read Application {ns}/{}", t.app)));
            }
            t.app.as_str()
        }
        TrackedBy::Label(name) => name.as_str(),
    };
    let list_access = access_for(&kind, hub, Verb::List, None, None);
    if list_reach(policy, &list_access) != Mode::Allow {
        return Err(NotRead::refused("your rules don't let you list applications.argoproj.io".into()));
    }
    // In a task of its own: a list cut by `READ_TIMEOUT` still ends, and is kept for the next
    // call (and the window's Argo CD page).
    let (tx, rx) = tokio::sync::oneshot::channel();
    let target = argo::Hub { context: hub.to_string(), client };
    k8s::spawn_detached(async move {
        let _ = tx.send(argo::applications(&target).await);
    });
    let list = match rx.await {
        Ok(listed) => listed.map_err(|e| NotRead::of(&e))?,
        Err(_) => return Err(NotRead { why: "did not answer".into(), unreached: true }),
    };
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

/// Where an Application named for the object was read.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Via {
    /// On a hub context's Kubernetes API (the candidate's `hub`).
    Hub,
    /// By Argo CD's API (the candidate's `hub` is the server's name), with the hub context tied
    /// to that server in Settings that the agent's rules name, for `gitops` and `wait_for`.
    Server(Option<String>),
}

/// An Application a hub or an Argo CD server named for the object.
struct Candidate {
    /// The hub context, or the server's name.
    hub: String,
    via: Via,
    app: Value,
    /// It deploys to the object's cluster; None when that can't be told (a destination named
    /// as an Argo CD cluster Secret is, see `argo::app_deploys_to`).
    here: Option<bool>,
    /// Its `status.resources` lists the object.
    tracks: bool,
}

/// How surely an Application picked deployed the object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fit {
    /// It deploys to the object's cluster and tracks the object.
    Both,
    /// It does one of the two.
    One,
    /// It does neither surely: the only one of that name found, its destination named as an
    /// Argo CD cluster that matches no context.
    Unsure,
}

/// Which Application deployed the object.
enum Pick {
    One {
        hub: String,
        via: Via,
        app: Value,
        fit: Fit,
    },
    /// Several fit as well: the hub and name of each.
    Several(Vec<String>),
    /// Found by name, but none deploys to this cluster or tracks the object: the hub and
    /// name of each.
    Elsewhere(Vec<String>),
    NotFound,
}

/// The Application that deployed the object among those named for it. A tracking id names an
/// Application by name only, and another hub may have one of the same name deploying
/// elsewhere: it must deploy to this cluster or track the object, or else be the only one
/// whose destination can't be told (`Fit::Unsure`). A label is only a hint: it must do both.
/// One that does both wins over one that does one.
fn pick(by: &TrackedBy, found: Vec<Candidate>) -> Pick {
    let name = |c: &Candidate| {
        let field = |p: &str| c.app.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string();
        format!("{} {}/{}", c.hub, field("/metadata/namespace"), field("/metadata/name"))
    };
    let here = |c: &Candidate| c.here == Some(true);
    let fits = |c: &Candidate| argo::fits(by, c.here, c.tracks);
    let (mut fit, rest): (Vec<Candidate>, Vec<Candidate>) = found.into_iter().partition(fits);
    let rank = |c: &Candidate| u8::from(here(c)) + u8::from(c.tracks);
    let Some(best) = fit.iter().map(rank).max() else {
        let (mut unsure, elsewhere): (Vec<Candidate>, Vec<Candidate>) =
            rest.into_iter().partition(|c| c.here.is_none());
        return match (by, unsure.len()) {
            (TrackedBy::Id(_), 1) => {
                let c = unsure.remove(0);
                Pick::One { hub: c.hub, via: c.via, app: c.app, fit: Fit::Unsure }
            }
            (TrackedBy::Id(_), 2..) => Pick::Several(unsure.iter().map(name).collect()),
            _ if unsure.is_empty() && elsewhere.is_empty() => Pick::NotFound,
            _ => Pick::Elsewhere(unsure.iter().chain(&elsewhere).map(name).collect()),
        };
    };
    fit.retain(|c| rank(c) == best);
    if fit.len() == 1 {
        let c = fit.remove(0);
        let fit = if best == 2 { Fit::Both } else { Fit::One };
        return Pick::One { hub: c.hub, via: c.via, app: c.app, fit };
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
    let (hub, via, app, fit) = match (found, by) {
        (Pick::One { hub, via, app, fit }, _) => (hub, via, app, *fit),
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
        (Pick::NotFound, _) if hubs.is_empty() => return Some(format!("{lead}. {NO_ARGO}\n")),
        (Pick::NotFound, _) if notes.is_empty() => {
            return Some(format!("{lead}, not found on {}.\n", hubs.join(", ")));
        }
        (Pick::NotFound, _) => return Some(format!("{lead}, not found on the Argo CDs read.{not_read}\n")),
    };
    let s = Summary::of(app).with_own(app, &key.group, &key.kind, &key.namespace, &key.name);
    let on = match via {
        Via::Hub => format!("hub {hub}"),
        Via::Server(_) => format!("Argo CD server {hub}"),
    };
    let mut out = format!(
        "Argo CD: managed by Application {}/{} on {on} ({how}): {}, {}, auto-sync {}",
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
    if fit == Fit::Unsure {
        let dest = app.pointer("/spec/destination/name").and_then(Value::as_str).unwrap_or_default();
        out.push_str(&format!(
            " Unsure: it deploys to Argo CD cluster `{dest}`, which matches no context here, and doesn't list this \
             object; it is the only Application of that name found."
        ));
    }
    // Another hub not read may have held a surer one.
    if fit != Fit::Both && !not_read.is_empty() {
        out.push_str(&not_read);
    }
    let act = match via {
        Via::Hub => Some(hub),
        Via::Server(tied) => tied.as_ref(),
    };
    match act {
        Some(context) => out.push_str(&format!(
            " Act on it with gitops or wait_for: context {context}, kind applications, namespace {}, name {}.\n",
            s.namespace, s.name
        )),
        None => out.push_str(
            " gitops and wait_for act on it through a hub context: none tied to this server is open to you.\n",
        ),
    }
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
        let one = Pick::One { hub: "hub".into(), via: Via::Hub, app: found.clone(), fit: Fit::Both };
        let text = managed_text(&by, &one, &key(), &["hub".into()], &[]).unwrap();
        assert!(text.starts_with("Argo CD: managed by Application argocd/web-workload on hub hub"), "{text}");
        assert!(text.contains("OutOfSync, Healthy, auto-sync off, revision aaaaaaa."), "{text}");
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
        // Read by Argo CD's API: named by its server, acted on through the hub tied to it, if any.
        let server = "argocd.example.com".to_string();
        let on = |via: Via| Pick::One { hub: server.clone(), via, app: found.clone(), fit: Fit::Both };
        let text = managed_text(&by, &on(Via::Server(None)), &key(), std::slice::from_ref(&server), &[]).unwrap();
        assert!(
            text.starts_with(
                "Argo CD: managed by Application argocd/web-workload on Argo CD server argocd.example.com"
            ),
            "{text}"
        );
        assert!(text.contains("This object in it: OutOfSync, Healthy."), "{text}");
        assert!(text.contains("none tied to this server is open to you"), "{text}");
        assert!(!text.contains("context argocd.example.com"), "{text}");
        let text = managed_text(&by, &on(Via::Server(Some("hub".into()))), &key(), std::slice::from_ref(&server), &[])
            .unwrap();
        assert!(text.contains("context hub, kind applications, namespace argocd, name web-workload"), "{text}");
        let text = managed_text(&by, &Pick::NotFound, &key(), &[server], &[]).unwrap();
        assert!(text.contains("not found on argocd.example.com."), "{text}");
    }

    /// A hub left alone since it failed, and an Application found on another: the reply names
    /// the one found, and says the hub not read when the pick isn't sure (that hub may hold a
    /// surer one), not when it is.
    #[test]
    fn a_hub_not_read_is_said_beside_an_application_found_unless_it_is_sure() {
        let by = TrackedBy::Id(argo::parse_tracking("web-workload:apps/Deployment:web/web").unwrap());
        let skipped = [skipped_note("hub-b", "did not answer")];
        let found = || app("web-workload", "Synced", "Healthy", true, json!({"server": "https://prod.example"}));
        let hubs = ["hub-a".to_string(), "hub-b".to_string()];
        for (fit, said) in [(Fit::One, true), (Fit::Unsure, true), (Fit::Both, false)] {
            let one = Pick::One { hub: "hub-a".into(), via: Via::Hub, app: found(), fit };
            let text = managed_text(&by, &one, &key(), &hubs, &skipped).unwrap();
            assert!(text.starts_with("Argo CD: managed by Application argocd/web-workload on hub hub-a"), "{text}");
            let note = "Not read: hub-b: not asked again yet, it failed less than 5 minutes ago: did not answer.";
            assert_eq!(text.contains(note), said, "{fit:?}: {text}");
            assert!(text.ends_with("name web-workload.\n"), "{text}");
        }
        let unsure = Pick::One {
            hub: "hub-a".into(),
            via: Via::Hub,
            app: app("web-workload", "Synced", "Healthy", true, json!({"name": "prod-eu"})),
            fit: Fit::Unsure,
        };
        let text = managed_text(&by, &unsure, &key(), &hubs, &[]).unwrap();
        assert!(text.contains("Unsure: it deploys to Argo CD cluster `prod-eu`, which matches no context"), "{text}");
    }

    /// The wait said is `RETRY`'s, not a number written apart from it.
    #[test]
    fn the_note_of_a_hub_left_alone_says_retry() {
        let minutes = argo::RETRY.as_secs().div_ceil(60);
        let note = skipped_note("hub", "did not answer");
        assert!(note.contains(&format!("less than {minutes} minute")), "{note}");
        assert_eq!(note, "hub: not asked again yet, it failed less than 5 minutes ago: did not answer");
    }

    fn candidate(hub: &str, here: bool, tracks: bool) -> Candidate {
        Candidate {
            hub: hub.into(),
            via: Via::Hub,
            app: app("web", "Synced", "Healthy", true, json!({})),
            here: Some(here),
            tracks,
        }
    }

    /// A candidate whose destination can't be told (named as an Argo CD cluster Secret).
    fn unsure(hub: &str) -> Candidate {
        Candidate { here: None, ..candidate(hub, false, false) }
    }

    /// A tracking id's Application whose destination can't be told is taken when it is the
    /// only one so (said unsure), never over one that fits, and never for a label.
    #[test]
    fn a_lone_application_whose_destination_cant_be_told_is_taken_unsure() {
        let id = TrackedBy::Id(argo::parse_tracking("web:apps/Deployment:web/web").unwrap());
        let fit_of = |p: Pick| match p {
            Pick::One { hub, fit, .. } => Some((hub, fit)),
            _ => None,
        };
        assert_eq!(fit_of(pick(&id, vec![unsure("hub")])), Some(("hub".into(), Fit::Unsure)));
        assert_eq!(
            fit_of(pick(&id, vec![unsure("hub"), candidate("other", false, false)])),
            Some(("hub".into(), Fit::Unsure))
        );
        assert_eq!(
            fit_of(pick(&id, vec![unsure("hub"), candidate("prod", true, false)])),
            Some(("prod".into(), Fit::One))
        );
        assert_eq!(fit_of(pick(&id, vec![candidate("prod", true, true)])), Some(("prod".into(), Fit::Both)));
        assert!(matches!(pick(&id, vec![unsure("a"), unsure("b")]), Pick::Several(n) if n.len() == 2));
        let label = TrackedBy::Label("web".into());
        assert!(matches!(pick(&label, vec![unsure("hub")]), Pick::Elsewhere(_)));
    }

    /// Unreached (left alone for `RETRY`): no answer, credentials refused, a login failing.
    /// Answered (asked again): a refusal, not found, any other status of the API server.
    #[test]
    fn only_a_hub_that_cant_be_reached_is_left_alone() {
        let status = |code: u16| -> anyhow::Error {
            kube::Error::Api(kube::core::Status::failure("no", "").with_code(code).boxed()).into()
        };
        assert!(unreachable_by(&anyhow::anyhow!("error trying to connect: tcp connect error: Connection refused")));
        assert!(unreachable_by(&status(401)));
        let login: anyhow::Error = kube::Error::Auth(kube::client::AuthError::AuthExecStart(std::io::Error::other(
            "No such file or directory",
        )))
        .into();
        assert!(unreachable_by(&login));
        assert!(NotRead::of(&login).unreached);
        for code in [403, 404, 409, 429, 500, 503] {
            assert!(!unreachable_by(&status(code)), "{code}");
        }
        assert!(!NotRead::refused("your rules don't let you list applications.argoproj.io".into()).unreached);
    }

    /// The hubs read together: one left alone since it failed is said for a tracking id and
    /// quiet for a label; one that hangs is cut at the bound without holding the others'
    /// answers; a label reads no hub that isn't connected.
    #[tokio::test]
    async fn hubs_are_read_together_and_one_that_hangs_holds_none() {
        let id = TrackedBy::Id(argo::parse_tracking("web:apps/Deployment:web/web").unwrap());
        let label = TrackedBy::Label("web".into());
        argo::note_unreached("read-hubs-down", "did not answer".into());
        let hubs: Vec<String> =
            ["read-hubs-down", "read-hubs-hangs", "read-hubs-ok", "read-hubs-cold"].map(String::from).to_vec();
        let ready = |h: &str| h != "read-hubs-cold";
        let asked = std::sync::Mutex::new(Vec::new());
        let read = |hub: String| {
            asked.lock().unwrap().push(hub.clone());
            async move {
                if hub == "read-hubs-hangs" {
                    std::future::pending::<()>().await;
                }
                Ok::<_, String>(vec![json!({"metadata": {"name": "web"}})])
            }
        };
        let bound = Duration::from_millis(300);
        let started = Instant::now();
        let reads = read_hubs(&hubs, &id, ready, bound, read).await;
        assert!(started.elapsed() < bound * 5, "{:?}", started.elapsed());
        assert_eq!(
            reads[0],
            Some(Err("read-hubs-down: not asked again yet, it failed less than 5 minutes ago: did not answer".into()))
        );
        assert_eq!(reads[1], Some(Err("read-hubs-hangs: did not answer".into())));
        assert_eq!(reads[2].as_ref().map(|r| r.as_ref().map(Vec::len)), Some(Ok(1)));
        // A tracking id may connect to a hub not connected yet.
        assert!(matches!(reads[3], Some(Ok(_))));
        assert_eq!(*asked.lock().unwrap(), ["read-hubs-hangs", "read-hubs-ok", "read-hubs-cold"]);
        // A label: the hub left alone says nothing, and the one not connected isn't asked.
        asked.lock().unwrap().clear();
        let reads = read_hubs(&hubs, &label, ready, bound, read).await;
        assert_eq!(reads[0], None);
        assert_eq!(reads[3], None);
        assert_eq!(*asked.lock().unwrap(), ["read-hubs-hangs", "read-hubs-ok"]);
        // What reaches the reply: the hub left alone for a tracking id, nothing for a label.
        let notes = [skipped_note("read-hubs-down", "did not answer")];
        let text = managed_text(&id, &Pick::NotFound, &key(), &hubs, &notes).unwrap();
        assert!(
            text.contains("not found on the Argo CDs read. Not read: read-hubs-down: not asked again yet"),
            "{text}"
        );
        assert_eq!(managed_text(&label, &Pick::NotFound, &key(), &hubs, &[]), None);
    }

    /// A tracking id names an Application by name only: one of the same name on another hub
    /// that neither deploys here nor tracks the object is not taken for its manager, and two
    /// that fit as well are said to be unclear.
    #[test]
    fn the_manager_must_deploy_here_or_track_the_object() {
        let id = TrackedBy::Id(argo::parse_tracking("web:apps/Deployment:web/web").unwrap());
        let hub_of = |p: Pick| match p {
            Pick::One { hub, .. } => hub,
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
