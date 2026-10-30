//! What changed on an object, from what the cluster already keeps: the revisions of the
//! workload that rolls it out, of the Helm release that installed it, an Argo CD
//! Application's deployments, which field managers wrote it and when
//! (`metadata.managedFields`), and its Events. Nothing runs in the background: each source
//! is read when asked (a page's History tab, the agents' `get_changes`).
//!
//! Every source makes [`Change`]s, which [`merge`] puts in one list, newest first. A
//! change's diff is kept as its two versions and only turned into lines when shown
//! ([`Diff::lines`]). Changes recorded while the app watches (`crate::timeline`) come as
//! `Change`s of their own [`Source`] and are merged the same way.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::Result;
use k8s_openapi::api::apps::v1 as apps;
use kube::Client;
use serde_json::{Value, json};

use crate::helm::{self, Release};
use crate::k8s::{self, EventLine, Kind, ObjectRef, rollouts};

/// Where a change was read from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Source {
    /// A Deployment's ReplicaSets, a StatefulSet's or DaemonSet's ControllerRevisions.
    Rollout,
    /// A Helm release's revisions.
    Helm,
    /// An Argo CD Application's deployments and its last operation.
    ArgoCd,
    /// `metadata.managedFields`: which manager last wrote which fields, and when.
    Fields,
    Event,
    /// Recorded by the app while it watched the object (see `crate::timeline`).
    Recorded,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rollout => "Rollout",
            Self::Helm => "Helm",
            Self::ArgoCd => "Argo CD",
            Self::Fields => "Fields",
            Self::Event => "Event",
            Self::Recorded => "Recorded",
        }
    }
}

/// One change to an object, whatever its source.
#[derive(Clone)]
pub struct Change {
    /// When it happened; None when the source doesn't say (listed last).
    pub time: Option<jiff::Timestamp>,
    pub source: Source,
    /// What changed: `ReplicaSet web-7d9f (revision 4)`, `Helm release web, revision 5`.
    pub object: String,
    /// What changed in it, in one line.
    pub summary: String,
    /// A failure, or a Warning event.
    pub warning: bool,
    /// The versions before and after, when the source keeps both.
    pub diff: Option<Arc<Diff>>,
}

impl Change {
    /// Names the change across reloads of the list (to keep its diff open).
    pub fn key(&self) -> String {
        format!("{}|{}|{}", self.source.label(), self.object, self.time.map_or(0, jiff::Timestamp::as_second))
    }
}

/// Two versions of what changed, noise taken out. They are written as YAML and compared
/// only when the diff is shown.
pub struct Diff {
    pub before: Value,
    pub after: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffLine {
    /// Unchanged lines left out, before each group of changes.
    Gap,
    Same(String),
    Removed(String),
    Added(String),
}

impl Diff {
    pub fn lines(&self) -> Vec<DiffLine> {
        diff_lines(&yaml(&self.before), &yaml(&self.after))
    }

    /// The changed lines alone (`-` and `+`), at most `max`, then how many more there are.
    pub fn short(&self, max: usize) -> String {
        let changed: Vec<String> = self
            .lines()
            .into_iter()
            .filter_map(|l| match l {
                DiffLine::Removed(t) => Some(format!("- {t}")),
                DiffLine::Added(t) => Some(format!("+ {t}")),
                DiffLine::Gap | DiffLine::Same(_) => None,
            })
            .collect();
        let mut out: Vec<String> = changed.iter().take(max).cloned().collect();
        if changed.len() > max {
            out.push(format!("… {} more changed lines", changed.len() - max));
        }
        out.join("\n")
    }
}

fn yaml(v: &Value) -> String {
    if v.is_null() { String::new() } else { crate::yaml::to_string(v).unwrap_or_default() }
}

/// A line diff of two texts as shown: the changes with three lines around them, a gap
/// before each group. Empty when they are the same.
pub fn diff_lines(before: &str, after: &str) -> Vec<DiffLine> {
    let d = similar::TextDiff::from_lines(before, after);
    let mut lines = Vec::new();
    for group in d.grouped_ops(3) {
        lines.push(DiffLine::Gap);
        for op in group {
            for change in d.iter_changes(&op) {
                let text = change.value().trim_end().to_string();
                lines.push(match change.tag() {
                    similar::ChangeTag::Delete => DiffLine::Removed(text),
                    similar::ChangeTag::Insert => DiffLine::Added(text),
                    similar::ChangeTag::Equal => DiffLine::Same(text),
                });
            }
        }
    }
    lines
}

/// Remove null fields, at every level (they read as noise in a diff).
pub fn drop_nulls(v: &mut Value) {
    match v {
        Value::Object(m) => {
            m.retain(|_, v| !v.is_null());
            m.values_mut().for_each(drop_nulls);
        }
        Value::Array(a) => a.iter_mut().for_each(drop_nulls),
        _ => {}
    }
}

/// Several sources' changes in one list, newest first; changes with no time last.
pub fn merge(lists: impl IntoIterator<Item = Vec<Change>>) -> Vec<Change> {
    let mut all: Vec<Change> = lists.into_iter().flatten().collect();
    // Stable: changes at the same second keep their source's order.
    all.sort_by_key(|c| std::cmp::Reverse(c.time));
    all
}

fn timestamp(v: Option<&Value>) -> Option<jiff::Timestamp> {
    v?.as_str()?.parse().ok()
}

// MARK: Rollouts

/// The kinds whose revisions Kubernetes keeps.
pub fn rolls_out(kind: &Kind) -> bool {
    kind.is("apps", "Deployment") || kind.is("apps", "StatefulSet") || kind.is("apps", "DaemonSet")
}

/// The annotation `kubectl rollout restart` sets in the pod template.
const RESTARTED_AT: &str = "kubectl.kubernetes.io/restartedAt";

/// Paths of a template diff shown in a summary, the rest counted.
const SHOWN_PATHS: usize = 5;

/// Paths of a template diff stop at this many segments (`spec.containers[app].env`).
const PATH_DEPTH: usize = 4;

/// One change per revision (ReplicaSets, or ControllerRevisions shaped like them by
/// `rollouts::revisions`), each compared with the revision before on its pod template.
pub fn revision_changes(revs: &[Value]) -> Vec<Change> {
    let mut sorted: Vec<&Value> = revs.iter().collect();
    sorted.sort_by_key(|r| rollouts::number(r));
    let mut before: Option<Value> = None;
    let mut out = Vec::with_capacity(sorted.len());
    for rev in sorted {
        let s = |p: &str| rev.pointer(p).and_then(Value::as_str).filter(|s| !s.is_empty());
        let after = comparable_template(rev);
        let mut summary = template_summary(before.as_ref(), &after);
        // Rolled back to: the old ReplicaSet takes the new number and keeps its creation time;
        // the controller's last write to it (not its status) is when it came back.
        let rolled_back = s("/metadata/annotations/deployment.kubernetes.io~1revision-history");
        let mut time = timestamp(rev.pointer("/metadata/creationTimestamp"));
        if let Some(old) = rolled_back {
            summary = format!("back to the template of revision {old}: {summary}");
            time = last_write(rev).or(time);
        }
        if let Some(cause) = s("/metadata/annotations/kubernetes.io~1change-cause") {
            summary = format!("{summary} ({cause})");
        }
        out.push(Change {
            time,
            source: Source::Rollout,
            object: format!(
                "{} {} (revision {})",
                s("/kind").unwrap_or("ReplicaSet"),
                s("/metadata/name").unwrap_or_default(),
                rollouts::number(rev)
            ),
            summary,
            warning: false,
            diff: before.take().map(|b| Arc::new(Diff { before: b, after: after.clone() })),
        });
        before = Some(after);
    }
    out
}

/// When a field manager last wrote the object, status writes left out.
fn last_write(obj: &Value) -> Option<jiff::Timestamp> {
    obj.pointer("/metadata/managedFields")?
        .as_array()?
        .iter()
        .filter(|m| m["subresource"] != "status")
        .filter_map(|m| timestamp(m.get("time")))
        .max()
}

/// A revision's pod template without what differs on every revision anyway: the
/// `pod-template-hash` label, the template's null `creationTimestamp`, and the `$patch` a
/// ControllerRevision keeps.
pub fn comparable_template(rev: &Value) -> Value {
    let mut t = rev.pointer("/spec/template").cloned().unwrap_or(Value::Null);
    if let Some(o) = t.as_object_mut() {
        o.remove("$patch");
    }
    if let Some(m) = t.get_mut("metadata").and_then(Value::as_object_mut) {
        m.remove("creationTimestamp");
        let no_labels = m.get_mut("labels").and_then(Value::as_object_mut).is_some_and(|labels| {
            labels.remove("pod-template-hash");
            labels.is_empty()
        });
        if no_labels {
            m.remove("labels");
        }
    }
    drop_nulls(&mut t);
    t
}

/// Each container's image (init containers included), by container name.
fn container_images(template: &Value) -> BTreeMap<String, String> {
    ["/spec/containers", "/spec/initContainers"]
        .iter()
        .filter_map(|p| template.pointer(p).and_then(Value::as_array))
        .flatten()
        .filter_map(|c| Some((c["name"].as_str()?.to_string(), c["image"].as_str().unwrap_or("?").to_string())))
        .collect()
}

/// What changed from one pod template to the next, in one line: images first, a restart,
/// then the other fields that changed.
fn template_summary(before: Option<&Value>, after: &Value) -> String {
    let images = container_images(after);
    let Some(before) = before else {
        let list: Vec<String> = images.iter().map(|(c, i)| format!("{c}={i}")).collect();
        return format!("oldest revision kept: {}", list.join(", "));
    };
    let old = container_images(before);
    let mut parts = Vec::new();
    for (c, image) in &images {
        match old.get(c) {
            None => parts.push(format!("container {c} added ({image})")),
            Some(was) if was != image => parts.push(format!("{c}: {was} → {image}")),
            Some(_) => {}
        }
    }
    parts.extend(old.keys().filter(|c| !images.contains_key(*c)).map(|c| format!("container {c} removed")));
    let restarted = |t: &Value| t.pointer("/metadata/annotations").and_then(|a| a.get(RESTARTED_AT)).cloned();
    if restarted(after).is_some() && restarted(before) != restarted(after) {
        parts.push("restarted".into());
    }
    let mut paths = Vec::new();
    changed_paths(&without_restart(before), &without_restart(after), &mut Vec::new(), &mut paths);
    // Said above already: images, containers added or removed.
    paths.retain(|p| {
        let in_containers = p.len() >= 3 && p[0] == "spec" && (p[1] == "containers" || p[1] == "initContainers");
        let image = in_containers && p.len() == 4 && p[3] == "image";
        let whole_container = in_containers && p.len() == 3;
        let said = image || whole_container;
        !said
    });
    if !paths.is_empty() {
        let shown: Vec<String> = paths.iter().take(SHOWN_PATHS).map(|p| join_path(p)).collect();
        let more = paths.len().saturating_sub(SHOWN_PATHS);
        let more = if more > 0 { format!(" (+{more} more)") } else { String::new() };
        parts.push(format!("changed {}{more}", shown.join(", ")));
    }
    if parts.is_empty() { "no change to the pod template".into() } else { parts.join("; ") }
}

/// A template without the restart annotation (said on its own), nor the annotations it
/// leaves empty.
fn without_restart(template: &Value) -> Value {
    let mut t = template.clone();
    if let Some(m) = t.get_mut("metadata").and_then(Value::as_object_mut) {
        let none_left = m.get_mut("annotations").and_then(Value::as_object_mut).is_some_and(|a| {
            a.remove(RESTARTED_AT);
            a.is_empty()
        });
        if none_left {
            m.remove("annotations");
        }
    }
    t
}

/// Where two values differ, as paths of at most `PATH_DEPTH` segments. Lists of named items
/// (containers, env, ports) are matched by name, so one added doesn't shift the others.
pub(crate) fn changed_paths(a: &Value, b: &Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    if a == b {
        return;
    }
    if path.len() < PATH_DEPTH {
        if let (Some(x), Some(y)) = (a.as_object(), b.as_object()) {
            let found = out.len();
            let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for k in keys {
                path.push(k.clone());
                changed_paths(x.get(k).unwrap_or(&Value::Null), y.get(k).unwrap_or(&Value::Null), path, out);
                path.pop();
            }
            // Only a null field added or removed: the object itself changed.
            if out.len() > found {
                return;
            }
        }
        if let (Some(x), Some(y)) = (named(a), named(b)) {
            let found = out.len();
            let keys: BTreeSet<&str> = x.keys().chain(y.keys()).copied().collect();
            for k in keys {
                path.push(format!("[{k}]"));
                let (before, after) = (x.get(k).copied(), y.get(k).copied());
                changed_paths(before.unwrap_or(&Value::Null), after.unwrap_or(&Value::Null), path, out);
                path.pop();
            }
            // The same items in another order (or a name twice): the list itself changed.
            if out.len() > found {
                return;
            }
        }
    }
    if !path.is_empty() {
        out.push(path.clone());
    }
}

/// A list whose items all have a name, by name; None for any other value.
fn named(v: &Value) -> Option<BTreeMap<&str, &Value>> {
    v.as_array()?.iter().map(|i| Some((i.get("name")?.as_str()?, i))).collect()
}

/// `spec.containers[app].env` from its segments.
pub(crate) fn join_path(segments: &[String]) -> String {
    let mut s = String::new();
    for seg in segments {
        if !s.is_empty() && !seg.starts_with('[') {
            s.push('.');
        }
        s.push_str(seg);
    }
    s
}

// MARK: Helm

/// The Helm release that installed an object, from the annotations Helm puts on what it
/// installs: (namespace, name).
pub fn helm_release_of(obj: &Value) -> Option<(String, String)> {
    let a = obj.pointer("/metadata/annotations")?;
    let name = a.get("meta.helm.sh/release-name")?.as_str()?;
    let ns = a.get("meta.helm.sh/release-namespace")?.as_str()?;
    Some((ns.to_string(), name.to_string()))
}

/// The values of a revision against those of the revision before, secret-looking values
/// masked unless `reveal`. None for the first revision.
pub fn helm_values_diff(before: Option<&Release>, rev: &Release, reveal: bool) -> Option<Diff> {
    let values = |r: &Release| {
        let mut v = if r.values.is_null() { json!({}) } else { r.values.clone() };
        if !reveal {
            helm::mask_secrets(&mut v);
        }
        v
    };
    before.map(|b| Diff { before: values(b), after: values(rev) })
}

/// One change per revision of a release: its status and chart, with the values diff from
/// the revision before (secret-looking values masked).
pub fn helm_changes(revs: &[Release]) -> Vec<Change> {
    let mut sorted: Vec<&Release> = revs.iter().collect();
    sorted.sort_by_key(|r| r.revision);
    let mut out = Vec::with_capacity(sorted.len());
    for (i, r) in sorted.iter().enumerate() {
        let prev = i.checked_sub(1).map(|j| sorted[j]);
        let diff = helm_values_diff(prev, r, false).filter(|d| d.before != d.after);
        let mut summary = format!("{}: {}-{}", r.status, r.chart, r.chart_version);
        if !r.app_version.is_empty() {
            summary = format!("{summary}, app {}", r.app_version);
        }
        if let Some(p) = prev
            && (&p.chart, &p.chart_version) != (&r.chart, &r.chart_version)
        {
            summary = format!("{summary} (was {}-{})", p.chart, p.chart_version);
        }
        if diff.is_some() {
            summary = format!("{summary}; values changed");
        }
        if !r.description.is_empty() {
            summary = format!("{summary} — {}", r.description);
        }
        out.push(Change {
            time: r.updated,
            source: Source::Helm,
            object: format!("Helm release {}, revision {}", r.name, r.revision),
            summary,
            warning: r.status == "failed",
            diff: diff.map(Arc::new),
        });
    }
    out
}

// MARK: Argo CD

/// An Argo CD Application's deployments (`status.history`), each compared with the one
/// before on its revision and source, and its last operation when it did not succeed (one
/// that did is the newest deployment).
pub fn argo_changes(app: &Value) -> Vec<Change> {
    let name = app.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default();
    let mut history: Vec<&Value> =
        app.pointer("/status/history").and_then(Value::as_array).into_iter().flatten().collect();
    history.sort_by_key(|h| h["id"].as_i64().unwrap_or(0));
    let mut out = Vec::with_capacity(history.len() + 1);
    let mut before: Option<Value> = None;
    for h in history {
        let mut deployed = json!({
            "revision": h.get("revision"), "revisions": h.get("revisions"),
            "source": h.get("source"), "sources": h.get("sources"),
        });
        drop_nulls(&mut deployed);
        let revision = k8s::argo::revisions(h).unwrap_or_else(|| "?".into());
        out.push(Change {
            time: timestamp(h.get("deployedAt")),
            source: Source::ArgoCd,
            object: format!("Application {name}, deployment {}", h["id"].as_i64().unwrap_or(0)),
            summary: format!("deployed {revision}{}{}", argo_source(h), argo_initiator(h)),
            warning: false,
            diff: before
                .take()
                .filter(|b| *b != deployed)
                .map(|b| Arc::new(Diff { before: b, after: deployed.clone() })),
        });
        before = Some(deployed);
    }
    if let Some(op) = app.pointer("/status/operationState")
        && let Some(phase) = op["phase"].as_str()
        && phase != "Succeeded"
    {
        let message = op["message"].as_str().filter(|m| !m.is_empty()).map(|m| format!(": {m}")).unwrap_or_default();
        out.push(Change {
            time: timestamp(op.get("finishedAt")).or_else(|| timestamp(op.get("startedAt"))),
            source: Source::ArgoCd,
            object: format!("Application {name}, last sync"),
            summary: format!("sync {phase}{message}"),
            warning: matches!(phase, "Failed" | "Error"),
            diff: None,
        });
    }
    out
}

/// ` from <repo> <path or chart> @ <target>`, from a deployment's (first) source.
fn argo_source(h: &Value) -> String {
    let Some(src) = h.get("source").filter(|s| s.is_object()).or_else(|| h.pointer("/sources/0")) else {
        return String::new();
    };
    let s = |k: &str| src[k].as_str().filter(|s| !s.is_empty());
    let repo = s("repoURL").map(|r| r.trim_start_matches("https://").trim_end_matches(".git").to_string());
    let what = s("path").or_else(|| s("chart")).map(String::from);
    let parts: Vec<String> =
        [repo, what, s("targetRevision").map(|t| format!("@ {t}"))].into_iter().flatten().collect();
    if parts.is_empty() { String::new() } else { format!(" from {}", parts.join(" ")) }
}

/// ` by <user>`, or ` by auto-sync`.
fn argo_initiator(h: &Value) -> String {
    if h.pointer("/initiatedBy/automated").and_then(Value::as_bool) == Some(true) {
        return " by auto-sync".into();
    }
    h.pointer("/initiatedBy/username").and_then(Value::as_str).map(|u| format!(" by {u}")).unwrap_or_default()
}

/// The deployments of the Applications that deployed an object, as its page's Argo CD
/// panel read them (the last 5 each, no diffs).
pub fn argo_lookup_changes(lookup: &k8s::argo::Lookup) -> Vec<Change> {
    lookup
        .apps
        .iter()
        .flat_map(|found| {
            found.app.history.iter().map(move |d| Change {
                time: d.deployed_at.as_deref().and_then(|t| t.parse().ok()),
                source: Source::ArgoCd,
                object: format!("Application {}", found.app.name),
                summary: format!(
                    "deployed {}{}",
                    d.revision,
                    d.by.as_ref().map(|b| format!(" by {b}")).unwrap_or_default()
                ),
                warning: false,
                diff: None,
            })
        })
        .collect()
}

// MARK: Field managers

/// Fields named in a field manager's summary, the rest counted.
const SHOWN_FIELDS: usize = 6;

/// Who last wrote the object, and when: one change per field manager in
/// `metadata.managedFields`, with the fields it owns. Writes to the status are left out:
/// controllers make them all the time, and they say nothing of what someone changed.
pub fn field_changes(obj: &Value, subject: &str) -> Vec<Change> {
    obj.pointer("/metadata/managedFields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|m| m["subresource"] != "status")
        .filter_map(|m| {
            let time = timestamp(m.get("time"))?;
            let mut paths = Vec::new();
            field_paths(m.get("fieldsV1")?, &mut Vec::new(), &mut paths);
            paths.retain(|p| p.first().is_some_and(|s| s != "status"));
            if paths.is_empty() {
                return None;
            }
            let manager = m["manager"].as_str().filter(|s| !s.is_empty()).unwrap_or("an unnamed manager");
            let verb = if m["operation"] == "Apply" { "applied" } else { "updated" };
            let through = m["subresource"].as_str().filter(|s| !s.is_empty()).map(|s| format!(" (through {s})"));
            Some(Change {
                time: Some(time),
                source: Source::Fields,
                object: subject.to_string(),
                summary: format!("{manager} {verb}{}: {}", through.unwrap_or_default(), fields_summary(&paths)),
                warning: false,
                diff: None,
            })
        })
        .collect()
}

/// The leaf fields of a `fieldsV1` set, as path segments.
pub(crate) fn field_paths(v: &Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    // "." marks the object itself as owned; its fields are listed next to it.
    let children: Vec<(&String, &Value)> =
        v.as_object().into_iter().flatten().filter(|(k, _)| k.as_str() != ".").collect();
    if children.is_empty() {
        if !path.is_empty() {
            out.push(path.clone());
        }
        return;
    }
    for (k, child) in children {
        path.push(field_segment(k));
        field_paths(child, path, out);
        path.pop();
    }
}

/// `f:image` → `image`, `k:{"name":"app"}` → `[app]`, `v:"x"` → `["x"]`.
fn field_segment(key: &str) -> String {
    if let Some(field) = key.strip_prefix("f:") {
        return field.to_string();
    }
    if let Some(item) = key.strip_prefix("k:") {
        let parsed: Value = serde_json::from_str(item).unwrap_or(Value::Null);
        let label = match parsed.get("name").and_then(Value::as_str) {
            Some(name) => name.to_string(),
            None => parsed
                .as_object()
                .map(|m| m.values().map(|v| v.as_str().map_or_else(|| v.to_string(), String::from)).collect::<Vec<_>>())
                .map_or_else(|| item.to_string(), |values| values.join(",")),
        };
        return format!("[{label}]");
    }
    match key.strip_prefix("v:").or_else(|| key.strip_prefix("i:")) {
        Some(value) => format!("[{value}]"),
        None => key.to_string(),
    }
}

/// The fields a manager owns, in a few paths: the fields under the parent that has the most
/// of them are folded into it, until `SHOWN_FIELDS` paths are left.
pub(crate) fn fields_summary(paths: &[Vec<String>]) -> String {
    let mut shown: BTreeSet<Vec<String>> = paths.iter().cloned().collect();
    while shown.len() > SHOWN_FIELDS {
        let mut parents: BTreeMap<&[String], usize> = BTreeMap::new();
        for p in shown.iter().filter(|p| p.len() > 1) {
            *parents.entry(&p[..p.len() - 1]).or_default() += 1;
        }
        let Some((parent, children)) = parents.into_iter().max_by_key(|(_, n)| *n) else { break };
        if children < 2 {
            break;
        }
        let parent = parent.to_vec();
        shown.retain(|p| !p.starts_with(&parent));
        shown.insert(parent);
    }
    let more = shown.len().saturating_sub(SHOWN_FIELDS);
    let text = shown.iter().take(SHOWN_FIELDS).map(|p| join_path(p)).collect::<Vec<_>>().join(", ");
    if more > 0 { format!("{text} (+{more} more)") } else { text }
}

// MARK: Events

/// An object's events, as changes (Warnings marked).
pub fn event_changes(events: &[EventLine], subject: &str) -> Vec<Change> {
    events
        .iter()
        .map(|e| Change {
            time: e.last,
            source: Source::Event,
            object: subject.to_string(),
            summary: format!(
                "{} {}: {}{}",
                e.kind,
                e.reason,
                e.message,
                if e.count > 1 { format!(" (×{})", e.count) } else { String::new() }
            ),
            warning: e.kind == "Warning",
            diff: None,
        })
        .collect()
}

// MARK: Reading

/// The changes the object itself tells: its field managers, and an Argo CD Application's
/// deployments.
pub fn own_changes(kind: &Kind, obj: &Value) -> Vec<Change> {
    let name = obj.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default();
    let mut out = field_changes(obj, &format!("{} {name}", kind.ar.kind));
    if kind.is("argoproj.io", "Application") {
        out.extend(argo_changes(obj));
    }
    out
}

/// A read [`fetch`] makes besides the object, for its caller to allow: the app reads what
/// its user may (the API server decides), an agent what its rules allow.
pub enum Read<'a> {
    /// One object: a pod's ReplicaSet, the ReplicaSet's Deployment.
    Get { kind: &'a Kind, ns: &'a str, name: &'a str },
    /// Helm's Secrets in a namespace: listing them (`secret` None), then each revision's.
    Helm { ns: &'a str, secret: Option<&'a str> },
}

pub type Allowed = Arc<dyn Fn(Read<'_>) -> bool + Send + Sync>;

/// Every read allowed: the app's own pages, where the API server decides.
pub fn all_allowed() -> Allowed {
    Arc::new(|_: Read<'_>| true)
}

/// The changes read from the cluster, and why a source could not be read.
#[derive(Clone, Default)]
pub struct Fetched {
    pub changes: Vec<Change>,
    pub errors: Vec<String>,
}

/// What the cluster keeps of an object's changes beyond the object itself: the revisions of
/// the workload that rolls it out (itself, or a pod's or ReplicaSet's owner) and of the Helm
/// release that installed it. A source that can't be read is said in `errors`; the others
/// are still read.
pub async fn fetch(client: Client, kind: Kind, ns: Option<String>, obj: Value, allowed: Allowed) -> Fetched {
    let mut out = Fetched::default();
    let mut workload = None;
    if let Some(ns) = &ns {
        match workload_of(&client, &kind, ns, &obj, &allowed).await {
            Ok(w) => workload = w,
            Err(e) => out.errors.push(format!("its owner: {}", k8s::short_error(&e))),
        }
        if let Some((wkind, wobj)) = &workload {
            match rollouts::revisions(client.clone(), wkind, ns.clone(), wobj).await {
                Ok(revs) => out.changes.extend(revision_changes(&revs)),
                Err(e) => out.errors.push(format!("{} revisions: {}", wkind.ar.kind, k8s::short_error(&e))),
            }
        }
    }
    let release = helm_release_of(&obj).or_else(|| workload.as_ref().and_then(|(_, w)| helm_release_of(w)));
    if let Some((rns, rname)) = release
        && allowed(Read::Helm { ns: &rns, secret: None })
    {
        match helm::history(client, rns.clone(), rname.clone()).await {
            Ok(revs) => {
                let revs: Vec<Release> = revs
                    .into_iter()
                    .filter(|r| {
                        let secret = format!("sh.helm.release.v1.{}.v{}", r.name, r.revision);
                        allowed(Read::Helm { ns: &rns, secret: Some(&secret) })
                    })
                    .collect();
                out.changes.extend(helm_changes(&revs));
            }
            Err(e) => out.errors.push(format!("Helm release {rname}: {}", k8s::short_error(&e))),
        }
    }
    out
}

/// The workload that rolls the object out: itself, or the controller that owns a pod or a
/// ReplicaSet (a pod's ReplicaSet's Deployment). Each owner read must be the one its
/// reference names (by uid): one re-created under the name is not.
async fn workload_of(
    client: &Client,
    kind: &Kind,
    ns: &str,
    obj: &Value,
    allowed: &Allowed,
) -> Result<Option<(Kind, Value)>> {
    if rolls_out(kind) {
        return Ok(Some((kind.clone(), obj.clone())));
    }
    if !kind.is("", "Pod") && !kind.is("apps", "ReplicaSet") {
        return Ok(None);
    }
    let mut current = obj.clone();
    // The owner, then the owner's owner.
    for _ in 0..2 {
        let Some((owner_kind, name, uid)) = controller_of(&current) else { return Ok(None) };
        anyhow::ensure!(
            allowed(Read::Get { kind: &owner_kind, ns, name: &name }),
            "{} {name}: not allowed by your rules",
            owner_kind.ar.kind
        );
        let owner_ref =
            ObjectRef { kind: owner_kind.clone(), name, namespace: Some(ns.to_string()), uid: String::new() };
        let owner = k8s::get_object(client.clone(), owner_ref).await?;
        if owner.pointer("/metadata/uid").and_then(Value::as_str) != Some(uid.as_str()) {
            return Ok(None);
        }
        if rolls_out(&owner_kind) {
            return Ok(Some((owner_kind, owner)));
        }
        current = owner;
    }
    Ok(None)
}

/// The `apps` controller that owns an object: its kind, name and uid.
fn controller_of(obj: &Value) -> Option<(Kind, String, String)> {
    let owners = obj.pointer("/metadata/ownerReferences")?.as_array()?;
    let owner = owners.iter().find(|o| o["controller"].as_bool() == Some(true))?;
    if !owner["apiVersion"].as_str()?.starts_with("apps/") {
        return None;
    }
    let ar = match owner["kind"].as_str()? {
        "ReplicaSet" => k8s::ar_of::<apps::ReplicaSet>(),
        "Deployment" => k8s::ar_of::<apps::Deployment>(),
        "StatefulSet" => k8s::ar_of::<apps::StatefulSet>(),
        "DaemonSet" => k8s::ar_of::<apps::DaemonSet>(),
        _ => return None,
    };
    let kind = Kind { label: ar.kind.clone(), ar, namespaced: true, section: "" };
    Some((kind, owner["name"].as_str()?.to_string(), owner["uid"].as_str()?.to_string()))
}

// MARK: For agents

/// Changed lines shown per change to an agent.
const SHORT_DIFF_LINES: usize = 12;

/// The changes as `get_changes` returns them: newest first, at most `limit`, each with the
/// lines its diff changes; then what could not be read.
pub fn report(subject: &str, changes: &[Change], errors: &[String], limit: usize) -> String {
    let mut out = format!("Changes to {subject}, newest first:\n");
    if changes.is_empty() {
        out.push_str(
            "None found. Kubernetes keeps revisions of Deployments, StatefulSets and DaemonSets only, \
             and events for about an hour; the app records other changes only while it watches the object.\n",
        );
    }
    for c in changes.iter().take(limit) {
        let when = c.time.map_or_else(
            || "time unknown".to_string(),
            |t| format!("{} ({} ago)", t.strftime("%Y-%m-%d %H:%M:%SZ"), k8s::age(t)),
        );
        out.push_str(&format!("{when}  {}  {}: {}\n", c.source.label(), c.object, c.summary));
        if let Some(d) = &c.diff {
            for line in d.short(SHORT_DIFF_LINES).lines() {
                out.push_str(&format!("    {line}\n"));
            }
        }
    }
    if changes.len() > limit {
        out.push_str(&format!("… {} older changes not shown (raise `limit`)\n", changes.len() - limit));
    }
    for e in errors {
        out.push_str(&format!("Could not read {e}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn rs(name: &str, revision: i64, hash: &str, image: &str, env: &str, cpu: &str) -> Value {
        json!({
            "metadata": {
                "name": name, "creationTimestamp": format!("2026-01-0{revision}T00:00:00Z"),
                "annotations": {"deployment.kubernetes.io/revision": revision.to_string()},
            },
            "spec": {"template": {
                "metadata": {"creationTimestamp": null, "labels": {"app": "web", "pod-template-hash": hash}},
                "spec": {"containers": [{
                    "name": "app", "image": image,
                    "env": [{"name": "MODE", "value": env}],
                    "resources": {"requests": {"cpu": cpu}},
                }]},
            }},
        })
    }

    /// The removed and added lines, sorted, without their indent or list dash: how the YAML
    /// writer lays lists out, and in which order it writes keys, is not what these test.
    fn texts(lines: &[DiffLine]) -> (Vec<&str>, Vec<&str>) {
        let pick = |removed: bool| {
            let mut v: Vec<&str> = lines
                .iter()
                .filter_map(|l| match l {
                    DiffLine::Removed(t) if removed => Some(t.trim().trim_start_matches("- ")),
                    DiffLine::Added(t) if !removed => Some(t.trim().trim_start_matches("- ")),
                    _ => None,
                })
                .collect();
            v.sort_unstable();
            v
        };
        (pick(true), pick(false))
    }

    /// The diff of two ReplicaSets' templates shows the image, env and resources change and
    /// nothing else: not the pod-template-hash each one has.
    #[test]
    fn a_template_diff_shows_only_what_changed() {
        let revs = [
            rs("web-bbb", 2, "bbb", "nginx:1.26", "fast", "200m"),
            rs("web-aaa", 1, "aaa", "nginx:1.25", "safe", "100m"),
        ];
        let changes = revision_changes(&revs);
        assert_eq!(changes.len(), 2);
        let (first, second) = (&changes[0], &changes[1]);
        assert!(first.diff.is_none() && first.summary.contains("app=nginx:1.25"), "{}", first.summary);
        assert_eq!(second.object, "ReplicaSet web-bbb (revision 2)");
        assert_eq!(second.time, Some("2026-01-02T00:00:00Z".parse().unwrap()));
        let lines = second.diff.as_ref().unwrap().lines();
        let (removed, added) = texts(&lines);
        assert_eq!(removed, ["cpu: 100m", "image: nginx:1.25", "value: safe"], "{lines:?}");
        assert_eq!(added, ["cpu: 200m", "image: nginx:1.26", "value: fast"], "{lines:?}");
        assert!(!lines.iter().any(|l| format!("{l:?}").contains("pod-template-hash")), "{lines:?}");
        assert_eq!(
            second.summary,
            "app: nginx:1.25 → nginx:1.26; changed spec.containers[app].env, spec.containers[app].resources"
        );
    }

    #[test]
    fn a_controller_revision_s_patch_marker_is_noise() {
        let shaped = |n: i64, image: &str| {
            json!({"kind": "ControllerRevision", "metadata": {"name": format!("db-{n}"),
                "annotations": {"deployment.kubernetes.io/revision": n.to_string()}},
                "spec": {"template": {"$patch": "replace", "spec": {"containers": [{"name": "db", "image": image}]}}}})
        };
        let changes = revision_changes(&[shaped(1, "pg:16"), shaped(2, "pg:17")]);
        assert_eq!(changes[1].object, "ControllerRevision db-2 (revision 2)");
        assert_eq!(changes[1].summary, "db: pg:16 → pg:17");
        let lines = changes[1].diff.as_ref().unwrap().lines();
        assert!(!lines.iter().any(|l| format!("{l:?}").contains("$patch")), "{lines:?}");
    }

    #[test]
    fn a_restart_and_a_rollback_are_said() {
        let plain = rs("web-ccc", 2, "ccc", "nginx:1.25", "safe", "100m");
        let mut restarted = rs("web-ddd", 3, "ddd", "nginx:1.25", "safe", "100m");
        restarted["spec"]["template"]["metadata"]["annotations"] = json!({RESTARTED_AT: "2026-01-03T00:00:00Z"});
        let mut back = rs("web-aaa", 4, "aaa", "nginx:1.24", "safe", "100m");
        back["metadata"]["annotations"]["deployment.kubernetes.io/revision-history"] = json!("1");
        back["metadata"]["managedFields"] = json!([
            {"manager": "kube-controller-manager", "operation": "Update", "time": "2026-01-09T00:00:00Z"},
            {"manager": "kube-controller-manager", "operation": "Update", "subresource": "status", "time": "2026-01-10T00:00:00Z"},
        ]);
        let changes = revision_changes(&[back, restarted, plain]);
        assert_eq!(changes[1].summary, "restarted");
        assert_eq!(changes[2].summary, "back to the template of revision 1: app: nginx:1.25 → nginx:1.24");
        // When it came back, not when the ReplicaSet was first made; status writes don't count.
        assert_eq!(changes[2].time, Some("2026-01-09T00:00:00Z".parse().unwrap()));
    }

    #[test]
    fn field_managers_say_who_wrote_what() {
        let obj = json!({"metadata": {"managedFields": [
            {"manager": "kubectl-set", "operation": "Update", "time": "2026-01-02T00:00:00Z", "fieldsType": "FieldsV1",
             "fieldsV1": {"f:spec": {"f:template": {"f:spec": {"f:containers": {"k:{\"name\":\"app\"}": {".": {}, "f:image": {}}}}}}}},
            {"manager": "kubelet", "operation": "Update", "subresource": "status", "time": "2026-01-03T00:00:00Z",
             "fieldsV1": {"f:status": {"f:phase": {}}}},
            {"manager": "argocd-controller", "operation": "Apply", "time": "2026-01-01T00:00:00Z",
             "fieldsV1": {"f:spec": {"f:ports": {"k:{\"port\":80,\"protocol\":\"TCP\"}": {}}}, "f:status": {}}},
            {"manager": "kubectl", "operation": "Update", "subresource": "scale", "time": "2026-01-04T00:00:00Z",
             "fieldsV1": {"f:spec": {"f:replicas": {}}}},
        ]}});
        let changes = field_changes(&obj, "Deployment web");
        let summaries: Vec<&str> = changes.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(
            summaries,
            [
                "kubectl-set updated: spec.template.spec.containers[app].image",
                "argocd-controller applied: spec.ports[80,TCP]",
                "kubectl updated (through scale): spec.replicas",
            ]
        );
        assert!(changes.iter().all(|c| c.object == "Deployment web" && c.diff.is_none()));
    }

    #[test]
    fn many_fields_are_summed_up_shallower() {
        let fields: serde_json::Map<String, Value> = (0..20).map(|i| (format!("f:key{i:02}"), json!({}))).collect();
        let obj = json!({"metadata": {"managedFields": [{"manager": "helm", "operation": "Update",
            "time": "2026-01-01T00:00:00Z", "fieldsV1": {"f:data": fields, "f:metadata": {"f:labels": {"f:app": {}}}}}]}});
        assert_eq!(field_changes(&obj, "ConfigMap c")[0].summary, "helm updated: data, metadata.labels.app");
    }

    fn release(revision: i64, chart_version: &str, values: Value, status: &str) -> Release {
        let mut r = helm::decode(&helm::tests::helm_secret(
            (&format!("sh.helm.release.v1.web.v{revision}"), "shop", "web"),
            &helm::tests::payload("web", "shop", revision),
            false,
        ))
        .unwrap();
        r.chart_version = chart_version.into();
        r.values = values;
        r.status = status.into();
        r
    }

    #[test]
    fn helm_revisions_diff_their_values_with_secrets_masked() {
        let revs = [
            release(2, "1.1.0", json!({"db": {"password": "example-b", "host": "pg-2"}}), "failed"),
            release(1, "1.0.0", json!({"db": {"password": "example-a", "host": "pg-1"}}), "superseded"),
        ];
        let changes = helm_changes(&revs);
        assert_eq!(changes[1].object, "Helm release web, revision 2");
        assert_eq!(changes[1].summary, "failed: c-1.1.0 (was c-1.0.0); values changed");
        assert!(changes[1].warning && !changes[0].warning);
        let lines = changes[1].diff.as_ref().unwrap().lines();
        let (removed, added) = texts(&lines);
        assert_eq!((removed, added), (vec!["host: pg-1"], vec!["host: pg-2"]));
        assert!(!format!("{lines:?}").contains("example-"), "{lines:?}");
        let revealed = helm_values_diff(Some(&revs[1]), &revs[0], true).unwrap();
        assert!(format!("{:?}", revealed.lines()).contains("example-b"));
        // Same values: no diff to show.
        let same =
            [release(1, "1.0.0", json!({"a": 1}), "superseded"), release(2, "1.0.0", json!({"a": 1}), "deployed")];
        assert!(helm_changes(&same)[1].diff.is_none());
    }

    #[test]
    fn argo_deployments_and_a_failed_sync() {
        let app = json!({"metadata": {"name": "shop"}, "status": {
            "history": [
                {"id": 2, "revision": "b".repeat(40), "deployedAt": "2026-01-02T00:00:00Z", "initiatedBy": {"automated": true},
                 "source": {"repoURL": "https://github.com/acme/deploy.git", "path": "shop", "targetRevision": "main"}},
                {"id": 1, "revision": "a".repeat(40), "deployedAt": "2026-01-01T00:00:00Z", "initiatedBy": {"username": "ana"},
                 "source": {"repoURL": "https://github.com/acme/deploy.git", "path": "shop", "targetRevision": "main"}},
            ],
            "operationState": {"phase": "Failed", "message": "one or more objects failed", "finishedAt": "2026-01-03T00:00:00Z"},
        }});
        let changes = merge([argo_changes(&app)]);
        let summaries: Vec<&str> = changes.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(
            summaries,
            [
                "sync Failed: one or more objects failed",
                "deployed bbbbbbbb from github.com/acme/deploy shop @ main by auto-sync",
                "deployed aaaaaaaa from github.com/acme/deploy shop @ main by ana",
            ]
        );
        assert!(changes[0].warning);
        let lines = changes[1].diff.as_ref().unwrap().lines();
        let (removed, added) = texts(&lines);
        assert_eq!((removed.len(), added.len()), (1, 1));
        assert!(added[0].contains(&"b".repeat(40)));
        let done = json!({"metadata": {"name": "shop"}, "status": {"operationState": {"phase": "Succeeded"}}});
        assert!(argo_changes(&done).is_empty(), "a sync that worked is its newest deployment");
    }

    fn change(secs: Option<i64>, summary: &str) -> Change {
        Change {
            time: secs.map(|s| jiff::Timestamp::from_second(s).unwrap()),
            source: Source::Event,
            object: "Pod p".into(),
            summary: summary.into(),
            warning: false,
            diff: None,
        }
    }

    #[test]
    fn merged_newest_first_untimed_last() {
        let merged = merge([vec![change(Some(10), "a"), change(None, "x")], vec![change(Some(30), "b")]]);
        let order: Vec<&str> = merged.iter().map(|c| c.summary.as_str()).collect();
        assert_eq!(order, ["b", "a", "x"]);
    }

    #[test]
    fn the_report_keeps_to_its_limit_and_says_what_failed() {
        let mut with_diff = change(Some(20), "app: a:1 → a:2");
        with_diff.diff = Some(Arc::new(Diff { before: json!({"image": "a:1"}), after: json!({"image": "a:2"}) }));
        let changes = [with_diff, change(Some(10), "the earlier one")];
        let out = report("Deployment web", &changes, &["Helm release web: forbidden".into()], 1);
        assert!(out.starts_with("Changes to Deployment web, newest first:\n"), "{out}");
        assert!(out.contains("1970-01-01 00:00:20Z") && out.contains("Event  Pod p: app: a:1 → a:2"), "{out}");
        assert!(out.contains("    - image: a:1\n    + image: a:2\n"), "{out}");
        assert!(!out.contains("the earlier one") && out.contains("1 older changes not shown"), "{out}");
        assert!(out.ends_with("Could not read Helm release web: forbidden\n"), "{out}");
    }

    #[test]
    fn a_pod_s_owner_is_its_apps_controller() {
        let pod = json!({"metadata": {"ownerReferences": [
            {"apiVersion": "v1", "kind": "Node", "name": "n", "uid": "x"},
            {"apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "web-abc", "uid": "rs", "controller": true},
        ]}});
        let (kind, name, uid) = controller_of(&pod).unwrap();
        assert!(kind.is("apps", "ReplicaSet") && kind.namespaced);
        assert_eq!((name.as_str(), uid.as_str()), ("web-abc", "rs"));
        let job = json!({"metadata": {"ownerReferences": [{"apiVersion": "batch/v1", "kind": "Job", "name": "j", "uid": "j", "controller": true}]}});
        assert!(controller_of(&job).is_none());
    }

    /// Namespace `shop`: Deployment `web` (uid `deployment`), its ReplicaSets `web-aaa` and
    /// `web-bbb`, whatever else is asked for not found.
    #[derive(Clone)]
    struct Shop {
        deployment: &'static str,
    }

    impl tower::Service<http::Request<kube::client::Body>> for Shop {
        type Response = http::Response<http_body_util::Full<bytes::Bytes>>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<kube::client::Body>) -> Self::Future {
            let replica_set = |name: &str, revision: i64, image: &str| {
                let mut v = rs(name, revision, name, image, "x", "1");
                v["apiVersion"] = json!("apps/v1");
                v["kind"] = json!("ReplicaSet");
                v["metadata"]["uid"] = json!(name);
                v["metadata"]["ownerReferences"] = json!([{"apiVersion": "apps/v1", "kind": "Deployment", "name": "web", "uid": "deployment", "controller": true}]);
                v
            };
            let body = match req.uri().path() {
                "/apis/apps/v1/namespaces/shop/replicasets/web-bbb" => replica_set("web-bbb", 2, "nginx:1.26"),
                "/apis/apps/v1/namespaces/shop/deployments/web" => json!({
                    "apiVersion": "apps/v1", "kind": "Deployment",
                    "metadata": {"name": "web", "namespace": "shop", "uid": self.deployment},
                    "spec": {"selector": {"matchLabels": {"app": "web"}}},
                }),
                "/apis/apps/v1/namespaces/shop/replicasets" => json!({
                    "apiVersion": "apps/v1", "kind": "ReplicaSetList", "metadata": {},
                    "items": [replica_set("web-aaa", 1, "nginx:1.25"), replica_set("web-bbb", 2, "nginx:1.26")],
                }),
                _ => {
                    json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "code": 404, "reason": "NotFound"})
                }
            };
            let code = if body["kind"] == "Status" { 404 } else { 200 };
            let response = http::Response::builder().status(code).body(body.to_string().into_bytes().into()).unwrap();
            std::future::ready(Ok(response))
        }
    }

    fn pod_of_web() -> (Kind, Value) {
        let ar = k8s::ar_of::<k8s_openapi::api::core::v1::Pod>();
        let pod = json!({"metadata": {"name": "web-bbb-x1", "namespace": "shop", "ownerReferences": [
            {"apiVersion": "apps/v1", "kind": "ReplicaSet", "name": "web-bbb", "uid": "web-bbb", "controller": true},
        ]}});
        (Kind { ar, namespaced: true, section: "", label: "Pod".into() }, pod)
    }

    /// A pod's page shows its owner's history: the Deployment of its ReplicaSet.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pod_shows_its_deployment_s_revisions() {
        let (kind, pod) = pod_of_web();
        let client = Client::new(Shop { deployment: "deployment" }, "default");
        let fetched = fetch(client, kind.clone(), Some("shop".into()), pod.clone(), all_allowed()).await;
        assert!(fetched.errors.is_empty(), "{:?}", fetched.errors);
        let objects: Vec<&str> = fetched.changes.iter().map(|c| c.object.as_str()).collect();
        assert_eq!(objects, ["ReplicaSet web-aaa (revision 1)", "ReplicaSet web-bbb (revision 2)"]);
        assert_eq!(fetched.changes[1].summary, "app: nginx:1.25 → nginx:1.26");

        // A Deployment re-created under the name is not this pod's owner.
        let client = Client::new(Shop { deployment: "another" }, "default");
        let fetched = fetch(client, kind.clone(), Some("shop".into()), pod.clone(), all_allowed()).await;
        assert!(fetched.changes.is_empty() && fetched.errors.is_empty());

        // An owner the rules don't let the caller read: said, not read.
        let client = Client::new(Shop { deployment: "deployment" }, "default");
        let no_owner: Allowed = Arc::new(|read: Read<'_>| !matches!(read, Read::Get { .. }));
        let fetched = fetch(client, kind, Some("shop".into()), pod, no_owner).await;
        assert!(fetched.changes.is_empty());
        assert_eq!(fetched.errors, ["its owner: ReplicaSet web-bbb: not allowed by your rules"]);
    }

    proptest! {
        /// Revisions that differ only in their pod-template-hash have no diff.
        #[test]
        fn only_the_hash_differs_no_diff(a in "[a-z0-9]{1,10}", b in "[a-z0-9]{1,10}", image in "[a-z:.0-9]{1,20}") {
            let first = rs("web-1", 1, &a, &image, "x", "1");
            let second = rs("web-2", 2, &b, &image, "x", "1");
            let changes = revision_changes(&[first, second]);
            prop_assert!(changes[1].diff.as_ref().unwrap().lines().is_empty());
            prop_assert_eq!(&changes[1].summary, "no change to the pod template");
        }

        /// Two values differ exactly when some path says where.
        #[test]
        fn changed_paths_find_every_difference(a in json_value(), b in json_value()) {
            let mut paths = Vec::new();
            changed_paths(&a, &b, &mut vec!["root".into()], &mut paths);
            prop_assert_eq!(paths.is_empty(), a == b);
        }
    }

    fn json_value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            (0..5i64).prop_map(|n| json!(n)),
            "[ab]{0,2}".prop_map(Value::String),
        ];
        leaf.prop_recursive(4, 24, 4, |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
                prop::collection::btree_map("[nab]{1,2}", inner, 0..4)
                    .prop_map(|m| Value::Object(m.into_iter().collect())),
                prop::collection::vec(("[ab]{1,2}", (0..3i64)), 0..4).prop_map(|items| {
                    Value::Array(items.into_iter().map(|(name, v)| json!({"name": name, "v": v})).collect())
                }),
            ]
        })
    }
}
