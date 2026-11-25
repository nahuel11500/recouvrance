//! Argo CD for agents, only through Argo CD's API: the servers the argocd CLI is logged in to
//! and those set in Settings → Argo CD. `argocd_apps` lists their Applications, each with the
//! kubeconfig context it deploys to; `argocd_diff` says what a sync would change; `gitops` and
//! `wait_for` act on one, or wait for it; `describe_resource` and `get_changes` name the
//! Application that deployed an object (`KubeMcp::managed_by`). No cluster Argo CD runs in is
//! ever asked through the kubeconfig.
//!
//! The rules judge an action on an Application where it deploys ([`ArgoPlace`]): the
//! contexts that reach its destination cluster and its `destination.namespace`, verbs
//! `list`, `get`, `refresh` and `sync` on `applications.argoproj.io`. Never the server that
//! holds it: a server set read-only refuses writes before the rules decide.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Call, KubeMcp, cap_chars, format_table};
use crate::k8s::argo::grouped_diff::{self, AppDiff};
use crate::k8s::argo::{self, Fleet, FleetApp, Summary, TrackedBy, api, windows};
use crate::k8s::{self, Kind};
use crate::mcp::{Access, ArgoPlace, Mode, Policy, Verb};
use crate::status::Tone;

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ArgoAppsParams {
    #[schemars(description = "only this Argo CD server, by its name (argocd.example.com); omit for every one")]
    pub server: Option<String>,
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
    #[schemars(
        description = "only those whose name, namespace, project, destination cluster or namespace contains this text (case ignored)"
    )]
    pub search: Option<String>,
    #[schemars(description = "at most this many rows (default and most: your row limit)")]
    pub limit: Option<u32>,
    #[schemars(description = "skip this many rows first, to read the next page")]
    pub offset: Option<u32>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct ArgoDiffParams {
    #[schemars(description = "the Applications, by name, or namespace/name; several are read as one grouped diff")]
    pub applications: Vec<String>,
    #[schemars(
        description = "the Argo CD server holding them, by its name (argocd_apps names it); needed only when several hold one of that name"
    )]
    pub server: Option<String>,
}

/// Said when no Argo CD server can be read.
const NO_ARGO: &str = "No Argo CD server is set in Recouvrance (Settings → Argo CD) or logged in to with the argocd \
     CLI: the user can connect one there. Argo CD is read only through its API.";

/// Longest message kept from Argo CD in a reply line.
const MAX_MESSAGE_CHARS: usize = 300;

/// The longest wait for a server's first list of Applications (a list of thousands is slow).
const LIST_WAIT: Duration = Duration::from_secs(30);

/// Applications one `argocd_diff` reads, at most.
const MAX_DIFF_APPS: usize = 50;

/// Longest `argocd_diff` reply.
const MAX_DIFF_CHARS: usize = 60_000;

/// What a request on Argo CD's Applications names when no rule could let it through.
pub(super) fn no_argo_rule(verb: Verb) -> String {
    format!(
        "{verb} applications.argoproj.io — no rule lets you {verb} Argo CD Applications anywhere. Grantable: the \
         user can add one in Recouvrance (Agents page, or rules.yaml), on the contexts the Applications deploy to \
         and their destination namespaces, e.g. `{verb} applications.argoproj.io -n <namespace> --context <context>`"
    )
}

/// Whether a `kind` argument names Argo CD's Applications.
pub(super) fn is_argo_kind(kind: &str) -> bool {
    matches!(
        kind.trim().to_lowercase().as_str(),
        "applications"
            | "application"
            | "app"
            | "apps.argoproj.io"
            | "applications.argoproj.io"
            | "application.argoproj.io"
    )
}

/// The cluster an Application deploys to as it writes it: its destination's name, else
/// `in-cluster` or the server URL's host. What the rules name when no context reaches it.
fn written_cluster(s: &Summary) -> String {
    argo::cluster_name(&s.dest_name, &s.dest_server, &BTreeMap::new())
}

/// Where the rules judge an action on `a`.
pub(super) fn place_of(a: &FleetApp) -> ArgoPlace {
    ArgoPlace {
        contexts: a.dest.contexts.clone(),
        cluster: written_cluster(&a.app),
        namespace: a.app.dest_namespace.clone(),
        name: a.app.name.clone(),
    }
}

/// Every Application of the Argo CD servers (`server` alone, when named) as their feeds hold
/// them, each with where it deploys among the kubeconfig's contexts. A feed not running is
/// started (and kept a while for the next calls); a server down answers at once with why.
/// None when no server is set or logged in to.
async fn fleet(server: Option<&str>) -> Option<Fleet> {
    let only = server.map(str::to_lowercase);
    let states = api::held_states(|n| only.as_ref().is_none_or(|o| n.to_lowercase() == *o), LIST_WAIT).await;
    if states.is_empty() {
        return None;
    }
    let reads = states
        .into_iter()
        .map(|(endpoint, state)| {
            let state = state.unwrap_or_else(|why| api::FeedState { error: Some(why), ..Default::default() });
            argo::ApiRead { endpoint, state }
        })
        .collect();
    let read = k8s::run(async move {
        let targets = argo::targets();
        Ok(argo::from_api(reads, &targets, &[]))
    });
    read.await.ok()
}

/// What the fleet's servers said besides their Applications: those that failed, and those
/// still reading their first list.
fn fleet_notes(fleet: &Fleet) -> Vec<String> {
    let mut notes: Vec<String> = fleet.errors.iter().map(|e| format!("Not read: {e}")).collect();
    notes.extend(fleet.reading.iter().map(|s| format!("{s}: still reading its first list, call again in a moment")));
    notes
}

/// When no server's list is in: an error when none could be read, else that they are still
/// read. Never "no Applications": they are unknown, not none.
fn none_listed(fleet: &Fleet) -> Option<anyhow::Result<String>> {
    if !fleet.apps.is_empty() || !fleet.read().is_empty() {
        return None;
    }
    let notes = fleet_notes(fleet).join("\n");
    Some(if fleet.reading.is_empty() {
        Err(anyhow::anyhow!("Could not read any Argo CD server, so its Applications are unknown:\n{notes}"))
    } else {
        Ok(format!("No Argo CD server's Applications are read yet:\n{notes}\n"))
    })
}

/// The Application `name` (in the Argo CD namespace `namespace`, on `server`, when given):
/// the only one that fits, or why there is none or several.
pub(super) fn find<'a>(
    fleet: &'a Fleet,
    server: Option<&str>,
    namespace: Option<&str>,
    name: &str,
) -> Result<&'a FleetApp, String> {
    let found: Vec<&FleetApp> = fleet
        .apps
        .iter()
        .filter(|a| a.app.name == name)
        .filter(|a| namespace.is_none_or(|ns| a.app.namespace == ns))
        .filter(|a| server.is_none_or(|s| a.server.name().eq_ignore_ascii_case(s)))
        .collect();
    match found.as_slice() {
        [one] => Ok(one),
        [] => {
            let at = namespace.map_or_default(|ns| format!("{ns}/"));
            // Not found only where a list was read: elsewhere it could not be looked for.
            let read = fleet.read();
            let mut why = if read.is_empty() {
                format!(
                    "Argo CD Application {at}{name} could not be looked for: no Argo CD server's list could be read"
                )
            } else {
                format!("Argo CD Application {at}{name} not found on {}", read.join(", "))
            };
            for note in fleet_notes(fleet) {
                why.push_str(&format!("; {note}"));
            }
            Err(why)
        }
        several => {
            let names: Vec<String> =
                several.iter().map(|a| format!("{} {}/{}", a.server.name(), a.app.namespace, a.app.name)).collect();
            Err(format!(
                "several Applications are named {name}: {}; pass `server` (and `namespace`) to pick one",
                names.join(", ")
            ))
        }
    }
}

#[tool_router(router = argo_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "Argo CD Applications, read through Argo CD's API like the Argo CD page: for each, its Argo CD server, project, sync, health, auto-sync, last operation, revision, where it deploys (the kubeconfig context that reaches its destination, and whether your rules open it), when its next sync can start under its project's sync windows, and the Application that made it (app of apps); those needing a look first, with Argo CD's message. Out of sync with auto-sync off is not a problem: it is how an Application synced by hand waits. Filter with `search`, `project`, `destination` or `problems_only`, and page with `limit` and `offset`. Your rules judge each row where it deploys: list on applications.argoproj.io in its destination's context and namespace; it never asks for approval and leaves out the rows only an Ask rule covers. Then: argocd_diff for what a sync would change, describe_resource or get_changes on the destination, gitops on the Application, wait_for its sync or health.",
        annotations(read_only_hint = true)
    )]
    async fn argocd_apps(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ArgoAppsParams>,
    ) -> Result<String, String> {
        let args = json!({"server": p.server, "problems_only": p.problems_only, "out_of_sync": p.out_of_sync,
            "project": p.project, "destination": p.destination, "dest_namespace": p.dest_namespace,
            "search": p.search, "limit": p.limit, "offset": p.offset});
        let call = self.begin(&parts, "argocd_apps", args)?;
        if !call.policy().may_reach_argo(&[Verb::List]) {
            return Err(call.deny(no_argo_rule(Verb::List)));
        }
        let Some(fleet) = fleet(p.server.as_deref()).await else {
            return call.finish(Ok(NO_ARGO.into()));
        };
        if let Some(unread) = none_listed(&fleet) {
            return call.finish(unread);
        }
        let (mut rows, mut hidden) = (Vec::new(), 0);
        for a in fleet.apps.iter().filter(|a| keeps(&p, a)) {
            if place_of(a).accesses(Verb::List).iter().all(|x| call.may_see(x)) {
                rows.push(a);
            } else {
                hidden += 1;
            }
        }
        let max = call.policy().max_rows.max(1) as usize;
        let page = Page {
            offset: p.offset.unwrap_or(0) as usize,
            limit: p.limit.map_or(max, |l| (l.max(1) as usize).min(max)),
        };
        let now = jiff::Timestamp::now();
        let mut planner = windows::Planner::new(now);
        let next = |a: &FleetApp| fleet.next_sync(a, &mut planner).text(now);
        let allowed = |c: &str| call.policy().context_allowed(c);
        let text = apps_text(&fleet, rows, next, allowed, hidden, &fleet_notes(&fleet), page);
        call.finish(Ok(text))
    }

    #[tool(
        description = "What syncing Argo CD Applications now would change, read through Argo CD's API (their managed resources, as the Argo CD page's diff): for each resource that differs, what changes (images, replicas, keys, fields) and its lines, a Secret's values masked, noise (checksums, defaults Kubernetes fills) folded and counted. Several Applications (environments of one service) are read as one grouped diff: each change once, with the Applications it is in. Needs get on applications.argoproj.io where each deploys (its destination's context and namespace); one Application may ask for approval, several show only those your Allow rules open.",
        annotations(read_only_hint = true)
    )]
    async fn argocd_diff(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ArgoDiffParams>,
    ) -> Result<String, String> {
        let args = json!({"server": p.server, "applications": p.applications});
        let mut call = self.begin(&parts, "argocd_diff", args)?;
        if p.applications.is_empty() {
            return Err(call.fail("name at least one Application"));
        }
        if p.applications.len() > MAX_DIFF_APPS {
            return Err(call.fail(format!("at most {MAX_DIFF_APPS} Applications at once")));
        }
        if !call.policy().may_reach_argo(&[Verb::Get]) {
            return Err(call.deny(no_argo_rule(Verb::Get)));
        }
        let Some(fleet) = fleet(p.server.as_deref()).await else {
            return call.finish(Ok(NO_ARGO.into()));
        };
        let mut picked: Vec<&FleetApp> = Vec::new();
        let mut notes = Vec::new();
        for named in &p.applications {
            let (ns, name) = named.split_once('/').map_or((None, named.as_str()), |(ns, n)| (Some(ns), n));
            match find(&fleet, p.server.as_deref(), ns, name) {
                Ok(a) if !picked.iter().any(|b| std::ptr::eq(*b, a)) => picked.push(a),
                Ok(_) => {}
                Err(why) if p.applications.len() == 1 => return call.finish(Err(anyhow::anyhow!(why))),
                Err(why) => notes.push(why),
            }
        }
        // One Application: the rules may ask. Several: those an Allow rule opens.
        if let [one] = picked.as_slice() {
            for a in place_of(one).accesses(Verb::Get) {
                self.authorize(&mut call, &a).await?;
            }
        } else {
            picked.retain(|a| {
                let open = place_of(a).accesses(Verb::Get).iter().all(|x| call.may_see(x));
                if !open {
                    notes.push(format!("{}/{}: your rules don't let you get it", a.app.namespace, a.app.name));
                }
                open
            });
            call.allowed_by(None);
        }
        let reads = picked.iter().map(|a| {
            let (endpoint, ns, name) = (a.server.clone(), a.app.namespace.clone(), a.app.name.clone());
            let blocked = grouped_diff::not_compared(&a.app);
            async move {
                match blocked {
                    Some(why) => Err(format!("not compared with git: {why}")),
                    None => api::sync_preview(endpoint, ns, name).await,
                }
            }
        });
        let diffs: Vec<Result<Arc<AppDiff>, String>> = futures::future::join_all(reads).await;
        let names: Vec<String> = picked.iter().map(|a| format!("{}/{}", a.app.namespace, a.app.name)).collect();
        let text = diff_text(&names, &diffs, &notes);
        call.finish(Ok(cap_chars(&text, MAX_DIFF_CHARS)))
    }
}

impl KubeMcp {
    /// The Application that deployed `obj` (read on the context `here`), as `describe_resource`
    /// and `get_changes` say it: by the object's tracking id or instance label, found in what
    /// Argo CD's API feeds hold (`argo::api_candidates`: no list of its own, a server down
    /// answers at once), among the Applications an Allow rule lets the agent get or list where
    /// they deploy. A label alone reads only feeds running already. None for an object that
    /// names no Application: nothing is asked of any server then.
    pub(super) async fn managed_by(&self, call: &Call, here: &str, kind: &Kind, obj: &Value) -> Option<Managed> {
        let by = argo::tracked_by(&kind.ar.group, &kind.ar.kind, obj)?;
        let key = ObjectKey::of(kind, obj);
        let policy = call.policy();
        // No server is asked for an agent no rule lets read Applications anywhere.
        if !policy.may_reach_argo(&[Verb::Get, Verb::List]) {
            return managed_text(&by, &Pick::NotFound, &key, &[], &[no_argo_rule(Verb::Get)])
                .map(|text| Managed { text, app: None });
        }
        let targets = argo::targets();
        let may_read = |_: &api::Endpoint, app: &Summary| {
            let place = ArgoPlace {
                contexts: argo::api_contexts(app, &targets),
                cluster: written_cluster(app),
                namespace: app.dest_namespace.clone(),
                name: app.name.clone(),
            };
            let allowed = |verb: Verb| policy.decide_all(&place.accesses(verb)).mode == Mode::Allow;
            allowed(Verb::Get) || allowed(Verb::List)
        };
        let start = matches!(by, TrackedBy::Id(_));
        let api =
            argo::api_candidates(here, &by, (key.group.as_str(), key.kind.as_str(), obj), |_| true, start, may_read)
                .await;
        let (found, notes, read) = match api {
            Some((candidates, notes, read)) => {
                let found = candidates
                    .into_iter()
                    .map(|c| Candidate { server: c.endpoint.name(), app: c.app, here: c.here, tracks: c.tracks })
                    .collect();
                (found, notes, read)
            }
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        let picked = pick(&by, found);
        let text = managed_text(&by, &picked, &key, &read, &notes)?;
        let app = match picked {
            Pick::One { app, .. } => Some(app),
            _ => None,
        };
        Some(Managed { text, app })
    }

    /// The Application a `gitops` or `wait_for` call names, through Argo CD's API: on
    /// `server` when given, in the Argo CD namespace `namespace` when given. An error says
    /// why none or several fit.
    pub(super) async fn argo_app(
        &self,
        call: &Call,
        server: Option<&str>,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<FleetApp, String> {
        let Some(fleet) = fleet(server).await else {
            return Err(call.fail(NO_ARGO));
        };
        find(&fleet, server, namespace, name).cloned().map_err(|why| call.fail(why))
    }

    /// Ask the rules for each request `verb` on `place` makes (one per context reaching its
    /// destination): each must go ahead; an Ask waits for the user.
    pub(super) async fn authorize_argo(&self, call: &mut Call, place: &ArgoPlace, verb: Verb) -> Result<(), String> {
        for a in place.accesses(verb) {
            self.authorize(call, &a).await?;
        }
        Ok(())
    }

    /// Right before an Argo CD write: each request decided again on the agent's current rules.
    pub(super) fn recheck_argo(&self, call: &mut Call, place: &ArgoPlace, verb: Verb) -> Result<(), String> {
        for a in place.accesses(verb) {
            self.recheck(call, &a)?;
        }
        Ok(())
    }

    /// What decides again, right as an Argo CD write is sent, whether it may still go.
    pub(super) fn argo_still_allowed(
        &self,
        call: &Call,
        place: &ArgoPlace,
        verb: Verb,
    ) -> impl Fn() -> Option<String> + Send + 'static {
        let checks: Vec<_> = place
            .accesses(verb)
            .into_iter()
            .map(|a| super::stop_check(self.hub.clone(), call.agent.id.clone(), call.approved, a, call.gone.clone()))
            .collect();
        move || checks.iter().find_map(|check| check())
    }
}

/// Whether a write by an agent to Argo CD at `endpoint` is refused whatever its rules say,
/// and the line to deny it with.
pub(super) fn server_read_only(endpoint: &api::Endpoint) -> Option<String> {
    api::agent_refusal(endpoint)
}

/// What `managed_by` found: the line to say, and the Application when it was read.
pub(super) struct Managed {
    pub text: String,
    pub app: Option<Value>,
}

/// An Application a server named for the object.
struct Candidate {
    /// The Argo CD server's name.
    server: String,
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
        server: String,
        app: Value,
        fit: Fit,
    },
    /// Several fit as well: the server and name of each.
    Several(Vec<String>),
    /// Found by name, but none deploys to this cluster or tracks the object: the server and
    /// name of each.
    Elsewhere(Vec<String>),
    NotFound,
}

/// The Application that deployed the object among those named for it. A tracking id names an
/// Application by name only, and another server may have one of the same name deploying
/// elsewhere: it must deploy to this cluster or track the object, or else be the only one
/// whose destination can't be told (`Fit::Unsure`). A label is only a hint: it must do both.
/// One that does both wins over one that does one.
fn pick(by: &TrackedBy, found: Vec<Candidate>) -> Pick {
    let name = |c: &Candidate| {
        let field = |p: &str| c.app.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string();
        format!("{} {}/{}", c.server, field("/metadata/namespace"), field("/metadata/name"))
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
                Pick::One { server: c.server, app: c.app, fit: Fit::Unsure }
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
        return Pick::One { server: c.server, app: c.app, fit };
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

/// What `problems_only` keeps: what the Argo CD page shows red or amber, and Missing health
/// with auto-sync off too (what it should deploy isn't there). Out of sync alone with
/// auto-sync off is not kept: an Application synced by hand waits for its sync so.
fn needs_a_look(app: &Summary) -> bool {
    matches!(argo::attention(app), Tone::Bad | Tone::Busy) || app.health == "Missing"
}

fn keeps(p: &ArgoAppsParams, row: &FleetApp) -> bool {
    let on = |flag: Option<bool>| flag == Some(true);
    let search = p.search.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_lowercase);
    let found = |s: &str| {
        let a = &row.app;
        [a.name.as_str(), &a.namespace, &a.project, &row.cluster, &a.dest_namespace]
            .into_iter()
            .chain(row.dest.contexts.iter().map(String::as_str))
            .any(|f| f.to_lowercase().contains(s))
    };
    (!on(p.problems_only) || needs_a_look(&row.app))
        && (!on(p.out_of_sync) || row.app.sync == "OutOfSync")
        && p.project.as_ref().is_none_or(|project| row.app.project == *project)
        && p.destination.as_ref().is_none_or(|c| row.dest.contexts.contains(c))
        && p.dest_namespace.as_ref().is_none_or(|ns| row.dest.namespace == *ns)
        && search.as_deref().is_none_or(found)
}

/// Where it deploys: the first context the agent may use, else one it may not (said), else
/// the cluster Argo CD names.
fn destination_cell(a: &FleetApp, allowed: impl Fn(&str) -> bool) -> String {
    let dest = &a.dest;
    let ns = if dest.namespace.is_empty() { String::new() } else { format!(" / {}", dest.namespace) };
    match (dest.contexts.iter().find(|c| allowed(c)), dest.contexts.first()) {
        (Some(c), _) => format!("{c}{ns}"),
        (None, Some(c)) => format!("{c}{ns} (not open to you)"),
        (None, None) if a.cluster.is_empty() => format!("?{ns}"),
        (None, None) => format!("{}{ns} (no context)", a.cluster),
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

/// The name of the Application that made `a` (an app of apps).
fn parent_name(fleet: &Fleet, a: &FleetApp) -> Option<String> {
    a.parent.and_then(|i| fleet.apps.get(i)).map(|p| p.app.name.clone())
}

/// Which rows of the reply to show.
#[derive(Clone, Copy, Debug)]
struct Page {
    offset: usize,
    limit: usize,
}

/// `argocd_apps`'s reply: those needing a look first, then the others, the page asked for;
/// what Argo CD says of each one needing a look; then what was left out and why.
fn apps_text(
    fleet: &Fleet,
    mut rows: Vec<&FleetApp>,
    mut next_sync: impl FnMut(&FleetApp) -> String,
    allowed: impl Fn(&str) -> bool,
    hidden: usize,
    notes: &[String],
    page: Page,
) -> String {
    let rank = |a: &Summary| match argo::attention(a) {
        Tone::Bad => 0,
        _ if needs_a_look(a) => 1,
        _ => 2,
    };
    rows.sort_by_cached_key(|a| (rank(&a.app), a.server.name(), a.app.namespace.clone(), a.app.name.clone()));
    let total = rows.len();
    let shown: Vec<&FleetApp> = rows.into_iter().skip(page.offset).take(page.limit).collect();
    let mut out = String::new();
    if shown.is_empty() {
        if total > 0 {
            out.push_str(&format!("No Applications past offset {} ({total} in all).\n", page.offset));
        } else if fleet.unread.is_empty() && fleet.reading.is_empty() {
            out.push_str("No Applications found.\n");
        } else {
            // Not on the servers read; those not read are said below.
            out.push_str(&format!("No Applications found on {}.\n", fleet.read().join(", ")));
        }
    } else {
        let table: Vec<Vec<String>> = shown
            .iter()
            .map(|r| {
                let a = &r.app;
                let operation = if a.in_progress {
                    "Running".to_string()
                } else {
                    a.operation.as_ref().map_or_else(|| "-".into(), |(phase, _, _)| phase.clone())
                };
                vec![
                    r.server.name(),
                    format!("{}/{}", a.namespace, a.name),
                    a.project.clone(),
                    a.sync.clone(),
                    a.health.clone(),
                    if a.automated { "on" } else { "off" }.into(),
                    operation,
                    a.revision.clone().unwrap_or_else(|| "-".into()),
                    destination_cell(r, &allowed),
                    next_sync(r),
                    parent_name(fleet, r).unwrap_or_else(|| "-".into()),
                ]
            })
            .collect();
        let headers = [
            "SERVER",
            "APPLICATION",
            "PROJECT",
            "SYNC",
            "HEALTH",
            "AUTO-SYNC",
            "OPERATION",
            "REVISION",
            "DESTINATION",
            "NEXT SYNC",
            "PARENT",
        ];
        out.push_str(&format_table(&headers, &table));
        out.push('\n');
        let looks: Vec<String> = shown
            .iter()
            .filter(|r| needs_a_look(&r.app))
            .filter_map(|r| {
                why(&r.app).map(|w| format!("  {} {}/{}: {w}", r.server.name(), r.app.namespace, r.app.name))
            })
            .collect();
        if !looks.is_empty() {
            out.push_str("\nNeeding a look:\n");
            out.push_str(&looks.join("\n"));
            out.push('\n');
        }
        let end = page.offset + shown.len();
        if page.offset > 0 || end < total {
            out.push_str(&format!("Rows {}–{end} of {total}", page.offset + 1));
            if end < total {
                out.push_str(&format!("; the next page: offset {end}"));
            }
            out.push_str(" (or filter by search, project, destination or problems_only).\n");
        }
    }
    if hidden > 0 {
        out.push_str(&format!(
            "{} your rules don't let you list (where they deploy) left out.\n",
            crate::plural::count(hidden, "Application")
        ));
    }
    for note in notes {
        out.push_str(&format!("{note}\n"));
    }
    out
}

/// Lines of one resource's diff shown, at most.
const MAX_LINES: usize = 80;

/// One resource's change: what changed, then its lines (noise left out).
fn resource_text(r: &crate::smartdiff::ResourceChange) -> String {
    use crate::history::DiffLine;
    let facts: Vec<String> = r.facts.iter().map(crate::smartdiff::Fact::text).collect();
    let mut out = if facts.is_empty() {
        format!("{}: noise only", r.key.label())
    } else {
        format!("{}: {}", r.key.label(), facts.join("; "))
    };
    if !r.noise.is_empty() {
        out.push_str(&format!(" ({} folded as noise)", crate::plural::count(r.noise.len(), "change")));
    }
    let lines = r.lines(false);
    for l in lines.iter().take(MAX_LINES) {
        out.push('\n');
        out.push_str(&match l {
            DiffLine::Gap => "    …".to_string(),
            DiffLine::Same(t) => format!("     {t}"),
            DiffLine::Removed(t) => format!("    -{t}"),
            DiffLine::Added(t) => format!("    +{t}"),
        });
    }
    if lines.len() > MAX_LINES {
        out.push_str(&format!("\n    … {} more lines", lines.len() - MAX_LINES));
    }
    out
}

/// What else one Application's diff says: resources in sync, hooks left out, and those Argo
/// CD says differ where no change is found.
fn diff_footer(d: &AppDiff) -> String {
    let mut parts = vec![format!("{} in sync", crate::plural::count(d.same, "resource"))];
    if d.hooks > 0 {
        parts.push(format!("{} left out (they run during a sync)", crate::plural::count(d.hooks, "hook")));
    }
    let mut out = parts.join(", ");
    if !d.unexplained.is_empty() {
        let names: Vec<String> = d.unexplained.iter().map(crate::smartdiff::ObjectKey::label).collect();
        out.push_str(&format!(
            "; Argo CD says these differ, though no change is found (its normalization, or noise): {}",
            names.join(", ")
        ));
    }
    out
}

/// `argocd_diff`'s reply: one Application's diff by resource; several as one grouped diff,
/// each change once with the Applications it is in; then those that could not be read.
fn diff_text(names: &[String], diffs: &[Result<Arc<AppDiff>, String>], notes: &[String]) -> String {
    let mut out = String::new();
    if let ([name], [diff]) = (names, diffs) {
        match diff {
            Ok(d) if d.in_sync() => out.push_str(&format!("{name}: in sync, a sync would change nothing.\n")),
            Ok(d) => {
                let changed = d.diff.resources.iter().filter(|r| !r.facts.is_empty()).count();
                out.push_str(&format!("{name}: a sync would change {}.\n", crate::plural::count(changed, "resource")));
                for r in &d.diff.resources {
                    out.push_str(&format!("{}\n", resource_text(r)));
                }
                out.push_str(&format!("{}.\n", diff_footer(d)));
            }
            Err(e) => out.push_str(&format!("{name}: could not be read: {e}\n")),
        }
    } else if !names.is_empty() {
        let read: Vec<Option<&crate::smartdiff::SmartDiff>> =
            diffs.iter().map(|d| d.as_ref().ok().map(|d| &d.diff)).collect();
        // Each Application's own names (its name, the client it says) replaced before values
        // are compared, as the Argo CD page does.
        let types = argo::environments::EnvTypes::default();
        let own: Vec<grouped_diff::Names> =
            names.iter().map(|n| grouped_diff::Names::new([n.clone()], types.client(n).map(String::from))).collect();
        let groups = grouped_diff::regroup(&read, &own, false);
        let with_changes = diffs.iter().filter(|d| d.as_ref().is_ok_and(|d| !d.diff.groups.is_empty())).count();
        match grouped_diff::summary(&groups, with_changes) {
            Some(s) => out.push_str(&format!("{s}\n")),
            None => out.push_str("No change in the Applications read.\n"),
        }
        for g in &groups {
            let apps: Vec<&str> = g.apps.iter().filter_map(|(i, _)| names.get(*i).map(String::as_str)).collect();
            out.push_str(&format!("  {} — in {}\n", g.text, apps.join(", ")));
            if !g.differs.is_empty() {
                let odd: Vec<&str> = g.differs.iter().filter_map(|i| names.get(*i).map(String::as_str)).collect();
                out.push_str(&format!("    otherwise than in the others: {}\n", odd.join(", ")));
            }
        }
        let in_sync: Vec<&str> = names
            .iter()
            .zip(diffs)
            .filter(|(_, d)| d.as_ref().is_ok_and(|d| d.in_sync()))
            .map(|(n, _)| n.as_str())
            .collect();
        if !in_sync.is_empty() {
            out.push_str(&format!("In sync: {}\n", in_sync.join(", ")));
        }
        let errors = grouped_diff::errors_by_text(
            diffs.iter().enumerate().filter_map(|(i, d)| Some((i, d.as_ref().err()?.as_str()))),
        );
        for (e, apps) in errors {
            let apps: Vec<&str> = apps.iter().filter_map(|i| names.get(*i).map(String::as_str)).collect();
            out.push_str(&format!("Could not be read ({}): {e}\n", apps.join(", ")));
        }
        out.push_str("For one Application's lines, call argocd_diff with it alone.\n");
    }
    for note in notes {
        out.push_str(&format!("{note}\n"));
    }
    out
}

/// What `describe_resource` and `get_changes` say of the Application that deployed an object.
/// Nothing for an instance label that names no Application found: Helm sets the same label,
/// so most objects carrying it have nothing to do with Argo CD (and no server may be known).
fn managed_text(by: &TrackedBy, found: &Pick, key: &ObjectKey, servers: &[String], notes: &[String]) -> Option<String> {
    let (named, how) = match by {
        TrackedBy::Id(t) => (
            t.app_ns.as_ref().map_or_else(|| t.app.clone(), |ns| format!("{ns}/{}", t.app)),
            "its argocd.argoproj.io/tracking-id annotation",
        ),
        TrackedBy::Label(l) => (l.clone(), "its app.kubernetes.io/instance label, Argo CD's label tracking"),
    };
    let lead = format!("Argo CD: managed by Application {named} ({how})");
    let not_read = if notes.is_empty() { String::new() } else { format!(" Not read: {}.", notes.join("; ")) };
    let (server, app, fit) = match (found, by) {
        (Pick::One { server, app, fit }, _) => (server, app, *fit),
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
        (Pick::NotFound, _) if servers.is_empty() && notes.is_empty() => return Some(format!("{lead}. {NO_ARGO}\n")),
        (Pick::NotFound, _) if servers.is_empty() => return Some(format!("{lead}.{not_read}\n")),
        (Pick::NotFound, _) if notes.is_empty() => {
            return Some(format!("{lead}, not found on {}.\n", servers.join(", ")));
        }
        (Pick::NotFound, _) => return Some(format!("{lead}, not found on the Argo CD servers read.{not_read}\n")),
    };
    let s = Summary::of(app).with_own(app, &key.group, &key.kind, &key.namespace, &key.name);
    let mut out = format!(
        "Argo CD: managed by Application {}/{} on Argo CD server {server} ({how}): {}, {}, auto-sync {}",
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
    // Another server not read may have held a surer one.
    if fit != Fit::Both && !not_read.is_empty() {
        out.push_str(&not_read);
    }
    out.push_str(&format!(
        " Act on it with gitops or wait_for: server {server}, kind applications, namespace {}, name {}.\n",
        s.namespace, s.name
    ));
    Some(out)
}

/// The access an agent's `find_problems` needs to see an Argo CD Application a scan found on
/// `context`: list on Applications in the context it deploys to (the one scanned) and its
/// destination namespace.
pub(super) fn problem_access(context: &str, dest_namespace: Option<&str>, name: &str) -> Access {
    let place = ArgoPlace {
        contexts: vec![context.to_string()],
        cluster: String::new(),
        namespace: dest_namespace.unwrap_or_default().to_string(),
        name: name.to_string(),
    };
    place.accesses(Verb::List).remove(0)
}

/// Whether an agent's rules may let it read Argo CD's Applications deploying to `context`,
/// in some namespace: `find_problems` then says which Argo CD servers it could not read.
pub(super) fn argo_open_on(policy: &Policy, context: &str) -> bool {
    let probe = problem_access(context, None, "*");
    let probe = Access { namespace: None, name: None, ..probe };
    policy.decide_any_namespace(&probe).mode != Mode::Deny
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::k8s::ContextTarget;

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
            server: None,
            problems_only: None,
            out_of_sync: None,
            project: None,
            destination: None,
            dest_namespace: None,
            search: None,
            limit: None,
            offset: None,
        }
    }

    fn endpoint(url: &str) -> Arc<api::Endpoint> {
        Arc::new(api::Endpoint {
            server: argo::server::Server::parse(url).unwrap(),
            cli_context: None,
            in_settings: true,
            insecure: false,
            login: Err("no login in tests".into()),
            on: true,
            settings_token: false,
            renewable: false,
            cli: None,
            cli_renews: false,
        })
    }

    /// A server's Applications, as `fleet` makes them: where each deploys, mapped to a
    /// context, and its parent.
    fn fleet_of(list: Vec<Value>) -> Fleet {
        let targets = [ctx("prod", "https://prod.example")];
        let apps = list
            .into_iter()
            .map(|v| {
                let a = api::ApiApp { summary: Summary::of(&v), value: v };
                ((a.summary.namespace.clone(), a.summary.name.clone()), Arc::new(a))
            })
            .collect();
        let state = api::FeedState { apps, listed: true, live: true, ..Default::default() };
        let reads = vec![argo::ApiRead { endpoint: endpoint("https://argocd.example.com"), state }];
        argo::from_api(reads, &targets, &[])
    }

    fn fleet() -> Fleet {
        let mut root = app("root", "Synced", "Healthy", true, json!({"server": "https://kubernetes.default.svc"}));
        root["status"]["resources"] =
            json!([{"group": "argoproj.io", "kind": "Application", "namespace": "argocd", "name": "web-workload"}]);
        let mut failed =
            app("api", "OutOfSync", "Healthy", false, json!({"server": "https://prod.example", "namespace": "api"}));
        failed["status"]["operationState"] =
            json!({"phase": "Failed", "message": "one or more objects failed to apply"});
        fleet_of(vec![
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
        ])
    }

    #[test]
    fn rows_map_the_destination_to_a_context_and_name_the_parent() {
        let fleet = fleet();
        let row = |name: &str| fleet.apps.iter().find(|r| r.app.name == name).unwrap();
        assert!(row("root").dest.contexts.is_empty());
        assert_eq!(row("web-workload").dest.contexts, ["prod"]);
        assert_eq!(row("web-workload").dest.namespace, "web");
        assert_eq!(parent_name(&fleet, row("web-workload")).as_deref(), Some("root"));
        assert_eq!(parent_name(&fleet, row("root")), None);
        // The context the agent may use, else one it may not, else the cluster.
        let all = |_: &str| true;
        let none = |_: &str| false;
        assert_eq!(destination_cell(row("web-workload"), all), "prod / web");
        assert_eq!(destination_cell(row("web-workload"), none), "prod / web (not open to you)");
        assert_eq!(destination_cell(row("cache"), all), "other.example / cache (no context)");
        // The rules judge each where it deploys: its contexts, else its cluster as written.
        assert_eq!(place_of(row("web-workload")).accesses(Verb::Sync)[0].context, "prod");
        assert_eq!(place_of(row("root")).accesses(Verb::Sync)[0].context, "in-cluster");
        assert_eq!(place_of(row("cache")).accesses(Verb::Sync)[0].context, "other.example");
        assert_eq!(place_of(row("cache")).accesses(Verb::Sync)[0].namespace.as_deref(), Some("cache"));
    }

    /// A rule on the destination decides, never one on the server: an Allow naming the
    /// Argo CD server opens nothing, one naming the destination context and namespace does.
    #[test]
    fn rules_decide_on_the_destination_not_the_server() {
        let fleet = fleet();
        let web = place_of(fleet.apps.iter().find(|r| r.app.name == "web-workload").unwrap());
        let policy = |rule: &str| Policy {
            rules: vec![crate::mcp::rules_file::parse_rule(Mode::Allow, rule).unwrap()],
            ..Policy::default()
        };
        let decide = |rule: &str| policy(rule).decide_all(&web.accesses(Verb::Sync)).mode;
        assert_eq!(decide("sync applications.argoproj.io --context argocd.example.com"), Mode::Deny);
        assert_eq!(decide("sync applications.argoproj.io -n web --context prod"), Mode::Allow);
        assert_eq!(decide("sync applications.argoproj.io -n other --context prod"), Mode::Deny);
        assert_eq!(decide("sync applications -n * --context *"), Mode::Allow);
        assert!(policy("sync applications.argoproj.io --context *").may_reach_argo(&[Verb::Sync]));
        assert!(!policy("sync pods --context *").may_reach_argo(&[Verb::Sync]));
        assert!(!policy("get applications.argoproj.io --context *").may_reach_argo(&[Verb::Sync]));
    }

    /// Out of sync with auto-sync off is how an Application synced by hand waits: not a
    /// problem. Missing is, auto-sync on or off; so are a failed sync and out of sync with
    /// auto-sync on.
    #[test]
    fn problems_leave_out_what_waits_for_a_sync_by_hand() {
        let fleet = fleet();
        let names = |p: &ArgoAppsParams| -> Vec<&str> {
            fleet.apps.iter().filter(|r| keeps(p, r)).map(|r| r.app.name.as_str()).collect()
        };
        assert_eq!(names(&ArgoAppsParams { problems_only: Some(true), ..params() }), ["api", "cache", "db"]);
        assert_eq!(names(&ArgoAppsParams { out_of_sync: Some(true), ..params() }).len(), 4);
        let to_prod =
            ArgoAppsParams { destination: Some("prod".into()), dest_namespace: Some("web".into()), ..params() };
        assert_eq!(names(&to_prod), ["web-workload"]);
        assert!(names(&ArgoAppsParams { project: Some("x".into()), ..params() }).is_empty());
        // A search reads the name, the project and where it deploys.
        assert_eq!(names(&ArgoAppsParams { search: Some("WEB".into()), ..params() }), ["web-workload"]);
        assert_eq!(names(&ArgoAppsParams { search: Some("other.example".into()), ..params() }), ["cache"]);
        assert_eq!(names(&ArgoAppsParams { search: Some("shop".into()), ..params() }).len(), 5);
    }

    #[test]
    fn the_reply_puts_failures_first_and_pages() {
        let fleet = fleet();
        let rows: Vec<&FleetApp> = fleet.apps.iter().collect();
        let page = Page { offset: 0, limit: 4 };
        let next = |_: &FleetApp| "now".to_string();
        let notes = ["other: could not be read".to_string()];
        let text = apps_text(&fleet, rows.clone(), next, |c| c != "prod", 2, &notes, page);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("SERVER") && lines[0].contains("NEXT SYNC"), "{text}");
        // The failed sync first, then the others needing a look, then the rest (one cut by the limit).
        assert!(lines[1].contains("argocd/api"), "{text}");
        assert!(lines[2].contains("argocd/cache"), "{text}");
        assert!(lines[3].contains("argocd/db"), "{text}");
        assert!(lines[4].contains("argocd/root"), "{text}");
        assert!(text.contains("prod / api (not open to you)"), "{text}");
        assert!(
            text.contains("argocd.example.com argocd/api: last operation Failed: one or more objects failed to apply"),
            "{text}"
        );
        assert!(text.contains("Rows 1–4 of 5; the next page: offset 4"), "{text}");
        assert!(text.contains("2 Applications your rules don't let you list (where they deploy) left out."), "{text}");
        assert!(text.contains("other: could not be read"), "{text}");
        // The next page.
        let text = apps_text(&fleet, rows.clone(), next, |_| true, 0, &[], Page { offset: 4, limit: 4 });
        assert!(text.contains("argocd/web-workload") && !text.contains("argocd/api"), "{text}");
        assert!(text.contains("Rows 5–5 of 5 (or filter"), "{text}");
        let text = apps_text(&fleet, rows, next, |_| true, 0, &[], Page { offset: 9, limit: 4 });
        assert!(text.starts_with("No Applications past offset 9 (5 in all)."), "{text}");
        assert_eq!(apps_text(&fleet, Vec::new(), next, |_| true, 0, &[], page), "No Applications found.\n");
    }

    /// One Application by name; several of the same name are said, with how to pick one.
    #[test]
    fn an_application_is_found_by_name_namespace_and_server() {
        let fleet = fleet();
        assert_eq!(find(&fleet, None, None, "web-workload").unwrap().app.name, "web-workload");
        assert!(find(&fleet, Some("ARGOCD.example.com"), Some("argocd"), "db").is_ok());
        let none = find(&fleet, None, Some("other"), "db").map(|_| ()).unwrap_err();
        assert!(none.contains("other/db not found on argocd.example.com"), "{none}");
        let mut twice = fleet;
        let mut copy = twice.apps.iter().find(|a| a.app.name == "root").unwrap().clone();
        copy.server = endpoint("https://argocd.other.example");
        twice.apps.push(copy);
        let several = find(&twice, None, None, "root").map(|_| ()).unwrap_err();
        assert!(several.contains("several Applications are named root"), "{several}");
        assert!(find(&twice, Some("argocd.other.example"), None, "root").is_ok());
    }

    /// A server that could not be read is unknown, not empty: `argocd_apps` fails when none
    /// was read, and no Application is said not found on a server whose list isn't in.
    #[test]
    fn a_server_not_read_has_unknown_applications_not_none() {
        let failed = api::FeedState { error: Some("HTTP 403".into()), ..Default::default() };
        let bad = argo::ApiRead { endpoint: endpoint("https://argocd.example.org"), state: failed };
        let none = argo::from_api(vec![bad], &[], &[]);
        let err = none_listed(&none).unwrap().unwrap_err().to_string();
        assert!(err.starts_with("Could not read any Argo CD server") && err.contains("HTTP 403"), "{err}");
        let why = find(&none, None, None, "web").map(|_| ()).unwrap_err();
        assert!(why.contains("web could not be looked for") && !why.contains("not found"), "{why}");
        // Still reading: said, not an error.
        let pending = argo::ApiRead { endpoint: endpoint("https://argocd.example.org"), state: Default::default() };
        let reading = argo::from_api(vec![pending], &[], &[]);
        assert!(none_listed(&reading).unwrap().unwrap().contains("still reading"));
        // One read, one not: not found only on the one read; the list says so too.
        let mut some = fleet();
        assert!(none_listed(&some).is_none());
        some.servers.push("argocd.example.org".into());
        some.unread.push("argocd.example.org".into());
        let why = find(&some, None, None, "web").map(|_| ()).unwrap_err();
        assert_eq!(why, "Argo CD Application web not found on argocd.example.com");
        let next = |_: &FleetApp| "now".to_string();
        let page = Page { offset: 0, limit: 4 };
        let text = apps_text(&some, Vec::new(), next, |_| true, 0, &[], page);
        assert_eq!(text, "No Applications found on argocd.example.com.\n");
    }

    #[test]
    fn argo_kinds_are_known_without_discovery() {
        for k in ["applications", "Application", "app", "applications.argoproj.io"] {
            assert!(is_argo_kind(k), "{k}");
        }
        for k in ["deployments", "kustomizations", "apps", "applicationsets"] {
            assert!(!is_argo_kind(k), "{k}");
        }
    }

    /// A `find_problems` row is seen by the rules of the context scanned (where it deploys)
    /// and its destination namespace, never by the server's.
    #[test]
    fn a_problem_row_is_seen_by_its_destination() {
        let allow = |r: &str| Policy {
            rules: vec![crate::mcp::rules_file::parse_rule(Mode::Allow, r).unwrap()],
            ..Policy::default()
        };
        let a = problem_access("prod", Some("web"), "web");
        let seen = |r: &str| super::super::problem_visible(&allow(r), &[], false, &a);
        assert!(seen("list applications.argoproj.io --context prod"));
        assert!(seen("list applications.argoproj.io -n web --context prod"));
        assert!(!seen("list applications.argoproj.io -n other --context prod"));
        assert!(!seen("list applications.argoproj.io --context argocd.example.com"));
        assert!(!seen("list pods --context prod"));
        assert!(argo_open_on(&allow("list applications.argoproj.io -n web --context prod"), "prod"));
        assert!(!argo_open_on(&allow("list applications.argoproj.io --context staging"), "prod"));
    }

    fn key() -> ObjectKey {
        ObjectKey { group: "apps".into(), kind: "Deployment".into(), namespace: "web".into(), name: "web".into() }
    }

    #[test]
    fn an_object_names_the_application_that_deployed_it() {
        let by = TrackedBy::Id(argo::parse_tracking("web-workload:apps/Deployment:web/web").unwrap());
        let mut found = app("web-workload", "OutOfSync", "Healthy", false, json!({"server": "https://prod.example"}));
        found["status"]["resources"] = json!([{"group": "apps", "kind": "Deployment", "namespace": "web", "name": "web", "status": "OutOfSync", "health": {"status": "Healthy"}}]);
        let server = "argocd.example.com".to_string();
        let one = Pick::One { server: server.clone(), app: found.clone(), fit: Fit::Both };
        let text = managed_text(&by, &one, &key(), std::slice::from_ref(&server), &[]).unwrap();
        assert!(
            text.starts_with(
                "Argo CD: managed by Application argocd/web-workload on Argo CD server argocd.example.com"
            ),
            "{text}"
        );
        assert!(text.contains("OutOfSync, Healthy, auto-sync off, revision aaaaaaa."), "{text}");
        assert!(text.contains("This object in it: OutOfSync, Healthy."), "{text}");
        assert!(
            text.contains("server argocd.example.com, kind applications, namespace argocd, name web-workload"),
            "{text}"
        );
        // No server: how to connect one.
        let text = managed_text(&by, &Pick::NotFound, &key(), &[], &[]).unwrap();
        assert!(text.contains("managed by Application web-workload (its argocd.argoproj.io/tracking-id"), "{text}");
        assert!(text.contains("Settings → Argo CD"), "{text}");
        let denied = ["argocd.example.com: your rules don't let you read Application argocd/web".to_string()];
        let text = managed_text(&by, &Pick::NotFound, &key(), std::slice::from_ref(&server), &denied).unwrap();
        assert!(text.contains("Not read: argocd.example.com: your rules don't let you read"), "{text}");
        let text = managed_text(&by, &Pick::NotFound, &key(), std::slice::from_ref(&server), &[]).unwrap();
        assert!(text.contains("not found on argocd.example.com."), "{text}");
        // Found on several servers, none of them clearly this one: said so, not "not found".
        let several = Pick::Several(vec!["a argocd/web-workload".into(), "b argocd/web-workload".into()]);
        let text = managed_text(&by, &several, &key(), &["a".into(), "b".into()], &[]).unwrap();
        assert!(text.contains("several fit, which one is unclear: a argocd/web-workload, b"), "{text}");
        let elsewhere = Pick::Elsewhere(vec!["other argocd/web-workload".into()]);
        let text = managed_text(&by, &elsewhere, &key(), &["other".into()], &[]).unwrap();
        assert!(text.contains("found (other argocd/web-workload), but none deploys to this cluster"), "{text}");
        // An instance label is Helm's too: said only once the Application is found tracking
        // the object, and never with no server.
        let label = TrackedBy::Label("web-workload".into());
        assert_eq!(managed_text(&label, &Pick::NotFound, &key(), std::slice::from_ref(&server), &[]), None);
        assert_eq!(managed_text(&label, &Pick::NotFound, &key(), &[], &[]), None);
        assert_eq!(managed_text(&label, &elsewhere, &key(), &["other".into()], &[]), None);
        let text = managed_text(&label, &one, &key(), std::slice::from_ref(&server), &[]).unwrap();
        assert!(text.contains("on Argo CD server argocd.example.com (its app.kubernetes.io/instance label"), "{text}");
    }

    /// A server not read is said beside an Application found on another when the pick isn't
    /// sure (it may hold a surer one), not when it is.
    #[test]
    fn a_server_not_read_is_said_beside_an_application_found_unless_it_is_sure() {
        let by = TrackedBy::Id(argo::parse_tracking("web-workload:apps/Deployment:web/web").unwrap());
        let skipped = ["b.example: did not answer within 10 s".to_string()];
        let found = || app("web-workload", "Synced", "Healthy", true, json!({"server": "https://prod.example"}));
        for (fit, said) in [(Fit::One, true), (Fit::Unsure, true), (Fit::Both, false)] {
            let one = Pick::One { server: "a.example".into(), app: found(), fit };
            let text = managed_text(&by, &one, &key(), &["a.example".into()], &skipped).unwrap();
            assert!(
                text.starts_with("Argo CD: managed by Application argocd/web-workload on Argo CD server a.example")
            );
            assert_eq!(text.contains("Not read: b.example: did not answer"), said, "{fit:?}: {text}");
            assert!(text.ends_with("name web-workload.\n"), "{text}");
        }
        let unsure = Pick::One {
            server: "a.example".into(),
            app: app("web-workload", "Synced", "Healthy", true, json!({"name": "prod-eu"})),
            fit: Fit::Unsure,
        };
        let text = managed_text(&by, &unsure, &key(), &["a.example".into()], &[]).unwrap();
        assert!(text.contains("Unsure: it deploys to Argo CD cluster `prod-eu`, which matches no context"), "{text}");
    }

    fn candidate(server: &str, here: bool, tracks: bool) -> Candidate {
        Candidate {
            server: server.into(),
            app: app("web", "Synced", "Healthy", true, json!({})),
            here: Some(here),
            tracks,
        }
    }

    /// A candidate whose destination can't be told (named as an Argo CD cluster Secret).
    fn unsure(server: &str) -> Candidate {
        Candidate { here: None, ..candidate(server, false, false) }
    }

    /// A tracking id's Application whose destination can't be told is taken when it is the
    /// only one so (said unsure), never over one that fits, and never for a label.
    #[test]
    fn a_lone_application_whose_destination_cant_be_told_is_taken_unsure() {
        let id = TrackedBy::Id(argo::parse_tracking("web:apps/Deployment:web/web").unwrap());
        let fit_of = |p: Pick| match p {
            Pick::One { server, fit, .. } => Some((server, fit)),
            _ => None,
        };
        assert_eq!(fit_of(pick(&id, vec![unsure("a")])), Some(("a".into(), Fit::Unsure)));
        assert_eq!(fit_of(pick(&id, vec![unsure("a"), candidate("b", false, false)])), Some(("a".into(), Fit::Unsure)));
        assert_eq!(fit_of(pick(&id, vec![unsure("a"), candidate("b", true, false)])), Some(("b".into(), Fit::One)));
        assert_eq!(fit_of(pick(&id, vec![candidate("b", true, true)])), Some(("b".into(), Fit::Both)));
        assert!(matches!(pick(&id, vec![unsure("a"), unsure("b")]), Pick::Several(n) if n.len() == 2));
        let label = TrackedBy::Label("web".into());
        assert!(matches!(pick(&label, vec![unsure("a")]), Pick::Elsewhere(_)));
    }

    /// A tracking id names an Application by name only: one of the same name on another
    /// server that neither deploys here nor tracks the object is not taken for its manager,
    /// and two that fit as well are said to be unclear.
    #[test]
    fn the_manager_must_deploy_here_or_track_the_object() {
        let id = TrackedBy::Id(argo::parse_tracking("web:apps/Deployment:web/web").unwrap());
        let server_of = |p: Pick| match p {
            Pick::One { server, .. } => server,
            _ => String::new(),
        };
        assert!(matches!(pick(&id, vec![]), Pick::NotFound));
        assert!(
            matches!(pick(&id, vec![candidate("other", false, false)]), Pick::Elsewhere(a) if a == ["other argocd/web"])
        );
        assert_eq!(server_of(pick(&id, vec![candidate("prod", false, true)])), "prod");
        assert_eq!(
            server_of(pick(&id, vec![candidate("staging", true, false), candidate("prod", true, true)])),
            "prod"
        );
        let several = pick(&id, vec![candidate("staging", false, true), candidate("prod", false, true)]);
        assert!(matches!(several, Pick::Several(a) if a == ["staging argocd/web", "prod argocd/web"]));
        let label = TrackedBy::Label("web".into());
        assert!(matches!(pick(&label, vec![candidate("prod", false, true)]), Pick::Elsewhere(_)));
        assert_eq!(server_of(pick(&label, vec![candidate("prod", true, true)])), "prod");
    }

    /// One Application's diff names each resource and its lines; several are grouped, each
    /// change once with the Applications it is in; errors are said, grouped by text.
    #[test]
    fn a_diff_reads_by_resource_or_grouped() {
        let managed = |image: &str| {
            let live = json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "namespace": "shop"},
                "spec": {"template": {"spec": {"containers": [{"name": "web", "image": "web:1.15"}]}}}});
            let mut want = live.clone();
            want["spec"]["template"]["spec"]["containers"][0]["image"] = json!(image);
            json!({"items": [{"group": "apps", "kind": "Deployment", "namespace": "shop", "name": "web", "modified": true,
                "liveState": live.to_string(), "targetState": want.to_string()}]})
        };
        let one = Arc::new(AppDiff::of(&managed("web:1.16")));
        let text = diff_text(&["argocd/web-prod".into()], &[Ok(one.clone())], &[]);
        assert!(text.starts_with("argocd/web-prod: a sync would change 1 resource."), "{text}");
        assert!(text.contains("Deployment shop/web: "), "{text}");
        assert!(text.contains("-") && text.contains("+"), "{text}");
        let names = ["argocd/web-prod".to_string(), "argocd/web-staging".into(), "argocd/web-dev".into()];
        let diffs = [Ok(one.clone()), Ok(one), Err("did not answer".to_string())];
        let text = diff_text(&names, &diffs, &["argocd/web-qa: your rules don't let you get it".into()]);
        assert!(text.contains("in 2 environments"), "{text}");
        assert!(text.contains("in argocd/web-prod, argocd/web-staging"), "{text}");
        assert!(text.contains("Could not be read (argocd/web-dev): did not answer"), "{text}");
        assert!(text.contains("argocd/web-qa: your rules don't let you get it"), "{text}");
        let same = Arc::new(AppDiff::of(&json!({"items": []})));
        assert!(diff_text(&["argocd/a".into()], &[Ok(same)], &[]).contains("in sync, a sync would change nothing"));
    }
}
