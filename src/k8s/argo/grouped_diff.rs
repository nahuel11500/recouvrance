//! What syncing several Applications now would change, read together, the same change in
//! several of them shown once with the Applications it is in.
//!
//! - **Each Application's diff** is Argo CD's own, from its managed resources (`GET
//!   /api/v1/applications/{name}/managed-resources`): the live state (`normalizedLiveState`,
//!   else `liveState`) against what a sync would leave (`predictedLiveState`, else
//!   `targetState`), read with the smart diff (`crate::smartdiff`): facts, noise folded and
//!   counted, a Secret's values masked. Hooks are left out: they run during a sync rather than
//!   being kept in sync.
//! - **Across Applications** ([`regroup`]): an image, digest, replicas, env var or pod template
//!   change is the same change wherever it is (one image bump in 12 environments is one line);
//!   a change of a resource's own (a ConfigMap's key, a field, a resource added or pruned) is
//!   the same when it is in a resource of the same kind and name (namespaces differ between
//!   environments, and a ConfigMap kustomize regenerated is named by its stem).
//! - **Reads** ([`read_all`]) run at most [`PARALLEL`] at a time, off the UI thread, each
//!   Application's error its own. Each diff is kept by Argo CD instance, Application and
//!   resourceVersion ([`cached`]): picking the same Applications again reads only those that
//!   changed.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use futures::StreamExt as _;
use serde_json::Value;

use crate::smartdiff::{self, Fact, ObjectKey, Objects, SmartDiff};

/// Applications read at the same time, at most.
pub const PARALLEL: usize = 6;
/// Applications read in one grouped diff, at most; the others are said.
pub const MAX_APPS: usize = 300;
/// Diffs kept, at most (the last read kept).
const KEPT: usize = 512;
/// Changes named in the summary line, at most.
const SUMMARY_SHOWN: usize = 6;

// MARK: One Application

/// What syncing one Application now would change.
#[derive(Debug, Default)]
pub struct AppDiff {
    /// From the live objects to what the sync would leave.
    pub diff: SmartDiff,
    /// Resources Argo CD says are in sync.
    pub same: usize,
    /// Hooks left out.
    pub hooks: usize,
}

impl AppDiff {
    /// Nothing to sync: no resource differs, not even by noise.
    pub fn in_sync(&self) -> bool {
        self.diff.is_empty()
    }

    /// Only noise differs.
    pub fn noise_only(&self) -> bool {
        !self.diff.is_empty() && self.diff.groups.is_empty()
    }
}

/// A state Argo CD sends as JSON text; missing, empty or `null` is nothing.
fn state(item: &Value, key: &str) -> Value {
    item.get(key).and_then(Value::as_str).and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null)
}

/// What Argo CD's managed resources (its answer) say: the live objects, the objects a sync
/// would leave, how many resources it says are in sync, and how many hooks were left out. A
/// resource is keyed by the group, kind, namespace and name Argo CD gives it, the same on
/// both sides; its status and field managers are left out (a sync doesn't set them).
pub fn sides(body: &Value) -> (Objects, Objects, usize, usize) {
    let items = body.get("items").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let (mut live, mut desired) = (Objects::new(), Objects::new());
    let (mut same, mut hooks) = (0, 0);
    for item in items {
        if item.get("hook").and_then(Value::as_bool).unwrap_or(false) {
            hooks += 1;
            continue;
        }
        let s = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let key = ObjectKey { group: s("group"), kind: s("kind"), namespace: s("namespace"), name: s("name") };
        if key.kind.is_empty() || key.name.is_empty() {
            continue;
        }
        // Argo CD says whether it differs; an older one may not, then the states tell.
        if item.get("modified").and_then(Value::as_bool) == Some(false) {
            same += 1;
            continue;
        }
        let pick = |a: &str, b: &str| match state(item, a) {
            Value::Null => state(item, b),
            v => v,
        };
        let (l, d) = (pick("normalizedLiveState", "liveState"), pick("predictedLiveState", "targetState"));
        if !l.is_null() {
            live.insert(key.clone(), super::server::cleaned(l));
        }
        if !d.is_null() {
            desired.insert(key, super::server::cleaned(d));
        }
    }
    (live, desired, same, hooks)
}

impl AppDiff {
    /// The diff of Argo CD's managed resources (its answer). Pure, and long for a large
    /// Application: run it off the UI thread.
    pub fn of(body: &Value) -> Self {
        let (live, desired, same, hooks) = sides(body);
        let diff = smartdiff::compare(&live, &desired);
        let same = same + diff.unchanged;
        Self { diff, same, hooks }
    }
}

/// [`AppDiff::of`] on a blocking thread (in the runtime's task that read it).
pub(super) async fn of_managed(body: Value) -> Result<Arc<AppDiff>, String> {
    tokio::task::spawn_blocking(move || Arc::new(AppDiff::of(&body)))
        .await
        .map_err(|e| format!("Comparing the managed resources failed: {e}"))
}

/// What a sync of the Application `namespace/name` read on its hub `context` would change,
/// from the Argo CD server set for that context in Settings (else says why not).
pub async fn read_hub(context: String, namespace: String, name: String) -> Result<Arc<AppDiff>, String> {
    let read = crate::k8s::run_abortable(async move {
        let (server, token, _) = match super::server::login_for(&context) {
            Ok(login) => login,
            Err(why) => return Ok(Err(format!("{why}: what a sync would change is read from it"))),
        };
        let path = super::dashboard::managed_path(&namespace, &name);
        Ok(match super::server::get_path(&server, &token, &path).await {
            Ok(body) => of_managed(body).await,
            Err(e) => Err(e),
        })
    });
    read.await.unwrap_or_else(|e| Err(format!("{e:#}")))
}

// MARK: Kept

/// Which diff: the Argo CD instance, the Application's namespace and name, and its
/// resourceVersion when read.
pub type CacheKey = (String, String, String, String);
static KEPT_DIFFS: LazyLock<Mutex<VecDeque<(CacheKey, Arc<AppDiff>)>>> = LazyLock::new(Default::default);

fn kept(key: &CacheKey) -> Option<Arc<AppDiff>> {
    let all = KEPT_DIFFS.lock().unwrap_or_else(PoisonError::into_inner);
    all.iter().find(|(k, _)| k == key).map(|(_, d)| d.clone())
}

fn keep(key: CacheKey, diff: Arc<AppDiff>) {
    let mut all = KEPT_DIFFS.lock().unwrap_or_else(PoisonError::into_inner);
    // An older version of the same Application is no use any more.
    all.retain(|(k, _)| (&k.0, &k.1, &k.2) != (&key.0, &key.1, &key.2));
    all.push_back((key, diff));
    while all.len() > KEPT {
        all.pop_front();
    }
}

/// The diff kept under `key`, else `read` (kept when it succeeds). An Application whose
/// resourceVersion isn't known is always read.
pub async fn cached<Fut>(key: CacheKey, read: Fut) -> Result<Arc<AppDiff>, String>
where
    Fut: Future<Output = Result<Arc<AppDiff>, String>>,
{
    let known = !key.3.is_empty();
    if known && let Some(d) = kept(&key) {
        return Ok(d);
    }
    let read = read.await;
    if known && let Ok(d) = &read {
        keep(key, d.clone());
    }
    read
}

/// Each of `n` Applications read with `read` (given its index), at most [`PARALLEL`] at a
/// time; each result with its index as it comes. One failing doesn't stop the others; the
/// stream dropped, the reads running are dropped too.
pub fn read_all<F, Fut, T>(n: usize, mut read: F) -> impl futures::Stream<Item = (usize, T)>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = T>,
{
    futures::stream::iter(0..n)
        .map(move |i| {
            let f = read(i);
            async move { (i, f.await) }
        })
        .buffer_unordered(PARALLEL)
}

// MARK: Across Applications

/// What makes two changes in two Applications the same one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum CrossKey {
    /// The same wherever it is: an image, a digest, replicas, an env var, a pod template.
    Fact(Fact),
    /// A change of a resource's own, in a resource of this group, kind and name.
    OnResource(Fact, String, String, String),
    /// Noise folded by this rule.
    Noise(&'static str),
}

/// Whether a fact is the resource's own (the same only in a resource of the same name).
fn own_to_resource(fact: &Fact) -> bool {
    !matches!(
        fact,
        Fact::Image { .. } | Fact::Digest { .. } | Fact::Replicas { .. } | Fact::Env { .. } | Fact::PodTemplate { .. }
    )
}

/// A resource's name across environments: a ConfigMap's or Secret's kustomize regenerated is
/// named by its stem.
fn stable_name(key: &ObjectKey) -> String {
    let data = key.group.is_empty() && matches!(key.kind.as_str(), "ConfigMap" | "Secret");
    match smartdiff::rules::kustomize_stem(&key.name).filter(|_| data) {
        Some(stem) => stem.to_string(),
        None => key.name.clone(),
    }
}

fn cross_key(fact: &Fact, res: &ObjectKey) -> CrossKey {
    if !own_to_resource(fact) {
        return CrossKey::Fact(fact.clone());
    }
    // A regeneration's names hold its content's hash: the same regeneration by the stem.
    let fact = match fact {
        Fact::Renamed { .. } => Fact::Renamed { from: String::new(), to: String::new() },
        f => f.clone(),
    };
    CrossKey::OnResource(fact, res.group.clone(), res.kind.clone(), stable_name(res))
}

/// The line of a change across Applications: `image web 1.15 → 1.16`, `ConfigMap app-config
/// key LOG_LEVEL changed`, `Deployment worker added`, `Service web: spec.type: A → B`.
fn cross_text(key: &CrossKey) -> String {
    match key {
        CrossKey::Fact(f) => f.text(),
        CrossKey::OnResource(fact, _, kind, name) => match fact {
            Fact::ConfigKey { key, edit } | Fact::SecretKey { key, edit } => {
                format!("{kind} {name} key {key} {}", edit.word())
            }
            Fact::Renamed { .. } => format!("{kind} {name} regenerated under a new name"),
            Fact::Added => format!("{kind} {name} added"),
            Fact::Pruned => format!("{kind} {name} pruned (with prune)"),
            f => format!("{kind} {name}: {}", f.text()),
        },
        CrossKey::Noise(rule) => format!("noise: {rule}"),
    }
}

/// One change across the Applications read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cross {
    pub text: String,
    /// Each Application it is in (its index in the diffs given, in order), with the
    /// resources where (their index in that Application's `diff.resources`, in order).
    pub apps: Vec<(usize, Vec<usize>)>,
}

/// The changes of `diffs` (one per Application; None for one not read), each once with the
/// Applications and resources it is in: images first, then pod templates, replicas, env,
/// keys, resources, fields (the smart diff's order); within each, the one in the most
/// Applications first, then by text. With `noise`, the noise by rule instead.
pub fn regroup(diffs: &[Option<&SmartDiff>], noise: bool) -> Vec<Cross> {
    let mut by: BTreeMap<CrossKey, BTreeMap<usize, Vec<usize>>> = BTreeMap::new();
    for (app, diff) in diffs.iter().enumerate() {
        let Some(diff) = diff else { continue };
        for (r, res) in diff.resources.iter().enumerate() {
            let keys: Vec<CrossKey> = if noise {
                res.noise.iter().map(|n| CrossKey::Noise(n.rule)).collect()
            } else {
                res.facts.iter().map(|f| cross_key(f, &res.key)).collect()
            };
            for k in keys {
                let at = by.entry(k).or_default().entry(app).or_default();
                if at.last() != Some(&r) {
                    at.push(r);
                }
            }
        }
    }
    let rank = |k: &CrossKey| match k {
        CrossKey::Fact(f) | CrossKey::OnResource(f, ..) => f.rank(),
        CrossKey::Noise(_) => u8::MAX,
    };
    let mut out: Vec<(u8, Cross)> = by
        .into_iter()
        .map(|(k, apps)| (rank(&k), Cross { text: cross_text(&k), apps: apps.into_iter().collect() }))
        .collect();
    out.sort_by(|(ra, a), (rb, b)| ra.cmp(rb).then(b.apps.len().cmp(&a.apps.len())).then(a.text.cmp(&b.text)));
    out.into_iter().map(|(_, c)| c).collect()
}

/// The summary line: `3 changes in 12 environments: image web 1.15 → 1.16 (12) · replicas 3 →
/// 5 (2) · …`, `with_changes` the Applications that have one. None when nothing changes.
pub fn summary(groups: &[Cross], with_changes: usize) -> Option<String> {
    if groups.is_empty() {
        return None;
    }
    let mut parts: Vec<String> =
        groups.iter().take(SUMMARY_SHOWN).map(|g| format!("{} ({})", g.text, g.apps.len())).collect();
    if groups.len() > SUMMARY_SHOWN {
        parts.push(format!("{} more", groups.len() - SUMMARY_SHOWN));
    }
    Some(format!(
        "{} in {}: {}",
        crate::plural::count(groups.len(), "change"),
        crate::plural::count(with_changes, "environment"),
        parts.join(" · ")
    ))
}

#[cfg(test)]
mod tests;
