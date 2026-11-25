//! What syncing several Applications now would change, read together, the same change in
//! several of them shown once with the Applications it is in.
//!
//! - **Each Application's diff** is Argo CD's own, from its managed resources (`GET
//!   /api/v1/applications/{name}/managed-resources`): the live state (`normalizedLiveState`,
//!   else `liveState`) against what a sync would leave (`predictedLiveState`, else
//!   `targetState`), read with the smart diff (`crate::smartdiff`): facts, noise folded and
//!   counted, a Secret's values masked. Hooks are left out (Argo CD usually leaves them out
//!   already): they run during a sync rather than being kept in sync. A resource Argo CD says
//!   differs (`modified`) where the smart diff finds no change is said, never taken as in sync.
//!   An Application Argo CD couldn't compare with git ([`not_compared`]) is not read: its
//!   managed resources would read as everything pruned, or nothing to sync.
//! - **Across Applications** ([`regroup`]): values are compared with each environment's own
//!   names (its name, namespace, client) replaced by placeholders ([`Names`]). An image,
//!   digest, replicas, env var or pod template change is the same change wherever it is (one
//!   image bump in 12 environments is one line); a change of a resource's own (a ConfigMap's
//!   key, a field, a resource added or pruned) is one per field and edit on a kind, whatever
//!   the resources' names (one annotation added on 36 Deployments is one line), the
//!   environments where it differs said.
//! - **Reads** ([`read_all`]) run at most [`PARALLEL`] at a time, off the UI thread, each
//!   Application's error its own. Each diff is kept by Argo CD instance, Application and
//!   resourceVersion for a short while ([`cached`]: live drift can change the diff without the
//!   resourceVersion moving), bounded in count and size.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use serde_json::Value;

use super::Summary;
use crate::smartdiff::{self, Fact, ObjectKey, Objects, SmartDiff};

/// Applications read at the same time, at most.
pub const PARALLEL: usize = 6;
/// Applications read in one grouped diff, at most; the others are said.
pub const MAX_APPS: usize = 300;
/// Diffs kept, at most, and their size in all (the last read kept).
const KEPT: usize = 512;
const KEPT_BYTES: usize = 64 * 1024 * 1024;
/// How long a diff is kept: the live objects may drift with the Application unchanged.
const KEPT_FOR: Duration = Duration::from_secs(60);
/// Changes named in the summary, at most.
const SUMMARY_SHOWN: usize = 5;

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
    /// Resources Argo CD says differ where the smart diff finds no change (its normalization
    /// differs, or only noise): said, not shown.
    pub unexplained: Vec<ObjectKey>,
    /// About how many bytes it takes, its lines worked out.
    pub bytes: usize,
}

impl AppDiff {
    /// Nothing to sync: no resource differs, not even by noise, and Argo CD agrees.
    pub fn in_sync(&self) -> bool {
        self.diff.is_empty() && self.unexplained.is_empty()
    }

    /// Only noise differs, and Argo CD says nothing more.
    pub fn noise_only(&self) -> bool {
        !self.diff.is_empty() && self.diff.groups.is_empty() && self.unexplained.is_empty()
    }
}

/// Why Argo CD couldn't compare the Application with git, from its status: an error
/// condition (`ComparisonError`, `InvalidSpecError`, `UnknownError`...), or a sync status
/// Unknown. Its managed resources then say nothing true about a sync.
pub fn not_compared(app: &Summary) -> Option<String> {
    let blocking = |t: &str| t.ends_with("Error") && !matches!(t, "SyncError" | "DeletionError");
    if let Some((t, m)) = app.conditions.iter().find(|(t, _)| blocking(t)) {
        return Some(if m.trim().is_empty() { t.clone() } else { format!("{t}: {}", m.trim()) });
    }
    (app.sync == "Unknown").then(|| "its sync status is Unknown: Argo CD couldn't compare it with git".into())
}

/// A state Argo CD sends as JSON text; missing, empty or `null` is nothing.
fn state(item: &Value, key: &str) -> Value {
    item.get(key).and_then(Value::as_str).and_then(|s| serde_json::from_str(s).ok()).unwrap_or(Value::Null)
}

/// What Argo CD's managed resources (its answer) say.
#[derive(Debug, Default)]
pub struct Sides {
    pub live: Objects,
    /// What a sync would leave.
    pub desired: Objects,
    /// Resources Argo CD says are in sync, and hooks left out.
    pub same: usize,
    pub hooks: usize,
    /// Resources Argo CD says differ.
    pub modified: BTreeSet<ObjectKey>,
}

/// The live objects and those a sync would leave, from Argo CD's managed resources. A
/// resource is keyed by the group, kind, namespace and name Argo CD gives it, the same on both
/// sides; its status and field managers are left out (a sync doesn't set them). `modified` is
/// left out of the answer when false: when any item has it, one without it is in sync; when
/// none has it (an older Argo CD), the states tell.
pub fn sides(body: &Value) -> Sides {
    let items = body.get("items").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let said = items.iter().any(|i| i.get("modified").is_some());
    let mut out = Sides::default();
    for item in items {
        if item.get("hook").and_then(Value::as_bool).unwrap_or(false) {
            out.hooks += 1;
            continue;
        }
        let s = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let key = ObjectKey { group: s("group"), kind: s("kind"), namespace: s("namespace"), name: s("name") };
        if key.kind.is_empty() || key.name.is_empty() {
            continue;
        }
        let modified = item.get("modified").and_then(Value::as_bool).or(said.then_some(false));
        if modified == Some(false) {
            out.same += 1;
            continue;
        }
        if modified == Some(true) {
            out.modified.insert(key.clone());
        }
        let pick = |a: &str, b: &str| match state(item, a) {
            Value::Null => state(item, b),
            v => v,
        };
        let (l, d) = (pick("normalizedLiveState", "liveState"), pick("predictedLiveState", "targetState"));
        if !l.is_null() {
            out.live.insert(key.clone(), super::server::cleaned(l));
        }
        if !d.is_null() {
            out.desired.insert(key, super::server::cleaned(d));
        }
    }
    out
}

impl AppDiff {
    /// The diff of Argo CD's managed resources (its answer), every line worked out. Pure, and
    /// long for a large Application: run it off the UI thread.
    pub fn of(body: &Value) -> Self {
        let s = sides(body);
        let diff = smartdiff::compare(&s.live, &s.desired);
        // Worked out here, not when a change is unfolded on the UI thread.
        diff.prepare();
        let with_facts: BTreeSet<&ObjectKey> =
            diff.resources.iter().filter(|r| !r.facts.is_empty()).map(|r| &r.key).collect();
        let unexplained = s.modified.iter().filter(|k| !with_facts.contains(k)).cloned().collect();
        let lines: usize = diff.resources.iter().map(smartdiff::ResourceChange::lines_bytes).sum();
        // Both sides of each resource are kept too: about as much again as their lines.
        let bytes = 2 * lines + 1024;
        let same = s.same + diff.unchanged;
        Self { diff, same, hooks: s.hooks, unexplained, bytes }
    }
}

/// [`AppDiff::of`] on a blocking thread (in the runtime's task that read it).
pub(super) async fn of_managed(body: Value) -> Result<Arc<AppDiff>, String> {
    tokio::task::spawn_blocking(move || Arc::new(AppDiff::of(&body)))
        .await
        .map_err(|e| format!("Comparing the managed resources failed: {e}"))
}

// MARK: Kept

/// Which diff: the Argo CD instance, the Application's namespace and name, and its
/// resourceVersion when read.
pub type CacheKey = (String, String, String, String);
/// The diffs kept with when they were read, the last read last.
type Kept = VecDeque<(CacheKey, Instant, Arc<AppDiff>)>;
static KEPT_DIFFS: LazyLock<Mutex<Kept>> = LazyLock::new(Default::default);

fn kept(key: &CacheKey) -> Option<Arc<AppDiff>> {
    let all = KEPT_DIFFS.lock().unwrap_or_else(PoisonError::into_inner);
    all.iter().find(|(k, at, _)| k == key && at.elapsed() < KEPT_FOR).map(|(_, _, d)| d.clone())
}

fn keep(key: CacheKey, diff: Arc<AppDiff>) {
    let mut all = KEPT_DIFFS.lock().unwrap_or_else(PoisonError::into_inner);
    // An older version of the same Application is no use any more.
    all.retain(|(k, at, _)| (&k.0, &k.1, &k.2) != (&key.0, &key.1, &key.2) && at.elapsed() < KEPT_FOR);
    all.push_back((key, Instant::now(), diff));
    let total = |all: &Kept| all.iter().map(|(_, _, d)| d.bytes).sum::<usize>();
    while all.len() > KEPT || (all.len() > 1 && total(&all) > KEPT_BYTES) {
        all.pop_front();
    }
}

/// The diff kept under `key` (read less than a minute ago), else `read` (kept when it
/// succeeds). `fresh` (Read again) reads whatever is kept; an Application whose
/// resourceVersion isn't known is always read.
pub async fn cached<Fut>(key: CacheKey, fresh: bool, read: Fut) -> Result<Arc<AppDiff>, String>
where
    Fut: Future<Output = Result<Arc<AppDiff>, String>>,
{
    let known = !key.3.is_empty();
    if known
        && !fresh
        && let Some(d) = kept(&key)
    {
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

/// An environment's own names, replaced by placeholders before two environments' values are
/// compared: the same rule edit in nine environments is one change even though each one's rule
/// names its own environment and client. Its names (the environment's, the Application's, its
/// destination namespace) become `‹env›`, the longest first; the client (what follows the
/// environment's type in its name: `acme` in `shop-prod-acme`) becomes `‹client›` where it is a
/// word of its own.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Names {
    own: Vec<String>,
    client: Option<String>,
}

/// Names shorter than this are left alone: they would match inside too many words.
const MIN_NAME: usize = 3;
const ENV: &str = "‹env›";
const CLIENT: &str = "‹client›";

impl Names {
    pub fn new(own: impl IntoIterator<Item = String>, client: Option<String>) -> Self {
        // A name inside a placeholder would replace it again.
        let usable =
            |n: &String| n.chars().count() >= MIN_NAME && !ENV.contains(n.as_str()) && !CLIENT.contains(n.as_str());
        let mut own: Vec<String> = own.into_iter().filter(usable).collect();
        own.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
        own.dedup();
        let client = client.filter(|c| usable(c) && !own.contains(c));
        Self { own, client }
    }

    /// `text` with the names replaced.
    pub fn apply(&self, text: &str) -> String {
        let mut out = text.to_string();
        for n in &self.own {
            if out.contains(n.as_str()) {
                out = out.replace(n.as_str(), ENV);
            }
        }
        if let Some(c) = &self.client {
            out = replace_word(&out, c, CLIENT);
        }
        out
    }

    /// `fact` with the names replaced in its values.
    fn fact(&self, fact: &Fact) -> Fact {
        let t = |s: &String| self.apply(s);
        let o = |s: &Option<String>| s.as_ref().map(|s| self.apply(s));
        match fact {
            Fact::Image { from_repo, from_tag, to_repo, to_tag } => {
                Fact::Image { from_repo: t(from_repo), from_tag: t(from_tag), to_repo: t(to_repo), to_tag: t(to_tag) }
            }
            Fact::Digest { repo, tag } => Fact::Digest { repo: t(repo), tag: t(tag) },
            Fact::PodTemplate { keys } => Fact::PodTemplate { keys: t(keys) },
            Fact::Replicas { from, to } => Fact::Replicas { from: o(from), to: o(to) },
            Fact::Env { name, edit, from, to } => Fact::Env { name: t(name), edit: *edit, from: o(from), to: o(to) },
            Fact::ConfigKey { key, edit } => Fact::ConfigKey { key: t(key), edit: *edit },
            Fact::SecretKey { key, edit } => Fact::SecretKey { key: t(key), edit: *edit },
            Fact::Renamed { from, to } => Fact::Renamed { from: t(from), to: t(to) },
            Fact::Field { path, edit, from, to } => {
                Fact::Field { path: t(path), edit: *edit, from: o(from), to: o(to) }
            }
            Fact::Added | Fact::Pruned => fact.clone(),
        }
    }
}

/// `text` with `word` replaced by `with` where neither side of it is a letter or a digit.
fn replace_word(text: &str, word: &str, with: &str) -> String {
    let alnum = |c: Option<char>| c.is_some_and(char::is_alphanumeric);
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(word) {
        // What precedes: in what is left, else the last of what is written already.
        let before = if at > 0 { rest[..at].chars().last() } else { out.chars().last() };
        let after = rest[at + word.len()..].chars().next();
        out.push_str(&rest[..at]);
        out.push_str(if alnum(before) || alnum(after) { word } else { with });
        rest = &rest[at + word.len()..];
    }
    out.push_str(rest);
    out
}

/// What makes two changes in two Applications the same one.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CrossKey {
    /// The same wherever it is: an image, a digest, replicas, an env var, a pod template (its
    /// values with each environment's names replaced).
    Fact(Fact),
    /// A change of a resource's own, the same field with the same edit, on resources of this
    /// group and kind whatever their names and values (said in the line).
    OnKind(Fact, String, String),
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

/// A fact without its values: what makes it the same field with the same edit.
fn shape(fact: &Fact) -> Fact {
    match fact {
        Fact::Field { path, edit, .. } => Fact::Field { path: path.clone(), edit: *edit, from: None, to: None },
        Fact::Renamed { .. } => Fact::Renamed { from: String::new(), to: String::new() },
        f => f.clone(),
    }
}

/// A resource's name across environments: a core ConfigMap or Secret kustomize regenerated
/// is named by its stem.
fn stable_name(key: &ObjectKey) -> String {
    let data = key.group.is_empty() && matches!(key.kind.as_str(), "ConfigMap" | "Secret");
    match smartdiff::rules::kustomize_stem(&key.name).filter(|_| data) {
        Some(stem) => stem.to_string(),
        None => key.name.clone(),
    }
}

/// Kinds of the core API that other groups name too (Knative's Service...).
const CORE_KINDS: [&str; 9] =
    ["Service", "ConfigMap", "Secret", "Pod", "Namespace", "ServiceAccount", "Endpoints", "Event", "Node"];

/// A kind as shown: with its group when another group's kind has a core kind's name.
pub fn kind_text(group: &str, kind: &str) -> String {
    if !group.is_empty() && CORE_KINDS.contains(&kind) { format!("{kind}.{group}") } else { kind.to_string() }
}

/// The line of a change of a resource's own: `fact` on the one resource named, or on as many
/// resources of the kind as there are names.
fn own_text(fact: &Fact, kind: &str, names: &[&str]) -> String {
    if let [name] = names {
        return match fact {
            Fact::ConfigKey { key, edit } | Fact::SecretKey { key, edit } => {
                format!("{kind} {name} key {key} {}", edit.word())
            }
            Fact::Renamed { .. } => format!("{kind} {name} regenerated under a new name"),
            Fact::Added => format!("{kind} {name} added"),
            Fact::Pruned => format!("{kind} {name} pruned (with prune)"),
            f => format!("{kind} {name}: {}", f.text()),
        };
    }
    let many = crate::plural::count(names.len(), kind);
    match fact {
        Fact::ConfigKey { key, edit } | Fact::SecretKey { key, edit } => format!("{many}: key {key} {}", edit.word()),
        Fact::Renamed { .. } => format!("{many} regenerated under new names"),
        Fact::Added => format!("{many} added"),
        Fact::Pruned => format!("{many} pruned (with prune)"),
        f => format!("{many}: {}", f.text()),
    }
}

/// One change across the Applications read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cross {
    /// What makes it one: the same across reads, to keep it unfolded.
    pub key: CrossKey,
    pub text: String,
    /// Each Application it is in (its index in the diffs given, in order), with the
    /// resources where (their index in that Application's `diff.resources`, in order).
    pub apps: Vec<(usize, Vec<usize>)>,
    /// The Applications where it is otherwise than in most: on other resources, or to other
    /// values once their own names are replaced.
    pub differs: Vec<usize>,
}

/// Where a change of a resource's own is: the resource's name and the fact with its values
/// (each environment's names replaced), and the hash of the values it was read from.
type Seen = (String, Fact, u64);
/// A change's places in each Application: its resources, and what each says.
type Places = BTreeMap<usize, (Vec<usize>, Vec<Seen>)>;

/// The changes of `diffs` (one per Application; None for one not read), each once with the
/// Applications and resources it is in; `names`: each Application's own names (see [`Names`];
/// one missing is none). Images first, then pod templates, replicas, env, keys, resources,
/// fields (the smart diff's order); within each, the one in the most Applications first, then
/// on the most resources, then by text.
///
/// An image, replicas or env change is one wherever it is, with the same values once each
/// environment's names are replaced (the same one to other values is a line each, said: `(one
/// of 2 different values)`). A change of a resource's own (a field, a key, a resource added)
/// is one per field and edit on a kind, whatever the resources' names (`36 Deployments:
/// annotation example.io/role added`), its values shown when they are the same everywhere;
/// the Applications where it is on other resources or to other values are said (`differs in
/// 1 environment`). With `noise`, the noise by rule instead.
pub fn regroup(diffs: &[Option<&SmartDiff>], names: &[Names], noise: bool) -> Vec<Cross> {
    let none = Names::default();
    let mut by: BTreeMap<CrossKey, Places> = BTreeMap::new();
    for (app, diff) in diffs.iter().enumerate() {
        let Some(diff) = diff else { continue };
        let n = names.get(app).unwrap_or(&none);
        let text = |s: &str| n.apply(s);
        for (r, res) in diff.resources.iter().enumerate() {
            let found: Vec<(CrossKey, Option<Seen>)> = if noise {
                res.noise.iter().map(|x| (CrossKey::Noise(x.rule), None)).collect()
            } else {
                res.facts
                    .iter()
                    .enumerate()
                    .map(|(i, f)| {
                        let f = n.fact(f);
                        if !own_to_resource(&f) {
                            return (CrossKey::Fact(f), None);
                        }
                        let key = CrossKey::OnKind(shape(&f), res.key.group.clone(), res.key.kind.clone());
                        // A regeneration's names hold its content's hash: the same one by its stem.
                        let f = if matches!(f, Fact::Renamed { .. }) { shape(&f) } else { f };
                        (key, Some((n.apply(&stable_name(&res.key)), f, res.digest_by(i, &text))))
                    })
                    .collect()
            };
            for (k, seen) in found {
                let (at, all) = by.entry(k).or_default().entry(app).or_default();
                if at.last() != Some(&r) {
                    at.push(r);
                }
                all.extend(seen);
            }
        }
    }
    // How many lines say the same but for their values.
    let mut variants: BTreeMap<String, usize> = BTreeMap::new();
    for k in by.keys() {
        if let CrossKey::Fact(f) = k {
            *variants.entry(f.text()).or_default() += 1;
        }
    }
    let rank = |k: &CrossKey| match k {
        CrossKey::Fact(f) | CrossKey::OnKind(f, ..) => f.rank(),
        CrossKey::Noise(_) => u8::MAX,
    };
    let mut out: Vec<(u8, usize, Cross)> = by
        .into_iter()
        .map(|(k, places)| {
            let resources: usize = places.values().map(|(r, _)| r.len()).sum();
            let (text, differs) = match &k {
                CrossKey::Fact(f) => {
                    let mut text = f.text();
                    if let Some(n) = variants.get(&text).copied().filter(|n| *n > 1) {
                        text.push_str(&format!(" (one of {n} different values)"));
                    }
                    (text, Vec::new())
                }
                CrossKey::OnKind(shaped, group, kind) => own_line(shaped, &kind_text(group, kind), &places),
                CrossKey::Noise(rule) => (format!("noise: {rule}"), Vec::new()),
            };
            let apps = places.into_iter().map(|(a, (r, _))| (a, r)).collect();
            (rank(&k), resources, Cross { key: k, text, apps, differs })
        })
        .collect();
    out.sort_by(|(ra, na, a), (rb, nb, b)| {
        ra.cmp(rb).then(b.apps.len().cmp(&a.apps.len())).then(nb.cmp(na)).then(a.text.cmp(&b.text))
    });
    out.into_iter().map(|(_, _, c)| c).collect()
}

/// The line of a change of a resource's own on a kind (`shaped`: without its values), and the
/// Applications where it is otherwise than in most.
fn own_line(shaped: &Fact, kind: &str, places: &Places) -> (String, Vec<usize>) {
    let seen = || places.values().flat_map(|(_, s)| s.iter());
    let names: BTreeSet<&str> = seen().map(|(n, _, _)| n.as_str()).collect();
    let values: BTreeSet<(&Fact, u64)> = seen().map(|(_, f, d)| (f, *d)).collect();
    let fact = match values.first() {
        Some((f, _)) if values.len() == 1 => (*f).clone(),
        _ => shaped.clone(),
    };
    let names: Vec<&str> = names.into_iter().collect();
    let mut text = own_text(&fact, kind, &names);
    let valued = !matches!(fact, Fact::Added | Fact::Pruned | Fact::Renamed { .. });
    if values.len() > 1 && valued {
        text.push_str(&format!(" ({} values)", values.len()));
    }
    // Each Application's resources and values: those unlike the most's differ.
    let signature = |seen: &[Seen]| {
        let mut sig: Vec<(&str, &Fact, u64)> = seen.iter().map(|(n, f, d)| (n.as_str(), f, *d)).collect();
        sig.sort();
        sig.dedup();
        sig
    };
    let sigs: Vec<(usize, Vec<(&str, &Fact, u64)>)> = places.iter().map(|(a, (_, s))| (*a, signature(s))).collect();
    let mut count: BTreeMap<&[(&str, &Fact, u64)], usize> = BTreeMap::new();
    for (_, sig) in &sigs {
        *count.entry(sig.as_slice()).or_default() += 1;
    }
    let differs: Vec<usize> = match count.iter().max_by_key(|(_, n)| **n) {
        Some((most, _)) if count.len() > 1 => {
            sigs.iter().filter(|(_, s)| s.as_slice() != *most).map(|(a, _)| *a).collect()
        }
        _ => Vec::new(),
    };
    if !differs.is_empty() {
        text.push_str(&format!(" (differs in {})", crate::plural::count(differs.len(), "environment")));
    }
    (text, differs)
}

/// The summary's heading and its first lines: `3 changes in 12 environments`, then the first
/// [`SUMMARY_SHOWN`] changes (images first) with how many environments each is in, then how
/// many more. `with_changes`: the Applications that have one. None when nothing changes.
pub fn summary_lines(groups: &[Cross], with_changes: usize) -> Option<(String, Vec<String>)> {
    if groups.is_empty() {
        return None;
    }
    let heading = format!(
        "{} in {}",
        crate::plural::count(groups.len(), "change"),
        crate::plural::count(with_changes, "environment")
    );
    let mut lines: Vec<String> =
        groups.iter().take(SUMMARY_SHOWN).map(|g| format!("{} ({})", g.text, g.apps.len())).collect();
    if groups.len() > SUMMARY_SHOWN {
        lines.push(format!("{} more", crate::plural::count(groups.len() - SUMMARY_SHOWN, "change")));
    }
    Some((heading, lines))
}

/// The summary in a line: `3 changes in 12 environments: image web 1.15 → 1.16 (12) · replicas
/// 3 → 5 (2) · …`. None when nothing changes.
pub fn summary(groups: &[Cross], with_changes: usize) -> Option<String> {
    let (heading, lines) = summary_lines(groups, with_changes)?;
    Some(format!("{heading}: {}", lines.join(" · ")))
}

/// The same error said once with the Applications (their index) it is of: the one of the
/// most first, then by text.
pub fn errors_by_text<'a>(errors: impl IntoIterator<Item = (usize, &'a str)>) -> Vec<(String, Vec<usize>)> {
    let mut by: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, e) in errors {
        by.entry(e).or_default().push(i);
    }
    let mut out: Vec<(String, Vec<usize>)> = by.into_iter().map(|(e, apps)| (e.to_string(), apps)).collect();
    out.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    out
}

/// A made-up example, for the developer tools' snapshots and the tests.
#[cfg(any(test, feature = "dev"))]
pub mod example;
#[cfg(test)]
mod tests;
