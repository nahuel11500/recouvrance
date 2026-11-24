//! A sync of several Applications at once, each the way it was read: on its hub through the
//! Kubernetes API (`k8s::argo_start`, with the hub's window client: the write gate applies),
//! or through Argo CD's API (`api::act`, which checks write mode as it sends).
//!
//! Each Application is read first ([`check`]): one whose sync window denies a manual sync now,
//! one already syncing, or one being deleted is left out, saying why; what prune would delete
//! on each is listed from its managed resources (`requiresPruning`). The syncs are sent at
//! most [`PARALLEL`] at a time; each Application is then read again until its operation ends,
//! and its progress told from `status.operationState` ([`progress`]).
//!
//! Options: with hooks (Argo CD's hook strategy, its default) or without (the apply strategy:
//! the manifests are applied, PreSync/Sync/PostSync hooks such as a secret refresh or a
//! migration are skipped), prune (off by default; never deleting more than was shown), and
//! dry run (Argo CD applies nothing and says what it would do).

use std::future::Future;
use std::pin::Pin;

use futures::StreamExt as _;
use serde_json::Value;

use super::diagnosis::Res;
use super::ops::{self, Op, SyncRequest};
use super::windows::{HORIZON_DAYS, NextSync};
use super::{Source, api};

/// Syncs sent, and Applications read, at the same time, at most.
pub const PARALLEL: usize = 4;

/// What the sync of the picked Applications asks of each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Run the hooks (the hook strategy); off: the apply strategy, without hooks.
    pub hooks: bool,
    pub prune: bool,
    pub dry_run: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { hooks: true, prune: false, dry_run: false }
    }
}

impl Options {
    /// The sync of one whole Application, `pruned` being what prune deletes on it as shown:
    /// if git changed since and it would delete more, the sync of it is refused.
    pub fn request(&self, pruned: &[Res]) -> SyncRequest {
        SyncRequest {
            resources: Vec::new(),
            prune: self.prune,
            dry_run: self.dry_run,
            apply: !self.hooks,
            prune_shown: self.prune.then(|| pruned.to_vec()),
        }
    }

    /// `with hooks, prune, dry run`: what the confirmation says.
    pub fn text(&self) -> String {
        let mut parts = vec![if self.hooks { "with hooks" } else { "without hooks" }];
        if self.prune {
            parts.push("prune");
        }
        if self.dry_run {
            parts.push("dry run");
        }
        parts.join(", ")
    }
}

/// What the read of one Application before the sync found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checked {
    /// Why it is left out of the sync, when it is.
    pub refused: Option<String>,
    /// What prune would delete on it (when prune is on).
    pub pruned: Vec<Res>,
    pub auto_sync: bool,
    /// When its last operation started: the sync sent starts another.
    pub before: Option<String>,
}

/// Why a manual sync can't start now under the sync windows that apply (`next`, from the
/// Application's project); None when it can, or when the windows aren't known (Argo CD
/// decides then, and says so if it refuses).
pub fn window_refusal(next: &NextSync, now: jiff::Timestamp) -> Option<String> {
    match next {
        NextSync::NoWindow | NextSync::Now | NextSync::Unknown(_) => None,
        NextSync::At(_) => Some(format!("a sync window denies a sync now: it can sync {}", next.text(now))),
        NextSync::NotSoon => Some(format!("a sync window denies a sync now, and for the next {HORIZON_DAYS} days")),
    }
}

/// Check the Application `app`, as just read, for the sync: left out when it is already
/// syncing, being deleted, or a sync window (`window`) denies it now.
pub fn check(app: &Value, window: &NextSync, now: jiff::Timestamp) -> Checked {
    let refused = if ops::in_progress(app) {
        Some("a sync is already running: wait for it to end, or terminate it".to_string())
    } else {
        ops::refusal(app, &Op::Sync(SyncRequest::default())).or_else(|| window_refusal(window, now))
    };
    Checked {
        refused,
        pruned: ops::pruned(app, &[]),
        auto_sync: super::auto_sync(app),
        before: app.pointer("/status/operationState/startedAt").and_then(Value::as_str).map(String::from),
    }
}

/// An Application to sync: where it is read, its Argo CD, its namespace and name.
#[derive(Clone)]
pub struct Target {
    pub source: Source,
    pub instance: String,
    pub namespace: String,
    pub name: String,
}

/// A request of a grouped sync to one Application.
pub type Call<T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send>>;

impl Target {
    /// The Application as Argo CD has it now, whole; its operation goes to the sync journal.
    pub fn read(&self) -> Call<Value> {
        let (t, instance) = (self.clone(), self.instance.clone());
        Box::pin(async move {
            let app = match &t.source {
                Source::Hub(hub) => {
                    let obj = crate::k8s::ObjectRef {
                        kind: super::application_kind(),
                        name: t.name.clone(),
                        namespace: Some(t.namespace.clone()),
                        uid: String::new(),
                    };
                    crate::k8s::get_object(hub.client.clone(), obj).await.map_err(|e| crate::k8s::short_error(&e))?
                }
                Source::Api(endpoint) => api::read_app(endpoint.clone(), t.namespace.clone(), t.name.clone()).await?,
            };
            super::journal::observe(&instance, &app);
            Ok(app)
        })
    }

    /// Send `op` (a sync or a terminate): on a hub with its window client (the write gate
    /// refuses it while write mode is off), by Argo CD's API with write mode checked as it is
    /// sent. A dry run's result is not waited for: it is followed like any sync.
    pub fn send(&self, op: Op) -> Call<String> {
        let (ns, name) = (self.namespace.clone(), self.name.clone());
        match &self.source {
            Source::Hub(hub) => {
                let client = hub.client.clone();
                Box::pin(async move {
                    crate::k8s::argo_start(client, super::application_kind(), Some(ns), name, op)
                        .await
                        .map_err(|e| crate::k8s::short_error(&e))
                })
            }
            Source::Api(endpoint) => {
                let action = match op {
                    Op::Sync(req) => api::Action::Sync(req),
                    Op::Terminate => api::Action::Terminate,
                    Op::Rollback { .. } => return Box::pin(async { Err("not part of a grouped sync".to_string()) }),
                };
                Box::pin(api::act(endpoint.clone(), ns, name, action))
            }
        }
    }
}

/// `work` for each of `0..n`, at most `parallel` at a time, each result as it ends, with its
/// index.
pub fn bounded<F, Fut, T>(n: usize, parallel: usize, mut work: F) -> impl futures::Stream<Item = (usize, T)>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = T>,
{
    futures::stream::iter(0..n)
        .map(move |i| {
            let f = work(i);
            async move { (i, f.await) }
        })
        .buffer_unordered(parallel.max(1))
}

/// Send each sync of `jobs`, at most [`PARALLEL`] at a time: each result as it comes, with
/// the index of its job.
pub fn send_all(jobs: Vec<(Target, SyncRequest)>) -> impl futures::Stream<Item = (usize, Result<String, String>)> {
    bounded(jobs.len(), PARALLEL, move |i| {
        let (target, req) = &jobs[i];
        target.send(Op::Sync(req.clone()))
    })
}

/// How the sync of one Application is going, from its `status.operationState`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// Argo CD's phase; `Waiting` until its controller starts the operation sent.
    pub phase: String,
    pub message: Option<String>,
    /// Resources and hooks done, of those in its result so far.
    pub done: usize,
    pub total: usize,
    pub ended: bool,
    pub succeeded: bool,
}

/// The progress of the sync sent to the Application `app` (as read now), whose operation
/// before it started at `before`: until a newer one starts, the one sent waits for Argo CD's
/// controller.
pub fn progress(app: &Value, before: Option<&str>) -> Progress {
    let started = app.pointer("/status/operationState/startedAt").and_then(Value::as_str);
    let running = ops::in_progress(app);
    let ours = started.is_some() && started != before;
    let Some(last) = super::dashboard::last_sync(app).filter(|_| ours) else {
        return Progress {
            phase: "Waiting".into(),
            message: Some("for Argo CD to start it".into()),
            ..Default::default()
        };
    };
    let ended = !running && !last.running;
    Progress {
        succeeded: ended && last.phase == "Succeeded",
        ended,
        phase: last.phase,
        message: last.message,
        done: last.done,
        total: last.steps.len(),
    }
}

/// How many of `rows` succeeded, failed (or could not be sent), and are still going: what
/// the summary says.
pub fn tally<'a>(rows: impl Iterator<Item = &'a Result<Progress, String>>) -> (usize, usize, usize) {
    let (mut ok, mut failed, mut going) = (0, 0, 0);
    for r in rows {
        match r {
            Ok(p) if p.ended && p.succeeded => ok += 1,
            Ok(p) if p.ended => failed += 1,
            Ok(_) => going += 1,
            Err(_) => failed += 1,
        }
    }
    (ok, failed, going)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn app() -> Value {
        json!({
            "metadata": {"name": "web", "namespace": "argocd", "resourceVersion": "7"},
            "spec": {"syncPolicy": {}},
            "status": {
                "resources": [
                    {"group": "apps", "kind": "Deployment", "namespace": "shop", "name": "web", "status": "OutOfSync"},
                    {"kind": "ConfigMap", "namespace": "shop", "name": "old", "status": "OutOfSync", "requiresPruning": true}
                ],
                "operationState": {"phase": "Succeeded", "startedAt": "2026-01-01T00:00:00Z"}
            }
        })
    }

    fn now() -> jiff::Timestamp {
        "2026-01-02T00:00:00Z".parse().unwrap()
    }

    #[test]
    fn each_option_is_in_the_request() {
        let pruned =
            vec![Res { kind: "ConfigMap".into(), namespace: "shop".into(), name: "old".into(), ..Default::default() }];
        let plain = Options::default().request(&pruned);
        assert_eq!(plain, SyncRequest::default(), "with hooks, no prune, no dry run: Argo CD's own Sync");
        let r = Options { hooks: false, prune: true, dry_run: true }.request(&pruned);
        assert!(r.apply && r.prune && r.dry_run && r.resources.is_empty());
        assert_eq!(r.prune_shown, Some(pruned.clone()), "prune deletes no more than was shown");
        assert_eq!(ops::sync_patch(&r)["operation"]["sync"]["syncStrategy"], json!({"apply": {}}));
        assert_eq!(Options { hooks: false, prune: true, dry_run: true }.text(), "without hooks, prune, dry run");
        assert_eq!(Options::default().text(), "with hooks");
    }

    #[test]
    fn what_prune_deletes_is_listed() {
        let c = check(&app(), &NextSync::NoWindow, now());
        assert_eq!(c.refused, None);
        assert_eq!(c.pruned.iter().map(Res::label).collect::<Vec<_>>(), ["ConfigMap shop/old"]);
        assert_eq!(c.before.as_deref(), Some("2026-01-01T00:00:00Z"));
    }

    /// One already syncing, one being deleted and one a sync window holds are left out, each
    /// saying why; auto-sync on is not a reason (a manual sync of it is allowed).
    #[test]
    fn refusals_say_why() {
        let mut busy = app();
        busy["operation"] = json!({"sync": {}});
        assert!(check(&busy, &NextSync::NoWindow, now()).refused.unwrap().contains("already running"));
        let mut deleting = app();
        deleting["metadata"]["deletionTimestamp"] = json!("2026-01-01T00:00:00Z");
        assert!(check(&deleting, &NextSync::NoWindow, now()).refused.unwrap().contains("deleted"));
        let later = NextSync::At("2026-01-02T02:00:00Z".parse().unwrap());
        let why = check(&app(), &later, now()).refused.unwrap();
        assert!(why.contains("sync window") && why.contains("in 2h"), "{why}");
        assert!(check(&app(), &NextSync::NotSoon, now()).refused.unwrap().contains("next 8 days"));
        // Windows not known, or none, or open: Argo CD decides.
        for open in [NextSync::Now, NextSync::NoWindow, NextSync::Unknown("project can't be read".into())] {
            assert_eq!(check(&app(), &open, now()).refused, None);
        }
        let mut auto = app();
        auto["spec"]["syncPolicy"]["automated"] = json!({"prune": true});
        let c = check(&auto, &NextSync::NoWindow, now());
        assert!(c.auto_sync && c.refused.is_none());
    }

    /// A future that is pending once, then ready: work that takes a little while.
    struct Pause(bool);

    impl Future for Pause {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
            if self.0 {
                std::task::Poll::Ready(())
            } else {
                self.0 = true;
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }
    }

    #[test]
    fn no_more_than_the_bound_run_at_once() {
        for (n, parallel) in [(10, 4), (3, 4), (9, 1), (0, 4)] {
            let (now, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let stream = bounded(n, parallel, |i| {
                let (now, most) = (now.clone(), most.clone());
                async move {
                    let at = now.fetch_add(1, Ordering::SeqCst) + 1;
                    most.fetch_max(at, Ordering::SeqCst);
                    Pause(false).await;
                    Pause(false).await;
                    now.fetch_sub(1, Ordering::SeqCst);
                    i
                }
            });
            let mut done: Vec<(usize, usize)> = futures::executor::block_on(stream.collect());
            done.sort_unstable();
            assert_eq!(done, (0..n).map(|i| (i, i)).collect::<Vec<_>>());
            assert_eq!(most.load(Ordering::SeqCst), n.min(parallel), "{n} at most {parallel}");
        }
    }

    #[test]
    fn progress_follows_the_operation_sent() {
        let before = Some("2026-01-01T00:00:00Z");
        // Written, not started yet: the last operation is still the one before.
        let mut a = app();
        a["operation"] = json!({"sync": {}});
        let p = progress(&a, before);
        assert_eq!((p.phase.as_str(), p.ended), ("Waiting", false));
        // Started: its resources done so far, a hook still running not counted.
        a["status"]["operationState"] = json!({
            "phase": "Running", "message": "waiting for completion of hook batch/Job/migrate",
            "startedAt": "2026-01-02T00:00:00Z",
            "syncResult": {"resources": [
                {"kind": "ConfigMap", "namespace": "shop", "name": "a", "status": "Synced"},
                {"group": "batch", "kind": "Job", "namespace": "shop", "name": "migrate", "hookPhase": "Running", "hookType": "PreSync"}
            ]}
        });
        let p = progress(&a, before);
        assert_eq!((p.phase.as_str(), p.done, p.total, p.ended), ("Running", 1, 2, false));
        assert!(p.message.unwrap().contains("migrate"));
        // Ended: succeeded or failed, with its message.
        a["operation"] = Value::Null;
        a["status"]["operationState"]["phase"] = json!("Failed");
        a["status"]["operationState"]["message"] = json!("one or more objects failed to apply");
        a["status"]["operationState"]["syncResult"]["resources"][1]["hookPhase"] = json!("Failed");
        let p = progress(&a, before);
        assert_eq!((p.ended, p.succeeded, p.done), (true, false, 2));
        a["status"]["operationState"]["phase"] = json!("Succeeded");
        assert!(progress(&a, before).succeeded);
        // Never synced before: the first operation is the one sent.
        assert!(progress(&a, None).ended);
        let rows = [Ok(progress(&a, None)), Ok(Progress::default()), Err("refused".to_string())];
        assert_eq!(tally(rows.iter()), (1, 1, 1));
    }
}
