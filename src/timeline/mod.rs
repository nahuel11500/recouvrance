//! The timeline: changes to objects recorded while the app watches them. What Kubernetes
//! keeps no history of (ConfigMaps, which keys of a Secret, Services, Ingresses, HPAs, scale
//! changes, custom resources) is only known if someone saw it change.
//!
//! Records come from what the app already reads: the lists' watches (their metadata, see
//! `k8s::watch_table_paused`) and the Problems scan's lists, through a [`Tap`]. No request
//! is added, unless the settings name kinds to watch whole (`timeline.kinds`): those get a
//! watch of their own while connected, and their records a diff of the whole object.
//!
//! The work stays off the UI thread: taps run in the watches' tasks, files are written by
//! one thread in batches (see `store`), and a record's diff is only worked out when shown.

mod observe;
pub mod store;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

use kube::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::history::{Change, Diff, Source};
use crate::k8s::{self, Kind, TaskGuard};
use observe::{How, KindInfo, KindState, Observed};
use store::{Limits, Log, Pushed};

/// What happened to the object.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum What {
    Created,
    Updated,
    Deleted,
}

/// One change recorded, a line of a context's file.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Record {
    /// When it changed: the writer's time from `managedFields`, the object's creation, or
    /// when it was seen.
    pub time: jiff::Timestamp,
    /// kubectl's name for its kind: `configmaps`, `deployments.apps`.
    pub resource: String,
    /// `ConfigMap`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub name: String,
    pub uid: String,
    pub what: What,
    /// What changed, and who changed it.
    pub summary: String,
    /// The versions compared (status and noise left out, a Secret's values never), as JSON:
    /// parsed only when the diff is shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<Box<RawValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<Box<RawValue>>,
}

impl Record {
    /// The two versions, when the record kept either.
    pub fn diff(&self) -> Option<Diff> {
        if self.before.is_none() && self.after.is_none() {
            return None;
        }
        let parse = |r: &Option<Box<RawValue>>| {
            r.as_ref().and_then(|r| serde_json::from_str::<Value>(r.get()).ok()).unwrap_or(Value::Null)
        };
        Some(Diff { before: parse(&self.before), after: parse(&self.after) })
    }

    /// As an entry of an object's history.
    pub fn change(&self) -> Change {
        Change {
            time: Some(self.time),
            source: Source::Recorded,
            object: format!("{} {}", self.kind, self.name),
            summary: self.summary.clone(),
            warning: false,
            diff: self.diff().map(Arc::new),
        }
    }
}

/// Kinds never recorded: they change all the time, and other pages tell their story (a
/// pod's is its owner's revisions and its events, a ReplicaSet's its Deployment's).
const NOT_RECORDED: [(&str, &str); 9] = [
    ("", "Pod"),
    ("", "Event"),
    ("events.k8s.io", "Event"),
    ("", "Endpoints"),
    ("discovery.k8s.io", "EndpointSlice"),
    ("coordination.k8s.io", "Lease"),
    ("apps", "ControllerRevision"),
    ("apps", "ReplicaSet"),
    ("", "ComponentStatus"),
];

/// Whether changes to this kind are recorded.
pub fn recorded(kind: &Kind) -> bool {
    kind.ar.group != "metrics.k8s.io" && !NOT_RECORDED.iter().any(|(g, k)| kind.is(g, k))
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Writes waiting up to this long go to disk together.
const BATCH: Duration = Duration::from_millis(500);

enum Write {
    Append(Arc<str>, String),
    Rewrite(Arc<str>, Vec<Arc<Record>>),
}

/// Every context's records and what is known of the objects watched.
pub struct Recorder {
    /// Where the files go; None keeps everything in memory (tests).
    dir: Option<PathBuf>,
    /// The records of each context read so far.
    logs: Mutex<HashMap<Arc<str>, Log>>,
    /// Per context and kind (`Kind::key`).
    kinds: Mutex<HashMap<(Arc<str>, String), Arc<Mutex<KindState>>>>,
    writer: Mutex<Option<mpsc::Sender<Write>>>,
    /// Counts records, for pages to see something new came.
    version: AtomicU64,
    /// Files kept aside and writes that failed, to show on the Timeline page.
    problems: Arc<Mutex<Vec<String>>>,
}

static RECORDER: LazyLock<Recorder> = LazyLock::new(|| Recorder::new(Some(crate::paths::data_dir().join("timeline"))));

impl Recorder {
    fn new(dir: Option<PathBuf>) -> Self {
        Self {
            dir,
            logs: Mutex::default(),
            kinds: Mutex::default(),
            writer: Mutex::new(None),
            version: AtomicU64::new(0),
            problems: Arc::default(),
        }
    }

    /// A tap on one kind's lists or watch in `scope` (a namespace, or all).
    fn tap(&'static self, context: &str, kind: &Kind, scope: Option<String>, full: bool) -> Tap {
        let ctx: Arc<str> = context.into();
        let state = lock(&self.kinds)
            .entry((ctx.clone(), kind.key()))
            .or_insert_with(|| Arc::new(Mutex::new(KindState::new(jiff::Timestamp::now()))))
            .clone();
        if full {
            lock(&state).full_taps += 1;
        }
        let info =
            KindInfo { resource: kind.kubectl_name(), kind: kind.ar.kind.clone(), secret: kind.is("", "Secret") };
        let scope = if kind.namespaced { scope } else { None };
        Tap { rec: self, ctx, info: Arc::new(info), state, scope, full, listing: Mutex::new(None) }
    }

    /// Read a context's file, once. The file is read without holding the lock: pages
    /// reading the records meanwhile don't wait for it.
    fn load(&self, ctx: &Arc<str>) {
        if lock(&self.logs).contains_key(ctx) {
            return;
        }
        let Some(dir) = &self.dir else {
            lock(&self.logs).entry(ctx.clone()).or_default();
            return;
        };
        let loaded =
            store::load(&dir.join(store::file_name(ctx)), ctx, Limits::from_settings(), jiff::Timestamp::now());
        if let Some(e) = loaded.set_aside {
            lock(&self.problems).push(e);
        }
        let mut logs = lock(&self.logs);
        if logs.contains_key(ctx) {
            return;
        }
        if loaded.rewrite {
            self.write(Write::Rewrite(ctx.clone(), loaded.log.records.iter().cloned().collect()));
        }
        logs.insert(ctx.clone(), loaded.log);
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    fn add(&self, ctx: &Arc<str>, r: Record) {
        self.load(ctx);
        let Some(line) = store::line(&r) else { return };
        let limits = Limits::from_settings();
        let mut logs = lock(&self.logs);
        let log = logs.entry(ctx.clone()).or_default();
        // Sent under the lock: the writer gets them in the order the log has them.
        match log.push(Arc::new(r), line.len(), limits, jiff::Timestamp::now()) {
            Pushed::Added => self.write(Write::Append(ctx.clone(), line)),
            Pushed::Trimmed => self.write(Write::Rewrite(ctx.clone(), log.records.iter().cloned().collect())),
            Pushed::TooOld => return,
        }
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    fn write(&self, w: Write) {
        let Some(dir) = &self.dir else { return };
        let mut writer = lock(&self.writer);
        let tx = writer.get_or_insert_with(|| {
            let (tx, rx) = mpsc::channel();
            let (dir, problems) = (dir.clone(), self.problems.clone());
            let spawned = std::thread::Builder::new()
                .name("timeline-writer".into())
                .spawn(move || write_files(&dir, &rx, &problems));
            if let Err(e) = spawned {
                eprintln!("timeline: no writer thread: {e}");
            }
            tx
        });
        let _ = tx.send(w);
    }

    fn records_of(&self, ctx: &str) -> Vec<Arc<Record>> {
        lock(&self.logs).get(ctx).map(|l| l.records.iter().cloned().collect()).unwrap_or_default()
    }
}

/// The writer thread: batches of writes, at most one file operation per context each.
fn write_files(dir: &std::path::Path, rx: &mpsc::Receiver<Write>, problems: &Mutex<Vec<String>>) {
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        let until = Instant::now() + BATCH;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match rx.recv_timeout(left) {
                Ok(w) => batch.push(w),
                Err(_) => break,
            }
        }
        // Per context: the last rewrite in the batch, then the lines appended after it.
        let mut files: HashMap<Arc<str>, (Option<Vec<Arc<Record>>>, String)> = HashMap::new();
        for w in batch {
            match w {
                Write::Append(ctx, line) => files.entry(ctx).or_default().1.push_str(&line),
                Write::Rewrite(ctx, records) => {
                    let file = files.entry(ctx).or_default();
                    file.0 = Some(records);
                    file.1.clear();
                }
            }
        }
        for (ctx, (whole, lines)) in files {
            let path = dir.join(store::file_name(&ctx));
            let written = match whole {
                Some(records) => store::rewrite(&path, &ctx, &records)
                    .and_then(|()| if lines.is_empty() { Ok(()) } else { store::append(&path, &ctx, &lines) }),
                None => store::append(&path, &ctx, &lines),
            };
            if let Err(e) = written {
                let msg = format!("{}: {e}", path.display());
                eprintln!("timeline: {msg}");
                let mut problems = lock(problems);
                if !problems.contains(&msg) {
                    problems.push(msg);
                }
            }
        }
    }
}

/// A list being read through a tap: what it had, and when it started.
struct Listing {
    started: Instant,
    uids: HashSet<String>,
}

/// Records the changes a list or watch of one kind sees. It runs in the watch's task; the
/// lock it takes is its kind's, held only to compare.
pub struct Tap {
    rec: &'static Recorder,
    ctx: Arc<str>,
    info: Arc<KindInfo>,
    state: Arc<Mutex<KindState>>,
    /// The namespace its lists cover (None: all, or a cluster-scoped kind).
    scope: Option<String>,
    /// Watching whole objects (`timeline.kinds`): diffs of the whole object.
    full: bool,
    listing: Mutex<Option<Listing>>,
}

impl Drop for Tap {
    fn drop(&mut self) {
        if self.full {
            lock(&self.state).full_taps -= 1;
        }
    }
}

impl Tap {
    /// Whether its watch reads whole objects.
    pub fn full(&self) -> bool {
        self.full
    }

    /// A list starts: objects it doesn't have were deleted (see `end_list`).
    pub fn begin_list(&self) {
        *lock(&self.listing) = Some(Listing { started: Instant::now(), uids: HashSet::new() });
    }

    /// An object of the list being read.
    pub fn listed(&self, obj: &RawValue) {
        self.observe(How::Listed, obj);
    }

    /// A watch event (`ADDED`, `MODIFIED`, `DELETED`) for an object.
    pub fn event(&self, kind: &str, obj: &RawValue) {
        let how = match kind {
            "ADDED" => How::Added,
            "MODIFIED" => How::Modified,
            "DELETED" => How::Deleted,
            _ => return,
        };
        self.observe(how, obj);
    }

    /// The list is complete: what was seen in its scope before it started and isn't in it
    /// was deleted meanwhile (while no watch was running).
    pub fn end_list(&self) {
        let listing = lock(&self.listing).take();
        let Some(listing) = listing else { return };
        let mut state = lock(&self.state);
        if !self.full && state.full_taps > 0 {
            return;
        }
        let gone: Vec<String> = (state.objects.iter())
            .filter(|(uid, s)| {
                s.in_scope(self.scope.as_deref()) && s.at < listing.started && !listing.uids.contains(*uid)
            })
            .map(|(uid, _)| uid.clone())
            .collect();
        let now = jiff::Timestamp::now();
        let records: Vec<Record> = gone
            .into_iter()
            .filter_map(|uid| {
                let seen = state.remove(&uid)?;
                observe::deleted(&self.info, uid, seen, now, "deleted (gone when listed again)")
            })
            .collect();
        drop(state);
        for r in records {
            self.rec.add(&self.ctx, r);
        }
    }

    fn observe(&self, how: How, obj: &RawValue) {
        let Ok(head) = serde_json::from_str::<observe::Head<'_>>(obj.get()) else { return };
        let Some(uid) = head.metadata.uid.clone() else { return };
        let at = Instant::now();
        if how == How::Listed {
            let mut listing = lock(&self.listing);
            if let Some(l) = listing.as_mut() {
                l.uids.insert(uid.clone());
            }
        }
        {
            let mut state = lock(&self.state);
            // Whole objects are recorded by their own watch.
            if !self.full && state.full_taps > 0 {
                return;
            }
            // Seen at this version already (another list or watch of the kind): nothing to
            // compare, and no object to parse.
            let rv = observe::rv_number(head.metadata.resource_version.as_deref());
            if how != How::Deleted
                && let Some(seen) = state.objects.get_mut(&uid)
                && rv.is_some()
                && seen.rv() >= rv
            {
                seen.at = at;
                return;
            }
        }
        let whole = if self.full { serde_json::from_str::<Value>(obj.get()).ok() } else { None };
        let Some(obs) = Observed::new(head, whole, &self.info) else { return };
        let record = {
            let mut state = lock(&self.state);
            observe::observe(&mut state, &self.info, self.full, obs, how, jiff::Timestamp::now(), at)
        };
        if let Some(r) = record {
            self.rec.add(&self.ctx, r);
        }
    }
}

// MARK: For the app

/// A tap for a list or watch of `kind` in `scope` on `context`, reading metadata; None when
/// recording is off or the kind is not recorded.
pub fn tap(context: &str, kind: &Kind, scope: Option<String>) -> Option<Tap> {
    let on = crate::settings::with(|s| s.timeline.record);
    (on && recorded(kind)).then(|| RECORDER.tap(context, kind, scope, false))
}

/// Read a context's records in the background, if not read yet.
pub fn open(context: &str) {
    let ctx: Arc<str> = context.into();
    let spawned = std::thread::Builder::new().name("timeline-load".into()).spawn(move || RECORDER.load(&ctx));
    if let Err(e) = spawned {
        eprintln!("timeline: {e}");
    }
}

/// Read a context's records, if not read yet, off the async runtime (for agents).
pub async fn opened(context: &str) {
    let ctx: Arc<str> = context.into();
    let _ = k8s::run(async move { tokio::task::spawn_blocking(move || RECORDER.load(&ctx)).await.map_err(Into::into) })
        .await;
}

/// Changes recorded on an object of `context`, as history entries. By uid: an object
/// re-created under the name is another one (an empty uid matches by name only).
pub fn changes_for(context: &str, resource: &str, namespace: Option<&str>, name: &str, uid: &str) -> Vec<Change> {
    let logs = lock(&RECORDER.logs);
    let Some(log) = logs.get(context) else { return Vec::new() };
    log.records
        .iter()
        .filter(|r| {
            r.resource == resource
                && r.name == name
                && r.namespace.as_deref() == namespace
                && (uid.is_empty() || r.uid == uid)
        })
        .map(|r| r.change())
        .collect()
}

/// Every record of the contexts read so far, by context.
pub fn all() -> Vec<(Arc<str>, Arc<Record>)> {
    let contexts: Vec<Arc<str>> = lock(&RECORDER.logs).keys().cloned().collect();
    contexts.into_iter().flat_map(|c| RECORDER.records_of(&c).into_iter().map(move |r| (c.clone(), r))).collect()
}

/// Counts records added and contexts read: what a page shows is stale once it moved.
pub fn version() -> u64 {
    RECORDER.version.load(Ordering::Relaxed)
}

/// Files kept aside and writes that failed.
pub fn problems() -> Vec<String> {
    lock(&RECORDER.problems).clone()
}

/// The watches of whole objects the settings ask for (`timeline.kinds`), on each cluster
/// that serves the kind, until the guards are dropped. Their lists aren't shown: they
/// only feed the recorder. A kind named that isn't found (or never recorded) is said on
/// the Timeline page.
pub fn start_watches(
    clusters: &[(String, Client)],
    kinds: &[Kind],
    kind_clusters: &HashMap<String, Vec<usize>>,
) -> Vec<TaskGuard> {
    let (on, names) = crate::settings::with(|s| (s.timeline.record, s.timeline.kinds.clone()));
    let mut guards = Vec::new();
    if !on {
        return guards;
    }
    for name in names {
        let Some(kind) = k8s::find_kind(kinds, &name).filter(|k| recorded(k)).cloned() else {
            let msg = format!("timeline.kinds: no kind {name:?} to record on the connected clusters");
            let mut problems = lock(&RECORDER.problems);
            if !problems.contains(&msg) {
                problems.push(msg);
            }
            continue;
        };
        let serving = kind_clusters.get(&kind.key()).cloned().unwrap_or_default();
        for (ix, (ctx, client)) in clusters.iter().enumerate() {
            if !serving.contains(&ix) {
                continue;
            }
            // Where the user may not list cluster-wide: each namespace set for the cluster.
            let accessible = crate::settings::cluster(ctx).namespaces;
            let scopes: Vec<Option<String>> = if kind.namespaced && !accessible.is_empty() {
                accessible.into_iter().map(Some).collect()
            } else {
                vec![None]
            };
            for ns in scopes {
                let tap = RECORDER.tap(ctx, &kind, ns.clone(), true);
                // Held back for good: events apply, no rows are ever sent.
                let pause = k8s::Pause::default();
                pause.set(true);
                let (guard, mut rx) =
                    k8s::watch_table_paused(client.clone(), kind.clone(), ns, Arc::default(), pause, false, Some(tap));
                let drain = k8s::spawn_guarded(async move {
                    use futures::StreamExt as _;
                    while rx.next().await.is_some() {}
                });
                guards.push(guard);
                guards.push(drain);
            }
        }
    }
    guards
}

#[cfg(test)]
mod tests;
