//! One observation of an object (a list's item or a watch event) against the last one:
//! whether someone changed it, and the record that says what.
//!
//! What is kept per object is small: its resourceVersion, generation, a hash of what is
//! compared, and when each field manager last wrote it. The version compared (labels and
//! annotations from a list's metadata, the whole object without its status from a watch
//! of whole objects) is kept as text only within a budget per kind, for the next diff.

use std::collections::{BTreeMap, HashMap};
use std::hash::{BuildHasher as _, Hash as _, Hasher as _};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;

use super::{Record, What};
use crate::history;

/// The kind recorded, as records name it.
pub(super) struct KindInfo {
    /// kubectl's name: `configmaps`, `deployments.apps`.
    pub resource: String,
    /// `ConfigMap`.
    pub kind: String,
    /// A Secret: its values are never kept, only whether each changed.
    pub secret: bool,
}

/// Annotations that change without anyone changing the object (leader election, an HPA's
/// v1 status), or that hold a copy of it (and of a Secret's values).
const NOISE_ANNOTATIONS: &[&str] = &[
    "kubectl.kubernetes.io/last-applied-configuration",
    "control-plane.alpha.kubernetes.io/leader",
    "autoscaling.alpha.kubernetes.io/conditions",
    "autoscaling.alpha.kubernetes.io/current-metrics",
    "autoscaling.alpha.kubernetes.io/behavior",
    "argocd.argoproj.io/refresh",
    "endpoints.kubernetes.io/last-change-trigger-time",
];

fn noise(annotation: &str) -> bool {
    NOISE_ANNOTATIONS.contains(&annotation)
}

/// Text kept per kind for the next diffs, in bytes: labels and annotations, or whole objects
/// (a watch of whole objects). Past it, changes are still recorded, without their diff.
const KEEP_META: usize = 8 << 20;
const KEEP_FULL: usize = 64 << 20;

/// Either side of a record's diff, at most (a huge ConfigMap's is left out).
const MAX_SIDE: usize = 64 << 10;

/// Records per object and hour: an object written all the time (a status kept in a
/// ConfigMap) would push every other record out. The ones past it are counted.
const BURST: u8 = 10;
const BURST_WINDOW: Duration = Duration::from_secs(3600);

#[derive(Deserialize)]
pub(super) struct Head<'a> {
    #[serde(borrow, default)]
    pub metadata: Meta<'a>,
}

/// The metadata compared. `fieldsV1` stays raw: it is read only for the managers that wrote.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub(super) struct Meta<'a> {
    pub uid: Option<String>,
    pub name: Option<String>,
    pub namespace: Option<String>,
    pub resource_version: Option<String>,
    generation: Option<i64>,
    creation_timestamp: Option<String>,
    deletion_timestamp: Option<String>,
    labels: Option<BTreeMap<String, String>>,
    annotations: Option<BTreeMap<String, String>>,
    #[serde(borrow)]
    managed_fields: Option<Vec<Writer<'a>>>,
    owner_references: Option<Vec<Owner>>,
}

#[derive(Deserialize)]
struct Writer<'a> {
    #[serde(default)]
    manager: Option<String>,
    #[serde(default)]
    operation: Option<String>,
    #[serde(default)]
    subresource: Option<String>,
    #[serde(default)]
    time: Option<String>,
    #[serde(borrow, default, rename = "fieldsV1")]
    fields: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Owner {
    #[serde(default)]
    controller: Option<bool>,
}

/// A resourceVersion as a number, to tell an older observation from a newer one (another
/// list or watch of the same kind may be behind). Opaque in theory; when it is not a
/// number, observations are taken in the order they come.
pub(super) fn rv_number(rv: Option<&str>) -> Option<u64> {
    rv?.parse().ok()
}

/// A manager's write: who, through what, and when (unix seconds).
pub(super) struct Stamp<'a> {
    key: u64,
    time: i64,
    manager: String,
    subresource: String,
    fields: Option<&'a RawValue>,
}

impl Stamp<'_> {
    fn label(&self) -> String {
        if self.subresource.is_empty() {
            self.manager.clone()
        } else {
            format!("{} (through {})", self.manager, self.subresource)
        }
    }

    /// The fields this manager owns, outside the status and noisy annotations.
    fn paths(&self) -> Vec<Vec<String>> {
        let Some(v) = self.fields.and_then(|f| serde_json::from_str::<Value>(f.get()).ok()) else {
            return Vec::new();
        };
        let mut paths = Vec::new();
        history::field_paths(&v, &mut Vec::new(), &mut paths);
        paths.retain(|p| {
            let status = p.first().is_some_and(|s| s == "status");
            let noisy = p.len() >= 3 && p[0] == "metadata" && p[1] == "annotations" && noise(&p[2]);
            !status && !noisy
        });
        paths
    }
}

/// One observation, ready to compare.
pub(super) struct Observed<'a> {
    pub uid: String,
    ns: Option<String>,
    name: String,
    rv: Option<u64>,
    generation: Option<i64>,
    created: Option<jiff::Timestamp>,
    deleting: Option<jiff::Timestamp>,
    /// Made by a controller (a Job of a CronJob…): its creation and deletion are not news.
    owned: bool,
    /// What is compared, as JSON text, and its hash.
    version: String,
    hash: u64,
    /// The writes outside the status subresource.
    writers: Vec<Stamp<'a>>,
}

fn hash_of(s: &str) -> u64 {
    let mut h = std::hash::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// A Secret's value in memory: a hash keyed for this run only, never written anywhere.
fn hidden(value: &Value) -> Value {
    static KEY: LazyLock<std::hash::RandomState> = LazyLock::new(std::hash::RandomState::new);
    Value::String(format!("#{:016x}", KEY.hash_one(value.to_string())))
}

impl<'a> Observed<'a> {
    /// From an object's metadata, and the whole object when the watch reads whole objects.
    pub fn new(head: Head<'a>, whole: Option<Value>, info: &KindInfo) -> Option<Self> {
        let m = head.metadata;
        let ts = |t: &Option<String>| t.as_deref().and_then(|t| t.parse::<jiff::Timestamp>().ok());
        let version = match whole {
            Some(obj) => comparable(obj, info),
            None => {
                let mut meta = serde_json::Map::new();
                if let Some(l) = m.labels.as_ref().filter(|l| !l.is_empty()) {
                    meta.insert("labels".into(), serde_json::to_value(l).unwrap_or_default());
                }
                let annotations: BTreeMap<&String, &String> =
                    m.annotations.iter().flatten().filter(|(k, _)| !noise(k)).collect();
                if !annotations.is_empty() {
                    meta.insert("annotations".into(), serde_json::to_value(annotations).unwrap_or_default());
                }
                serde_json::json!({ "metadata": meta })
            }
        };
        let version = serde_json::to_string(&version).ok()?;
        let writers = m
            .managed_fields
            .into_iter()
            .flatten()
            .filter(|w| w.subresource.as_deref() != Some("status"))
            .map(|w| {
                let manager = w.manager.unwrap_or_default();
                let subresource = w.subresource.unwrap_or_default();
                let key = hash_of(&format!("{manager}\n{}\n{subresource}", w.operation.unwrap_or_default()));
                let time = ts(&w.time).map_or(0, jiff::Timestamp::as_second);
                Stamp { key, time, manager, subresource, fields: w.fields }
            })
            .collect();
        Some(Self {
            uid: m.uid?,
            ns: m.namespace,
            name: m.name.unwrap_or_default(),
            rv: rv_number(m.resource_version.as_deref()),
            generation: m.generation,
            created: ts(&m.creation_timestamp),
            deleting: ts(&m.deletion_timestamp),
            owned: m.owner_references.iter().flatten().any(|o| o.controller == Some(true)),
            hash: hash_of(&version),
            version,
            writers,
        })
    }
}

/// A whole object as compared: no status, no metadata that changes on every write, no
/// noisy annotations, a Secret's values hidden.
fn comparable(mut obj: Value, info: &KindInfo) -> Value {
    if let Some(o) = obj.as_object_mut() {
        o.remove("status");
        if info.secret {
            for key in ["data", "stringData"] {
                if let Some(values) = o.get_mut(key).and_then(Value::as_object_mut) {
                    values.values_mut().for_each(|v| *v = hidden(v));
                }
            }
        }
    }
    if let Some(m) = obj.get_mut("metadata").and_then(Value::as_object_mut) {
        for key in ["resourceVersion", "managedFields", "generation", "uid", "selfLink", "creationTimestamp"] {
            m.remove(key);
        }
        let none_left = m.get_mut("annotations").and_then(Value::as_object_mut).is_some_and(|a| {
            a.retain(|k, _| !noise(k));
            a.is_empty()
        });
        if none_left {
            m.remove("annotations");
        }
    }
    history::drop_nulls(&mut obj);
    obj
}

/// What is kept of an object between observations.
pub(super) struct Seen {
    ns: Option<Box<str>>,
    name: Box<str>,
    rv: Option<u64>,
    generation: Option<i64>,
    hash: u64,
    /// `hash` is of the whole object (a watch of whole objects), not of its metadata.
    full: bool,
    /// When each manager (by key) last wrote it.
    writers: Box<[(u64, i64)]>,
    owned: bool,
    /// The version compared, for the next diff (within the kind's budget).
    kept: Option<Box<str>>,
    /// When it was last observed.
    pub at: Instant,
    /// Records in the current hour, from when it started, and changes not recorded since.
    burst: (Instant, u8),
    missed: u32,
}

impl Seen {
    pub fn rv(&self) -> Option<u64> {
        self.rv
    }

    pub fn in_scope(&self, scope: Option<&str>) -> bool {
        scope.is_none_or(|s| self.ns.as_deref() == Some(s))
    }
}

/// What is known of one kind on one cluster, shared by every list and watch of it.
pub(super) struct KindState {
    /// When it was first observed (unix seconds): objects created before were there already.
    since: i64,
    pub objects: HashMap<String, Seen>,
    kept_bytes: usize,
    /// Watches of whole objects running for it: they record it, lists of metadata don't.
    pub full_taps: usize,
}

impl KindState {
    pub fn new(now: jiff::Timestamp) -> Self {
        Self { since: now.as_second(), objects: HashMap::new(), kept_bytes: 0, full_taps: 0 }
    }

    /// Keep `version` for the next diff if the budget allows; the text kept before is let go.
    fn keep(&mut self, old: Option<Box<str>>, version: &str, full: bool) -> Option<Box<str>> {
        if let Some(old) = old {
            self.kept_bytes -= old.len();
        }
        let budget = if full { KEEP_FULL } else { KEEP_META };
        (self.kept_bytes + version.len() <= budget).then(|| {
            self.kept_bytes += version.len();
            version.into()
        })
    }

    /// Forget an object (deleted), returning what was kept of it.
    pub fn remove(&mut self, uid: &str) -> Option<Seen> {
        let seen = self.objects.remove(uid)?;
        if let Some(k) = &seen.kept {
            self.kept_bytes -= k.len();
        }
        Some(seen)
    }
}

/// How the object was observed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum How {
    /// In a list.
    Listed,
    Added,
    Modified,
    Deleted,
}

/// Compare an observation with what was kept, keep it, and say what changed, if anything.
pub(super) fn observe(
    state: &mut KindState,
    info: &KindInfo,
    full: bool,
    obs: Observed<'_>,
    how: How,
    now: jiff::Timestamp,
    at: Instant,
) -> Option<Record> {
    if how == How::Deleted {
        let seen = state.remove(&obs.uid)?;
        return deleted(info, obs.uid, seen, obs.deleting.unwrap_or(now), "deleted");
    }
    let writers: Box<[(u64, i64)]> = obs.writers.iter().map(|w| (w.key, w.time)).collect();
    if !state.objects.contains_key(&obs.uid) {
        let kept = state.keep(None, &obs.version, full);
        let new = match how {
            How::Added => true,
            How::Listed => obs.created.is_some_and(|c| c.as_second() > state.since),
            How::Modified | How::Deleted => false,
        };
        let record = (new && !obs.owned).then(|| {
            let who = by(obs.writers.iter());
            Record {
                time: obs.created.unwrap_or(now),
                resource: info.resource.clone(),
                kind: info.kind.clone(),
                namespace: obs.ns.clone(),
                name: obs.name.clone(),
                uid: obs.uid.clone(),
                what: What::Created,
                summary: format!("created{who}"),
                before: None,
                after: side(&obs.version, info, None),
            }
        });
        let burst = (at, u8::from(record.is_some()));
        state.objects.insert(
            obs.uid,
            Seen {
                ns: obs.ns.map(Into::into),
                name: obs.name.into(),
                rv: obs.rv,
                generation: obs.generation,
                hash: obs.hash,
                full,
                writers,
                owned: obs.owned,
                kept,
                at,
                burst,
                missed: 0,
            },
        );
        return record;
    }
    let seen = &state.objects[&obs.uid];
    // The same version, or an older one than already seen (another list behind a watch).
    if let (Some(old), Some(new)) = (seen.rv, obs.rv)
        && new <= old
    {
        if let Some(seen) = state.objects.get_mut(&obs.uid) {
            seen.at = at;
        }
        return None;
    }
    // Last compared the other way (a watch of whole objects started or stopped): this
    // observation is the new baseline.
    let same_way = seen.full == full;
    let generation_changed = matches!((seen.generation, obs.generation), (Some(a), Some(b)) if a != b);
    let hash_changed = seen.hash != obs.hash;
    let new_writers: Vec<&Stamp<'_>> =
        obs.writers.iter().filter(|w| !seen.writers.contains(&(w.key, w.time))).collect();
    let written: Vec<(&Stamp<'_>, Vec<Vec<String>>)> =
        new_writers.iter().map(|w| (*w, w.paths())).filter(|(_, p)| !p.is_empty()).collect();
    let changed = if !same_way {
        false
    } else if full {
        hash_changed
    } else {
        // A write to the status alone (an operator whose CRD has no status subresource bumps
        // the generation with it) is not a change; a spec write in the same second as the
        // writer's last one shows only in the generation.
        hash_changed || !written.is_empty() || (generation_changed && new_writers.is_empty())
    };
    let before = seen.kept.as_deref().and_then(|k| serde_json::from_str::<Value>(k).ok());
    let mut record = None;
    if changed {
        let after: Value = serde_json::from_str(&obs.version).unwrap_or_default();
        let mut parts = Vec::new();
        if full {
            match &before {
                Some(b) => {
                    let mut paths = Vec::new();
                    history::changed_paths(b, &after, &mut Vec::new(), &mut paths);
                    parts.push(format!("changed {}", history::fields_summary(&paths)));
                }
                None => parts.push("changed".to_string()),
            }
        } else {
            if generation_changed {
                parts.push(format!(
                    "spec changed (generation {} → {})",
                    seen.generation.unwrap_or_default(),
                    obs.generation.unwrap_or_default()
                ));
            }
            if hash_changed {
                parts.push(meta_summary(before.as_ref(), &after));
            }
            if parts.is_empty() {
                let paths: Vec<Vec<String>> = written.iter().flat_map(|(_, p)| p.iter().cloned()).collect();
                parts.push(format!("changed {}", history::fields_summary(&paths)));
            }
        }
        let who =
            if written.is_empty() { by(new_writers.iter().copied()) } else { by(written.iter().map(|(w, _)| *w)) };
        let diff = full || hash_changed;
        record = Some(Record {
            time: new_writers
                .iter()
                .map(|w| w.time)
                .filter(|&t| t > 0)
                .max()
                .and_then(|t| jiff::Timestamp::from_second(t).ok())
                .unwrap_or(now),
            resource: info.resource.clone(),
            kind: info.kind.clone(),
            namespace: obs.ns.clone(),
            name: obs.name.clone(),
            uid: obs.uid.clone(),
            what: What::Updated,
            summary: format!("{}{who}", parts.join("; ")),
            before: seen.kept.as_deref().filter(|_| diff).and_then(|k| side(k, info, None)),
            after: seen.kept.as_deref().filter(|_| diff).and_then(|k| side(&obs.version, info, Some(k))),
        });
    }
    let old_kept = state.objects.get_mut(&obs.uid).and_then(|s| s.kept.take());
    let kept = state.keep(old_kept, &obs.version, full);
    let seen = state.objects.get_mut(&obs.uid)?;
    seen.rv = obs.rv;
    seen.generation = obs.generation;
    seen.hash = obs.hash;
    seen.full = full;
    seen.writers = writers;
    seen.kept = kept;
    seen.at = at;
    let mut record = record?;
    // At most `BURST` records an hour per object; the next one says how many were skipped.
    if at.duration_since(seen.burst.0) > BURST_WINDOW {
        seen.burst = (at, 0);
    }
    if seen.burst.1 >= BURST {
        seen.missed += 1;
        return None;
    }
    seen.burst.1 += 1;
    if seen.missed > 0 {
        record.summary = format!("{} ({} changes before it not recorded: too many)", record.summary, seen.missed);
        seen.missed = 0;
    }
    Some(record)
}

/// The record of a deleted object, from what was kept of it; none for one a controller
/// made (a Job of a CronJob): its deletion is not news.
pub(super) fn deleted(
    info: &KindInfo,
    uid: String,
    seen: Seen,
    time: jiff::Timestamp,
    summary: &str,
) -> Option<Record> {
    (!seen.owned).then(|| Record {
        time,
        resource: info.resource.clone(),
        kind: info.kind.clone(),
        namespace: seen.ns.map(Into::into),
        name: seen.name.into(),
        uid,
        what: What::Deleted,
        summary: summary.to_string(),
        before: seen.kept.as_deref().and_then(|k| side(k, info, None)),
        after: None,
    })
}

/// ` by kubectl-edit, helm`: who wrote, each once.
fn by<'a, 'b: 'a>(writers: impl Iterator<Item = &'a Stamp<'b>>) -> String {
    let mut names: Vec<String> = Vec::new();
    for w in writers {
        let label = w.label();
        if !label.is_empty() && !names.contains(&label) {
            names.push(label);
        }
    }
    if names.is_empty() { String::new() } else { format!(" by {}", names.join(", ")) }
}

/// Which labels and annotations changed: `labels changed: app, tier; annotations changed: x`.
fn meta_summary(before: Option<&Value>, after: &Value) -> String {
    let Some(before) = before else { return "labels or annotations changed".into() };
    let mut parts = Vec::new();
    for field in ["labels", "annotations"] {
        let get = |v: &Value| v.pointer(&format!("/metadata/{field}")).and_then(Value::as_object).cloned();
        let (a, b) = (get(before).unwrap_or_default(), get(after).unwrap_or_default());
        let mut keys: Vec<&String> = a.keys().chain(b.keys()).filter(|k| a.get(*k) != b.get(*k)).collect();
        keys.sort();
        keys.dedup();
        if !keys.is_empty() {
            let shown: Vec<&str> = keys.iter().take(5).map(|k| k.as_str()).collect();
            let more = keys.len().saturating_sub(5);
            let more = if more > 0 { format!(" (+{more} more)") } else { String::new() };
            parts.push(format!("{field} changed: {}{more}", shown.join(", ")));
        }
    }
    if parts.is_empty() { "metadata changed".into() } else { parts.join("; ") }
}

/// One side of a record's diff from a version kept as text: a Secret's values hidden (the
/// after side says which changed, against `before`), None past `MAX_SIDE`.
fn side(version: &str, info: &KindInfo, before: Option<&str>) -> Option<Box<RawValue>> {
    if version.len() > MAX_SIDE {
        return None;
    }
    if !info.secret {
        return RawValue::from_string(version.to_string()).ok();
    }
    let mut v: Value = serde_json::from_str(version).ok()?;
    let old: Option<Value> = before.and_then(|b| serde_json::from_str(b).ok());
    for key in ["data", "stringData"] {
        let was = old.as_ref().and_then(|o| o.get(key)).and_then(Value::as_object).cloned();
        if let Some(values) = v.get_mut(key).and_then(Value::as_object_mut) {
            for (k, value) in values.iter_mut() {
                let changed = was.as_ref().and_then(|w| w.get(k)).is_some_and(|w| w != value);
                *value = Value::String(if changed { "(hidden, changed)" } else { "(hidden)" }.into());
            }
        }
    }
    serde_json::value::to_raw_value(&v).ok()
}

#[cfg(test)]
mod tests;
