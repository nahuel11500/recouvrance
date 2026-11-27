//! An Application's dashboard: its cards (state, versions, what runs in the cluster,
//! problems, last sync, sync diff, history), worked out from what Argo CD says of it. The
//! Application itself, its resource tree, its diff (managed resources) and the commits it
//! deploys are read by Argo CD's API ([`super::api::dashboard`]). The cards are pure functions of what was read, worked out off the UI thread.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use jiff::Timestamp;
use serde_json::Value;

use super::Summary;
use super::diagnosis::{Diagnosis, Finding, Res, SyncEffect, sync_effects};
use super::server::{Change, GitDiff, encode};
use crate::json::{arr_at as arr, nonempty_at as s, time_at as time};
use crate::status::Tone;

/// Deployments shown as dots.
pub const HISTORY_DOTS: usize = 10;
/// Commits kept by revision, at most (they never change).
const MAX_COMMITS: usize = 500;
/// Applications whose images are remembered by deployment, at most.
const MAX_SEEN: usize = 500;
/// A sync's message, cut to this many characters on its card.
const MESSAGE_CHARS: usize = 240;

// MARK: Requests

/// The resource tree of the Application `namespace/name`: every live object under it, with
/// each one's health and, for pods, their state.
pub(super) fn tree_path(namespace: &str, name: &str) -> String {
    format!("/api/v1/applications/{}/resource-tree?appNamespace={}", encode(name), encode(namespace))
}

/// Its managed resources: live and target state of each, what Argo CD's diff is made of.
pub(super) fn managed_path(namespace: &str, name: &str) -> String {
    format!("/api/v1/applications/{}/managed-resources?appNamespace={}", encode(name), encode(namespace))
}

/// The metadata (author, date, message) of the commit `revision` of one of its sources:
/// `source` and `version` (its deployment's id) for an Application with several sources.
pub(super) fn revision_path(
    namespace: &str,
    name: &str,
    revision: &str,
    source: Option<usize>,
    version: Option<i64>,
) -> String {
    let mut path = format!(
        "/api/v1/applications/{}/revisions/{}/metadata?appNamespace={}",
        encode(name),
        encode(revision),
        encode(namespace)
    );
    if let Some(i) = source {
        path.push_str(&format!("&sourceIndex={i}"));
        if let Some(v) = version {
            path.push_str(&format!("&versionId={v}"));
        }
    }
    path
}

// MARK: State

/// The State card: the sync as a word, the health, and the resources Degraded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    /// `Synced`, `OutOfSync`, `Sync Failed` or `Syncing`.
    pub sync: String,
    pub health: String,
    pub health_message: Option<String>,
    /// The names of the resources Degraded.
    pub degraded: Vec<String>,
    pub tone: Tone,
    /// What the resource tree holds in trouble that Argo CD's health leaves out (the objects
    /// its resources made): `3 Degraded below`.
    pub below: Option<String>,
}

impl State {
    /// Take the worst of the Application's health and what its resource tree holds in
    /// trouble (`problems`, read from the tree): Argo CD counts only the resources it
    /// manages, not the Jobs of a CronJob, for example. Those in trouble make it Bad, said
    /// in `below` unless Argo CD says Degraded itself.
    pub fn with_tree(&mut self, problems: &Problems) {
        if !problems.from_tree {
            return;
        }
        let bad: Vec<&Problem> = problems.rows.iter().filter(|r| r.tone == Tone::Bad).collect();
        let Some(first) = bad.first() else { return };
        self.tone = Tone::Bad;
        if self.health == "Degraded" {
            return;
        }
        let status = if bad.iter().all(|r| r.status == first.status) { first.status.as_str() } else { "in trouble" };
        self.below = Some(format!("{} {status} below", bad.len()));
    }
}

/// The State of `app`: a sync running or failed says so over the comparison with git. The
/// resources Degraded are those its status says, else the top objects of its tree (Argo CD 3
/// keeps their health in the tree only).
pub fn state(app: &Value, tree: Option<&Value>) -> State {
    let summary = Summary::of(app);
    let phase = s(app, "/status/operationState/phase");
    let sync = if summary.in_progress || matches!(phase, Some("Running" | "Terminating")) {
        "Syncing".to_string()
    } else if matches!(phase, Some("Failed" | "Error")) {
        "Sync Failed".to_string()
    } else {
        summary.sync.clone()
    };
    let degraded_named = |r: &&Value| s(r, "/health/status") == Some("Degraded");
    let mut degraded: Vec<String> = arr(app, "/status/resources")
        .iter()
        .filter(degraded_named)
        .filter_map(|r| s(r, "/name").map(String::from))
        .collect();
    if degraded.is_empty()
        && let Some(tree) = tree
    {
        degraded = arr(tree, "/nodes")
            .iter()
            .filter(|n| arr(n, "/parentRefs").is_empty())
            .filter(degraded_named)
            .filter_map(|n| s(n, "/name").map(String::from))
            .collect();
    }
    // An autoscaler is often named as its Deployment: each name once.
    let mut named = std::collections::HashSet::new();
    degraded.retain(|n| named.insert(n.clone()));
    State {
        sync,
        health: summary.health.clone(),
        health_message: summary.health_message.clone(),
        degraded,
        tone: super::attention(&summary),
        below: None,
    }
}

// MARK: Versions

/// One component's image: its tag, and the tag it had at the deployment before when it changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    /// The repository's last part: `web` for `registry.example.com/shop/web`.
    pub component: String,
    pub repo: String,
    pub tag: String,
    pub from: Option<String>,
}

/// `registry:5000/team/web:1.2` → (`registry:5000/team/web`, `1.2`). A digest without a tag
/// reads as `@` and its first 12 characters; no tag at all is `latest`.
pub fn split_image(image: &str) -> (String, String) {
    let (name, digest) = match image.split_once('@') {
        Some((n, d)) => (n, Some(d)),
        None => (image, None),
    };
    let last = name.rfind('/').map_or(0, |i| i + 1);
    let (repo, tag) = match name[last..].rfind(':') {
        Some(i) => (&name[..last + i], &name[last + i + 1..]),
        None => (name, ""),
    };
    let tag = match (tag, digest) {
        ("", Some(d)) => {
            let hex = d.split_once(':').map_or(d, |(_, h)| h);
            format!("@{}", hex.chars().take(12).collect::<String>())
        }
        ("", None) => "latest".to_string(),
        (t, _) => t.to_string(),
    };
    (repo.to_string(), tag)
}

/// Each image of `current`, by component, with its tag in `previous` (the images of the
/// deployment before) when the same repository was there at other tags only.
pub fn image_changes(current: &[String], previous: Option<&[String]>) -> Vec<Image> {
    // A repository may run at several tags at once (two components of one image): each tag of
    // it before. A tag that was there is no change; otherwise the change is from the first.
    let mut before: HashMap<String, Vec<String>> = HashMap::new();
    for (repo, tag) in previous.unwrap_or_default().iter().map(|i| split_image(i)) {
        let tags = before.entry(repo).or_default();
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    let mut out: Vec<Image> = current
        .iter()
        .map(|i| {
            let (repo, tag) = split_image(i);
            let from = before.get(&repo).filter(|tags| !tags.contains(&tag)).and_then(|tags| tags.first().cloned());
            let component = repo.rsplit('/').next().unwrap_or(&repo).to_string();
            Image { component, repo, tag, from }
        })
        .collect();
    out.sort_by(|a, b| (&a.component, &a.repo, &a.tag).cmp(&(&b.component, &b.repo, &b.tag)));
    out.dedup_by(|a, b| a.repo == b.repo && a.tag == b.tag);
    out
}

/// The images a source pins: kustomize's `images` (`[name=]repo:tag`), and the Helm
/// parameters `<x>repository` with their `<x>tag`.
pub fn source_images(source: &Value) -> Vec<String> {
    let mut out: Vec<String> = arr(source, "/kustomize/images")
        .iter()
        .filter_map(Value::as_str)
        .map(|i| i.split_once('=').map_or(i, |(_, v)| v).to_string())
        .collect();
    let params: BTreeMap<&str, &str> = arr(source, "/helm/parameters")
        .iter()
        .filter_map(|p| Some((p.get("name")?.as_str()?, p.get("value")?.as_str()?)))
        .collect();
    for (name, repo) in &params {
        if let Some(prefix) = name.strip_suffix("repository")
            && let Some(tag) = params.get(format!("{prefix}tag").as_str())
        {
            out.push(format!("{repo}:{tag}"));
        }
    }
    out
}

/// The sources of a deployment of the history, or of a spec: each of `sources`, else `source`.
/// `sources` wins: with several, Argo CD's API still sends `source` as `{"repoURL": ""}`.
fn sources_of(entry: &Value) -> Vec<&Value> {
    let many = arr(entry, "/sources");
    if !many.is_empty() {
        return many.iter().collect();
    }
    entry.get("source").filter(|v| v.is_object()).into_iter().collect()
}

/// Who started an operation: its user, or `autosync`.
pub fn who(initiated_by: &Value) -> Option<String> {
    if initiated_by.get("automated").and_then(Value::as_bool) == Some(true) {
        return Some("autosync".into());
    }
    initiated_by.get("username").and_then(Value::as_str).filter(|u| !u.is_empty()).map(String::from)
}

/// The Versions card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Versions {
    pub images: Vec<Image>,
    /// When the deployment running was made, and by whom.
    pub released: Option<Timestamp>,
    pub by: Option<String>,
    /// What the deployment before ran: the tags that changed since, else its revision.
    pub previous: Option<String>,
    /// The deployment a rollback goes to; None when there is none before, or auto-sync is on
    /// (Argo CD refuses a rollback then).
    pub rollback_to: Option<i64>,
    pub auto_sync: bool,
}

/// The Versions of `app`. `seen`: the images seen running under the deployment before, when
/// this page saw them; else those its sources pinned (kustomize images, Helm image tags).
pub fn versions(app: &Value, seen: Option<&[String]>) -> Versions {
    let history = arr(app, "/status/history");
    let last = history.last();
    let before = history.len().checked_sub(2).and_then(|i| history.get(i));
    let current = current_images(app);
    let pinned: Option<Vec<String>> =
        before.map(|h| sources_of(h).into_iter().flat_map(source_images).collect::<Vec<_>>()).filter(|v| !v.is_empty());
    let previous_images = seen.map(<[String]>::to_vec).or(pinned);
    let images = image_changes(&current, previous_images.as_deref());
    let succeeded = s(app, "/status/operationState/phase") == Some("Succeeded");
    let op_by = app.pointer("/status/operationState/operation/initiatedBy").filter(|_| succeeded).and_then(who);
    let by = last.and_then(|h| h.get("initiatedBy")).and_then(who).or(op_by);
    let changed: Vec<String> =
        images.iter().filter_map(|i| Some(format!("{} {}", i.component, i.from.as_ref()?))).collect();
    let previous = before.map(|h| {
        if changed.is_empty() {
            super::revisions(h).map_or_else(|| "unknown revision".to_string(), |r| format!("revision {r}"))
        } else {
            changed.join(", ")
        }
    });
    let auto_sync = super::auto_sync(app);
    Versions {
        images,
        released: last.and_then(|h| time(h, "/deployedAt")),
        by,
        previous,
        rollback_to: before.and_then(|h| h["id"].as_i64()).filter(|_| !auto_sync),
        auto_sync,
    }
}

/// The images running, as Argo CD sums them up.
pub fn current_images(app: &Value) -> Vec<String> {
    arr(app, "/status/summary/images").iter().filter_map(Value::as_str).map(String::from).collect()
}

/// Images seen running under each deployment, by Application (Argo CD's history doesn't keep
/// them): the next deployment's chips say what changed.
type Seen = HashMap<(String, String, String), Vec<(i64, Vec<String>)>>;
static SEEN: LazyLock<Mutex<Seen>> = LazyLock::new(Default::default);

/// Keep `images` as those of deployment `id` in `seen` (its last few), unless it is known.
pub fn remember(seen: &mut Vec<(i64, Vec<String>)>, id: i64, images: &[String]) {
    if images.is_empty() || seen.iter().any(|(i, _)| *i == id) {
        return;
    }
    seen.push((id, images.to_vec()));
    if seen.len() > 5 {
        seen.remove(0);
    }
}

/// Remember what `app` runs, when it is settled (no sync running, Healthy), under its newest
/// deployment; the images seen under the one before, when they were.
pub fn seen_images(key: &(String, String, String), app: &Value) -> Option<Vec<String>> {
    let history = arr(app, "/status/history");
    let id = |i: usize| history.get(i).and_then(|h| h["id"].as_i64());
    let (last, before) = (history.len().checked_sub(1).and_then(id), history.len().checked_sub(2).and_then(id));
    let settled = !super::ops::in_progress(app) && s(app, "/status/health/status") == Some("Healthy");
    let mut all = SEEN.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(last) = last.filter(|_| settled) {
        if all.len() >= MAX_SEEN && !all.contains_key(key) {
            all.clear();
        }
        remember(all.entry(key.clone()).or_default(), last, &current_images(app));
    }
    let before = before?;
    all.get(key)?.iter().find(|(i, _)| *i == before).map(|(_, images)| images.clone())
}

// MARK: In cluster

/// A source of the deployment running, and the revision of it deployed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deployed {
    /// Its index among several sources; None for an Application with one.
    pub index: Option<usize>,
    pub repo: String,
    pub revision: String,
    /// The Helm chart, for a chart repository's source: its revision is a version, no commit.
    pub chart: Option<String>,
}

/// The sources of the deployment running, with their revisions, and that deployment's id.
/// From the newest deployment of the history, else the revision(s) compared with git.
pub fn deployed(app: &Value) -> (Vec<Deployed>, Option<i64>) {
    let last = arr(app, "/status/history").last();
    let (holder, sources): (&Value, Vec<&Value>) = if let Some(h) = last {
        (h, sources_of(h))
    } else {
        let spec = app.pointer("/spec").map_or_default(sources_of);
        (app.pointer("/status/sync").unwrap_or(&Value::Null), spec)
    };
    let one = |source: Option<&Value>, revision: &str, index: Option<usize>| Deployed {
        index,
        repo: source.and_then(|v| s(v, "/repoURL")).unwrap_or_default().to_string(),
        revision: revision.to_string(),
        chart: source.and_then(|v| s(v, "/chart")).map(String::from),
    };
    let many: Vec<&str> = arr(holder, "/revisions").iter().filter_map(Value::as_str).collect();
    let out = match s(holder, "/revision") {
        Some(rev) if many.is_empty() => vec![one(sources.first().copied(), rev, None)],
        _ => many
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.is_empty())
            .map(|(i, r)| one(sources.get(i).copied(), r, Some(i)))
            .collect(),
    };
    (out, last.and_then(|h| h["id"].as_i64()))
}

/// A commit, as Argo CD's revision metadata says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub author: Option<String>,
    pub date: Option<Timestamp>,
    pub message: String,
    pub tags: Vec<String>,
}

pub fn commit_of(v: &Value) -> Commit {
    Commit {
        author: s(v, "/author").map(String::from),
        date: time(v, "/date"),
        message: s(v, "/message").unwrap_or_default().trim().to_string(),
        tags: arr(v, "/tags").iter().filter_map(Value::as_str).map(String::from).collect(),
    }
}

// MARK: Problems

/// A workload (or another object) in trouble, from the resource tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// What its pods are in (`CrashLoopBackOff`, `Pending`...), or its own health.
    pub status: String,
    pub tone: Tone,
    pub res: Res,
    /// Its pods in trouble, of all its pods.
    pub pods: Option<(usize, usize)>,
    /// The oldest of its pods in trouble was made then.
    pub since: Option<Timestamp>,
    /// What Argo CD says of it (or of its first pod in trouble).
    pub why: Option<String>,
}

/// A pod of the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pod {
    pub res: Res,
    pub status: String,
    pub tone: Tone,
}

/// The Problems card.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Problems {
    pub rows: Vec<Problem>,
    pub pods: Vec<Pod>,
    pub troubled: usize,
    /// Read from the resource tree (pods known); else from the Application's status.
    pub from_tree: bool,
}

fn info<'a>(node: &'a Value, name: &str) -> Option<&'a str> {
    arr(node, "/info").iter().find(|i| i["name"] == name).and_then(|i| i["value"].as_str()).filter(|v| !v.is_empty())
}

fn res_of_node(node: &Value) -> Res {
    let t = |p: &str| s(node, p).unwrap_or_default().to_string();
    Res { group: t("/group"), kind: t("/kind"), namespace: t("/namespace"), name: t("/name"), version: t("/version") }
}

/// A pod's state, as Argo CD's tree says it: its reason (`CrashLoopBackOff`, `Running`...),
/// and its tone. A pod running but not ready is `Not ready`.
pub fn pod_state(node: &Value) -> (String, Tone) {
    let health = s(node, "/health/status");
    let reason = info(node, "Status Reason").or(health).unwrap_or("Unknown").to_string();
    let tone = crate::status::tone(&reason).unwrap_or(Tone::Done);
    match health {
        Some("Degraded" | "Missing") if tone != Tone::Bad => (reason, Tone::Bad),
        Some("Progressing") if tone == Tone::Good => ("Not ready".into(), Tone::Busy),
        _ => (reason, tone),
    }
}

/// The pods under one object: how many, and those in trouble with their state.
type Group<'a> = (usize, Vec<(&'a Value, String, Tone)>);

/// How bad a tone is: the higher, the worse.
fn rank(t: Tone) -> u8 {
    match t {
        Tone::Bad => 3,
        Tone::Busy => 2,
        Tone::Done => 1,
        Tone::Good => 0,
    }
}

/// What is in trouble in the resource tree of `app`: one row per workload whose pods are (pods
/// grouped under the object at the top of their owners), then the other objects Degraded. The
/// tree holds only live objects: what is missing comes from the Application's status (see
/// [`missing_rows`]). The worst first.
pub fn problems_from_tree(app: &Value, tree: &Value) -> Problems {
    let nodes = arr(tree, "/nodes");
    let by_uid: HashMap<&str, &Value> = nodes.iter().filter_map(|n| Some((s(n, "/uid")?, n))).collect();
    // The object at the top of a node's owners (itself when it has none known).
    let top = |n: &Value| -> String {
        let mut at: &Value = n;
        for _ in 0..8 {
            let parent = arr(at, "/parentRefs").iter().find_map(|p| by_uid.get(s(p, "/uid")?).copied());
            match parent {
                Some(p) => at = p,
                None => break,
            }
        }
        s(at, "/uid").map_or_else(|| res_of_node(at).label(), String::from)
    };
    let mut out = Problems { from_tree: true, ..Default::default() };
    // By top: (the top's node, its pods, those in trouble with their state).
    let mut groups: BTreeMap<String, Group<'_>> = BTreeMap::new();
    for n in nodes.iter().filter(|n| s(n, "/kind") == Some("Pod") && s(n, "/group").unwrap_or("").is_empty()) {
        let (status, tone) = pod_state(n);
        let trouble = matches!(tone, Tone::Bad | Tone::Busy);
        out.pods.push(Pod { res: res_of_node(n), status: status.clone(), tone });
        let g = groups.entry(top(n)).or_default();
        g.0 += 1;
        if trouble {
            out.troubled += 1;
            g.1.push((n, status, tone));
        }
    }
    let node_of = |key: &str| by_uid.get(key).copied();
    for (key, (total, troubled)) in &groups {
        if troubled.is_empty() {
            continue;
        }
        // The most common state of its pods in trouble, the worse one on a tie.
        let mut counts: Vec<(&str, Tone, usize)> = Vec::new();
        for (_, status, tone) in troubled {
            match counts.iter_mut().find(|c| c.0 == status.as_str()) {
                Some(c) => c.2 += 1,
                None => counts.push((status.as_str(), *tone, 1)),
            }
        }
        counts.sort_by_key(|c| (std::cmp::Reverse(c.2), std::cmp::Reverse(rank(c.1))));
        let (status, tone, _) = counts[0];
        let owner = node_of(key.as_str()).unwrap_or(troubled[0].0);
        let why = troubled
            .iter()
            .find_map(|(n, _, _)| s(n, "/health/message"))
            .or_else(|| s(owner, "/health/message"))
            .map(String::from);
        out.rows.push(Problem {
            status: status.to_string(),
            tone,
            res: res_of_node(owner),
            pods: Some((troubled.len(), *total)),
            since: troubled.iter().filter_map(|(n, _, _)| time(n, "/createdAt")).min(),
            why,
        });
    }
    let mut tops_done: Vec<String> = groups.iter().filter(|(_, g)| !g.1.is_empty()).map(|(k, _)| k.clone()).collect();
    for n in nodes.iter().filter(|n| s(n, "/kind") != Some("Pod")) {
        let Some(health @ "Degraded") = s(n, "/health/status") else { continue };
        let key = top(n);
        if tops_done.contains(&key) {
            continue;
        }
        tops_done.push(key.clone());
        let owner = node_of(key.as_str()).unwrap_or(n);
        out.rows.push(Problem {
            status: health.to_string(),
            tone: Tone::Bad,
            res: res_of_node(owner),
            pods: None,
            since: None,
            why: s(n, "/health/message").map(String::from),
        });
    }
    let live: Vec<Res> = nodes.iter().map(res_of_node).collect();
    let missing = missing_rows(app, |r| live.iter().any(|l| l.same(r)));
    let shown: Vec<Res> = out.rows.iter().map(|p| p.res.clone()).collect();
    out.rows.extend(missing.into_iter().filter(|m| !shown.iter().any(|s| s.same(&m.res))));
    sort_problems(&mut out.rows);
    out
}

/// The resources of `app` that should be live and aren't, when auto-sync is on (synced by
/// hand, what is not deployed yet is expected): those its status says Missing, and those out
/// of sync that `is_live` doesn't know (not to prune, not a hook).
pub fn missing_rows(app: &Value, is_live: impl Fn(&Res) -> bool) -> Vec<Problem> {
    if !super::auto_sync(app) {
        return Vec::new();
    }
    arr(app, "/status/resources")
        .iter()
        .filter(|r| r["hook"] != true && !super::to_prune(r))
        .filter_map(|r| {
            let res = Res::of(r)?;
            let missing =
                s(r, "/health/status") == Some("Missing") || (s(r, "/status") == Some("OutOfSync") && !is_live(&res));
            missing.then(|| Problem {
                status: "Missing".into(),
                tone: Tone::Bad,
                why: Some(s(r, "/health/message").unwrap_or("In git, not in the cluster.").to_string()),
                res,
                pods: None,
                since: None,
            })
        })
        .collect()
}

fn sort_problems(rows: &mut [Problem]) {
    rows.sort_by(|a, b| {
        let key = |p: &Problem| (std::cmp::Reverse(rank(p.tone)), p.res.name.clone(), p.res.kind.clone());
        key(a).cmp(&key(b))
    });
}

/// What is in trouble, from the Application's status alone (no tree to read): its resources
/// Degraded, or Missing with auto-sync on (synced by hand, Missing is not deployed yet).
pub fn problems_from_status(app: &Value) -> Problems {
    let automated = super::auto_sync(app);
    let mut rows: Vec<Problem> = arr(app, "/status/resources")
        .iter()
        .filter_map(|r| {
            let health = s(r, "/health/status")?;
            let bad = health == "Degraded" || (health == "Missing" && automated);
            if !bad && health != "Progressing" {
                return None;
            }
            Some(Problem {
                status: health.to_string(),
                tone: if bad { Tone::Bad } else { Tone::Busy },
                res: Res::of(r)?,
                pods: None,
                since: None,
                why: s(r, "/health/message").map(String::from),
            })
        })
        .collect();
    sort_problems(&mut rows);
    Problems { rows, ..Default::default() }
}

// MARK: Selective sync

/// A sync of some of the Application's resources only (a selective sync), newer than its
/// newest deployment. Argo CD doesn't add one to `status.history`, so the history's newest
/// deployment is not all that runs: these resources run at `revision`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Partial {
    /// The revision(s) it synced to, short.
    pub revision: String,
    /// How many resources it synced, of how many the Application has.
    pub resources: usize,
    pub total: usize,
    /// Some of them (`Kind/name`), for a message.
    pub names: Vec<String>,
    pub phase: String,
    /// When it ended, else when it started.
    pub at: Option<Timestamp>,
    pub by: Option<String>,
    /// The deployment it came after (the history's newest), if any.
    pub after: Option<i64>,
}

impl Partial {
    /// `9 of 31 resources at ebf7503 (selective sync, 5d ago by jdoe)`.
    pub fn text(&self) -> String {
        let mut how = vec!["selective sync".to_string()];
        if let Some(t) = self.at {
            how.push(format!("{} ago", crate::k8s::age(t)));
        }
        let by = self.by.as_ref().map_or_default(|b| format!(" by {b}"));
        format!(
            "{} of {} at {} ({}{by})",
            self.resources,
            crate::plural::count(self.total.max(self.resources), "resource"),
            self.revision,
            how.join(", ")
        )
    }
}

/// Names shown of a selective sync's resources, at most.
const PARTIAL_NAMES: usize = 5;

/// The selective sync of `app` newer than its newest deployment, if its last operation is one
/// that ended (see `Partial`).
pub fn partial(app: &Value) -> Option<Partial> {
    let op = app.pointer("/status/operationState")?;
    let picked = arr(op, "/operation/sync/resources");
    if picked.is_empty() || op.pointer("/operation/sync/dryRun").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let phase = s(op, "/phase")?.to_string();
    if matches!(phase.as_str(), "Running" | "Terminating") {
        return None;
    }
    let started = time(op, "/startedAt")?;
    let newest = arr(app, "/status/history").iter().max_by_key(|h| h["id"].as_i64().unwrap_or(0));
    if newest.and_then(|h| time(h, "/deployedAt")).is_some_and(|d| d >= started) {
        return None;
    }
    let revision = op
        .pointer("/syncResult")
        .and_then(super::revisions)
        .or_else(|| op.pointer("/operation/sync").and_then(super::revisions))?;
    let total = arr(app, "/status/resources").iter().filter(|r| r["hook"] != true).count();
    let names = picked
        .iter()
        .take(PARTIAL_NAMES)
        .filter_map(|r| Some(format!("{}/{}", s(r, "/kind")?, s(r, "/name")?)))
        .collect();
    Some(Partial {
        revision,
        resources: picked.len(),
        total,
        names,
        phase,
        at: time(op, "/finishedAt").or(Some(started)),
        by: op.pointer("/operation/initiatedBy").and_then(who),
        after: newest.and_then(|h| h["id"].as_i64()),
    })
}

/// What rolling back `app` also undoes: its selective sync after the newest deployment, said.
pub fn rollback_undoes(app: &Value) -> Option<String> {
    let p = partial(app)?;
    let mut names = p.names.join(", ");
    if p.resources > p.names.len() {
        names.push_str(&format!(" and {} more", p.resources - p.names.len()));
    }
    Some(format!(
        "Its last sync was selective: {}, after deployment {}. Rolling back also takes those resources ({names}) \
         back.",
        p.text(),
        p.after.map_or_else(|| "none".to_string(), |id| format!("#{id}"))
    ))
}

// MARK: Last sync

/// The Last sync card.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastSync {
    pub phase: String,
    pub tone: Tone,
    /// Its message's first line, cut.
    pub message: Option<String>,
    pub started: Option<Timestamp>,
    pub finished: Option<Timestamp>,
    pub by: Option<String>,
    pub revision: Option<String>,
    /// Its resources' results, as the sync journal keeps them (those without a kind or a
    /// name left out).
    pub steps: Vec<super::journal::Step>,
    /// Them summed up, the cause first: worked out once per read.
    pub summary: super::steps::Summary,
    /// Steps done so far (a hook still running isn't).
    pub done: usize,
    pub running: bool,
    /// A selective sync: the resources picked, of the Application's.
    pub selective: Option<(usize, usize)>,
}

impl LastSync {
    /// How long it took, or has taken so far at `now`.
    pub fn seconds(&self, now: Timestamp) -> Option<i64> {
        let end = self.finished.unwrap_or(now);
        self.started.map(|s| (end.as_second() - s.as_second()).max(0))
    }
}

/// The last operation of `app`, from `status.operationState`.
pub fn last_sync(app: &Value) -> Option<LastSync> {
    let op = app.pointer("/status/operationState")?;
    let phase = s(op, "/phase")?.to_string();
    let steps: Vec<super::journal::Step> = arr(op, "/syncResult/resources")
        .iter()
        .map(super::journal::Step::of)
        .filter(|st| !st.kind.is_empty() && !st.name.is_empty())
        .collect();
    let summary = super::steps::Summary::of(&steps);
    let done = summary.done;
    let message = s(op, "/message").map(|m| {
        let line = m.lines().next().unwrap_or_default();
        let cut: String = line.chars().take(MESSAGE_CHARS).collect();
        if cut.len() < line.len() || m.lines().nth(1).is_some() { format!("{cut}…") } else { cut }
    });
    let revision = op
        .pointer("/syncResult")
        .and_then(super::revisions)
        .or_else(|| op.pointer("/operation/sync").and_then(super::revisions));
    Some(LastSync {
        tone: super::operation_tone(&phase),
        running: matches!(phase.as_str(), "Running" | "Terminating"),
        phase,
        message,
        started: time(op, "/startedAt"),
        finished: time(op, "/finishedAt"),
        by: op.pointer("/operation/initiatedBy").and_then(who),
        revision,
        steps,
        summary,
        done,
        selective: Some(arr(op, "/operation/sync/resources").len()).filter(|&n| n > 0).map(|n| {
            let total = arr(app, "/status/resources").iter().filter(|r| r["hook"] != true).count();
            (n, total.max(n))
        }),
    })
}

// MARK: Sync diff

/// The Sync diff card: the resources a sync would change, create or prune.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffSummary {
    pub changed: Vec<Res>,
    pub added: Vec<Res>,
    pub pruned: Vec<Res>,
    /// Out of sync, but whether it is live isn't known (from the status alone).
    pub unknown: Vec<Res>,
    /// Resources that differ past those Argo CD's diff lists.
    pub more: usize,
    /// From Argo CD's own diff (managed resources); else from the Application's status.
    pub from_argo: bool,
}

impl DiffSummary {
    pub fn total(&self) -> usize {
        self.changed.len() + self.added.len() + self.pruned.len() + self.unknown.len() + self.more
    }

    /// `~ 3 changed, + 1 added, – 2 pruned`: the kinds of change there are.
    pub fn text(&self) -> String {
        let parts: Vec<String> = [
            ("~", self.changed.len() + self.unknown.len() + self.more, "changed"),
            ("+", self.added.len(), "added"),
            ("–", self.pruned.len(), "pruned"),
        ]
        .iter()
        .filter(|(_, n, _)| *n > 0)
        .map(|(sign, n, what)| format!("{sign} {n} {what}"))
        .collect();
        parts.join(", ")
    }
}

/// What Argo CD's diff says a sync would do.
pub fn diff_from_git(d: &GitDiff) -> DiffSummary {
    let mut out = DiffSummary { more: d.more, from_argo: true, ..Default::default() };
    for r in &d.changed {
        match r.change {
            Change::Modified => out.changed.push(r.res.clone()),
            Change::Created => out.added.push(r.res.clone()),
            Change::Pruned => out.pruned.push(r.res.clone()),
        }
    }
    out
}

/// What the Application's status says a sync would do (its resources out of sync).
pub fn diff_from_status(app: &Value) -> DiffSummary {
    let mut out = DiffSummary::default();
    for (res, effect) in sync_effects(app, &[]) {
        match effect {
            SyncEffect::Update => out.changed.push(res),
            SyncEffect::Create => out.added.push(res),
            SyncEffect::Prune => out.pruned.push(res),
            SyncEffect::Unknown => out.unknown.push(res),
        }
    }
    out
}

// MARK: History

/// A deployment, or a sync that failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dot {
    pub id: Option<i64>,
    pub ok: bool,
    pub revision: String,
    pub at: Option<Timestamp>,
    pub by: Option<String>,
}

/// The History card: the last deployments, oldest first, and the last sync when it failed
/// after them (Argo CD's history keeps only what deployed).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    pub dots: Vec<Dot>,
    pub succeeded: usize,
    pub failed: usize,
    /// Every deployment of the history, oldest first, as compared (see `sync_diff`).
    pub entries: Vec<super::sync_diff::Entry>,
    /// The Application's sources now: what any revision is rendered with.
    pub spec: super::sync_diff::Spec,
}

pub fn history(app: &Value, n: usize) -> History {
    let mut dots: Vec<Dot> = arr(app, "/status/history")
        .iter()
        .map(|h| Dot {
            id: h["id"].as_i64(),
            ok: true,
            revision: super::revisions(h).unwrap_or_default(),
            at: time(h, "/deployedAt"),
            by: h.get("initiatedBy").and_then(who),
        })
        .collect();
    if let Some(op) = app.pointer("/status/operationState")
        && matches!(s(op, "/phase"), Some("Failed" | "Error"))
    {
        let at = time(op, "/finishedAt");
        let newer = match (at, dots.last().and_then(|d| d.at)) {
            (Some(failed), Some(deployed)) => failed > deployed,
            _ => true,
        };
        if newer {
            let revision = op
                .pointer("/syncResult")
                .and_then(super::revisions)
                .or_else(|| op.pointer("/operation/sync").and_then(super::revisions))
                .unwrap_or_default();
            dots.push(Dot {
                id: None,
                ok: false,
                revision,
                at,
                by: op.pointer("/operation/initiatedBy").and_then(who),
            });
        }
    }
    let dots: Vec<Dot> = dots[dots.len().saturating_sub(n)..].to_vec();
    let failed = dots.iter().filter(|d| !d.ok).count();
    History {
        succeeded: dots.len() - failed,
        failed,
        dots,
        entries: super::sync_diff::entries(app),
        spec: super::sync_diff::Spec::of(app),
    }
}

/// The result per resource of the last operation of `app` (its `steps`), in Argo CD's order,
/// each with its wave (its status', or its manifest's in `managed`) and its health now (the
/// tree's, else the status'): the Sync status view, and the sync page while it follows a sync.
pub fn sync_rows(
    app: &Value,
    steps: &[super::journal::Step],
    managed: Option<&Value>,
    tree: Option<&Value>,
) -> Vec<SyncRow> {
    let healths = views::healths(app, tree);
    let waves = views::all_waves(app, managed);
    views::sync_rows(steps, &waves, |st| {
        healths.get(&(st.kind.clone(), st.namespace.clone(), st.name.clone())).cloned()
    })
}

// MARK: Ask the assistant

/// Where the problems asked about are: the environment, its Application, where it deploys,
/// and the context of the kubeconfig that reaches it, when one does.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AskScope {
    pub environment: String,
    pub application: String,
    /// Argo CD's name of the destination cluster, else its server's URL.
    pub cluster: String,
    pub namespace: String,
    pub context: Option<String>,
}

/// A problem in one line for the assistant: `CrashLoopBackOff Deployment shop/api (2 of 3 pods
/// in trouble, for 2h): why`.
pub fn problem_line(p: &Problem, now: Timestamp) -> String {
    let mut facts = Vec::new();
    if let Some((bad, all)) = p.pods {
        facts.push(format!("{bad} of {} in trouble", crate::plural::count(all, "pod")));
    }
    if let Some(since) = p.since {
        facts.push(format!("for {}", super::diagnosis::span_text(now.as_second() - since.as_second())));
    }
    let mut line = format!("{} {}", p.status, p.res.label());
    if !facts.is_empty() {
        line.push_str(&format!(" ({})", facts.join(", ")));
    }
    if let Some(why) = p.why.as_deref().map(str::trim).filter(|w| !w.is_empty()) {
        // One line each: a long message cut, its whole in the cluster.
        let why: String = why.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(600).collect();
        line.push_str(&format!(": {why}"));
    }
    line
}

/// The question "Ask the assistant" asks about `problems` (all of an environment's, or one):
/// where they are, each in a line, and how to answer (read-only, from the evidence).
pub fn problems_question(scope: &AskScope, problems: &[&Problem], now: Timestamp) -> String {
    let mut target = format!("the Argo CD Application {}", scope.application);
    if scope.environment != scope.application {
        target.push_str(&format!(" (environment {})", scope.environment));
    }
    let mut dest = Vec::new();
    if !scope.cluster.is_empty() {
        dest.push(format!("cluster {}", scope.cluster));
    }
    if !scope.namespace.is_empty() {
        dest.push(format!("namespace {}", scope.namespace));
    }
    if !dest.is_empty() {
        target.push_str(&format!(", deployed to {}", dest.join(", ")));
    }
    let context = match &scope.context {
        Some(c) => format!("The kubeconfig context {c} reaches that cluster: use it."),
        None => "No kubeconfig context is known to reach that cluster: say so if it can't be read.".to_string(),
    };
    let what = if problems.len() == 1 { "this problem" } else { "these problems" };
    let lines: Vec<String> = problems.iter().map(|p| format!("- {}", problem_line(p, now))).collect();
    format!(
        "Investigate {what} of {target}. {context} Recouvrance found, from Argo CD's resource tree:\n{}\n\n\
         Find the root cause from the evidence: describe_resource on each object and the workload that owns it, \
         get_events, get_logs (with previous=true after a restart or OOMKilled), its usage_history against requests \
         and limits, and what changed recently (the Application's last sync and history, image tags). Use the team's \
         notes and code where they apply. Change nothing. Answer with: the cause of each (or the one they share), the \
         evidence (quote the lines), and the fix (the command, manifest or values change).",
        lines.join("\n")
    )
}

// MARK: Cards

/// What was read for one Application's dashboard.
#[derive(Clone, Debug)]
pub struct Read {
    /// The Application, whole.
    pub app: Value,
    /// Its resource tree; None when it can't be asked (the Application is a list's copy).
    pub tree: Option<Result<Value, String>>,
    /// Argo CD's diff; None when it can't be asked.
    pub diff: Option<Result<GitDiff, String>>,
    /// Argo CD's managed resources, as read: what the smart diff is worked out from.
    pub managed: Option<Value>,
    /// The commit of each source deployed (see [`deployed`]); None when it can't be asked.
    pub commits: Vec<Option<Result<Commit, String>>>,
    /// Why the Application couldn't be read now (one line), when it is a list's copy: its
    /// tree, diff and commits then aren't asked.
    pub note: Option<String>,
}

/// The first line of an error with text, to say it in one line.
pub fn first_line(e: &str) -> String {
    e.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("it failed").to_string()
}

impl Read {
    /// The Application as a feed holds it (its list's copy), when reading it failed with `why`:
    /// its tree, diff and commits aren't asked, and the note says why in one line.
    pub fn held(app: Value, why: &str) -> Self {
        Self { app, tree: None, diff: None, managed: None, commits: Vec::new(), note: Some(first_line(why)) }
    }
}

/// Every card of one Application's dashboard.
#[derive(Clone, Debug)]
pub struct Cards {
    pub summary: Summary,
    pub state: State,
    /// What the diagnosis finds wrong (State's Why).
    pub findings: Vec<Finding>,
    pub versions: Versions,
    pub deployed: Vec<(Deployed, Option<Result<Commit, String>>)>,
    pub problems: Problems,
    /// Why the resource tree couldn't be read (the problems then come from the status).
    pub tree_error: Option<String>,
    pub last_sync: Option<LastSync>,
    /// The last sync by phase, for the Sync dialog.
    pub sync_phases: Option<super::sync_phases::SyncPhases>,
    pub diff: DiffSummary,
    /// Argo CD's diff, to show whole; or why it couldn't be read.
    pub git_diff: Option<Result<GitDiff, String>>,
    /// What a sync would change, read with the smart diff (facts, noise folded, a Secret's
    /// values masked), from the managed resources; None when they weren't read.
    pub smart: Option<Arc<super::grouped_diff::AppDiff>>,
    /// What it changes, each change once with where, grouped as the grouped diff groups them
    /// (a field added on 36 Deployments is one), the fields of one resource in one line
    /// (`grouped_diff::by_resource`): images first.
    pub changes: Vec<super::grouped_diff::Cross>,
    /// The images of its workloads, as they run and as git renders them (see
    /// `release::managed_images`); None when the managed resources weren't read.
    pub workloads: Option<(Vec<String>, Vec<String>)>,
    pub history: History,
    pub note: Option<String>,
    /// Its resources with their health, by category (the Resources view): from the tree,
    /// else from the Application's status.
    pub nodes: Vec<Node>,
    /// The last operation, whole, and its result per resource in Argo CD's order (the Sync
    /// status view).
    pub operation: Option<Operation>,
    pub sync_rows: Vec<SyncRow>,
    /// What the Details panel says.
    pub details: Details,
}

impl Cards {
    /// Whether finding no problem means there is none: the resource tree was read, or the
    /// Application is Healthy. From a status without health per resource (Argo CD 3 keeps it
    /// in the tree), finding nothing says nothing.
    pub fn clean_is_known(&self) -> bool {
        self.problems.from_tree || self.summary.health == "Healthy"
    }

    /// The cards of what was read, at `now`; `seen`: the images seen under the deployment before.
    pub fn of(read: Read, seen: Option<&[String]>, now: Timestamp) -> Self {
        let app = &read.app;
        let tree = read.tree.as_ref().and_then(|t| t.as_ref().ok());
        let problems = tree.map_or_else(|| problems_from_status(app), |t| problems_from_tree(app, t));
        let diff = match &read.diff {
            Some(Ok(d)) => diff_from_git(d),
            _ => diff_from_status(app),
        };
        let (deployed, _) = deployed(app);
        let commits = read.commits.into_iter().chain(std::iter::repeat(None));
        // Worked out here, off the UI thread, with every line.
        let smart = read.managed.as_ref().map(|m| Arc::new(super::grouped_diff::AppDiff::of(m)));
        let changes = smart.as_ref().map_or_else(Vec::new, |s| super::grouped_diff::by_resource(&s.diff));
        let mut state = state(app, tree);
        state.with_tree(&problems);
        let last_sync = last_sync(app);
        let sync_rows =
            last_sync.as_ref().map_or_else(Vec::new, |l| sync_rows(app, &l.steps, read.managed.as_ref(), tree));
        let sync_phases = last_sync.as_ref().and_then(|l| super::sync_phases::of(app, &l.steps, tree));
        Self {
            summary: Summary::of(app),
            state,
            findings: Diagnosis::of(app, now).findings,
            versions: versions(app, seen),
            deployed: deployed.into_iter().zip(commits).collect(),
            problems,
            nodes: tree.map_or_else(|| views::nodes_from_status(app), views::nodes_from_tree),
            operation: views::operation(app),
            sync_rows,
            details: views::details(app),
            tree_error: read.tree.and_then(Result::err),
            last_sync,
            sync_phases,
            diff,
            git_diff: read.diff,
            // Worked out here, off the UI thread, with every line.
            smart,
            changes,
            workloads: read.managed.as_ref().map(super::release::managed_images),
            history: history(app, HISTORY_DOTS),
            note: read.note,
        }
    }
}

mod read;
pub(in crate::k8s::argo) use read::extras;
mod views;
pub use views::{Category, Details, Node, Operation, SyncRow, failed_first, health_tone, sections};

/// A made-up example, for the developer tools' snapshots and the tests.
#[cfg(any(test, feature = "dev"))]
pub mod example;
#[cfg(test)]
mod tests;
