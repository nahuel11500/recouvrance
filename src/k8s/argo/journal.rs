//! The sync journal: every Argo CD operation the app sees, kept on this machine so a sync's
//! steps can still be read once Argo CD's own history (its last operation, a few deployments)
//! has moved on, and after a restart.
//!
//! An operation is seen wherever an Application is read whole enough: an Argo CD API server's
//! list and stream, a hub's list, a dashboard, a grouped sync following its Applications. Each
//! state of it (its phase, message, each resource's result) is one JSON line of
//! `argocd-sync-journal.jsonl` in the data directory, readable only by the user; the same
//! state seen again (on each stream event, by a hub and its API server) is not written again.
//! An operation is its Argo CD, the Application's namespace and name, and when it started.
//!
//! The journal is bounded: past 25 MB it moves to `<file>.1` (replacing the older one), so
//! two files hold at most 50 MB, and what is older than 30 days is left out when read (a
//! `.1` file all older is removed). Reading fails closed: a line that doesn't read is skipped
//! and counted, never taking the rest with it.

use std::collections::HashMap;
use std::hash::{Hash as _, Hasher as _};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError, mpsc};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(feature = "dev")]
pub mod example;

/// The journal's file, in the data directory.
const FILE: &str = "argocd-sync-journal.jsonl";
/// The journal moves to `<file>.1` (replacing the older one) past this size.
pub const MAX_BYTES: u64 = 25 * 1024 * 1024;
/// Operations seen longer ago than this are left out.
pub const KEEP_DAYS: i64 = 30;
/// Resources kept of one operation, at most.
const MAX_RESOURCES: usize = 2000;
/// Characters kept of a message, at most.
const MESSAGE_CHARS: usize = 4000;
/// Operations whose last state is remembered to skip writing it again, at most.
const REMEMBERED: usize = 100_000;
/// One line of the journal, at most (an operation of thousands of resources with long
/// messages is cut to fit).
const MAX_LINE: usize = 256 * 1024;

/// One resource of an operation's result.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Step {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub group: String,
    #[serde(default)]
    pub kind: String,
    /// Empty for a cluster-scoped resource.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub namespace: String,
    #[serde(default)]
    pub name: String,
    /// `Synced`, `SyncFailed`, `Pruned`…
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// A hook's phase (`Running`, `Succeeded`, `Failed`…); empty for a resource that isn't one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hook_phase: String,
    /// `PreSync`, `Sync`, `PostSync`… for a hook.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hook_type: String,
    /// The phase it ran in (`PreSync`, `Sync`, `PostSync`, `SyncFail`); empty in lines written
    /// before it was kept.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sync_phase: String,
}

impl Step {
    /// `Deployment shop/web`.
    pub fn label(&self) -> String {
        if self.namespace.is_empty() {
            format!("{} {}", self.kind, self.name)
        } else {
            format!("{} {}/{}", self.kind, self.namespace, self.name)
        }
    }

    /// What it came to: a hook's phase, else its status.
    pub fn result(&self) -> &str {
        if self.hook_phase.is_empty() { &self.status } else { &self.hook_phase }
    }

    pub fn is_hook(&self) -> bool {
        !self.hook_phase.is_empty() || !self.hook_type.is_empty()
    }

    /// The phase it ran in: as Argo CD says, else (an older line) its hook's type, else Sync.
    pub fn phase(&self) -> &str {
        if !self.sync_phase.is_empty() {
            &self.sync_phase
        } else if matches!(self.hook_type.as_str(), "PreSync" | "Sync" | "PostSync" | "SyncFail") {
            &self.hook_type
        } else {
            "Sync"
        }
    }

    /// An entry of `syncResult.resources`, its message cut.
    pub fn of(r: &Value) -> Self {
        Self {
            group: text(r, "group"),
            kind: text(r, "kind"),
            namespace: text(r, "namespace"),
            name: text(r, "name"),
            status: text(r, "status"),
            message: cut(&text(r, "message")),
            hook_phase: text(r, "hookPhase"),
            hook_type: text(r, "hookType"),
            sync_phase: text(r, "syncPhase"),
        }
    }

    /// Which resource it is, hashed: the same step in two states of an operation.
    fn key(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (&self.group, &self.kind, &self.namespace, &self.name, &self.hook_type).hash(&mut h);
        h.finish()
    }
}

/// One state of an operation, as written.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    /// When this state was seen, in seconds (not part of the state).
    #[serde(default)]
    pub seen: i64,
    /// Its Argo CD (`grouping::server_instance`).
    #[serde(default)]
    pub instance: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub name: String,
    /// When it started (`operationState.startedAt`): with the Application, what it is.
    #[serde(default)]
    pub started: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<String>,
    #[serde(default)]
    pub phase: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
    /// Who started it: a user, `autosync`, or `recouvrance`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// The revision(s) it synced to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revisions: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    /// `hook` (hooks run) or `apply` (without hooks), when it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<Step>,
}

/// An operation: its Argo CD, the Application's namespace and name, when it started.
pub type OpId = (String, String, String, String);

impl Record {
    pub fn id(&self) -> OpId {
        (self.instance.clone(), self.namespace.clone(), self.name.clone(), self.started.clone())
    }

    /// The state it holds, whenever it was seen: two equal ones are written once.
    fn fingerprint(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        Self { seen: 0, ..self.clone() }.hash(&mut h);
        h.finish()
    }

    /// When it started, read.
    pub fn started_at(&self) -> Option<jiff::Timestamp> {
        self.started.parse().ok()
    }

    pub fn finished_at(&self) -> Option<jiff::Timestamp> {
        self.finished.as_deref().and_then(|t| t.parse().ok())
    }

    /// Still running: not ended yet when last seen.
    pub fn running(&self) -> bool {
        matches!(self.phase.as_str(), "Running" | "Terminating")
    }
}

/// `s` cut to `MESSAGE_CHARS`.
fn cut(s: &str) -> String {
    if s.chars().count() <= MESSAGE_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MESSAGE_CHARS).collect();
    out.push('…');
    out
}

/// A string field of `v`, empty when absent.
fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// The revision(s) of a sync result or a sync operation: `revisions`, else `revision`.
fn revisions_of(v: Option<&Value>) -> Vec<String> {
    let Some(v) = v else { return Vec::new() };
    let many: Vec<String> = v
        .get("revisions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|r| !r.is_empty())
        .map(String::from)
        .collect();
    if !many.is_empty() {
        return many;
    }
    v.get("revision").and_then(Value::as_str).filter(|r| !r.is_empty()).map(String::from).into_iter().collect()
}

/// The operation of the Application `app` of the Argo CD `instance`, as seen at `seen`
/// (seconds): None when it has none (never synced), or none that says when it started.
pub fn record_of(instance: &str, app: &Value, seen: i64) -> Option<Record> {
    let state = app.pointer("/status/operationState").filter(|s| s.is_object())?;
    let started = state.get("startedAt").and_then(Value::as_str).filter(|s| !s.is_empty())?.to_string();
    let name = app.pointer("/metadata/name").and_then(Value::as_str).filter(|s| !s.is_empty())?.to_string();
    let namespace = app
        .pointer("/metadata/namespace")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(super::DEFAULT_NAMESPACE)
        .to_string();
    let phase = state.get("phase").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or("Unknown");
    let sync = state.pointer("/operation/sync");
    let mut revisions = revisions_of(state.get("syncResult"));
    if revisions.is_empty() {
        revisions = revisions_of(sync);
    }
    let strategy = sync.and_then(|s| s.get("syncStrategy")).and_then(|s| {
        if s.get("apply").is_some() {
            Some("apply".to_string())
        } else if s.get("hook").is_some() {
            Some("hook".to_string())
        } else {
            None
        }
    });
    let resources = state
        .pointer("/syncResult/resources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_RESOURCES)
        .map(Step::of)
        .collect();
    Some(Record {
        seen,
        instance: instance.to_string(),
        namespace,
        name,
        started,
        finished: state.get("finishedAt").and_then(Value::as_str).filter(|s| !s.is_empty()).map(String::from),
        phase: phase.to_string(),
        message: cut(state.get("message").and_then(Value::as_str).unwrap_or_default()),
        by: state.pointer("/operation/initiatedBy").and_then(super::dashboard::who),
        revisions,
        dry_run: sync.and_then(|s| s.get("dryRun")).and_then(Value::as_bool) == Some(true),
        strategy,
        resources,
    })
}

// MARK: Writing

/// `<path>.1`, where a full journal moves.
pub fn older(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".1");
    PathBuf::from(p)
}

/// The journal file: appends the states of operations not written yet.
pub struct Journal {
    path: PathBuf,
    /// Each operation's last state written.
    written: HashMap<OpId, u64>,
}

impl Journal {
    /// The journal at `path`, knowing what it holds already (so a restart writes no state
    /// twice); a `.1` file whose newest change is older than `KEEP_DAYS` is removed.
    pub fn open(path: PathBuf, now: i64) -> Self {
        let old = older(&path);
        let stale = std::fs::metadata(&old)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|t| now - (t.as_secs() as i64) > KEEP_DAYS * 86_400);
        if stale {
            let _ = std::fs::remove_file(&old);
        }
        let mut written = HashMap::new();
        for p in [old, path.clone()] {
            let Ok(bytes) = std::fs::read(&p) else { continue };
            for line in String::from_utf8_lossy(&bytes).lines() {
                if let Ok(r) = serde_json::from_str::<Record>(line) {
                    written.insert(r.id(), r.fingerprint());
                }
            }
        }
        Self { path, written }
    }

    /// Write `r` unless this state of its operation is written already: whether it was. A
    /// full file moves to `.1` first. The file and its directory are the user's alone.
    pub fn append(&mut self, r: &Record) -> std::io::Result<bool> {
        use std::io::Write as _;
        let print = r.fingerprint();
        if self.written.get(&r.id()) == Some(&print) {
            return Ok(false);
        }
        if let Some(dir) = self.path.parent() {
            crate::private_files::create_dir(dir)?;
        }
        if std::fs::metadata(&self.path).is_ok_and(|m| m.len() >= MAX_BYTES) {
            std::fs::rename(&self.path, older(&self.path))?;
        }
        let line = line_of(r)?;
        let mut f = crate::private_files::private_file(std::fs::OpenOptions::new().append(true).create(true))
            .open(&self.path)?;
        writeln!(f, "{line}")?;
        if self.written.len() >= REMEMBERED {
            self.written.clear();
        }
        self.written.insert(r.id(), print);
        Ok(true)
    }
}

/// Where the app's journal is.
pub fn path() -> PathBuf {
    crate::paths::data_dir().join(FILE)
}

/// The app's journal, written on a thread of its own; none in unit tests.
static WRITER: LazyLock<Option<mpsc::Sender<Record>>> = LazyLock::new(|| {
    if cfg!(test) {
        return None;
    }
    let (tx, rx) = mpsc::channel::<Record>();
    std::thread::Builder::new()
        .name("sync-journal".into())
        .spawn(move || {
            let mut journal = Journal::open(path(), jiff::Timestamp::now().as_second());
            while let Ok(r) = rx.recv() {
                if let Err(e) = journal.append(&r) {
                    eprintln!("the sync journal could not be written: {e}");
                }
            }
        })
        .ok()?;
    Some(tx)
});

/// Each operation's last state passed to the writer: the same seen again (each stream event
/// of an Application, each list of a hub) is not passed again.
static SENT: LazyLock<Mutex<HashMap<OpId, u64>>> = LazyLock::new(Default::default);

/// Keep the operation of the Application `app` of the Argo CD `instance`, if it has one and
/// this state of it isn't kept yet. Cheap when nothing changed; never waits on the file.
pub fn observe(instance: &str, app: &Value) {
    let Some(tx) = WRITER.as_ref() else { return };
    let Some((id, print)) = quick_print(instance, app) else { return };
    {
        let mut sent = SENT.lock().unwrap_or_else(PoisonError::into_inner);
        if sent.get(&id) == Some(&print) {
            return;
        }
        if sent.len() >= REMEMBERED {
            sent.clear();
        }
        sent.insert(id, print);
    }
    if let Some(r) = record_of(instance, app, jiff::Timestamp::now().as_second()) {
        let _ = tx.send(r);
    }
}

/// The operation of `app` and a cheap print of its state, read in place (nothing copied but
/// its id): its phase, when it ended, its message's length, each resource's status and hook
/// phase. The same print is the same state, so the record isn't built again.
fn quick_print(instance: &str, app: &Value) -> Option<(OpId, u64)> {
    let state = app.pointer("/status/operationState").filter(|s| s.is_object())?;
    fn s<'a>(v: &'a Value, k: &str) -> &'a str {
        v.get(k).and_then(Value::as_str).unwrap_or_default()
    }
    let started = state.get("startedAt").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let name = app.pointer("/metadata/name").and_then(Value::as_str).filter(|s| !s.is_empty())?;
    let namespace = app
        .pointer("/metadata/namespace")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(super::DEFAULT_NAMESPACE);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (s(state, "phase"), s(state, "finishedAt"), s(state, "message").len()).hash(&mut h);
    let resources = state.pointer("/syncResult/resources").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    resources.len().hash(&mut h);
    for r in resources {
        (s(r, "status"), s(r, "hookPhase")).hash(&mut h);
    }
    let id = (instance.to_string(), namespace.to_string(), name.to_string(), started.to_string());
    Some((id, h.finish()))
}

/// `r` as one line of at most `MAX_LINE` bytes: past that, its steps' messages are left out,
/// then its last steps.
fn line_of(r: &Record) -> std::io::Result<String> {
    let line = serde_json::to_string(r).map_err(std::io::Error::other)?;
    if line.len() <= MAX_LINE {
        return Ok(line);
    }
    let mut cut = r.clone();
    for s in &mut cut.resources {
        s.message.clear();
    }
    loop {
        let line = serde_json::to_string(&cut).map_err(std::io::Error::other)?;
        if line.len() <= MAX_LINE || cut.resources.is_empty() {
            return Ok(line);
        }
        let keep = cut.resources.len() * MAX_LINE / line.len();
        cut.resources.truncate(keep.min(cut.resources.len() - 1));
    }
}

// MARK: Reading

/// An operation as the journal last saw it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Operation {
    /// Its last state.
    pub last: Record,
    /// How many states of it were written.
    pub states: usize,
    /// When each step of `last` was seen, in the same order (empty when not worked out).
    pub seen: Vec<Seen>,
}

/// When the journal saw a step, in seconds: how long it ran, as far as this machine knows
/// (a step seen in one state only has no length).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Seen {
    /// The first state holding it.
    pub first: i64,
    /// The first state where it had the result it has now.
    pub settled: i64,
}

/// Each step's first sighting and that of its result, by step: worked out while reading.
type Sightings = HashMap<u64, (i64, u64, i64)>;

/// Note the steps of `r` in `seen`.
fn sight(seen: &mut Sightings, r: &Record) {
    for s in &r.resources {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        s.result().hash(&mut h);
        let result = h.finish();
        let e = seen.entry(s.key()).or_insert((r.seen, result, r.seen));
        if e.1 != result {
            e.1 = result;
            e.2 = r.seen;
        }
    }
}

/// The journal as read: its operations, newest first, and how many lines didn't read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Loaded {
    pub ops: Vec<Operation>,
    pub bad: usize,
}

/// The operations of the journal's `texts` (the older file first), seen within `KEEP_DAYS`
/// of `now`: each once, as last seen, newest first. A line that doesn't read is counted.
pub fn parse(texts: &[&str], now: i64) -> Loaded {
    let mut by_id: HashMap<OpId, usize> = HashMap::new();
    let mut ops: Vec<Operation> = Vec::new();
    let mut sightings: Vec<Sightings> = Vec::new();
    let mut bad = 0;
    for text in texts {
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let r = match serde_json::from_str::<Record>(line) {
                Ok(r) if !r.name.is_empty() && !r.started.is_empty() => r,
                _ => {
                    bad += 1;
                    continue;
                }
            };
            if now - r.seen > KEEP_DAYS * 86_400 {
                continue;
            }
            if let Some(&i) = by_id.get(&r.id()) {
                sight(&mut sightings[i], &r);
                ops[i].states += 1;
                ops[i].last = r;
            } else {
                by_id.insert(r.id(), ops.len());
                let mut seen = Sightings::new();
                sight(&mut seen, &r);
                sightings.push(seen);
                ops.push(Operation { last: r, states: 1, seen: Vec::new() });
            }
        }
    }
    for (op, seen) in ops.iter_mut().zip(&sightings) {
        op.seen = op
            .last
            .resources
            .iter()
            .map(|s| seen.get(&s.key()).map_or_default(|&(first, _, settled)| Seen { first, settled }))
            .collect();
    }
    // Newest first: by when it started, else when it was seen.
    ops.sort_by_key(|o| {
        std::cmp::Reverse((o.last.started_at().map_or(o.last.seen, jiff::Timestamp::as_second), o.last.seen))
    });
    Loaded { ops, bad }
}

/// The journal at `path` (with its `.1`), as of `now`. A file that can't be read is an error,
/// said; one that isn't there is an empty journal.
pub fn read(path: &Path, now: i64) -> Result<Loaded, String> {
    let mut texts = Vec::new();
    for p in [older(path), path.to_path_buf()] {
        match std::fs::read(&p) {
            Ok(bytes) => texts.push(String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", p.display())),
        }
    }
    let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
    Ok(parse(&refs, now))
}

/// The app's journal now.
pub fn load() -> Result<Loaded, String> {
    read(&path(), jiff::Timestamp::now().as_second())
}

/// What the Journal view keeps.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    /// Words the Application's name, namespace or Argo CD must all have, in lowercase.
    pub app: String,
    /// Only this phase (`Succeeded`, `Failed`…).
    pub phase: Option<String>,
    /// Only those started by this one.
    pub who: Option<String>,
    /// Only those started within this many seconds.
    pub since: Option<i64>,
}

impl Filter {
    pub fn keeps(&self, op: &Operation, now: i64) -> bool {
        let r = &op.last;
        let haystack = format!("{} {} {}", r.name, r.namespace, r.instance).to_lowercase();
        self.app.split_whitespace().all(|w| haystack.contains(w))
            && self.phase.as_ref().is_none_or(|p| *p == r.phase)
            && self.who.as_ref().is_none_or(|w| r.by.as_ref() == Some(w))
            && self.since.is_none_or(|s| r.started_at().map_or(r.seen, jiff::Timestamp::as_second) >= now - s)
    }
}

/// The phases and the people of `ops`, each once, sorted: what the filters offer.
pub fn choices(ops: &[Operation]) -> (Vec<String>, Vec<String>) {
    let mut phases: Vec<String> = ops.iter().map(|o| o.last.phase.clone()).collect();
    let mut who: Vec<String> = ops.iter().filter_map(|o| o.last.by.clone()).collect();
    for v in [&mut phases, &mut who] {
        v.sort();
        v.dedup();
    }
    (phases, who)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    fn app(phase: &str, started: &str, resources: Value) -> Value {
        json!({
            "metadata": {"name": "web", "namespace": "argocd"},
            "status": {"operationState": {
                "phase": phase, "message": "successfully synced", "startedAt": started,
                "finishedAt": if phase == "Running" { Value::Null } else { json!("2026-01-01T00:01:00Z") },
                "operation": {"initiatedBy": {"username": "ana"}, "sync": {"dryRun": false, "syncStrategy": {"apply": {}}}},
                "syncResult": {"revision": "abc123", "resources": resources},
            }}
        })
    }

    fn res(name: &str, status: &str) -> Value {
        json!({"group": "apps", "kind": "Deployment", "namespace": "shop", "name": name, "status": status,
            "message": "deployment.apps/web configured"})
    }

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("recouvrance-journal-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn an_operation_reads_with_its_steps() {
        let mut a = app("Succeeded", "2026-01-01T00:00:00Z", json!([res("web", "Synced")]));
        a["status"]["operationState"]["syncResult"]["resources"]
            .as_array_mut()
            .unwrap()
            .push(json!({"kind": "Job", "namespace": "shop", "name": "migrate", "hookPhase": "Succeeded", "hookType": "PreSync"}));
        let r = record_of("argocd.example.com", &a, 5).unwrap();
        assert_eq!((r.name.as_str(), r.namespace.as_str(), r.phase.as_str()), ("web", "argocd", "Succeeded"));
        assert_eq!(r.by.as_deref(), Some("ana"));
        assert_eq!(r.revisions, ["abc123"]);
        assert_eq!(r.strategy.as_deref(), Some("apply"));
        assert_eq!(r.resources.len(), 2);
        assert_eq!(r.resources[0].label(), "Deployment shop/web");
        assert_eq!((r.resources[1].result(), r.resources[1].hook_type.as_str()), ("Succeeded", "PreSync"));
        // Never synced, or no start: nothing to keep.
        assert!(record_of("x", &json!({"metadata": {"name": "web"}}), 0).is_none());
        let mut no_start = a.clone();
        no_start["status"]["operationState"]["startedAt"] = Value::Null;
        assert!(record_of("x", &no_start, 0).is_none());
        // Auto-sync starts some: said so.
        let mut auto = a;
        auto["status"]["operationState"]["operation"]["initiatedBy"] = json!({"automated": true});
        assert_eq!(record_of("x", &auto, 0).unwrap().by.as_deref(), Some("autosync"));
    }

    /// Each state of an operation is written once, however often it is seen, and the journal
    /// knows what it holds after a restart; the file is the user's alone.
    #[test]
    fn states_are_written_once_each() {
        let d = dir();
        let path = d.join("data").join("journal.jsonl");
        let mut j = Journal::open(path.clone(), 0);
        let running = record_of("i", &app("Running", "2026-01-01T00:00:00Z", json!([res("web", "")])), 10).unwrap();
        let done =
            record_of("i", &app("Succeeded", "2026-01-01T00:00:00Z", json!([res("web", "Synced")])), 20).unwrap();
        assert!(j.append(&running).unwrap());
        assert!(!j.append(&Record { seen: 11, ..running.clone() }).unwrap(), "the same state, seen later");
        assert!(j.append(&done).unwrap());
        let mut again = Journal::open(path.clone(), 30);
        assert!(!again.append(&Record { seen: 40, ..done.clone() }).unwrap(), "known after a restart");
        let next = record_of("i", &app("Running", "2026-01-02T00:00:00Z", json!([])), 50).unwrap();
        assert!(again.append(&next).unwrap(), "another operation of the same Application");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3);
        let mode = crate::private_files::mode;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(&d.join("data")), 0o700);
        let loaded = read(&path, 60).unwrap();
        assert_eq!(loaded.ops.len(), 2);
        assert_eq!(loaded.ops[0].last.started, "2026-01-02T00:00:00Z", "newest first");
        assert_eq!((loaded.ops[1].last.phase.as_str(), loaded.ops[1].states), ("Succeeded", 2));
        std::fs::remove_dir_all(d).unwrap();
    }

    /// An operation of many resources with long messages fits one line: the steps' messages go
    /// first, then the last steps.
    #[test]
    fn a_huge_operation_is_cut_to_one_line() {
        let long = "x".repeat(MESSAGE_CHARS);
        let many: Vec<Value> = (0..MAX_RESOURCES)
            .map(|i| json!({"kind": "ConfigMap", "namespace": "shop", "name": format!("c{i}"), "status": "Synced", "message": long}))
            .collect();
        let r = record_of("i", &app("Succeeded", "2026-01-01T00:00:00Z", json!(many)), 1).unwrap();
        let line = line_of(&r).unwrap();
        assert!(line.len() <= MAX_LINE, "{}", line.len());
        let back: Record = serde_json::from_str(&line).unwrap();
        assert!(back.resources.iter().all(|s| s.message.is_empty()));
        assert_eq!(back.resources.len(), MAX_RESOURCES, "names alone fit");
        let small =
            record_of("i", &app("Succeeded", "2026-01-01T00:00:00Z", json!([res("web", "Synced")])), 1).unwrap();
        assert_eq!(line_of(&small).unwrap(), serde_json::to_string(&small).unwrap());
    }

    /// The cheap print moves with the state: a step that ends changes it, the same state
    /// read again doesn't.
    #[test]
    fn the_quick_print_follows_the_state() {
        let running = app("Running", "2026-01-01T00:00:00Z", json!([res("web", "")]));
        let (id, a) = quick_print("i", &running).unwrap();
        assert_eq!(id, ("i".to_string(), "argocd".to_string(), "web".to_string(), "2026-01-01T00:00:00Z".to_string()));
        assert_eq!(quick_print("i", &running).unwrap().1, a);
        let synced = app("Running", "2026-01-01T00:00:00Z", json!([res("web", "Synced")]));
        assert_ne!(quick_print("i", &synced).unwrap().1, a);
        assert!(quick_print("i", &json!({"metadata": {"name": "web"}})).is_none());
    }

    /// Each step of an operation read from several states says when it was first seen and
    /// when it came to its result; its phase is kept.
    #[test]
    fn steps_say_when_they_were_seen() {
        let job = |phase: &str| {
            json!([{"group": "batch", "kind": "Job", "namespace": "shop", "name": "migrate", "hookType": "PreSync",
                "hookPhase": phase, "syncPhase": "PreSync"}, res("web", "")])
        };
        let lines: Vec<String> = [(10, "Running"), (40, "Running"), (70, "Failed")]
            .iter()
            .map(|&(seen, phase)| {
                let r = record_of("i", &app("Running", "2026-01-01T00:00:00Z", job(phase)), seen).unwrap();
                serde_json::to_string(&r).unwrap()
            })
            .collect();
        let loaded = parse(&[&lines.join("\n")], 100);
        let op = &loaded.ops[0];
        assert_eq!(op.last.resources[0].phase(), "PreSync");
        assert_eq!(op.seen, [Seen { first: 10, settled: 70 }, Seen { first: 10, settled: 10 }]);
    }

    #[test]
    fn a_full_journal_moves_aside_and_both_are_read() {
        let d = dir();
        let path = d.join("journal.jsonl");
        let mut j = Journal::open(path.clone(), 0);
        let first = record_of("i", &app("Succeeded", "2026-01-01T00:00:00Z", json!([])), 1).unwrap();
        j.append(&first).unwrap();
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(MAX_BYTES).unwrap();
        let second = record_of("i", &app("Failed", "2026-01-02T00:00:00Z", json!([])), 2).unwrap();
        j.append(&second).unwrap();
        assert_eq!(std::fs::metadata(older(&path)).unwrap().len(), MAX_BYTES);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 1);
        // The older file's padding is not JSON: counted, the rest read.
        let loaded = read(&path, 3).unwrap();
        assert_eq!(loaded.ops.len(), 2);
        assert!(loaded.bad >= 1);
        // A `.1` older than the days kept goes when the journal opens.
        let _ = Journal::open(path.clone(), jiff::Timestamp::now().as_second() + (KEEP_DAYS + 1) * 86_400);
        assert!(!older(&path).exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn bad_lines_are_skipped_and_counted_and_old_ones_left_out() {
        let ok =
            serde_json::to_string(&record_of("i", &app("Succeeded", "2026-01-01T00:00:00Z", json!([])), 100).unwrap())
                .unwrap();
        let old: Record = serde_json::from_str(&ok).unwrap();
        let old = Record { seen: 100 - KEEP_DAYS * 86_400, started: "2025-01-01T00:00:00Z".into(), ..old };
        let old = serde_json::to_string(&old).unwrap();
        let text = format!("{ok}\nnot json\n{{\"seen\": \"x\"}}\n\n{{}}\n{old}\n{ok}");
        let loaded = parse(&[&text], 100 + 86_400);
        assert_eq!(loaded.bad, 3, "not JSON, a wrong type, no Application");
        assert_eq!(loaded.ops.len(), 1, "the old one left out, the same one twice is one");
        assert_eq!(loaded.ops[0].states, 2);
        // A journal that isn't there is empty; one that can't be read says why.
        assert_eq!(read(Path::new("/nonexistent/recouvrance/journal.jsonl"), 0), Ok(Loaded::default()));
    }

    #[test]
    fn filters_keep_what_they_say() {
        let mk = |name: &str, phase: &str, by: &str, started: &str| Operation {
            last: Record {
                instance: "argocd.example.com".into(),
                namespace: "argocd".into(),
                name: name.into(),
                started: started.into(),
                phase: phase.into(),
                by: Some(by.into()),
                ..Default::default()
            },
            states: 1,
            seen: Vec::new(),
        };
        let ops = [
            mk("web-prod", "Succeeded", "ana", "2026-01-10T00:00:00Z"),
            mk("web-staging", "Failed", "autosync", "2026-01-09T00:00:00Z"),
            mk("api-prod", "Failed", "ana", "2026-01-01T00:00:00Z"),
        ];
        let now = "2026-01-10T12:00:00Z".parse::<jiff::Timestamp>().unwrap().as_second();
        let kept =
            |f: &Filter| ops.iter().filter(|o| f.keeps(o, now)).map(|o| o.last.name.as_str()).collect::<Vec<_>>();
        assert_eq!(kept(&Filter::default()).len(), 3);
        assert_eq!(kept(&Filter { app: "web".into(), ..Default::default() }), ["web-prod", "web-staging"]);
        assert_eq!(kept(&Filter { app: "web prod".into(), ..Default::default() }), ["web-prod"]);
        assert_eq!(kept(&Filter { phase: Some("Failed".into()), ..Default::default() }), ["web-staging", "api-prod"]);
        assert_eq!(kept(&Filter { who: Some("ana".into()), ..Default::default() }), ["web-prod", "api-prod"]);
        assert_eq!(kept(&Filter { since: Some(2 * 86_400), ..Default::default() }), ["web-prod", "web-staging"]);
        let (phases, who) = choices(&ops);
        assert_eq!(
            (phases, who),
            (vec!["Failed".to_string(), "Succeeded".into()], vec!["ana".to_string(), "autosync".into()])
        );
    }

    proptest! {
        /// However lines are broken, whatever they hold, reading never fails: each line that
        /// doesn't read is counted, and every operation that does is there once.
        #[test]
        fn reading_never_fails_on_bad_lines(junk in proptest::collection::vec("[ -~]{0,40}", 0..20), good in 0usize..5) {
            let mut lines: Vec<String> = junk.clone();
            for i in 0..good {
                let started = format!("2026-01-0{}T00:00:00Z", i + 1);
                let r = record_of("i", &app("Succeeded", &started, json!([])), 10).unwrap();
                lines.push(serde_json::to_string(&r).unwrap());
            }
            let text = lines.join("\n");
            let loaded = parse(&[&text], 20);
            prop_assert_eq!(loaded.ops.len(), good);
            let blank = junk.iter().filter(|l| l.trim().is_empty()).count();
            prop_assert!(loaded.bad <= junk.len() - blank);
        }
    }
}
