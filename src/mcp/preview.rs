//! What an agent's change will do, worked out before its approval is asked for: the replicas
//! a `scale` sets against those asked for now; the fields a patch, a `set_image` or an
//! `apply_manifest` changes, with the diff, from a server-side dry run of the same request (a
//! dry run changes nothing); the revision a `gitops` `sync` deploys and what it leaves; what
//! a delete takes with it. Only a call that waits for an approval is worked out, in
//! [`TIMEOUT`] at most, and the approval is asked for all the same when it fails. The banner
//! shows the preview, and the activity log keeps it with the call.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::time::Duration;

use futures::StreamExt as _;
use k8s_openapi::api::apps::v1::ReplicaSet;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::api::{Api, ListParams, PartialObjectMeta};
use serde_json::Value;

use crate::history::{self, DiffLine};
use crate::json::{arr_at, nonempty_at, str_at};
use crate::k8s::argo::ops::SyncRequest;
use crate::k8s::argo::{self, diagnosis};
use crate::k8s::{self, Cascade, Kind, ObjectRef, rollouts};
use crate::plural;

/// How long working out a preview may take: past that, the approval is asked for without it.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(5);
/// Objects of a manifest worked out, the rest counted.
const MAX_OBJECTS: usize = 20;
/// Objects of a manifest worked out at once.
const AT_ONCE: usize = 6;
/// Fields listed for one object, the rest counted.
const SHOWN_FIELDS: usize = 12;
/// Field paths stop at this many segments (`spec.template.spec.containers[web].resources.limits`).
const FIELD_DEPTH: usize = 8;
/// A value longer than this is said to change, not written out.
const SHOWN_VALUE: usize = 80;
/// Diff lines kept for one object.
const OBJECT_DIFF_LINES: usize = 150;
/// Diff lines kept in all: later objects keep their fields, not their diff.
const DIFF_LINES: usize = 400;
/// Out-of-sync resources named, the rest counted.
const SHOWN_RESOURCES: usize = 20;
/// Names of the objects a delete takes with it, per kind, the rest counted.
const SHOWN_NAMES: usize = 3;
/// What the activity log keeps of a preview, at most (as the Timeline keeps of a version).
pub(crate) const MAX_LOG_BYTES: usize = 64 * 1024;
/// A Secret's value, as a preview shows it.
const HIDDEN: &str = "(hidden)";
/// The annotation `kubectl apply` keeps a copy of the object in, a Secret's values too.
const LAST_APPLIED: &str = "kubectl.kubernetes.io/last-applied-configuration";
/// Fields the server sets on every write, which a diff would only show as noise.
const NOISE: [&str; 6] = [
    "/metadata/managedFields",
    "/metadata/resourceVersion",
    "/metadata/generation",
    "/metadata/uid",
    "/metadata/creationTimestamp",
    "/status",
];
/// Why `apply_manifest` would fail where its dry run, forced, works.
pub(crate) const CONFLICT: &str =
    "apply_manifest would fail: another field manager owns some of these fields; shown as if forced";
/// The kinds whose pods (and a Deployment's ReplicaSets) a delete preview reads.
const WORKLOADS: [(&str, &str); 5] =
    [("apps", "Deployment"), ("apps", "ReplicaSet"), ("apps", "StatefulSet"), ("apps", "DaemonSet"), ("batch", "Job")];

/// What an agent's change will do, as its approval banner shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preview {
    /// Why there is no preview, or not all of it: it failed, or took too long.
    pub note: Option<String>,
    /// One per object the change touches.
    pub sections: Vec<Section>,
}

/// What a change does to one object.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Section {
    /// The object and what happens to it: `Deployment shop/web: changed`.
    pub title: String,
    /// What changes, a line each: `spec.replicas: 3 → 0`.
    pub lines: Vec<String>,
    /// A warning: the dry run failed, the change would fail, every pod stops.
    pub note: Option<String>,
    /// Its YAML now and after: the changed lines, with three around them. Empty for an object
    /// that doesn't change, and where no diff can be read (a scale, a sync, a delete).
    pub diff: Vec<DiffLine>,
}

impl Preview {
    /// These sections, within `DIFF_LINES` lines of diff in all.
    pub(crate) fn of(mut sections: Vec<Section>) -> Self {
        let mut left = DIFF_LINES;
        for s in &mut sections {
            if s.diff.len() > left {
                s.diff.clear();
                s.lines.push(format!("diff not shown: the preview holds {DIFF_LINES} lines of diff at most"));
            } else {
                left -= s.diff.len();
            }
        }
        Self { note: None, sections }
    }

    /// No preview, and why.
    pub(crate) fn unavailable(why: impl std::fmt::Display) -> Self {
        Self { note: Some(format!("Preview not available: {why}")), sections: Vec::new() }
    }

    /// The preview as the activity log keeps it: cut past `MAX_LOG_BYTES`.
    pub fn text(&self) -> String {
        let mut out = String::new();
        if let Some(note) = &self.note {
            let _ = writeln!(out, "{note}");
        }
        for s in &self.sections {
            let _ = writeln!(out, "{}", s.title);
            for line in &s.lines {
                let _ = writeln!(out, "  {line}");
            }
            if let Some(note) = &s.note {
                let _ = writeln!(out, "  ! {note}");
            }
            for line in &s.diff {
                let _ = match line {
                    DiffLine::Gap => writeln!(out, "  ⋯"),
                    DiffLine::Same(t) => writeln!(out, "    {t}"),
                    DiffLine::Removed(t) => writeln!(out, "  - {t}"),
                    DiffLine::Added(t) => writeln!(out, "  + {t}"),
                };
            }
        }
        cap(out, MAX_LOG_BYTES)
    }
}

impl Section {
    /// An object whose preview failed, and why (said as a warning).
    pub(crate) fn failed(label: &str, why: impl std::fmt::Display) -> Self {
        Self { title: label.to_string(), note: Some(why.to_string()), ..Self::default() }
    }
}

/// `text` cut to `max` bytes, on a character boundary, saying it was cut.
fn cap(mut text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    let _ = write!(text, "\n… (cut at {} KB)", max / 1024);
    text
}

/// One object of a manifest: its kind, namespace, name and content.
type Object = (Kind, Option<String>, String, Value);

/// What a change will do, worked out only when it waits for an approval (see [`preview`]).
pub(crate) enum Plan {
    /// `scale`: the replicas asked for, against those asked for now.
    Scale { client: Client, kind: Kind, ns: String, name: String, replicas: i32 },
    /// `patch_resource`: a dry run of its patch.
    Patch { client: Client, kind: Kind, ns: Option<String>, name: String, patch: kube::api::Patch<Value> },
    /// `set_image`: a dry run of the patch it makes from the live object.
    Image { client: Client, kind: Kind, ns: String, name: String, container: String, image: String },
    /// `apply_manifest`: a dry run of each object.
    Apply { client: Client, objects: Vec<Object> },
    /// `gitops` `sync` of an Argo CD Application, with its options; `app` when it was read
    /// already (to bind what a prune deletes to what is shown), so it isn't read twice.
    Sync { client: Client, kind: Kind, ns: String, name: String, req: SyncRequest, app: Option<Value> },
    /// `delete_resource`: the object, and what goes with it (or stays, with `cascade`).
    Delete { client: Client, kind: Kind, ns: Option<String>, name: String, cascade: Option<Cascade> },
    /// `evict_pod`: not a plain delete, a budget may refuse it.
    Evict { kind: Kind, ns: String, name: String },
}

/// What `plan` will change, worked out in [`TIMEOUT`] at most: what fails, or takes longer,
/// is said instead. Its dry runs change nothing.
pub(crate) async fn preview(plan: &Plan) -> Preview {
    tokio::time::timeout(TIMEOUT, work_out(plan))
        .await
        .unwrap_or_else(|_| Preview::unavailable(format!("no answer within {} s", TIMEOUT.as_secs())))
}

async fn work_out(plan: &Plan) -> Preview {
    let unread = |label: &str, e: &anyhow::Error| {
        Preview::unavailable(format!("reading {label} failed: {}", k8s::short_error(e)))
    };
    match plan {
        Plan::Scale { client, kind, ns, name, replicas } => {
            let label = object_label(kind, Some(ns.as_str()), name);
            match k8s::replicas(client.clone(), kind.clone(), Some(ns.clone()), name.clone()).await {
                Ok(now) => Preview::of(vec![scale_section(&label, now, *replicas)]),
                Err(e) => unread(&label, &e),
            }
        }
        Plan::Patch { client, kind, ns, name, patch } => {
            let label = object_label(kind, ns.as_deref(), name);
            // Neither waits for the other: the dry run doesn't need the live object.
            let live = super::server::get_named(client, kind, ns.as_deref(), name, false);
            let after = k8s::dry_run_patch(client.clone(), kind.clone(), ns.clone(), name.clone(), patch.clone());
            match tokio::join!(live, after) {
                (Ok(live), after) => Preview::of(vec![patched(&label, kind, &live, after)]),
                (Err(e), _) => unread(&label, &e),
            }
        }
        Plan::Image { client, kind, ns, name, container, image } => {
            let label = object_label(kind, Some(ns.as_str()), name);
            let live = match super::server::get_named(client, kind, Some(ns.as_str()), name, false).await {
                Ok(v) => v,
                Err(e) => return unread(&label, &e),
            };
            let patch = match rollouts::image_patch(kind, &live, container, image) {
                Ok(p) => kube::api::Patch::Strategic(p),
                Err(e) => return Preview::of(vec![Section::failed(&label, e)]),
            };
            let after = k8s::dry_run_patch(client.clone(), kind.clone(), Some(ns.clone()), name.clone(), patch).await;
            Preview::of(vec![patched(&label, kind, &live, after)])
        }
        Plan::Apply { client, objects } => Preview::of(applied(client, objects).await),
        Plan::Sync { app: Some(app), req, .. } => Preview::of(vec![sync_section(app, req)]),
        Plan::Sync { client, kind, ns, name, req, app: None } => {
            match super::server::get_named(client, kind, Some(ns.as_str()), name, false).await {
                Ok(app) => Preview::of(vec![sync_section(&app, req)]),
                Err(e) => unread(&object_label(kind, Some(ns.as_str()), name), &e),
            }
        }
        Plan::Delete { client, kind, ns, name, cascade } => deleted(client, kind, ns.as_deref(), name, *cascade).await,
        Plan::Evict { kind, ns, name } => {
            Preview::of(vec![evict_section(&object_label(kind, Some(ns.as_str()), name))])
        }
    }
}

/// `evict_pod` of the pod `label`: the API server decides with its budgets when it is sent.
fn evict_section(label: &str) -> Section {
    Section {
        title: format!("{label}: evicted"),
        lines: vec![
            "through the Eviction API, as a drain does".to_string(),
            "refused if a PodDisruptionBudget would be broken".to_string(),
        ],
        note: None,
        diff: Vec::new(),
    }
}

/// `Deployment shop/web`, `ClusterRole reader`.
pub(crate) fn object_label(kind: &Kind, ns: Option<&str>, name: &str) -> String {
    match ns {
        Some(ns) if kind.namespaced => format!("{} {ns}/{name}", kind.ar.kind),
        _ => format!("{} {name}", kind.ar.kind),
    }
}

/// Whether an object is a core Secret, by its kind or by what came back.
fn is_secret(kind: &Kind, v: &Value) -> bool {
    kind.is("", "Secret") || (str_at(v, "/kind") == Some("Secret") && str_at(v, "/apiVersion") == Some("v1"))
}

/// A patch of `live`: what its dry run returned (`after`), compared with `live`.
fn patched(label: &str, kind: &Kind, live: &Value, after: anyhow::Result<Value>) -> Section {
    match after {
        Ok(after) => object_section(label, Some(live), &after, is_secret(kind, live)),
        Err(e) => Section::failed(label, format!("the dry run failed: {}", k8s::short_error(&e))),
    }
}

/// Each object of a manifest as a dry run applies it, `MAX_OBJECTS` of them at most.
async fn applied(client: &Client, objects: &[Object]) -> Vec<Section> {
    let shown = &objects[..objects.len().min(MAX_OBJECTS)];
    // Futures that own what they work on: a closure over borrowed objects makes every tool's
    // future fail its `Send` check (rust-lang/rust#102211).
    let each: Vec<_> = shown.iter().map(|object| applied_one(client.clone(), object.clone())).collect();
    let mut sections: Vec<Section> = futures::stream::iter(each).buffered(AT_ONCE).collect().await;
    let more = objects.len() - shown.len();
    if more > 0 {
        sections.push(Section {
            title: format!("{} not worked out", plural::count(more, "more object")),
            ..Section::default()
        });
    }
    sections
}

/// One object of a manifest as a dry run applies it.
async fn applied_one(client: Client, (kind, ns, name, doc): Object) -> Section {
    let label = object_label(&kind, ns.as_deref(), &name);
    match dry_run_apply(&client, &kind, ns, &name, doc).await {
        Ok(a) => {
            let mut s = object_section(&label, a.live.as_ref(), &a.after, is_secret(&kind, &a.after));
            if a.conflict {
                s.note = Some(CONFLICT.into());
            }
            s
        }
        Err(e) => Section::failed(&label, format!("the dry run failed: {}", k8s::short_error(&e))),
    }
}

/// What applying a manifest's object makes of it (see [`dry_run_apply`]).
pub(crate) struct Applied {
    /// The object now; None when it doesn't exist yet.
    pub live: Option<Value>,
    /// The object as the server's dry run made it.
    pub after: Value,
    /// Fields another manager owns make the real apply fail: the dry run was forced, to show
    /// its diff anyway.
    pub conflict: bool,
}

/// A server-side dry run of applying `doc`, with the object as it is now.
pub(crate) async fn dry_run_apply(
    client: &Client,
    kind: &Kind,
    ns: Option<String>,
    name: &str,
    doc: Value,
) -> anyhow::Result<Applied> {
    let obj = ObjectRef { kind: kind.clone(), name: name.to_string(), namespace: ns.clone(), uid: String::new() };
    let live = match k8s::get_object(client.clone(), obj).await {
        Ok(v) => Some(v),
        Err(e) if k8s::is_not_found(&e) => None,
        Err(e) => return Err(e),
    };
    let mut conflict = false;
    let after = match k8s::apply_value(client.clone(), kind.clone(), ns.clone(), doc.clone(), true, false).await {
        Err(e) if k8s::api_status(&e) == Some(409) => {
            conflict = true;
            k8s::apply_value(client.clone(), kind.clone(), ns, doc, true, true).await?
        }
        other => other?,
    };
    Ok(Applied { live, after, conflict })
}

/// The object without the server's noise (`NOISE`), its keys sorted (the live object and the
/// dry run's can come in another order).
pub(crate) fn without_noise(v: &Value) -> Value {
    let mut v = v.clone();
    v.sort_all_objects();
    for path in NOISE {
        if let Some((parent, key)) = path.rsplit_once('/')
            && let Some(obj) =
                (if parent.is_empty() { Some(&mut v) } else { v.pointer_mut(parent) }).and_then(Value::as_object_mut)
        {
            obj.remove(key);
        }
    }
    v
}

/// An object as compared: without the server's noise, without the copy `kubectl apply`
/// keeps of it in an annotation (a Secret's values too), without null fields.
fn comparable(v: &Value) -> Value {
    let mut v = without_noise(v);
    if let Some(meta) = v.get_mut("metadata").and_then(Value::as_object_mut) {
        let none_left = meta.get_mut("annotations").and_then(Value::as_object_mut).is_some_and(|a| {
            a.remove(LAST_APPLIED);
            a.is_empty()
        });
        if none_left {
            meta.remove("annotations");
        }
    }
    history::drop_nulls(&mut v);
    v
}

/// What a write does to one object: `live` is the object now (None when it doesn't exist
/// yet), `after` what the server's dry run returned. A Secret's values are never shown: its
/// lines and its diff only say which changed.
pub(crate) fn object_section(label: &str, live: Option<&Value>, after: &Value, secret: bool) -> Section {
    let mut before = live.map(comparable);
    let mut after = comparable(after);
    let what = match &before {
        None => "created",
        Some(b) if *b == after => "no change",
        Some(_) => "changed",
    };
    let lines = before.as_ref().map_or_default(|b| field_lines(b, &after, secret));
    if secret {
        hide_values(before.as_mut(), &mut after);
    }
    let diff = diff_of(before.as_ref(), &after);
    Section { title: format!("{label}: {what}"), lines, note: None, diff }
}

/// The fields that differ between `before` and `after`, a line each: `spec.replicas: 3 → 0`,
/// `metadata.labels.tier: added (front)`. Lists of named items (containers, env) are
/// matched by name. A Secret's values are never written. `SHOWN_FIELDS` at most, then how
/// many more.
fn field_lines(before: &Value, after: &Value, secret: bool) -> Vec<String> {
    let mut paths = Vec::new();
    history::changed_paths_to(before, after, FIELD_DEPTH, &mut Vec::new(), &mut paths);
    let mut lines: Vec<String> =
        paths.iter().take(SHOWN_FIELDS).map(|p| field_line(before, after, p, secret)).collect();
    if paths.len() > SHOWN_FIELDS {
        lines.push(format!("… {}", plural::count(paths.len() - SHOWN_FIELDS, "more field")));
    }
    lines
}

fn field_line(before: &Value, after: &Value, path: &[String], secret: bool) -> String {
    let name = history::join_path(path);
    let at = |v: &Value| value_at(v, path).filter(|v| !v.is_null()).cloned();
    let hidden = secret && path.first().is_some_and(|s| s == "data" || s == "stringData");
    let said = |v: &Value| if hidden { None } else { short(v) };
    match (at(before), at(after)) {
        (None, Some(_)) if hidden => format!("{name}: added (value hidden)"),
        (None, Some(v)) => match said(&v) {
            Some(s) => format!("{name}: added ({s})"),
            None => format!("{name}: added"),
        },
        (Some(v), None) => match said(&v) {
            Some(s) => format!("{name}: removed (was {s})"),
            None => format!("{name}: removed"),
        },
        (Some(_), Some(_)) if hidden => format!("{name}: changed (value hidden)"),
        (Some(x), Some(y)) => match (short(&x), short(&y)) {
            (Some(x), Some(y)) if path.last().is_some_and(|s| s == "image") => {
                format!("{name}: {}", history::image_change(&x, &y))
            }
            (Some(x), Some(y)) => format!("{name}: {x} → {y}"),
            _ => format!("{name}: changed"),
        },
        (None, None) => format!("{name}: changed"),
    }
}

/// The value at a path of `history::changed_paths_to`: `[name]` picks the item of a list by
/// its name.
fn value_at<'a>(v: &'a Value, path: &[String]) -> Option<&'a Value> {
    path.iter().try_fold(v, |cur, seg| match cur {
        Value::Object(m) => m.get(seg),
        Value::Array(items) => {
            let name = seg.strip_prefix('[')?.strip_suffix(']')?;
            items.iter().find(|i| i.get("name").and_then(Value::as_str) == Some(name))
        }
        _ => None,
    })
}

/// A value as a field line writes it: a string as it is, anything else as compact JSON. None
/// when it is long, or holds several lines.
fn short(v: &Value) -> Option<String> {
    let text = match v {
        Value::String(s) if s.is_empty() => "\"\"".to_string(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    (text.chars().count() <= SHOWN_VALUE && !text.contains('\n')).then_some(text)
}

/// A Secret's values hidden on both sides; the after side says which changed or are new.
fn hide_values(mut before: Option<&mut Value>, after: &mut Value) {
    let existed = before.is_some();
    for key in ["data", "stringData"] {
        let was = before.as_deref().and_then(|b| b.get(key)).and_then(Value::as_object).cloned();
        if let Some(values) = after.get_mut(key).and_then(Value::as_object_mut) {
            for (k, v) in values {
                let mark = match was.as_ref().and_then(|w| w.get(k)) {
                    Some(old) if old == v => HIDDEN,
                    Some(_) => "(hidden, changed)",
                    None if existed => "(hidden, added)",
                    None => HIDDEN,
                };
                *v = Value::String(mark.into());
            }
        }
        if let Some(values) = before.as_deref_mut().and_then(|b| b.get_mut(key)).and_then(Value::as_object_mut) {
            for v in values.values_mut() {
                *v = Value::String(HIDDEN.into());
            }
        }
    }
}

/// The diff of an object's YAML now (`before`, None for a new object) and after, cut at
/// `OBJECT_DIFF_LINES`.
fn diff_of(before: Option<&Value>, after: &Value) -> Vec<DiffLine> {
    let before = before.map_or_default(history::diff_text);
    let mut lines = history::diff_lines(&before, &history::diff_text(after));
    if lines.len() > OBJECT_DIFF_LINES {
        let more = lines.len() - OBJECT_DIFF_LINES;
        lines.truncate(OBJECT_DIFF_LINES);
        lines.push(DiffLine::Same(format!("… {}", plural::count(more, "more line"))));
    }
    lines
}

/// `scale`: the replicas asked for now (`now`, None when not set) and after.
fn scale_section(label: &str, now: Option<i32>, replicas: i32) -> Section {
    let (title, line) = match now {
        Some(n) if n == replicas => (format!("{label}: no change"), format!("replicas: {n}")),
        Some(n) => (format!("{label}: scaled"), format!("replicas: {n} → {replicas}")),
        None => (format!("{label}: scaled"), format!("replicas: not set → {replicas}")),
    };
    let stops = replicas == 0 && now.is_none_or(|n| n > 0);
    let note = stops.then(|| "every pod of it stops".to_string());
    Section { title, lines: vec![line], note, diff: Vec::new() }
}

/// `gitops` `sync` of the Application `app`: Argo CD syncs it to its target revision, which
/// changes what is out of sync (of the resources `req` names, when it names some). What is
/// no longer in git stays unless `req` prunes. Git's manifests can't be read through the
/// Kubernetes API, so there is no diff.
fn sync_section(app: &Value, req: &SyncRequest) -> Section {
    let s = argo::Summary::of(app);
    let deployed = s.history.first().map(|d| d.revision.clone()).filter(|r| !r.is_empty());
    let mut lines = vec![match (deployed, s.revision) {
        (Some(d), Some(t)) if d == t => format!("revision: {t}, deployed already"),
        (Some(d), Some(t)) => format!("revision: {d} deployed → {t}"),
        (None, Some(t)) => format!("revision: {t}, nothing deployed yet"),
        (Some(d), None) => format!("revision: {d} deployed → its target revision"),
        (None, None) => "revision: its target revision".to_string(),
    }];
    if let Some(target) = target_revision(app) {
        lines.push(format!("target revision: {target} (read again when the sync starts)"));
    }
    lines.push(format!("auto-sync: {}", if s.automated { "on" } else { "off" }));
    if req.dry_run {
        lines.push("dry run: Argo CD applies nothing and says what it would do".into());
    }
    let mut effects = diagnosis::sync_effects(app, &[]);
    if !req.resources.is_empty() {
        effects.retain(|(r, _)| req.resources.iter().any(|p| p.same(r)));
        lines.push(format!("of {} only (hooks don't run)", plural::count(req.resources.len(), "resource")));
    }
    let pruned = match (req.prune, req.dry_run) {
        (false, _) => "no longer in git: left in place (this sync doesn't prune)",
        (true, false) => "no longer in git: deleted (prune)",
        (true, true) => "no longer in git: would be deleted (prune)",
    };
    if effects.is_empty() {
        lines.push("nothing out of sync: the sync applies the same manifests again".into());
    } else {
        lines.push(format!("out of sync: {}", plural::count(effects.len(), "resource")));
    }
    for (res, effect) in effects.iter().take(SHOWN_RESOURCES) {
        let what = match effect {
            diagnosis::SyncEffect::Create => "in git, not in the cluster yet: created",
            diagnosis::SyncEffect::Update => "differs from git: updated",
            diagnosis::SyncEffect::Prune => pruned,
            diagnosis::SyncEffect::Unknown => "out of sync with git: created or updated",
        };
        lines.push(format!("{}: {what}", res.label()));
    }
    if effects.len() > SHOWN_RESOURCES {
        lines.push(format!("… {} more", effects.len() - SHOWN_RESOURCES));
    }
    Section { title: format!("Application {}/{}: sync", s.namespace, s.name), lines, note: None, diff: Vec::new() }
}

/// The revision the Application's spec asks for (`main`, `v1.2.0`), or each of its sources'.
fn target_revision(app: &Value) -> Option<String> {
    let one = nonempty_at(app, "/spec/source/targetRevision").map(String::from);
    let many: Vec<&str> =
        arr_at(app, "/spec/sources").iter().filter_map(|s| nonempty_at(s, "/targetRevision")).collect();
    one.or_else(|| (!many.is_empty()).then(|| many.join(", ")))
}

/// `delete_resource`: the object, and what goes with it (see [`delete_section`]).
async fn deleted(client: &Client, kind: &Kind, ns: Option<&str>, name: &str, cascade: Option<Cascade>) -> Preview {
    let label = object_label(kind, ns, name);
    let obj = match super::server::get_named(client, kind, ns, name, false).await {
        Ok(v) => v,
        Err(e) if k8s::is_not_found(&e) => {
            let title = format!("{label}: not found, nothing to delete");
            return Preview::of(vec![Section { title, ..Section::default() }]);
        }
        Err(e) => return Preview::unavailable(format!("reading {label} failed: {}", k8s::short_error(&e))),
    };
    let found = match ns {
        Some(ns) if kind.namespaced => dependents(client, kind, ns, &obj).await,
        _ => Ok(Vec::new()),
    };
    let mut section = delete_section(kind, &label, &obj, found.as_deref().unwrap_or_default(), cascade);
    if let Err(e) = &found {
        section.lines.push(format!("what it owns could not be read: {}", k8s::short_error(e)));
    }
    Preview::of(vec![section])
}

/// What goes with a workload when it is deleted, from lean reads (metadata only, by its
/// selector): a Deployment's ReplicaSets and their pods, the pods of a ReplicaSet,
/// StatefulSet, DaemonSet or Job, a CronJob's Jobs. Nothing is read for other kinds.
async fn dependents(
    client: &Client,
    kind: &Kind,
    ns: &str,
    obj: &Value,
) -> anyhow::Result<Vec<(&'static str, Vec<String>)>> {
    let mut lists: Vec<(&'static str, Vec<Value>)> = Vec::new();
    if kind.is("batch", "CronJob") {
        let jobs = Api::<Job>::namespaced(client.clone(), ns).list_metadata(&ListParams::default()).await?;
        lists.push(("Job", metadata(jobs.items)));
    } else if WORKLOADS.iter().any(|(group, k)| kind.is(group, k)) {
        let k8s::Selection::Labels(selector) = k8s::pod_selection(kind, obj) else { return Ok(Vec::new()) };
        let lp = ListParams::default().labels(&selector);
        if kind.is("apps", "Deployment") {
            let sets = Api::<ReplicaSet>::namespaced(client.clone(), ns).list_metadata(&lp).await?;
            lists.push(("ReplicaSet", metadata(sets.items)));
        }
        let pods = Api::<Pod>::namespaced(client.clone(), ns).list_metadata(&lp).await?;
        lists.push(("Pod", metadata(pods.items)));
    }
    Ok(owned(str_at(obj, "/metadata/uid").unwrap_or_default(), &lists))
}

/// Each object's metadata, as JSON.
fn metadata<K>(items: Vec<PartialObjectMeta<K>>) -> Vec<Value> {
    items.into_iter().filter_map(|i| serde_json::to_value(i.metadata).ok()).collect()
}

/// The objects of `lists` (metadata) that the object `uid` owns, directly or through one
/// another (a Deployment's ReplicaSets, then their pods): each list's names, in its order.
fn owned(uid: &str, lists: &[(&'static str, Vec<Value>)]) -> Vec<(&'static str, Vec<String>)> {
    let mut owners: HashSet<String> = HashSet::from([uid.to_string()]);
    let mut out = Vec::new();
    for (kind, items) in lists {
        let mut names = Vec::new();
        for m in items {
            let mine =
                arr_at(m, "/ownerReferences").iter().any(|o| o["uid"].as_str().is_some_and(|u| owners.contains(u)));
            if mine {
                if let Some(u) = str_at(m, "/uid") {
                    owners.insert(u.to_string());
                }
                names.push(str_at(m, "/name").unwrap_or_default().to_string());
            }
        }
        out.push((*kind, names));
    }
    out
}

/// `delete_resource` of `obj`: it goes once its finalizers are done, and what it owns
/// (`owned`) goes with it, but a Job's pods: deleted through the API, a Job leaves them (the
/// API orphans them unless told otherwise). An orphan `cascade` leaves what it owns, and a
/// background or foreground one takes a Job's pods too. A namespace takes everything in it
/// and a CRD every object of its kind, whatever the cascade; a StatefulSet takes its claims
/// when its retention policy says so, unless orphaned.
fn delete_section(
    kind: &Kind,
    label: &str,
    obj: &Value,
    owned: &[(&'static str, Vec<String>)],
    cascade: Option<Cascade>,
) -> Section {
    let mut lines = Vec::new();
    let finalizers: Vec<&str> = arr_at(obj, "/metadata/finalizers").iter().filter_map(Value::as_str).collect();
    if !finalizers.is_empty() {
        lines.push(format!("it stays until its finalizers are done: {}", finalizers.join(", ")));
    }
    let orphans = match cascade {
        Some(c) => c == Cascade::Orphan,
        None => kind.is("batch", "Job"),
    };
    lines.push(if kind.is("", "Namespace") {
        "everything in the namespace goes with it".to_string()
    } else if kind.is("apiextensions.k8s.io", "CustomResourceDefinition") {
        let of = nonempty_at(obj, "/spec/names/kind").unwrap_or("its kind's");
        format!("every {of} object goes with it, in every namespace")
    } else if cascade == Some(Cascade::Orphan) {
        "what it owns stays, without an owner (cascade: orphan)".to_string()
    } else if orphans {
        "its pods stay: a Job deleted through the API leaves them".to_string()
    } else if cascade == Some(Cascade::Foreground) {
        "anything it owns goes first, then it (cascade: foreground)".to_string()
    } else {
        "anything it owns goes with it".to_string()
    });
    if kind.is("apps", "StatefulSet") {
        // Orphaned, the claims lose their owner like the pods and stay.
        let claims =
            str_at(obj, "/spec/persistentVolumeClaimRetentionPolicy/whenDeleted") == Some("Delete") && !orphans;
        lines.push(
            if claims {
                "its PersistentVolumeClaims go too (retention policy: Delete)"
            } else {
                "its PersistentVolumeClaims stay"
            }
            .to_string(),
        );
    }
    let found: Vec<String> = owned
        .iter()
        .filter(|(_, names)| !names.is_empty())
        .map(|(what, names)| format!("{} ({})", plural::count(names.len(), what), few(names)))
        .collect();
    if !found.is_empty() {
        lines.push(format!("{}: {}", if orphans { "left" } else { "goes with it" }, found.join(", ")));
    }
    Section { title: format!("{label}: deleted"), lines, note: None, diff: Vec::new() }
}

/// The first names, then how many more: `web-1, web-2, web-3, +2`.
fn few(names: &[String]) -> String {
    let mut out: Vec<String> = names.iter().take(SHOWN_NAMES).cloned().collect();
    if names.len() > SHOWN_NAMES {
        out.push(format!("+{}", names.len() - SHOWN_NAMES));
    }
    out.join(", ")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_change_says_each_field_with_its_value_now_and_after() {
        let before = json!({
            "metadata": {"name": "web", "labels": {"app": "web"}},
            "spec": {"replicas": 3, "template": {"spec": {"containers": [{"name": "web", "image": "registry/web:1.4", "args": ["serve"]}]}}},
        });
        let mut after = before.clone();
        after["spec"]["replicas"] = json!(0);
        after["spec"]["template"]["spec"]["containers"][0]["image"] = json!("registry/web:1.5");
        after["metadata"]["labels"]["tier"] = json!("front");
        assert_eq!(
            field_lines(&before, &after, false),
            [
                "metadata.labels.tier: added (front)",
                "spec.replicas: 3 → 0",
                "spec.template.spec.containers[web].image: web:1.4 → 1.5",
            ]
        );
        // An item of a named list removed, a value too long to write, a list changed.
        let before = json!({"spec": {"containers": [{"image": "a", "name": "web"}, {"image": "b", "name": "proxy"}]}, "data": {"script": "x".repeat(200)}, "args": ["a"]});
        let after = json!({"spec": {"containers": [{"image": "a", "name": "web"}]}, "data": {"script": "y".repeat(200)}, "args": ["a", "b"]});
        assert_eq!(
            field_lines(&before, &after, false),
            [
                "args: [\"a\"] → [\"a\",\"b\"]",
                "data.script: changed",
                "spec.containers[proxy]: removed (was {\"image\":\"b\",\"name\":\"proxy\"})",
            ]
        );
        // Past `SHOWN_FIELDS`, the rest is counted.
        let many = |v: i64| Value::Object((0..20).map(|i| (format!("k{i:02}"), json!(v))).collect());
        let lines = field_lines(&json!({"data": many(1)}), &json!({"data": many(2)}), false);
        assert_eq!(lines.len(), SHOWN_FIELDS + 1);
        assert_eq!(lines.last().map(String::as_str), Some("… 8 more fields"));
    }

    #[test]
    fn a_secret_s_values_are_never_shown_only_which_changed() {
        let live = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "db", "namespace": "app", "resourceVersion": "4",
            "annotations": {LAST_APPLIED: "{\"data\":{\"alpha\":\"value-old\"}}"}}, "data": {"alpha": "value-old", "beta": "value-same"}});
        let after = json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "db", "namespace": "app", "resourceVersion": "5",
            "annotations": {LAST_APPLIED: "{\"data\":{\"alpha\":\"value-new\"}}"}}, "data": {"alpha": "value-new", "beta": "value-same", "gamma": "value-added"}});
        let s = object_section("Secret app/db", Some(&live), &after, true);
        assert_eq!(s.title, "Secret app/db: changed");
        assert_eq!(s.lines, ["data.alpha: changed (value hidden)", "data.gamma: added (value hidden)"]);
        let text = Preview::of(vec![s.clone()]).text();
        for value in ["value-old", "value-new", "value-same", "value-added"] {
            assert!(!text.contains(value), "{value} in {text}");
        }
        let added = |what: &str| s.diff.iter().any(|l| matches!(l, DiffLine::Added(t) if t.contains(what)));
        assert!(added("(hidden, changed)") && added("(hidden, added)"), "{text}");
        // A new Secret: every value hidden, none said changed.
        let created = object_section("Secret app/db", None, &after, true);
        let text = Preview::of(vec![created]).text();
        assert!(!text.contains("value-added") && !text.contains("changed") && text.contains(HIDDEN), "{text}");
    }

    #[test]
    fn a_new_object_is_created_and_the_same_one_is_no_change() {
        let cm = json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "cfg", "namespace": "app", "uid": "u1",
            "creationTimestamp": "2026-11-22T10:00:00Z", "managedFields": [{"manager": "x"}]}, "data": {"mode": "debug"}});
        let created = object_section("ConfigMap app/cfg", None, &cm, false);
        assert_eq!(created.title, "ConfigMap app/cfg: created");
        assert!(created.lines.is_empty());
        let added = |what: &str| created.diff.iter().any(|l| matches!(l, DiffLine::Added(t) if t.contains(what)));
        assert!(added("mode: debug") && !added("u1") && !added("managedFields"), "{:?}", created.diff);
        let mut again = cm.clone();
        again["metadata"]["resourceVersion"] = json!("8");
        again["status"] = json!({"phase": "x"});
        let same = object_section("ConfigMap app/cfg", Some(&cm), &again, false);
        assert_eq!(same.title, "ConfigMap app/cfg: no change");
        assert!(same.lines.is_empty() && same.diff.is_empty());
    }

    #[test]
    fn a_scale_says_the_replicas_now_and_after() {
        let s = scale_section("Deployment shop/web", Some(3), 0);
        assert_eq!(s.title, "Deployment shop/web: scaled");
        assert_eq!(s.lines, ["replicas: 3 → 0"]);
        assert!(s.note.is_some(), "scaling to 0 stops every pod");
        let s = scale_section("Deployment shop/web", Some(2), 2);
        assert_eq!((s.title.as_str(), s.note), ("Deployment shop/web: no change", None));
        assert_eq!(scale_section("Deployment shop/web", None, 4).lines, ["replicas: not set → 4"]);
    }

    #[test]
    fn a_sync_says_the_revision_it_deploys_and_what_it_leaves() {
        let sha = |c: char| c.to_string().repeat(40);
        let app = json!({
            "metadata": {"name": "shop-workload", "namespace": "argocd"},
            "spec": {"source": {"targetRevision": "main"}, "syncPolicy": {}},
            "status": {
                "sync": {"status": "OutOfSync", "revision": sha('b')},
                "history": [{"id": 1, "revision": sha('a'), "deployedAt": "2026-11-20T10:00:00Z"}],
                "resources": [
                    {"group": "apps", "version": "v1", "kind": "Deployment", "namespace": "shop", "name": "web", "status": "OutOfSync", "health": {"status": "Healthy"}},
                    {"version": "v1", "kind": "ConfigMap", "namespace": "shop", "name": "new", "status": "OutOfSync", "health": {"status": "Missing"}},
                    {"version": "v1", "kind": "ConfigMap", "namespace": "shop", "name": "old", "status": "OutOfSync", "requiresPruning": true},
                    {"version": "v1", "kind": "Service", "namespace": "shop", "name": "web", "status": "Synced", "health": {"status": "Healthy"}},
                ],
            },
        });
        // With prune, of some resources only: what it deletes, and nothing else listed.
        let old = diagnosis::Res {
            kind: "ConfigMap".into(),
            namespace: "shop".into(),
            name: "old".into(),
            ..Default::default()
        };
        let new = diagnosis::Res { name: "new".into(), ..old.clone() };
        let req = SyncRequest { resources: vec![old, new], prune: true, ..Default::default() };
        let lines = sync_section(&app, &req).lines;
        assert_eq!(
            &lines[3..],
            [
                "of 2 resources only (hooks don't run)",
                "out of sync: 2 resources",
                "ConfigMap shop/new: in git, not in the cluster yet: created",
                "ConfigMap shop/old: no longer in git: deleted (prune)",
            ]
        );
        let dry = sync_section(&app, &SyncRequest { dry_run: true, ..req }).lines;
        assert!(dry.iter().any(|l| l.starts_with("dry run:")), "{dry:?}");
        assert!(dry.iter().any(|l| l.ends_with("would be deleted (prune)")), "{dry:?}");
        let s = sync_section(&app, &SyncRequest::default());
        assert_eq!(s.title, "Application argocd/shop-workload: sync");
        assert_eq!(
            s.lines,
            [
                "revision: aaaaaaaa deployed → bbbbbbbb",
                "target revision: main (read again when the sync starts)",
                "auto-sync: off",
                "out of sync: 3 resources",
                "Deployment shop/web: differs from git: updated",
                "ConfigMap shop/new: in git, not in the cluster yet: created",
                "ConfigMap shop/old: no longer in git: left in place (this sync doesn't prune)",
            ]
        );
        // Deployed already, auto-sync on, nothing out of sync.
        let synced = json!({
            "metadata": {"name": "web", "namespace": "argocd"},
            "spec": {"syncPolicy": {"automated": {}}},
            "status": {"sync": {"status": "Synced", "revision": "1.2.0"}, "history": [{"id": 3, "revision": "1.2.0"}]},
        });
        assert_eq!(
            sync_section(&synced, &SyncRequest::default()).lines,
            [
                "revision: 1.2.0, deployed already",
                "auto-sync: on",
                "nothing out of sync: the sync applies the same manifests again"
            ]
        );
    }

    #[test]
    fn a_delete_says_what_goes_with_the_object() {
        let dep = json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "namespace": "shop", "uid": "d1",
            "finalizers": ["example.com/keep"]}, "spec": {"selector": {"matchLabels": {"app": "web"}}}});
        let sets = vec![
            json!({"name": "web-1", "uid": "rs1", "ownerReferences": [{"uid": "d1"}]}),
            json!({"name": "other-1", "uid": "rs2", "ownerReferences": [{"uid": "x"}]}),
        ];
        let pods = vec![
            json!({"name": "web-1-a", "uid": "p1", "ownerReferences": [{"uid": "rs1"}]}),
            json!({"name": "web-1-b", "uid": "p2", "ownerReferences": [{"uid": "rs1"}]}),
            json!({"name": "other-1-a", "uid": "p3", "ownerReferences": [{"uid": "rs2"}]}),
            json!({"name": "lonely", "uid": "p4"}),
        ];
        let found = owned("d1", &[("ReplicaSet", sets), ("Pod", pods)]);
        assert_eq!(
            found,
            [("ReplicaSet", vec!["web-1".to_string()]), ("Pod", vec!["web-1-a".into(), "web-1-b".into()])]
        );
        let s = delete_section(&Kind::fake("apps", "Deployment"), "Deployment shop/web", &dep, &found, None);
        assert_eq!(s.title, "Deployment shop/web: deleted");
        assert_eq!(
            s.lines,
            [
                "it stays until its finalizers are done: example.com/keep",
                "anything it owns goes with it",
                "goes with it: 1 ReplicaSet (web-1), 2 Pods (web-1-a, web-1-b)",
            ]
        );
        // The API leaves a Job's pods.
        let job = json!({"metadata": {"name": "backup", "uid": "j1"}});
        let pods = vec![json!({"name": "backup-x", "uid": "p", "ownerReferences": [{"uid": "j1"}]})];
        let job_pods = owned("j1", &[("Pod", pods)]);
        let s = delete_section(&Kind::fake("batch", "Job"), "Job shop/backup", &job, &job_pods, None);
        assert_eq!(s.lines, ["its pods stay: a Job deleted through the API leaves them", "left: 1 Pod (backup-x)"]);
        // Asked for, a cascade decides: a Job's pods go, a Deployment's stay.
        let s =
            delete_section(&Kind::fake("batch", "Job"), "Job shop/backup", &job, &job_pods, Some(Cascade::Background));
        assert_eq!(s.lines, ["anything it owns goes with it", "goes with it: 1 Pod (backup-x)"]);
        let s = delete_section(
            &Kind::fake("apps", "Deployment"),
            "Deployment shop/web",
            &dep,
            &found,
            Some(Cascade::Orphan),
        );
        assert_eq!(
            s.lines,
            [
                "it stays until its finalizers are done: example.com/keep",
                "what it owns stays, without an owner (cascade: orphan)",
                "left: 1 ReplicaSet (web-1), 2 Pods (web-1-a, web-1-b)",
            ]
        );
        // A namespace, a CRD, a StatefulSet that deletes its claims.
        let mut ns = Kind::fake("", "Namespace");
        ns.namespaced = false;
        let s = delete_section(&ns, "Namespace shop", &json!({"metadata": {"name": "shop"}}), &[], None);
        assert_eq!(s.lines, ["everything in the namespace goes with it"]);
        // Orphaning doesn't keep what a namespace holds.
        let s =
            delete_section(&ns, "Namespace shop", &json!({"metadata": {"name": "shop"}}), &[], Some(Cascade::Orphan));
        assert_eq!(s.lines, ["everything in the namespace goes with it"]);
        let mut crd = Kind::fake("apiextensions.k8s.io", "CustomResourceDefinition");
        crd.namespaced = false;
        let widgets = json!({"spec": {"names": {"kind": "Widget"}}});
        let s = delete_section(&crd, "CustomResourceDefinition widgets.example.com", &widgets, &[], None);
        assert_eq!(s.lines, ["every Widget object goes with it, in every namespace"]);
        let sts = json!({"spec": {"persistentVolumeClaimRetentionPolicy": {"whenDeleted": "Delete"}}});
        let s = delete_section(&Kind::fake("apps", "StatefulSet"), "StatefulSet shop/db", &sts, &[], None);
        assert_eq!(
            s.lines,
            ["anything it owns goes with it", "its PersistentVolumeClaims go too (retention policy: Delete)"]
        );
        let s =
            delete_section(&Kind::fake("apps", "StatefulSet"), "StatefulSet shop/db", &sts, &[], Some(Cascade::Orphan));
        assert_eq!(
            s.lines,
            ["what it owns stays, without an owner (cascade: orphan)", "its PersistentVolumeClaims stay"]
        );
        assert_eq!(few(&["a", "b", "c", "d", "e"].map(String::from)), "a, b, c, +2");
    }

    #[test]
    fn an_eviction_is_not_shown_as_a_delete() {
        let s = evict_section("Pod shop/web-1");
        assert_eq!(s.title, "Pod shop/web-1: evicted");
        assert_eq!(
            s.lines,
            ["through the Eviction API, as a drain does", "refused if a PodDisruptionBudget would be broken"]
        );
    }

    #[test]
    fn the_log_keeps_the_preview_as_text_and_cut() {
        let p = Preview::of(vec![Section {
            title: "ConfigMap app/cfg: changed".into(),
            lines: vec!["data.a: 1 → 2".into()],
            note: Some("careful".into()),
            diff: vec![
                DiffLine::Gap,
                DiffLine::Same("data:".into()),
                DiffLine::Removed("  a: '1'".into()),
                DiffLine::Added("  a: '2'".into()),
            ],
        }]);
        assert_eq!(
            p.text(),
            "ConfigMap app/cfg: changed\n  data.a: 1 → 2\n  ! careful\n  ⋯\n    data:\n  -   a: '1'\n  +   a: '2'\n"
        );
        assert_eq!(
            Preview::unavailable("no answer within 5 s").text(),
            "Preview not available: no answer within 5 s\n"
        );
        // Cut on a character boundary, past 64 KB.
        let big =
            Preview::of(vec![Section { title: "x".into(), lines: vec!["é".repeat(40_000)], ..Section::default() }]);
        let text = big.text();
        assert!(text.len() <= MAX_LOG_BYTES + 32 && text.ends_with("(cut at 64 KB)"), "{}", text.len());
        // The diffs shown stop at `DIFF_LINES` in all: a later object keeps its fields.
        let long =
            |n: usize| Section { title: "o".into(), diff: vec![DiffLine::Added("x".into()); n], ..Section::default() };
        let p = Preview::of(vec![long(300), long(300)]);
        assert_eq!(p.sections[0].diff.len(), 300);
        assert!(p.sections[1].diff.is_empty() && p.sections[1].lines.len() == 1);
    }

    #[test]
    fn values_are_found_by_path_and_named_item() {
        fn path(p: &[&str]) -> Vec<String> {
            p.iter().copied().map(String::from).collect()
        }
        let v = json!({"spec": {"containers": [{"name": "web", "image": "a"}], "weird": {"[x]": 1}}});
        assert_eq!(value_at(&v, &path(&["spec", "containers", "[web]", "image"])), Some(&json!("a")));
        assert_eq!(value_at(&v, &path(&["spec", "weird", "[x]"])), Some(&json!(1)));
        assert_eq!(value_at(&v, &path(&["spec", "containers", "[api]"])), None);
        assert_eq!(short(&json!("")).as_deref(), Some("\"\""));
        assert_eq!(short(&json!("two\nlines")), None);
    }
}
