//! Why an Application drifts, keeps syncing, failed or is stuck: read from what the
//! Application keeps (its last operation, its deployments, its resources' status) and from
//! the live objects on its destination cluster.
//!
//! Git's manifests can't be read through the Kubernetes API: Argo CD's API server serves its
//! own diff, behind its own login. A live object is compared with what was last applied to
//! it, the `kubectl.kubernetes.io/last-applied-configuration` copy that Argo CD's sync writes
//! (client-side apply), and with who wrote which of its fields since the last sync
//! (`metadata.managedFields`). With server-side apply there is no such copy: only the writers
//! are said. When git moved since the last sync, the rest of the difference is in git.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use futures::StreamExt as _;
use jiff::Timestamp;
use kube::Client;
use kube::discovery::{ApiResource, Scope};
use serde_json::{Value, json};

use crate::history::DiffLine;
use crate::k8s::{self, Kind, ObjectRef};
use crate::status::Tone;

/// A sync still running after this long is stuck.
const STUCK_AFTER: i64 = 10 * 60;
/// An Application still there this long after its deletion was asked is held.
const DELETE_STUCK_AFTER: i64 = 5 * 60;
/// This many syncs of the same revision in `LOOP_WINDOW` make a loop...
const LOOP_SYNCS: usize = 3;
const LOOP_WINDOW: i64 = 60 * 60;
/// ...unless the last of them is older than this: the loop stopped.
const LOOP_RECENT: i64 = 6 * 60 * 60;
/// Live objects read for their drift, at most (the first ones Argo CD lists).
pub const DRIFT_READS: usize = 20;
/// Live objects read at the same time.
const READS_AT_ONCE: usize = 6;
/// Fields that differ kept for one object, the rest counted.
const SHOWN_FIELDS: usize = 30;
/// Objects named in a finding, the rest counted.
const SHOWN_OBJECTS: usize = 12;
/// A group's discovery is kept this long per context.
const GROUP_TTL: Duration = Duration::from_secs(600);
/// The annotation `kubectl apply` (and Argo CD's client-side apply) keeps what it applied in.
const LAST_APPLIED: &str = "kubectl.kubernetes.io/last-applied-configuration";

// MARK: Resources

/// An object an Application deploys, as its status names it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Res {
    pub group: String,
    pub kind: String,
    /// Empty for a cluster-scoped object.
    pub namespace: String,
    pub name: String,
    /// The API version Argo CD applied it with (empty when not said).
    pub version: String,
}

impl Res {
    fn of(v: &Value) -> Option<Self> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let (kind, name) = (s("kind"), s("name"));
        if kind.is_empty() || name.is_empty() {
            return None;
        }
        Some(Self { group: s("group"), kind, namespace: s("namespace"), name, version: s("version") })
    }

    /// `Deployment shop/web`, `ClusterRole reader`.
    pub fn label(&self) -> String {
        if self.namespace.is_empty() {
            format!("{} {}", self.kind, self.name)
        } else {
            format!("{} {}/{}", self.kind, self.namespace, self.name)
        }
    }

    /// The kind as `find_kind` reads it: `Deployment.apps`, `ConfigMap`.
    pub fn kind_query(&self) -> String {
        if self.group.is_empty() { self.kind.clone() } else { format!("{}.{}", self.kind, self.group) }
    }

    pub fn is_secret(&self) -> bool {
        self.group.is_empty() && self.kind == "Secret"
    }

    fn same(&self, other: &Self) -> bool {
        (&self.group, &self.kind, &self.namespace, &self.name)
            == (&other.group, &other.kind, &other.namespace, &other.name)
    }
}

// MARK: Findings

/// One thing found, worst first in a [`Diagnosis`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// Bad for a problem, Busy for something to look at, Done when it is expected.
    pub tone: Tone,
    pub title: String,
    /// What Argo CD said, in its words.
    pub message: Option<String>,
    /// The objects involved, each with what happened to it.
    pub objects: Vec<(Res, String)>,
    /// Objects left out of `objects`.
    pub more: usize,
    /// What to do about it.
    pub hint: String,
}

/// Why Argo CD could not apply something, read from its message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    Project,
    WebhookDown,
    Admission,
    MissingKind,
    MissingNamespace,
    Quota,
    Forbidden,
    Immutable,
    Invalid,
    Other,
}

impl Cause {
    pub fn of(message: &str) -> Self {
        let m = message.to_ascii_lowercase();
        let has = |s: &str| m.contains(s);
        if has("not permitted in project") {
            Self::Project
        } else if has("failed calling webhook") {
            Self::WebhookDown
        } else if has("admission webhook")
            || has("denied the request")
            || has("violates podsecurity")
            || has("validatingadmissionpolic")
        {
            Self::Admission
        } else if has("no matches for kind")
            || has("could not find the requested resource")
            || has("ensure crds are installed")
            || has("resource mapping not found")
        {
            Self::MissingKind
        } else if has("exceeded quota") {
            Self::Quota
        } else if has("namespaces \"") && has("not found") {
            Self::MissingNamespace
        } else if has("forbidden") || has("cannot create resource") || has("cannot patch resource") {
            Self::Forbidden
        } else if has("immutable") {
            Self::Immutable
        } else if has(" is invalid") || has("invalid value") || has("error validating") {
            Self::Invalid
        } else {
            Self::Other
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Project => "not permitted by its AppProject",
            Self::WebhookDown => "an admission webhook did not answer",
            Self::Admission => "rejected by admission",
            Self::MissingKind => "its kind is not served (CRD missing)",
            Self::MissingNamespace => "its namespace does not exist",
            Self::Quota => "over the namespace's quota",
            Self::Forbidden => "Argo CD is not allowed to (RBAC)",
            Self::Immutable => "a field that can't change was changed",
            Self::Invalid => "invalid",
            Self::Other => "failed",
        }
    }

    pub fn hint(self) -> &'static str {
        match self {
            Self::Project => {
                "Allow the kind or the destination in the AppProject (its resource allow lists and destinations)."
            }
            Self::WebhookDown => {
                "The webhook's Service has no ready pod, or the API server can't reach it: every write it covers fails until it answers."
            }
            Self::Admission => {
                "A policy refused it (an admission webhook, Pod Security, a ValidatingAdmissionPolicy): change the manifest to follow it, or get an exception."
            }
            Self::MissingKind => {
                "Install the CRD first (an earlier sync wave, or another Application); when it comes in the same sync, add `SkipDryRunOnMissingResource=true` to the resource's sync options."
            }
            Self::MissingNamespace => "Create the namespace, or add `CreateNamespace=true` to the sync options.",
            Self::Quota => "Raise the namespace's ResourceQuota, or lower the requests.",
            Self::Forbidden => {
                "Argo CD's service account lacks the permission on the destination cluster: give its role that verb on that kind."
            }
            Self::Immutable => {
                "Revert the field, or let the sync delete and recreate the object (the `Replace=true` sync option)."
            }
            Self::Invalid => "Fix the manifest: the message names the field.",
            Self::Other => "Read the message; the object's events may say more.",
        }
    }
}

/// A sync loop: the same revision synced again and again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncLoop {
    pub revision: String,
    pub syncs: usize,
    /// From the first of them to the last, in seconds.
    pub over: i64,
}

/// What [`Diagnosis::of`] finds in an Application.
#[derive(Clone, Debug, Default)]
pub struct Diagnosis {
    pub findings: Vec<Finding>,
    /// The resources whose live object is worth comparing: out of sync, or applied again
    /// and again by a sync loop.
    pub to_compare: Vec<Res>,
    /// Those a sync loop keeps applying: their fields go in the `ignoreDifferences` suggested.
    pub looping: Vec<Res>,
    /// When the last sync finished: writes after it are what changed since.
    pub since: Option<Timestamp>,
    /// The revision deployed and the one git is at, when they differ.
    pub moved: Option<(String, String)>,
}

fn s<'a>(v: &'a Value, p: &str) -> Option<&'a str> {
    v.pointer(p).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn arr<'a>(v: &'a Value, p: &str) -> &'a [Value] {
    v.pointer(p).and_then(Value::as_array).map_or(&[][..], Vec::as_slice)
}

fn time(v: &Value, p: &str) -> Option<Timestamp> {
    s(v, p)?.parse().ok()
}

/// A duration as said in a title: `40 s`, `12 min`, `5 h`, `3 days`.
pub fn span_text(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        s if s < 120 => format!("{s} s"),
        s if s < 2 * 3600 => format!("{} min", s.div_euclid(60)),
        s if s < 2 * 86400 => format!("{} h", s.div_euclid(3600)),
        s => format!("{} days", s.div_euclid(86400)),
    }
}

/// What a deployment or an operation synced: its revision, or its revisions joined.
fn revision_key(v: &Value) -> Option<String> {
    if let Some(r) = s(v, "/revision") {
        return Some(r.to_string());
    }
    let many: Vec<&str> = arr(v, "/revisions").iter().filter_map(Value::as_str).collect();
    (!many.is_empty()).then(|| many.join(","))
}

fn short(rev: &str) -> String {
    rev.split(',').map(super::short_revision).collect::<Vec<_>>().join(", ")
}

/// Keep the first `SHOWN_OBJECTS` objects, and count the others.
fn clip(mut objects: Vec<(Res, String)>) -> (Vec<(Res, String)>, usize) {
    let more = objects.len().saturating_sub(SHOWN_OBJECTS);
    objects.truncate(SHOWN_OBJECTS);
    (objects, more)
}

impl Diagnosis {
    /// What is wrong with the Application `app` at `now`, worst first. An Application with
    /// auto-sync off that is out of sync (one synced by hand) is not a problem by itself: it
    /// is said, neutral, with what differs.
    pub fn of(app: &Value, now: Timestamp) -> Self {
        let automated = app
            .pointer("/spec/syncPolicy/automated")
            .is_some_and(|a| a.is_object() && a.get("enabled").and_then(Value::as_bool) != Some(false));
        let mut d = Self { since: last_sync(app), moved: moved(app), ..Default::default() };
        d.findings.extend(deletion(app, now));
        d.findings.extend(stuck(app, now));
        d.findings.extend(failed(app));
        d.findings.extend(errors(app));
        let out_of_sync: Vec<&Value> =
            arr(app, "/status/resources").iter().filter(|r| r["status"] == "OutOfSync").collect();
        // Out of sync and still there: what the live object holds is worth comparing.
        let differing: Vec<Res> = out_of_sync
            .iter()
            .copied()
            .filter(|r| r["requiresPruning"] != true && s(r, "/health/status") != Some("Missing"))
            .filter_map(Res::of)
            .collect();
        if let Some(lp) = sync_loop(app, now) {
            d.looping = looping(app, &differing);
            d.findings.push(loop_finding(&lp, &d.looping));
        }
        if let Some(f) = out_of_sync_finding(&out_of_sync, automated, d.moved.as_ref()) {
            d.findings.push(f);
        }
        let mut seen = BTreeSet::new();
        d.to_compare = d
            .looping
            .iter()
            .chain(&differing)
            .filter(|r| seen.insert((r.group.clone(), r.kind.clone(), r.namespace.clone(), r.name.clone())))
            .take(DRIFT_READS)
            .cloned()
            .collect();
        d
    }

    /// Whether nothing was found.
    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }
}

/// When the last sync that worked finished: the newest deployment, else the last operation's end.
fn last_sync(app: &Value) -> Option<Timestamp> {
    let deployed = arr(app, "/status/history").iter().filter_map(|h| time(h, "/deployedAt")).max();
    deployed.or_else(|| time(app, "/status/operationState/finishedAt"))
}

/// (deployed, now in git) when git moved since the last sync.
fn moved(app: &Value) -> Option<(String, String)> {
    let target = app.pointer("/status/sync").and_then(revision_key)?;
    let newest = arr(app, "/status/history").iter().max_by_key(|h| h["id"].as_i64().unwrap_or(0));
    let deployed = newest.and_then(revision_key)?;
    (deployed != target).then(|| (short(&deployed), short(&target)))
}

/// Deleted, and still there: the finalizers holding it, and the resources it waits for.
fn deletion(app: &Value, now: Timestamp) -> Option<Finding> {
    let since = time(app, "/metadata/deletionTimestamp")?;
    let finalizers: Vec<&str> = arr(app, "/metadata/finalizers").iter().filter_map(Value::as_str).collect();
    let age = now.as_second() - since.as_second();
    let held = !finalizers.is_empty() && age >= DELETE_STUCK_AFTER;
    let lines: Vec<String> = finalizers
        .iter()
        .map(|f| {
            let what = match *f {
                "resources-finalizer.argocd.argoproj.io" | "resources-finalizer.argocd.argoproj.io/foreground" => {
                    "deletes every resource it deployed first, waiting for each"
                }
                "resources-finalizer.argocd.argoproj.io/background" => "deletes every resource it deployed first",
                f if f.starts_with("post-delete-finalizer.argocd.argoproj.io") => "runs its PostDelete hooks first",
                _ => "not Argo CD's: whatever set it removes it",
            };
            format!("{f}: {what}")
        })
        .collect();
    let objects: Vec<(Res, String)> =
        arr(app, "/status/resources").iter().filter_map(Res::of).map(|r| (r, "not deleted yet".to_string())).collect();
    let (objects, more) = clip(objects);
    let held_by = match finalizers.len() {
        0 => String::new(),
        1 => ", held by a finalizer".to_string(),
        n => format!(", held by {n} finalizers"),
    };
    Some(Finding {
        tone: if held { Tone::Bad } else { Tone::Busy },
        title: format!("Being deleted for {}{held_by}", span_text(age)),
        message: (!lines.is_empty()).then(|| lines.join("\n")),
        objects,
        more,
        hint: "Argo CD deletes each resource first: one that can't be deleted (a finalizer of its own, a namespace \
               that is terminating, an unreachable destination cluster) keeps the Application. Removing the finalizer \
               deletes the Application and leaves its resources in place."
            .into(),
    })
}

/// `group/Kind/name` in an operation's message ("waiting for completion of hook batch/Job/migrate").
fn named_in(message: &str) -> Vec<(String, String, String)> {
    message
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter_map(|word| {
            let mut parts = word.split('/');
            let (group, kind, name) = (parts.next()?, parts.next()?, parts.next()?);
            let kind_like = kind.chars().next().is_some_and(char::is_uppercase);
            (parts.next().is_none() && kind_like && !name.is_empty())
                .then(|| (group.to_string(), kind.to_string(), name.to_string()))
        })
        .collect()
}

/// Every resource the Application knows of: what it tracks, then what its last sync applied.
fn known(app: &Value) -> impl Iterator<Item = Res> + '_ {
    arr(app, "/status/resources")
        .iter()
        .chain(arr(app, "/status/operationState/syncResult/resources"))
        .filter_map(Res::of)
}

/// A sync running for long: what it waits for.
fn stuck(app: &Value, now: Timestamp) -> Option<Finding> {
    let op = app.pointer("/status/operationState")?;
    let phase = s(op, "/phase")?;
    if !matches!(phase, "Running" | "Terminating") {
        return None;
    }
    let age = now.as_second() - time(op, "/startedAt")?.as_second();
    if age < STUCK_AFTER {
        return None;
    }
    let message = s(op, "/message").map(String::from);
    let mut objects: Vec<(Res, String)> = Vec::new();
    for (group, kind, name) in named_in(message.as_deref().unwrap_or_default()) {
        let found = known(app).find(|r| r.group == group && r.kind == kind && r.name == name);
        let res = found.unwrap_or_else(|| Res { group, kind, name, ..Default::default() });
        if !objects.iter().any(|(r, _)| r.same(&res)) {
            objects.push((res, "waited for".into()));
        }
    }
    for r in arr(op, "/syncResult/resources").iter().filter(|r| r["hookPhase"] == "Running") {
        let Some(res) = Res::of(r) else { continue };
        if !objects.iter().any(|(o, _)| o.same(&res)) {
            let hook = r["hookType"].as_str().map(|h| format!("{h} hook ")).unwrap_or_default();
            objects.push((res, format!("{hook}still running")));
        }
    }
    let (objects, more) = clip(objects);
    let doing = if phase == "Terminating" { "Sync being terminated" } else { "Sync running" };
    Some(Finding {
        tone: Tone::Bad,
        title: format!("{doing} for {}", span_text(age)),
        message,
        objects,
        more,
        hint: "A hook Job that never ends, or a resource that never becomes healthy, keeps the sync running: fix it, \
               or terminate the sync in Argo CD (`argocd app terminate-op`)."
            .into(),
    })
}

/// The last sync failed: why, for each object that failed.
fn failed(app: &Value) -> Option<Finding> {
    let op = app.pointer("/status/operationState")?;
    let phase = s(op, "/phase")?;
    if !matches!(phase, "Failed" | "Error") {
        return None;
    }
    let message = s(op, "/message").map(String::from);
    let mut causes = Vec::new();
    let mut objects = Vec::new();
    for r in arr(op, "/syncResult/resources") {
        let hook_failed = matches!(r["hookPhase"].as_str(), Some("Failed" | "Error")) && r.get("hookType").is_some();
        if r["status"] != "SyncFailed" && !hook_failed {
            continue;
        }
        let Some(res) = Res::of(r) else { continue };
        let said = r["message"].as_str().unwrap_or_default();
        let text = if hook_failed {
            causes.push(None);
            format!("{} hook failed: {said}", r["hookType"].as_str().unwrap_or("A"))
        } else {
            let cause = Cause::of(said);
            causes.push(Some(cause));
            format!("{}: {said}", cause.label())
        };
        objects.push((res, text.trim_end_matches([':', ' ']).to_string()));
    }
    // The operation's own message says why when no object does ("one or more synchronization
    // tasks are not valid", a namespace that does not exist).
    let first = causes.first().copied().unwrap_or_else(|| message.as_deref().map(Cause::of));
    let all_same = causes.iter().all(|c| *c == first);
    let retries = op["retryCount"].as_i64().filter(|n| *n > 0);
    let mut title = match retries {
        Some(n) => format!("Last sync failed after {n} {}", if n == 1 { "retry" } else { "retries" }),
        None => "Last sync failed".to_string(),
    };
    if all_same {
        match first {
            None if !objects.is_empty() => title.push_str(": a hook failed"),
            Some(c) if c != Cause::Other => title = format!("{title}: {}", c.label()),
            _ => {}
        }
    }
    let hint = match first {
        None if !objects.is_empty() => {
            "Open the hook (its Job, and its pod's logs): the sync stops until it succeeds. Argo CD deletes hooks as \
             their delete policy says, so it may be gone already."
        }
        Some(c) => c.hint(),
        None => Cause::Other.hint(),
    };
    let (objects, more) = clip(objects);
    Some(Finding { tone: Tone::Bad, title, message, objects, more, hint: hint.into() })
}

/// Errors Argo CD reports in its conditions (a manifest it can't render, a repository it can't
/// read). A failed sync's `SyncError` is said by the failed sync already.
fn errors(app: &Value) -> Vec<Finding> {
    let failed = matches!(s(app, "/status/operationState/phase"), Some("Failed" | "Error"));
    arr(app, "/status/conditions")
        .iter()
        .filter_map(|c| {
            let ty = c["type"].as_str()?;
            if !ty.ends_with("Error") || (failed && ty == "SyncError") {
                return None;
            }
            let hint = match ty {
                "ComparisonError" => {
                    "Argo CD can't render the manifests or compare them (a Helm or Kustomize error, a repository it \
                     can't read): fix the source, then Refresh."
                }
                "InvalidSpecError" => "The Application's spec is wrong (its project, destination or source): fix it.",
                _ => "Read the message; Argo CD reports it on the Application.",
            };
            Some(Finding {
                tone: Tone::Bad,
                title: ty.to_string(),
                message: c["message"].as_str().filter(|m| !m.is_empty()).map(String::from),
                objects: Vec::new(),
                more: 0,
                hint: hint.into(),
            })
        })
        .collect()
}

/// The newest deployments synced the same revision `LOOP_SYNCS` times or more within
/// `LOOP_WINDOW`, without anyone asking for them (a user's syncs don't make a loop), and the
/// last of them is recent.
pub fn sync_loop(app: &Value, now: Timestamp) -> Option<SyncLoop> {
    let mut history: Vec<&Value> = arr(app, "/status/history").iter().collect();
    history.sort_by_key(|h| std::cmp::Reverse(h["id"].as_i64().unwrap_or(0)));
    let newest = *history.first()?;
    let revision = revision_key(newest)?;
    let last = time(newest, "/deployedAt")?;
    if now.as_second() - last.as_second() > LOOP_RECENT {
        return None;
    }
    let mut syncs = 0;
    let mut first = last;
    for h in history {
        let by_user = s(h, "/initiatedBy/username").is_some()
            && h.pointer("/initiatedBy/automated").and_then(Value::as_bool) != Some(true);
        let Some(at) = time(h, "/deployedAt") else { break };
        if revision_key(h).as_deref() != Some(revision.as_str()) || last.as_second() - at.as_second() > LOOP_WINDOW {
            break;
        }
        if by_user {
            continue;
        }
        syncs += 1;
        first = at;
    }
    (syncs >= LOOP_SYNCS).then(|| SyncLoop {
        revision: short(&revision),
        syncs,
        over: last.as_second() - first.as_second(),
    })
}

/// What a sync loop keeps applying: what the last sync changed (`configured`, client-side
/// apply), and what is out of sync now.
fn looping(app: &Value, differing: &[Res]) -> Vec<Res> {
    let configured = arr(app, "/status/operationState/syncResult/resources")
        .iter()
        .filter(|r| r["hookType"].is_null() && r["message"].as_str().is_some_and(|m| m.ends_with(" configured")))
        .filter_map(Res::of);
    let mut out: Vec<Res> = Vec::new();
    for r in configured.chain(differing.iter().cloned()) {
        if !out.iter().any(|o| o.same(&r)) {
            out.push(r);
        }
    }
    out
}

fn loop_finding(lp: &SyncLoop, looping: &[Res]) -> Finding {
    let objects: Vec<(Res, String)> =
        looping.iter().map(|r| (r.clone(), "applied again on each sync".into())).collect();
    let (objects, more) = clip(objects);
    let message = looping.is_empty().then(|| {
        "The last sync doesn't say which resources it changed (server-side apply): look at those that go out of \
         sync between syncs."
            .to_string()
    });
    Finding {
        tone: Tone::Busy,
        title: format!("Sync loop: revision {} synced {} times in {}", lp.revision, lp.syncs, span_text(lp.over)),
        message,
        objects,
        more,
        hint: "A controller or a webhook rewrites a field Argo CD sets, so it differs again after each sync. Ignore \
               that field with `ignoreDifferences` (and `RespectIgnoreDifferences=true` in the sync options, so the \
               sync leaves it alone too), or stop setting it in git."
            .into(),
    }
}

fn out_of_sync_finding(resources: &[&Value], automated: bool, moved: Option<&(String, String)>) -> Option<Finding> {
    if resources.is_empty() {
        return None;
    }
    let objects: Vec<(Res, String)> = resources
        .iter()
        .filter_map(|r| {
            let what = if r["requiresPruning"] == true {
                "in the cluster, no longer in git: a sync with prune deletes it"
            } else if r.pointer("/health/status").and_then(Value::as_str) == Some("Missing") {
                "in git, not in the cluster yet"
            } else {
                "differs from git"
            };
            Some((Res::of(r)?, what.to_string()))
        })
        .collect();
    let n = objects.len();
    let (objects, more) = clip(objects);
    let message = moved.map(|(deployed, target)| {
        format!("Git moved since the last sync: {deployed} deployed, {target} now. What changed in git is part of the difference.")
    });
    let hint = if automated {
        "Auto-sync is on: Argo CD should sync them. If they stay out of sync, a sync failed or something rewrites \
         them (see above)."
    } else {
        "Auto-sync is off: they change at the next sync. Not a problem by itself."
    };
    Some(Finding {
        tone: if automated { Tone::Busy } else { Tone::Done },
        title: format!("Out of sync: {n} {}", if n == 1 { "resource" } else { "resources" }),
        message,
        objects,
        more,
        hint: hint.into(),
    })
}

// MARK: Drift

/// A step in a path to a field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seg {
    Key(String),
    Index(usize),
    /// An item of a list, found by one of its fields (`containers[web]`).
    Item {
        key: String,
        value: Value,
    },
}

fn plain_key(k: &str) -> bool {
    !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `spec.template.spec.containers[web].image`, `metadata.annotations["a.io/b"]`.
pub fn path_text(path: &[Seg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Key(k) if plain_key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Seg::Key(k) => out.push_str(&format!("[{}]", Value::from(k.as_str()))),
            Seg::Index(i) => out.push_str(&format!("[{i}]")),
            Seg::Item { key, value } => {
                let v = value.as_str().map_or_else(|| value.to_string(), String::from);
                if key == "name" { out.push_str(&format!("[{v}]")) } else { out.push_str(&format!("[{key}={v}]")) }
            }
        }
    }
    out
}

/// The path as a JSON pointer (`/spec/replicas`); None when it goes through a list item found
/// by a field (a pointer can only give its index, which may move).
pub fn json_pointer(path: &[Seg]) -> Option<String> {
    let mut out = String::new();
    for seg in path {
        out.push('/');
        match seg {
            Seg::Key(k) => out.push_str(&k.replace('~', "~0").replace('/', "~1")),
            Seg::Index(i) => out.push_str(&i.to_string()),
            Seg::Item { .. } => return None,
        }
    }
    Some(out)
}

/// The path as a jq expression, as `ignoreDifferences.jqPathExpressions` takes it:
/// `.spec.template.spec.containers[] | select(.name == "web").image`.
pub fn jq_path(path: &[Seg]) -> String {
    let mut out = String::new();
    for seg in path {
        match seg {
            Seg::Key(k) if plain_key(k) && !k.starts_with(|c: char| c.is_ascii_digit()) && !k.contains('-') => {
                out.push('.');
                out.push_str(k);
            }
            Seg::Key(k) if out.is_empty() => out.push_str(&format!(".[{}]", Value::from(k.as_str()))),
            Seg::Key(k) => out.push_str(&format!("[{}]", Value::from(k.as_str()))),
            Seg::Index(i) => out.push_str(&format!("[{i}]")),
            Seg::Item { key, value } => out.push_str(&format!("[] | select(.{key} == {value})")),
        }
    }
    if out.is_empty() { ".".into() } else { out }
}

/// A field that differs: what was applied, and what is live (None: not there).
#[derive(Clone, Debug, PartialEq)]
pub struct FieldDiff {
    pub path: Vec<Seg>,
    pub applied: Option<Value>,
    pub live: Option<Value>,
}

/// Fields lists of objects are matched by, the first that every item has, once each.
const LIST_KEYS: [&str; 6] = ["name", "mountPath", "containerPort", "port", "ip", "devicePath"];

fn list_key(items: &[Value]) -> Option<&'static str> {
    LIST_KEYS.into_iter().find(|key| {
        let mut seen = BTreeSet::new();
        items.iter().all(|i| i.get(*key).is_some_and(|v| !v.is_null() && seen.insert(v.to_string())))
    })
}

/// Fields under these hold quantities: `1000m` and `1` are the same there.
fn holds_quantities(path: &[Seg]) -> bool {
    path.iter().any(
        |s| matches!(s, Seg::Key(k) if matches!(k.as_str(), "resources" | "requests" | "limits" | "capacity" | "hard")),
    )
}

/// A zero value the API server leaves out (`false`, `0`, `""`, `{}`, `[]`).
fn is_zero(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

/// Two scalars the API server would take for the same: `"80"` and `80`, `1Gi` and `1024Mi`
/// where quantities go.
fn same_scalar(a: &Value, l: &Value, path: &[Seg]) -> bool {
    if a == l {
        return true;
    }
    match (a, l) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::String(x), Value::String(y)) => holds_quantities(path) && crate::quantity::same_amount(x, y),
        (Value::String(x), other @ (Value::Number(_) | Value::Bool(_)))
        | (other @ (Value::Number(_) | Value::Bool(_)), Value::String(x)) => {
            // `"80"` set, `80` kept (an int-or-string field), or the other way round.
            serde_json::from_str::<Value>(x).is_ok_and(|parsed| parsed == *other)
        }
        _ => false,
    }
}

/// The fields set in `applied` whose live value differs, and the items the API server or a
/// webhook added to lists `applied` sets. Fields `applied` doesn't set (defaults, status) are
/// not compared.
pub fn compare(applied: &Value, live: &Value) -> Vec<FieldDiff> {
    let mut out = Vec::new();
    walk(applied, Some(live), &mut Vec::new(), &mut out);
    out
}

fn walk(a: &Value, l: Option<&Value>, path: &mut Vec<Seg>, out: &mut Vec<FieldDiff>) {
    let differs = |path: &Vec<Seg>, out: &mut Vec<FieldDiff>| {
        out.push(FieldDiff { path: path.clone(), applied: Some(a.clone()), live: l.cloned() });
    };
    // Nothing set, and nothing there (or the API server left the zero value out).
    if l.is_none_or(Value::is_null) {
        if !is_zero(a) {
            differs(path, out);
        }
        return;
    }
    let Some(l) = l else { return };
    match a {
        Value::Null => {}
        Value::Object(m) => {
            let Some(lm) = l.as_object() else { return differs(path, out) };
            for (k, v) in m {
                path.push(Seg::Key(k.clone()));
                walk(v, lm.get(k), path, out);
                path.pop();
            }
        }
        Value::Array(items) => {
            let Some(live_items) = l.as_array() else { return differs(path, out) };
            if items.iter().all(|i| !i.is_object() && !i.is_array()) {
                let here: &[Seg] = path;
                let same = items.len() == live_items.len()
                    && items.iter().zip(live_items).all(|(x, y)| same_scalar(x, y, here));
                if !same {
                    differs(path, out);
                }
                return;
            }
            if let Some(key) = list_key(items) {
                for item in items {
                    let value = item[key].clone();
                    let there = live_items.iter().find(|li| li.get(key) == Some(&value));
                    path.push(Seg::Item { key: key.to_string(), value });
                    walk(item, there, path, out);
                    path.pop();
                }
                // Added by something else: a webhook's sidecar, a controller's entry.
                for li in live_items {
                    let Some(value) = li.get(key).filter(|v| !v.is_null()) else { continue };
                    if !items.iter().any(|i| i.get(key) == Some(value)) {
                        path.push(Seg::Item { key: key.to_string(), value: value.clone() });
                        out.push(FieldDiff { path: path.clone(), applied: None, live: Some(li.clone()) });
                        path.pop();
                    }
                }
            } else {
                for (i, item) in items.iter().enumerate() {
                    path.push(Seg::Index(i));
                    walk(item, live_items.get(i), path, out);
                    path.pop();
                }
                for (i, li) in live_items.iter().enumerate().skip(items.len()) {
                    path.push(Seg::Index(i));
                    out.push(FieldDiff { path: path.clone(), applied: None, live: Some(li.clone()) });
                    path.pop();
                }
            }
        }
        scalar => {
            if !same_scalar(scalar, l, path) {
                differs(path, out);
            }
        }
    }
}

/// Who wrote fields of a live object after the last sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Writer {
    pub manager: String,
    pub time: Timestamp,
    /// The fields it owns, in a few paths.
    pub fields: String,
}

/// Fields every write touches, which say nothing of what changed.
const NOISE: [&str; 2] = [LAST_APPLIED, "deployment.kubernetes.io/revision"];

/// The field managers other than Argo CD that wrote the object after `since` (all of them
/// when it never synced), newest first. Status writes are left out.
pub fn writers(live: &Value, since: Option<Timestamp>) -> Vec<Writer> {
    let mut out: Vec<Writer> = arr(live, "/metadata/managedFields")
        .iter()
        .filter(|m| m["subresource"] != "status")
        .filter_map(|m| {
            let manager = s(m, "/manager").unwrap_or("an unnamed manager");
            if manager.starts_with("argocd") {
                return None;
            }
            let at = time(m, "/time")?;
            if since.is_some_and(|since| at <= since) {
                return None;
            }
            let mut paths = Vec::new();
            crate::history::field_paths(m.get("fieldsV1")?, &mut Vec::new(), &mut paths);
            paths
                .retain(|p| p.first().is_some_and(|s| s != "status") && !p.iter().any(|s| NOISE.contains(&s.as_str())));
            if paths.is_empty() {
                return None;
            }
            Some(Writer { manager: manager.to_string(), time: at, fields: crate::history::fields_summary(&paths) })
        })
        .collect();
    out.sort_by_key(|w| std::cmp::Reverse(w.time));
    out
}

/// What a live object was compared with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Compared {
    /// The copy of what was last applied (client-side apply).
    LastApplied,
    /// Argo CD applies it server side: no copy of what it applied, only who wrote since.
    ServerSide,
    /// Neither: only who wrote since.
    NoCopy,
    /// Not in the cluster.
    Gone,
    /// Not read, and why.
    NotRead(String),
}

/// How one live object differs from what was applied to it.
#[derive(Clone, Debug, PartialEq)]
pub struct Drift {
    pub res: Res,
    pub compared: Compared,
    pub fields: Vec<FieldDiff>,
    /// Fields that differ left out of `fields`.
    pub more: usize,
    pub writers: Vec<Writer>,
}

/// A Secret's `stringData`, as the `data` the API server keeps it in.
fn secret_as_stored(applied: &mut Value) {
    let Some(string_data) = applied.as_object_mut().and_then(|o| o.remove("stringData")) else { return };
    let Some(entries) = string_data.as_object() else { return };
    let data = applied.as_object_mut().map(|o| o.entry("data").or_insert_with(|| json!({})));
    if let Some(data) = data.and_then(Value::as_object_mut) {
        for (k, v) in entries {
            let text = v.as_str().map_or_else(|| v.to_string(), String::from);
            data.insert(k.clone(), Value::from(base64::engine::general_purpose::STANDARD.encode(text)));
        }
    }
}

/// How the live object `live` of `res` differs from what was last applied to it, and who
/// wrote it after `since`.
pub fn drift_of(res: &Res, live: &Value, since: Option<Timestamp>) -> Drift {
    let applied = live
        .pointer("/metadata/annotations")
        .and_then(|a| a.get(LAST_APPLIED))
        .and_then(Value::as_str)
        .and_then(|text| serde_json::from_str::<Value>(text).ok())
        .filter(Value::is_object);
    let server_side = arr(live, "/metadata/managedFields")
        .iter()
        .any(|m| m["operation"] == "Apply" && s(m, "/manager").is_some_and(|n| n.starts_with("argocd")));
    let (compared, mut fields) = match applied {
        Some(mut applied) => {
            if res.is_secret() {
                secret_as_stored(&mut applied);
            }
            (Compared::LastApplied, compare(&applied, live))
        }
        None if server_side => (Compared::ServerSide, Vec::new()),
        None => (Compared::NoCopy, Vec::new()),
    };
    let more = fields.len().saturating_sub(SHOWN_FIELDS);
    fields.truncate(SHOWN_FIELDS);
    Drift { res: res.clone(), compared, fields, more, writers: writers(live, since) }
}

impl Drift {
    fn not_read(res: &Res, compared: Compared) -> Self {
        Self { res: res.clone(), compared, fields: Vec::new(), more: 0, writers: Vec::new() }
    }

    /// The fields that differ as diff lines: what was applied removed, what is live added.
    /// A Secret's values are never shown.
    pub fn lines(&self) -> Vec<DiffLine> {
        let text = |path: &str, v: Option<&Value>, live: bool| -> String {
            let Some(v) = v else { return String::new() };
            if self.res.is_secret() {
                return field_text(path, &Value::from(if live { "(hidden, differs)" } else { "(hidden)" }));
            }
            field_text(path, v)
        };
        self.fields
            .iter()
            .flat_map(|f| {
                let path = path_text(&f.path);
                crate::history::diff_lines(&text(&path, f.applied.as_ref(), false), &text(&path, f.live.as_ref(), true))
            })
            .filter(|l| *l != DiffLine::Gap)
            .collect()
    }
}

/// `path: value`, a value of several lines (an object, a list, a text) as YAML under the path.
fn field_text(path: &str, v: &Value) -> String {
    let body = match v {
        Value::String(text) if text.is_empty() => "\"\"".to_string(),
        Value::String(text) if !text.contains('\n') => text.clone(),
        Value::String(text) => format!("|\n{}", text.trim_end()),
        Value::Object(_) | Value::Array(_) => crate::yaml::to_string(v).unwrap_or_default().trim_end().to_string(),
        other => other.to_string(),
    };
    let block = body.lines().count() > 1 && !matches!(v, Value::String(_));
    let mut lines = body.lines();
    let first = lines.next().unwrap_or_default();
    // A block under the path: `path:` then the block indented.
    let mut out = if block { format!("{path}:\n  {first}\n") } else { format!("{path}: {first}\n") };
    for line in lines {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// The `ignoreDifferences` that would stop a sync loop over what differs in `drifts`: the
/// fields that differ, else the managers that wrote it since the last sync. None when
/// nothing is known.
pub fn ignore_differences(drifts: &[&Drift]) -> Option<String> {
    let entries: Vec<Value> = drifts
        .iter()
        .filter_map(|d| {
            let mut e = json!({"kind": d.res.kind, "name": d.res.name});
            if !d.res.group.is_empty() {
                e["group"] = json!(d.res.group);
            }
            if !d.res.namespace.is_empty() {
                e["namespace"] = json!(d.res.namespace);
            }
            let pointers: Vec<String> = d.fields.iter().filter_map(|f| json_pointer(&f.path)).collect();
            let jq: Vec<String> =
                d.fields.iter().filter(|f| json_pointer(&f.path).is_none()).map(|f| jq_path(&f.path)).collect();
            if !pointers.is_empty() {
                e["jsonPointers"] = json!(pointers);
            }
            if !jq.is_empty() {
                e["jqPathExpressions"] = json!(jq);
            }
            if d.fields.is_empty() {
                let managers: BTreeSet<&str> = d.writers.iter().map(|w| w.manager.as_str()).collect();
                if managers.is_empty() {
                    return None;
                }
                e["managedFieldsManagers"] = json!(managers);
            }
            Some(e)
        })
        .collect();
    if entries.is_empty() {
        return None;
    }
    let spec = json!({"spec": {
        "ignoreDifferences": entries,
        "syncPolicy": {"syncOptions": ["RespectIgnoreDifferences=true"]},
    }});
    crate::yaml::to_string(&spec).ok()
}

// MARK: Reading

/// The drift of an Application's resources on its destination, as a page shows it.
#[derive(Clone, Debug, Default)]
pub struct DriftRead {
    /// The connected context that reaches the destination, the live objects read on it.
    pub context: Option<String>,
    pub drifts: Vec<Drift>,
    /// Why nothing could be read.
    pub note: Option<String>,
}

/// One group's kinds on a context, as discovery gave them.
type GroupKinds = Arc<Vec<(ApiResource, bool)>>;

/// Each group's kinds by (context, group), with when they were discovered.
type Groups = HashMap<(String, String), (Instant, GroupKinds)>;

/// Each context's groups, discovered when first needed.
static GROUPS: LazyLock<Mutex<Groups>> = LazyLock::new(Default::default);

/// The kinds of one API group on a context (every version), discovered once a while.
async fn group_kinds(client: &Client, context: &str, group: &str) -> anyhow::Result<GroupKinds> {
    let key = (context.to_string(), group.to_string());
    let cached = GROUPS.lock().unwrap_or_else(PoisonError::into_inner).get(&key).cloned();
    if let Some((at, kinds)) = cached
        && at.elapsed() < GROUP_TTL
    {
        return Ok(kinds);
    }
    let g = kube::discovery::group(client, group).await?;
    let mut kinds = Vec::new();
    for v in g.versions() {
        kinds.extend(g.versioned_resources(v).into_iter().map(|(ar, caps)| (ar, caps.scope == Scope::Namespaced)));
    }
    let kinds = Arc::new(kinds);
    GROUPS.lock().unwrap_or_else(PoisonError::into_inner).insert(key, (Instant::now(), kinds.clone()));
    Ok(kinds)
}

/// The kind to read `res` with: the version Argo CD applied it with, else any.
fn kind_for(kinds: &[(ApiResource, bool)], res: &Res) -> Option<Kind> {
    let of = |v: bool| kinds.iter().find(|(ar, _)| ar.kind == res.kind && (!v || ar.version == res.version));
    let (ar, namespaced) = of(true).or_else(|| of(false))?;
    Some(Kind { ar: ar.clone(), namespaced: *namespaced, section: "", label: res.kind.clone() })
}

/// Who may read what: the app reads what its user may (the API server decides), an agent
/// what its rules allow.
pub type MayRead = Arc<dyn Fn(&Kind, &Res) -> bool + Send + Sync>;

/// Read each of `targets` on the context `context` and say how it differs from what was
/// applied. `kinds` are the context's discovered kinds when known (else each group is
/// discovered). One object that can't be read is said, the others are still read.
pub async fn read_drift(
    client: Client,
    context: String,
    kinds: Option<Arc<Vec<Kind>>>,
    targets: Vec<Res>,
    since: Option<Timestamp>,
    may_read: MayRead,
) -> Vec<Drift> {
    let asked = targets.clone();
    let read = async move {
        let reads = targets.into_iter().map(|res| {
            let (client, context, kinds, may_read) = (client.clone(), context.clone(), kinds.clone(), may_read.clone());
            async move {
                let kind = match &kinds {
                    Some(kinds) => {
                        let listed: Vec<(ApiResource, bool)> = kinds
                            .iter()
                            .filter(|k| k.ar.group == res.group)
                            .map(|k| (k.ar.clone(), k.namespaced))
                            .collect();
                        kind_for(&listed, &res)
                    }
                    None => match group_kinds(&client, &context, &res.group).await {
                        Ok(listed) => kind_for(&listed, &res),
                        Err(e) => {
                            let why = format!("its API group: {}", k8s::lists::load_error(&e));
                            return Drift::not_read(&res, Compared::NotRead(why));
                        }
                    },
                };
                let Some(kind) = kind else {
                    return Drift::not_read(&res, Compared::NotRead(format!("{} is not served", res.kind_query())));
                };
                if !may_read(&kind, &res) {
                    return Drift::not_read(&res, Compared::NotRead("your rules don't allow get on it".into()));
                }
                let namespace = kind.namespaced.then(|| res.namespace.clone()).filter(|ns| !ns.is_empty());
                let obj = ObjectRef { kind, name: res.name.clone(), namespace, uid: String::new() };
                match k8s::get_object_opts(client, obj, true).await {
                    Ok(live) => drift_of(&res, &live, since),
                    Err(e) if k8s::is_not_found(&e) => Drift::not_read(&res, Compared::Gone),
                    Err(e) => Drift::not_read(&res, Compared::NotRead(k8s::lists::load_error(&e))),
                }
            }
        });
        Ok::<_, anyhow::Error>(futures::stream::iter(reads).buffered(READS_AT_ONCE).collect::<Vec<Drift>>().await)
    };
    k8s::run(read).await.unwrap_or_else(|e| {
        let why = k8s::short_error(&e);
        asked.iter().map(|res| Drift::not_read(res, Compared::NotRead(why.clone()))).collect()
    })
}

/// The drift of the Application `app`'s resources, read on `on` (the context that reaches its
/// destination) when it is one of the connected `clusters`.
pub async fn drift_for(app: &Value, on: Option<&str>, clusters: &[(String, Client)]) -> DriftRead {
    let connected = on.and_then(|c| clusters.iter().find(|(n, _)| n == c));
    let Some((context, client)) = connected else {
        let note = match on {
            Some(c) => {
                format!("{c} is not connected: open the destination to connect it, and compare the live objects.")
            }
            None => "No context of your kubeconfig reaches its destination: the live objects can't be read.".into(),
        };
        return DriftRead { note: Some(note), ..Default::default() };
    };
    let d = Diagnosis::of(app, Timestamp::now());
    let mut read = DriftRead { context: Some(context.clone()), ..Default::default() };
    if !d.to_compare.is_empty() {
        let all: MayRead = Arc::new(|_: &Kind, _: &Res| true);
        read.drifts = read_drift(client.clone(), context.clone(), None, d.to_compare, d.since, all).await;
    }
    read
}

// MARK: Report

/// What the source of a drift says, in one line.
pub fn compared_text(c: &Compared) -> &str {
    match c {
        Compared::LastApplied => "what was last applied (last-applied-configuration) vs live",
        Compared::ServerSide => "applied server side: no copy of what was applied, only who wrote since",
        Compared::NoCopy => "no copy of what was applied, only who wrote since",
        Compared::Gone => "not in the cluster",
        Compared::NotRead(why) => why,
    }
}

/// The diagnosis as text, for an agent.
pub fn report(head: &str, d: &Diagnosis, read: &DriftRead) -> String {
    let mut out = vec![head.to_string()];
    if d.findings.is_empty() {
        out.push("Nothing wrong found: no failed or stuck sync, no sync loop, nothing out of sync.".into());
    }
    for f in &d.findings {
        let level = match f.tone {
            Tone::Bad => "problem",
            Tone::Busy => "warning",
            Tone::Good | Tone::Done => "info",
        };
        out.push(format!("\n[{level}] {}", f.title));
        if let Some(m) = &f.message {
            out.extend(m.lines().map(|l| format!("  {l}")));
        }
        out.extend(f.objects.iter().map(|(r, what)| format!("  - {}: {what}", r.label())));
        if f.more > 0 {
            out.push(format!("  - … {} more", f.more));
        }
        out.push(format!("  hint: {}", f.hint));
    }
    if let Some(note) = &read.note {
        out.push(format!("\nLive objects: {note}"));
    }
    if !read.drifts.is_empty() {
        let on = read.context.as_deref().unwrap_or("?");
        out.push(format!(
            "\nDrift on {on} (git's manifests aren't readable through the Kubernetes API: compared with what was last applied):"
        ));
        for drift in &read.drifts {
            out.push(format!("{} — {}", drift.res.label(), compared_text(&drift.compared)));
            for line in drift.lines() {
                match line {
                    DiffLine::Removed(t) => out.push(format!("  - {t}")),
                    DiffLine::Added(t) => out.push(format!("  + {t}")),
                    DiffLine::Same(t) => out.push(format!("    {t}")),
                    DiffLine::Gap => {}
                }
            }
            if drift.more > 0 {
                out.push(format!("  … {} more fields differ", drift.more));
            }
            if drift.compared == Compared::LastApplied && drift.fields.is_empty() {
                out.push("  every field applied has its applied value".into());
            }
            for w in &drift.writers {
                out.push(format!("  written since the last sync by {} at {}: {}", w.manager, w.time, w.fields));
            }
        }
    }
    let looping: Vec<&Drift> = read.drifts.iter().filter(|dr| d.looping.iter().any(|r| r.same(&dr.res))).collect();
    if let Some(snippet) = ignore_differences(&looping) {
        out.push(format!("\nTo stop the sync loop, the Application could ignore what differs:\n{snippet}"));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Timestamp {
        "2026-10-30T12:00:00Z".parse().unwrap()
    }

    fn deployment() -> Res {
        Res {
            group: "apps".into(),
            kind: "Deployment".into(),
            namespace: "shop".into(),
            name: "web".into(),
            version: "v1".into(),
        }
    }

    #[test]
    fn a_workload_app_out_of_sync_is_said_but_not_a_problem() {
        let app = json!({
            "metadata": {"name": "shop-workload", "namespace": "shop-argo"},
            "spec": {"syncPolicy": {"syncOptions": ["CreateNamespace=true"]}},
            "status": {
                "sync": {"status": "OutOfSync", "revision": "b".repeat(40)},
                "history": [{"id": 1, "revision": "a".repeat(40), "deployedAt": "2026-10-29T12:00:00Z", "initiatedBy": {"username": "ana"}}],
                "operationState": {"phase": "Succeeded", "finishedAt": "2026-10-29T12:00:00Z"},
                "resources": [
                    {"group": "apps", "version": "v1", "kind": "Deployment", "namespace": "shop", "name": "web", "status": "OutOfSync", "health": {"status": "Healthy"}},
                    {"version": "v1", "kind": "ConfigMap", "namespace": "shop", "name": "new", "status": "OutOfSync", "health": {"status": "Missing"}},
                    {"version": "v1", "kind": "ConfigMap", "namespace": "shop", "name": "old", "status": "OutOfSync", "requiresPruning": true},
                    {"version": "v1", "kind": "Service", "namespace": "shop", "name": "web", "status": "Synced"}
                ]
            }
        });
        let d = Diagnosis::of(&app, now());
        assert_eq!(d.findings.len(), 1, "{:?}", d.findings);
        let f = &d.findings[0];
        assert_eq!(f.tone, Tone::Done);
        assert_eq!(f.title, "Out of sync: 3 resources");
        assert!(f.hint.contains("Auto-sync is off"), "{}", f.hint);
        assert!(f.message.as_deref().unwrap().contains("aaaaaaaa deployed, bbbbbbbb now"), "{:?}", f.message);
        let what: Vec<&str> = f.objects.iter().map(|(_, w)| w.as_str()).collect();
        assert_eq!(
            what,
            [
                "differs from git",
                "in git, not in the cluster yet",
                "in the cluster, no longer in git: a sync with prune deletes it"
            ]
        );
        // Only what is there is read: not the one to create, nor the one to prune.
        assert_eq!(d.to_compare, [deployment()]);
        assert!(d.looping.is_empty());
        // With auto-sync on, the same is something to look at.
        let mut auto = app;
        auto["spec"]["syncPolicy"]["automated"] = json!({"selfHeal": true});
        assert_eq!(Diagnosis::of(&auto, now()).findings[0].tone, Tone::Busy);
    }

    #[test]
    fn nothing_found_for_a_synced_app() {
        let app = json!({"status": {"sync": {"status": "Synced"}, "operationState": {"phase": "Succeeded"},
            "resources": [{"kind": "ConfigMap", "name": "a", "namespace": "x", "status": "Synced"}]}});
        assert!(Diagnosis::of(&app, now()).is_empty());
    }

    #[test]
    fn a_failed_hook_is_named_with_its_object() {
        let app = json!({"status": {"operationState": {
            "phase": "Failed",
            "message": "one or more synchronization tasks completed unsuccessfully",
            "retryCount": 2,
            "syncResult": {"resources": [
                {"group": "batch", "version": "v1", "kind": "Job", "namespace": "shop", "name": "migrate", "hookType": "PreSync", "hookPhase": "Failed", "status": "Synced", "message": "Job has reached the specified backoff limit"},
                {"group": "apps", "version": "v1", "kind": "Deployment", "namespace": "shop", "name": "web", "status": "Synced", "message": "deployment.apps/web unchanged"}
            ]}
        }}});
        let d = Diagnosis::of(&app, now());
        let f = &d.findings[0];
        assert_eq!(f.tone, Tone::Bad);
        assert_eq!(f.title, "Last sync failed after 2 retries: a hook failed");
        assert_eq!(f.objects.len(), 1);
        assert_eq!(f.objects[0].0.label(), "Job shop/migrate");
        assert_eq!(f.objects[0].1, "PreSync hook failed: Job has reached the specified backoff limit");
        assert!(f.hint.contains("hook"), "{}", f.hint);
    }

    #[test]
    fn an_admission_rejection_and_a_missing_crd_are_told_apart() {
        let app = |message: &str| {
            json!({"status": {"operationState": {"phase": "Failed", "message": "one or more objects failed to apply",
                "syncResult": {"resources": [{"group": "apps", "kind": "Deployment", "namespace": "shop", "name": "web", "status": "SyncFailed", "message": message}]}}}})
        };
        let denied = "admission webhook \"validate.kyverno.svc-fail\" denied the request: require-limits: CPU limits are required";
        let f = &Diagnosis::of(&app(denied), now()).findings[0];
        assert_eq!(f.title, "Last sync failed: rejected by admission");
        assert_eq!(f.objects[0].1, format!("rejected by admission: {denied}"));
        assert_eq!(f.hint, Cause::Admission.hint());
        let crd = "the server could not find the requested resource";
        let f = &Diagnosis::of(&app(crd), now()).findings[0];
        assert_eq!(f.title, "Last sync failed: its kind is not served (CRD missing)");
        assert!(f.hint.contains("SkipDryRunOnMissingResource"));
        // Only the operation's message: it is read too.
        let only = json!({"status": {"operationState": {"phase": "Failed",
            "message": "error validating data: no matches for kind \"ServiceMonitor\" in version \"monitoring.coreos.com/v1\"; ensure CRDs are installed first"}}});
        assert_eq!(Diagnosis::of(&only, now()).findings[0].hint, Cause::MissingKind.hint());
        assert_eq!(Cause::of("pods \"x\" is forbidden: violates PodSecurity \"restricted:latest\""), Cause::Admission);
        assert_eq!(
            Cause::of("Internal error occurred: failed calling webhook \"x\": connection refused"),
            Cause::WebhookDown
        );
        assert_eq!(Cause::of("pods \"x\" is forbidden: exceeded quota: q"), Cause::Quota);
        assert_eq!(Cause::of("namespaces \"shop\" not found"), Cause::MissingNamespace);
        assert_eq!(
            Cause::of("Deployment.apps \"web\" is invalid: spec.selector: Invalid value: field is immutable"),
            Cause::Immutable
        );
        assert_eq!(Cause::of("resource apps:Deployment is not permitted in project team"), Cause::Project);
        assert_eq!(
            Cause::of(
                "deployments.apps is forbidden: User \"system:serviceaccount:argocd:argocd-application-controller\" cannot patch resource"
            ),
            Cause::Forbidden
        );
    }

    #[test]
    fn a_long_sync_is_stuck_on_what_it_waits_for() {
        let app = |started: &str| {
            json!({"status": {
                "resources": [{"group": "batch", "version": "v1", "kind": "Job", "namespace": "shop", "name": "migrate"}],
                "operationState": {"phase": "Running", "startedAt": started,
                    "message": "waiting for completion of hook batch/Job/migrate and 1 more hooks",
                    "syncResult": {"resources": [{"kind": "Pod", "namespace": "shop", "name": "check", "hookType": "Sync", "hookPhase": "Running"}]}}
            }})
        };
        let f = &Diagnosis::of(&app("2026-10-30T11:18:00Z"), now()).findings[0];
        assert_eq!(f.tone, Tone::Bad);
        assert_eq!(f.title, "Sync running for 42 min");
        let named: Vec<(String, &str)> = f.objects.iter().map(|(r, w)| (r.label(), w.as_str())).collect();
        assert_eq!(
            named,
            [("Job shop/migrate".to_string(), "waited for"), ("Pod shop/check".to_string(), "Sync hook still running")]
        );
        // A sync that just started is not stuck.
        assert!(Diagnosis::of(&app("2026-10-30T11:58:00Z"), now()).is_empty());
    }

    #[test]
    fn a_finalizer_holding_the_deletion_is_explained() {
        let app = json!({
            "metadata": {"deletionTimestamp": "2026-10-30T11:00:00Z", "finalizers": ["resources-finalizer.argocd.argoproj.io"]},
            "status": {"resources": [{"version": "v1", "kind": "PersistentVolumeClaim", "namespace": "shop", "name": "data"}]}
        });
        let f = &Diagnosis::of(&app, now()).findings[0];
        assert_eq!(f.tone, Tone::Bad);
        assert_eq!(f.title, "Being deleted for 60 min, held by a finalizer");
        assert!(
            f.message.as_deref().unwrap().starts_with("resources-finalizer.argocd.argoproj.io: deletes every resource")
        );
        assert_eq!(f.objects[0].0.label(), "PersistentVolumeClaim shop/data");
        // Just asked: on its way, not held yet.
        let mut fresh = app;
        fresh["metadata"]["deletionTimestamp"] = json!("2026-10-30T11:59:00Z");
        assert_eq!(Diagnosis::of(&fresh, now()).findings[0].tone, Tone::Busy);
    }

    #[test]
    fn the_same_revision_synced_again_and_again_is_a_loop() {
        let sha = "c".repeat(40);
        let entry = |id: i64, at: &str, rev: &str| json!({"id": id, "revision": rev, "deployedAt": at, "initiatedBy": {"automated": true}});
        let app = |history: Vec<Value>| {
            json!({"spec": {"syncPolicy": {"automated": {"selfHeal": true}}}, "status": {
                "sync": {"status": "OutOfSync", "revision": sha},
                "history": history,
                "operationState": {"phase": "Succeeded", "syncResult": {"resources": [
                    {"group": "apps", "version": "v1", "kind": "Deployment", "namespace": "shop", "name": "web", "message": "deployment.apps/web configured"},
                    {"version": "v1", "kind": "Service", "namespace": "shop", "name": "web", "message": "service/web unchanged"}
                ]}},
                "resources": [{"group": "apps", "version": "v1", "kind": "Deployment", "namespace": "shop", "name": "web", "status": "OutOfSync", "health": {"status": "Healthy"}}]
            }})
        };
        let looping = app(vec![
            entry(1, "2026-10-29T08:00:00Z", "older"),
            entry(2, "2026-10-30T11:20:00Z", &sha),
            entry(3, "2026-10-30T11:35:00Z", &sha),
            entry(4, "2026-10-30T11:50:00Z", &sha),
        ]);
        let lp = sync_loop(&looping, now()).unwrap();
        assert_eq!(lp, SyncLoop { revision: "cccccccc".into(), syncs: 3, over: 30 * 60 });
        let d = Diagnosis::of(&looping, now());
        assert_eq!(d.looping, [deployment()]);
        assert_eq!(d.to_compare, [deployment()]);
        let f = d.findings.iter().find(|f| f.title.starts_with("Sync loop")).unwrap();
        assert_eq!(f.title, "Sync loop: revision cccccccc synced 3 times in 30 min");
        assert!(f.hint.contains("ignoreDifferences"));
        // Spread over hours, or long over: not a loop.
        let slow = app(vec![
            entry(2, "2026-10-30T08:00:00Z", &sha),
            entry(3, "2026-10-30T10:00:00Z", &sha),
            entry(4, "2026-10-30T11:50:00Z", &sha),
        ]);
        assert!(sync_loop(&slow, now()).is_none());
        let over = app(vec![
            entry(2, "2026-10-28T11:20:00Z", &sha),
            entry(3, "2026-10-28T11:35:00Z", &sha),
            entry(4, "2026-10-28T11:50:00Z", &sha),
        ]);
        assert!(sync_loop(&over, now()).is_none());
        // A user syncing three times by hand is not a loop.
        let by_hand: Vec<Value> = (2..5)
            .map(|i| json!({"id": i, "revision": sha, "deployedAt": format!("2026-10-30T11:{}:00Z", 20 + i * 5), "initiatedBy": {"username": "ana"}}))
            .collect();
        assert!(sync_loop(&app(by_hand), now()).is_none());
    }

    #[test]
    fn drift_names_the_fields_that_differ() {
        let applied = json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "shop", "labels": {"app": "web"}, "annotations": {"argocd.argoproj.io/sync-wave": "1"}},
            "spec": {
                "replicas": 2,
                "paused": false,
                "template": {"spec": {
                    "containers": [{"name": "web", "image": "web:1", "ports": [{"containerPort": 8080}],
                        "resources": {"limits": {"cpu": "1000m", "memory": "1Gi"}}, "args": ["--a"]}],
                    "tolerations": [{"key": "a", "operator": "Exists"}]
                }}
            }
        });
        let live = json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "shop", "labels": {"app": "web"}, "annotations": {"argocd.argoproj.io/sync-wave": "1", "other": "x"}},
            "spec": {
                "replicas": 5,
                "progressDeadlineSeconds": 600,
                "template": {"spec": {
                    "containers": [
                        {"name": "web", "image": "web:1", "imagePullPolicy": "IfNotPresent", "ports": [{"containerPort": 8080, "protocol": "TCP"}],
                            "resources": {"limits": {"cpu": "1", "memory": "1024Mi"}}, "args": ["--a"]},
                        {"name": "istio-proxy", "image": "proxy:1"}
                    ],
                    "tolerations": [{"key": "a", "operator": "Exists"}]
                }}
            }
        });
        let fields = compare(&applied, &live);
        let paths: Vec<String> = fields.iter().map(|f| path_text(&f.path)).collect();
        // Defaults, extra annotations, quantities written another way and `paused: false` left
        // out are not drift; the replicas and an injected sidecar are.
        assert_eq!(paths, ["spec.replicas", "spec.template.spec.containers[istio-proxy]"]);
        assert_eq!(fields[0].applied, Some(json!(2)));
        assert_eq!(fields[0].live, Some(json!(5)));
        assert_eq!(fields[1].applied, None);
        assert_eq!(json_pointer(&fields[0].path).as_deref(), Some("/spec/replicas"));
        assert_eq!(json_pointer(&fields[1].path), None);
        assert_eq!(jq_path(&fields[1].path), ".spec.template.spec.containers[] | select(.name == \"istio-proxy\")");
        // A field removed live, an image changed in place.
        let mut changed = live;
        changed["spec"]["template"]["spec"]["containers"][0]["image"] = json!("web:2");
        changed["metadata"]["labels"] = json!({});
        let paths: Vec<String> = compare(&applied, &changed).iter().map(|f| path_text(&f.path)).collect();
        assert!(paths.contains(&"spec.template.spec.containers[web].image".to_string()), "{paths:?}");
        assert!(paths.contains(&"metadata.labels.app".to_string()), "{paths:?}");
        assert!(crate::quantity::same_amount("500m", "0.5"));
        assert!(!crate::quantity::same_amount("1", "2"));
    }

    #[test]
    fn drift_reads_the_last_applied_copy_and_who_wrote_since() {
        let applied = json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "namespace": "shop"}, "spec": {"replicas": 2}});
        let live = json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "namespace": "shop",
                "annotations": {LAST_APPLIED: applied.to_string(), "deployment.kubernetes.io/revision": "4"},
                "managedFields": [
                    {"manager": "argocd-controller", "operation": "Update", "time": "2026-10-30T11:00:00Z", "fieldsV1": {"f:spec": {"f:template": {}}}},
                    {"manager": "kube-controller-manager", "operation": "Update", "time": "2026-10-30T11:30:00Z", "fieldsV1": {"f:metadata": {"f:annotations": {"f:deployment.kubernetes.io/revision": {}}}}},
                    {"manager": "kube-controller-manager", "operation": "Update", "subresource": "status", "time": "2026-10-30T11:31:00Z", "fieldsV1": {"f:status": {"f:replicas": {}}}},
                    {"manager": "hpa", "operation": "Update", "subresource": "scale", "time": "2026-10-30T11:40:00Z", "fieldsV1": {"f:spec": {"f:replicas": {}}}},
                    {"manager": "kubectl-edit", "operation": "Update", "time": "2026-10-30T10:00:00Z", "fieldsV1": {"f:spec": {"f:paused": {}}}}
                ]},
            "spec": {"replicas": 6}
        });
        let since: Timestamp = "2026-10-30T11:00:00Z".parse().unwrap();
        let d = drift_of(&deployment(), &live, Some(since));
        assert_eq!(d.compared, Compared::LastApplied);
        assert_eq!(d.fields.len(), 1);
        // Argo CD's own writes, status writes, a revision bump and writes before the sync left out.
        let who: Vec<(&str, &str)> = d.writers.iter().map(|w| (w.manager.as_str(), w.fields.as_str())).collect();
        assert_eq!(who, [("hpa", "spec.replicas")]);
        let lines = d.lines();
        assert_eq!(lines, [DiffLine::Removed("spec.replicas: 2".into()), DiffLine::Added("spec.replicas: 6".into())]);
        // Stop the loop: the field, as a pointer.
        let snippet = ignore_differences(&[&d]).unwrap();
        assert!(snippet.contains("- /spec/replicas"), "{snippet}");
        assert!(snippet.contains("RespectIgnoreDifferences=true"), "{snippet}");
        // Applied server side: no copy, only who wrote since; ignored by manager.
        let mut ssa = live;
        ssa["metadata"]["annotations"] = json!({});
        ssa["metadata"]["managedFields"][0]["operation"] = json!("Apply");
        let d = drift_of(&deployment(), &ssa, Some(since));
        assert_eq!(d.compared, Compared::ServerSide);
        assert!(d.fields.is_empty());
        let snippet = ignore_differences(&[&d]).unwrap();
        assert!(snippet.contains("managedFieldsManagers") && snippet.contains("- hpa"), "{snippet}");
    }

    #[test]
    fn a_secrets_values_never_show() {
        let secret = Res {
            kind: "Secret".into(),
            namespace: "shop".into(),
            name: "db".into(),
            version: "v1".into(),
            ..Default::default()
        };
        let applied = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "db"}, "stringData": {"password": "hunter2"}, "data": {"user": "YWRh"}});
        let live = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "db", "annotations": {LAST_APPLIED: applied.to_string()}},
            "data": {"password": base64::engine::general_purpose::STANDARD.encode("other"), "user": "YWRh"}});
        let d = drift_of(&secret, &live, None);
        let paths: Vec<String> = d.fields.iter().map(|f| path_text(&f.path)).collect();
        assert_eq!(paths, ["data.password"]);
        let text = format!("{:?}", d.lines());
        assert!(!text.contains("hunter2") && !text.contains("b3RoZXI") && !text.contains("aHVudGVyMg"), "{text}");
        assert!(text.contains("(hidden)"), "{text}");
        // The same value through stringData is no drift.
        let same = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "db", "annotations": {LAST_APPLIED: applied.to_string()}},
            "data": {"password": base64::engine::general_purpose::STANDARD.encode("hunter2"), "user": "YWRh"}});
        assert!(drift_of(&secret, &same, None).fields.is_empty());
    }

    #[test]
    fn paths_read_and_point() {
        let path = vec![
            Seg::Key("metadata".into()),
            Seg::Key("annotations".into()),
            Seg::Key("argocd.argoproj.io/sync-wave".into()),
        ];
        assert_eq!(path_text(&path), "metadata.annotations[\"argocd.argoproj.io/sync-wave\"]");
        assert_eq!(json_pointer(&path).as_deref(), Some("/metadata/annotations/argocd.argoproj.io~1sync-wave"));
        assert_eq!(jq_path(&path), ".metadata.annotations[\"argocd.argoproj.io/sync-wave\"]");
        assert_eq!(jq_path(&[Seg::Key("a.b".into())]), ".[\"a.b\"]");
        assert_eq!(field_text("data.config", &json!("a\nb\n")), "data.config: |\n  a\n  b\n");
        assert_eq!(field_text("spec.x", &json!({"a": 1, "b": 2})), "spec.x:\n  a: 1\n  b: 2\n");
        assert_eq!(field_text("spec.y", &json!("")), "spec.y: \"\"\n");
        let port = vec![
            Seg::Key("ports".into()),
            Seg::Item { key: "containerPort".into(), value: json!(8080) },
            Seg::Index(0),
        ];
        assert_eq!(path_text(&port), "ports[containerPort=8080][0]");
        assert_eq!(jq_path(&port), ".ports[] | select(.containerPort == 8080)[0]");
        assert_eq!(span_text(59), "59 s");
        assert_eq!(span_text(3 * 86400 + 5), "3 days");
    }

    #[test]
    fn the_report_says_what_was_found_and_where_it_comes_from() {
        let d = Diagnosis {
            findings: vec![Finding {
                tone: Tone::Bad,
                title: "Last sync failed".into(),
                message: Some("boom".into()),
                objects: vec![(deployment(), "rejected by admission: no".into())],
                more: 0,
                hint: "fix it".into(),
            }],
            looping: vec![deployment()],
            ..Default::default()
        };
        let drift = Drift {
            res: deployment(),
            compared: Compared::LastApplied,
            fields: vec![FieldDiff {
                path: vec![Seg::Key("spec".into()), Seg::Key("replicas".into())],
                applied: Some(json!(2)),
                live: Some(json!(3)),
            }],
            more: 0,
            writers: Vec::new(),
        };
        let read = DriftRead { context: Some("prod".into()), drifts: vec![drift], note: None };
        let text = report("Application shop", &d, &read);
        assert!(text.contains("[problem] Last sync failed"), "{text}");
        assert!(text.contains("- Deployment shop/web: rejected by admission: no"), "{text}");
        assert!(text.contains("Drift on prod"), "{text}");
        assert!(text.contains("  - spec.replicas: 2\n  + spec.replicas: 3"), "{text}");
        assert!(text.contains("ignoreDifferences"), "{text}");
    }
}
