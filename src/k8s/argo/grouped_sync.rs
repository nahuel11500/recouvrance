//! A sync of several Applications at once, through Argo CD's API (`api::act_read`, which
//! checks write mode and the server's protection as it sends).
//!
//! Each Application is read first ([`check`]): one whose sync window denies a manual sync now,
//! one already syncing, or one being deleted is left out, saying why; what prune would delete
//! on each is listed from its managed resources (`requiresPruning`). The syncs are sent at
//! most [`PARALLEL`] at a time; each Application is then read again until its operation ends,
//! and its progress told from `status.operationState` ([`progress`]).
//!
//! Options: the full sync with hooks (Argo CD's hook strategy, its own Sync) or the resources
//! out of sync only, without hooks (the apply strategy on the resources each Application
//! has out of sync, named in the sync: what is in sync is not applied again, and the
//! `PreSync`/`Sync`/`PostSync` hooks such as a secret refresh or a migration, listed per
//! Application, are skipped), prune (off by default; never deleting more than was shown), and
//! dry run (Argo CD applies nothing and says what it would do).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::StreamExt as _;
use serde_json::Value;

use super::api;
use super::diagnosis::Res;
use super::ops::{self, Op, SyncRequest};
use super::windows::{HORIZON_DAYS, NextSync};

/// Syncs sent, and Applications read, at the same time, at most.
pub const PARALLEL: usize = 4;

/// What the sync of the picked Applications asks of each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// The whole Application with its hooks (the hook strategy); off: its resources out of
    /// sync only, with the apply strategy, without hooks.
    pub hooks: bool,
    pub prune: bool,
    pub dry_run: bool,
    /// Argo CD's Force, Server-Side Apply and Replace (see `SyncRequest`).
    pub force: bool,
    pub server_side: bool,
    pub replace: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { hooks: true, prune: false, dry_run: false, force: false, server_side: false, replace: false }
    }
}

impl Options {
    /// The sync of one Application as `checked` found it: the whole of it, or its resources
    /// out of sync (and, with prune, those to prune) named one by one. What prune deletes on it
    /// as shown (if Argo CD's comparison then shows more, the sync of it is refused), and the
    /// revision shown when it can be pinned (so git moving on can't bring more). None when
    /// the sync is of its resources out of sync and it has none: nothing to sync (an empty
    /// list would sync it whole).
    pub fn request(&self, checked: &Checked) -> Option<SyncRequest> {
        let resources = if self.hooks {
            Vec::new()
        } else {
            let mut picked = checked.out_of_sync.clone();
            if self.prune {
                picked.extend(checked.pruned.iter().cloned());
            }
            if picked.is_empty() {
                return None;
            }
            picked
        };
        Some(SyncRequest {
            resources,
            prune: self.prune,
            dry_run: self.dry_run,
            apply: !self.hooks,
            revision: checked.revision.clone(),
            prune_shown: self.prune.then(|| checked.pruned.clone()),
            force: self.force,
            server_side: self.server_side,
            replace: self.replace,
        })
    }

    /// [`Self::request`], the resources `left_out` unpicked: a sync of the resources out of
    /// sync names the others only, and none left is nothing to sync. The full sync names none,
    /// so nothing is left out of it.
    pub fn request_picked(&self, checked: &Checked, left_out: &[Res]) -> Option<SyncRequest> {
        let mut req = self.request(checked)?;
        if self.hooks || left_out.is_empty() {
            return Some(req);
        }
        req.resources.retain(|r| !left_out.iter().any(|l| l.same(r)));
        if let Some(shown) = &mut req.prune_shown {
            shown.retain(|r| !left_out.iter().any(|l| l.same(r)));
        }
        (!req.resources.is_empty()).then_some(req)
    }

    /// `with hooks, prune, dry run`: what the confirmation says.
    pub fn text(&self) -> String {
        let mut parts = vec![if self.hooks { "full, with hooks" } else { "out of sync only, without hooks" }];
        if self.prune {
            parts.push("prune");
        }
        if self.dry_run {
            parts.push("dry run");
        }
        if self.force {
            parts.push("force");
        }
        if self.server_side {
            parts.push("server-side apply");
        }
        if self.replace {
            parts.push("replace");
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
    /// Its resources out of sync that a sync applies (hooks and those to prune left out):
    /// what the sync of the resources out of sync names.
    pub out_of_sync: Vec<Res>,
    /// Its hooks (`PreSync Job shop/migrate`): run by the full sync, skipped by the other.
    pub hooks: Vec<String>,
    pub auto_sync: bool,
    /// When its last operation started, as read for the page (the sync follows the operation
    /// after the one read right before it is written).
    pub before: Option<String>,
    /// The revision Argo CD compared, which the sync is pinned to (one source, auto-sync off).
    pub revision: Option<String>,
    /// The sync options of its own spec (`ServerSideApply=true`): what the page shows as
    /// already on. The sync sends them again with those ticked (`ops::sync_options`).
    pub sync_options: Vec<String>,
}

/// Of the Applications `checked`, how many have `option` in their own spec, and how many
/// there are: Server-side apply "already on for 1 of 3".
pub fn already_on<'a>(checked: impl IntoIterator<Item = &'a Checked>, option: &str) -> (usize, usize) {
    checked.into_iter().fold((0, 0), |(on, n), c| (on + usize::from(c.sync_options.iter().any(|o| o == option)), n + 1))
}

/// When the last operation of `app` started.
pub fn started_at(app: &Value) -> Option<String> {
    app.pointer("/status/operationState/startedAt").and_then(Value::as_str).map(String::from)
}

/// Why a manual sync can't start now under the sync windows that apply (`next`, from the
/// Application's project); None when it can, or when the windows aren't known
/// (argocd-server then refuses a sync a window denies, and says so).
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
    let pruned = ops::pruned(app, &[]);
    Checked {
        refused,
        out_of_sync: ops::out_of_sync(app).into_iter().filter(|r| !pruned.iter().any(|p| p.same(r))).collect(),
        pruned,
        hooks: hooks(app),
        auto_sync: super::auto_sync(app),
        before: started_at(app),
        revision: ops::shown_revision(app),
        sync_options: ops::spec_options(app),
    }
}

/// The hooks of `app`, each as `PreSync Job shop/migrate`: those its managed resources list,
/// and those its last sync ran (Argo CD keeps no other list of them; one that has never run
/// may be missing).
pub fn hooks(app: &Value) -> Vec<String> {
    let mut found: Vec<(Res, String)> = Vec::new();
    let ran = app.pointer("/status/operationState/syncResult/resources").and_then(Value::as_array);
    let listed = app.pointer("/status/resources").and_then(Value::as_array);
    let typed = ran.into_iter().flatten().filter(|r| r["hookType"].as_str().is_some_and(|t| !t.is_empty()));
    let marked = listed.into_iter().flatten().filter(|r| r["hook"] == true);
    for r in typed.chain(marked) {
        let Some(res) = Res::of(r) else { continue };
        if found.iter().any(|(f, _)| f.same(&res)) {
            continue;
        }
        let label = match r["hookType"].as_str().filter(|t| !t.is_empty()) {
            Some(t) => format!("{t} {}", res.label()),
            None => res.label(),
        };
        found.push((res, label));
    }
    found.into_iter().map(|(_, l)| l).collect()
}

/// An Application to sync: the Argo CD API server it is read from, its Argo CD, its
/// namespace and name.
#[derive(Clone)]
pub struct Target {
    pub server: Arc<api::Endpoint>,
    pub instance: String,
    pub namespace: String,
    pub name: String,
}

/// A request of a grouped sync to one Application.
pub type Call<T> = Pin<Box<dyn Future<Output = Result<T, String>> + Send>>;
/// What sending one did: Argo CD's answer and when the operation before it started.
pub type Sent = Result<(String, Option<String>), String>;

impl Target {
    /// The Application as Argo CD has it now, whole; its operation goes to the sync journal.
    pub fn read(&self) -> Call<Value> {
        let (t, instance) = (self.clone(), self.instance.clone());
        Box::pin(async move {
            let app = api::read_app(t.server.clone(), t.namespace.clone(), t.name.clone()).await?;
            super::journal::observe(&instance, &app);
            Ok(app)
        })
    }

    /// Send `op` (a sync or a terminate) by Argo CD's API, with write mode and the server's
    /// protection checked as it is sent. A dry run's result is not waited for: it is followed
    /// like any sync. What Argo CD said, and when the Application's last operation started as
    /// read right before the write (the operation sent is a later one).
    pub fn send(&self, op: Op) -> Call<(String, Option<String>)> {
        let (ns, name) = (self.namespace.clone(), self.name.clone());
        let action = match op {
            Op::Sync(req) => api::Action::Sync(req),
            Op::Terminate => api::Action::Terminate,
            Op::Rollback { .. } => return Box::pin(async { Err("not part of a grouped sync".to_string()) }),
        };
        Box::pin(api::act_read(self.server.clone(), ns, name, action))
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
pub fn send_all(jobs: Vec<(Target, SyncRequest)>) -> impl futures::Stream<Item = (usize, Sent)> {
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
        let web = Res {
            group: "apps".into(),
            kind: "Deployment".into(),
            namespace: "shop".into(),
            name: "web".into(),
            ..Default::default()
        };
        let checked = Checked { pruned: pruned.clone(), out_of_sync: vec![web.clone()], ..Default::default() };
        let plain = Options::default().request(&checked).unwrap();
        assert_eq!(plain, SyncRequest::default(), "full, no prune, no dry run: Argo CD's own Sync");
        let pinned = Checked { revision: Some("abc".into()), ..checked };
        let r = Options { hooks: false, prune: true, dry_run: true, ..Default::default() }.request(&pinned).unwrap();
        assert!(r.apply && r.prune && r.dry_run);
        assert_eq!(r.resources, [web.clone(), pruned[0].clone()], "what is out of sync, and what prune deletes");
        assert_eq!(r.prune_shown, Some(pruned), "prune deletes no more than was shown");
        assert_eq!(r.revision.as_deref(), Some("abc"), "pinned to the revision shown");
        assert_eq!(ops::strategy(&r), json!({"apply": {}}));
        let r = Options { hooks: false, prune: false, dry_run: false, ..Default::default() }.request(&pinned).unwrap();
        assert_eq!(r.resources, [web], "without prune, what is to prune is not named");
        assert_eq!(
            Options { hooks: false, prune: true, dry_run: true, ..Default::default() }.text(),
            "out of sync only, without hooks, prune, dry run"
        );
        assert_eq!(Options::default().text(), "full, with hooks");
        let all = Options { force: true, server_side: true, replace: true, ..Default::default() };
        let r = all.request(&pinned).unwrap();
        assert!(r.force && r.server_side && r.replace);
        assert_eq!(ops::ticked_options(&r), ["ServerSideApply=true", "Replace=true"]);
        assert_eq!(all.text(), "full, with hooks, force, server-side apply, replace");
    }

    /// Each Application's own sync options are read with it, and counted for the page.
    #[test]
    fn own_sync_options_are_read_and_counted() {
        let app = json!({"spec": {"syncPolicy": {"syncOptions": ["ServerSideApply=true", "CreateNamespace=true"]}}});
        let a = check(&app, &NextSync::NoWindow, now());
        assert_eq!(a.sync_options, ["ServerSideApply=true", "CreateNamespace=true"]);
        let b = check(&json!({}), &NextSync::NoWindow, now());
        assert!(b.sync_options.is_empty());
        assert_eq!(already_on([&a, &b], ops::SERVER_SIDE), (1, 2));
        assert_eq!(already_on([&a, &b], ops::REPLACE), (0, 2));
    }

    /// Resources unpicked are left out of a sync of the resources out of sync, and of what
    /// prune was shown to delete; with none left, nothing is synced.
    #[test]
    fn resources_unpicked_are_left_out() {
        let res = |name: &str| Res {
            kind: "ConfigMap".into(),
            namespace: "shop".into(),
            name: name.into(),
            ..Default::default()
        };
        let checked = Checked { out_of_sync: vec![res("a"), res("b")], pruned: vec![res("old")], ..Default::default() };
        let oos = Options { hooks: false, prune: true, ..Default::default() };
        let r = oos.request_picked(&checked, &[res("a"), res("old")]).unwrap();
        assert_eq!(r.resources, [res("b")]);
        assert_eq!(r.prune_shown, Some(Vec::new()));
        assert_eq!(oos.request_picked(&checked, &[res("a"), res("b"), res("old")]), None);
        let full = Options::default().request_picked(&checked, &[res("a")]).unwrap();
        assert!(full.resources.is_empty(), "the full sync is of the whole Application");
    }

    /// An Application with nothing out of sync is not synced at all without hooks: an empty
    /// list of resources would sync it whole.
    #[test]
    fn nothing_out_of_sync_is_nothing_to_sync() {
        let synced = Checked::default();
        let oos = Options { hooks: false, ..Default::default() };
        assert_eq!(oos.request(&synced), None);
        assert_eq!(Options { prune: true, ..oos }.request(&synced), None);
        let only_prune = Checked {
            pruned: vec![Res { kind: "ConfigMap".into(), name: "old".into(), ..Default::default() }],
            ..synced
        };
        assert_eq!(oos.request(&only_prune), None, "prune off: nothing to apply");
        assert_eq!(Options { prune: true, ..oos }.request(&only_prune).unwrap().resources.len(), 1);
        assert!(Options::default().request(&Checked::default()).is_some(), "the full sync always goes");
    }

    #[test]
    fn hooks_are_listed_once_with_their_type() {
        let mut a = app();
        a["status"]["resources"]
            .as_array_mut()
            .unwrap()
            .push(json!({"group": "batch", "kind": "Job", "namespace": "shop", "name": "migrate", "hook": true}));
        a["status"]["resources"]
            .as_array_mut()
            .unwrap()
            .push(json!({"group": "batch", "kind": "Job", "namespace": "shop", "name": "smoke", "hook": true}));
        a["status"]["operationState"]["syncResult"] = json!({"resources": [
            {"kind": "ConfigMap", "namespace": "shop", "name": "a", "status": "Synced"},
            {"group": "batch", "kind": "Job", "namespace": "shop", "name": "migrate", "hookType": "PreSync", "hookPhase": "Succeeded"}
        ]});
        assert_eq!(hooks(&a), ["PreSync Job shop/migrate", "Job shop/smoke"]);
        let c = check(&a, &NextSync::NoWindow, now());
        assert_eq!(c.hooks.len(), 2);
        assert_eq!(c.out_of_sync.iter().map(Res::label).collect::<Vec<_>>(), ["Deployment shop/web"]);
    }

    /// An entry of `status.resources`, any of its fields left out.
    fn entry() -> impl proptest::strategy::Strategy<Value = Value> {
        use proptest::prelude::*;
        let word = |words: Vec<&'static str>| proptest::sample::select(words);
        (
            word(vec!["", "apps", "batch"]),
            word(vec!["Deployment", "Job", "ConfigMap"]),
            word(vec!["", "shop"]),
            word(vec!["a", "b", "c"]),
            proptest::option::of(word(vec!["Synced", "OutOfSync", "Unknown"])),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(group, kind, namespace, name, sync, prune, hook)| {
                let mut e = json!({"group": group, "kind": kind, "namespace": namespace, "name": name});
                if let Some(sync) = sync {
                    e["status"] = json!(sync);
                }
                if prune {
                    e["requiresPruning"] = json!(true);
                }
                if hook {
                    e["hook"] = json!(true);
                }
                e
            })
    }

    proptest::proptest! {
        /// Without hooks, the sync names resources, never none (which would sync the whole
        /// Application, hooks run by none but applied), and only resources out of sync that
        /// are not hooks: to prune only with prune on, and then all those shown.
        #[test]
        fn out_of_sync_only_names_what_is_out_of_sync(
            entries in proptest::collection::vec(entry(), 0..12),
            prune: bool,
            dry_run: bool,
        ) {
            let mut a = app();
            a["status"]["resources"] = json!(entries);
            let c = check(&a, &NextSync::NoWindow, now());
            let opts = Options { hooks: false, prune, dry_run, ..Default::default() };
            let Some(req) = opts.request(&c) else {
                proptest::prop_assert!(c.out_of_sync.is_empty() && (!prune || c.pruned.is_empty()));
                return Ok(());
            };
            proptest::prop_assert!(!req.resources.is_empty() && req.apply);
            for r in &req.resources {
                let ok = entries.iter().filter(|e| Res::of(e).is_some_and(|x| x.same(r))).any(|e| {
                    e["status"] == "OutOfSync"
                        && (e["hook"] != true || e["requiresPruning"] == true)
                        && (prune || e["requiresPruning"] != true)
                });
                proptest::prop_assert!(ok, "{} named", r.label());
            }
            // Argo CD would take the request: every resource named is the Application's, and
            // prune deletes what was shown.
            proptest::prop_assert_eq!(ops::refusal(&a, &Op::Sync(req)), None);
        }
    }

    /// An auto-sync that ran between the page's read and the sync's write is not the sync
    /// sent: the operation followed is the one after the operation read right before the write.
    #[test]
    fn an_operation_that_ran_before_the_write_is_not_the_one_sent() {
        let page_read = "2026-01-01T00:00:00Z";
        let auto = "2026-01-01T12:00:00Z";
        let mut a = app();
        a["operation"] = json!({"sync": {}});
        a["status"]["operationState"] = json!({"phase": "Succeeded", "startedAt": auto,
            "finishedAt": "2026-01-01T12:01:00Z", "operation": {"initiatedBy": {"automated": true}}});
        // Taken from the page's read (stale), the auto-sync would pass for the sync sent.
        assert_eq!(progress(&a, Some(page_read)).phase, "Succeeded");
        // From the read right before the write: still waiting for Argo CD to start it.
        let p = progress(&a, Some(auto));
        assert_eq!((p.phase.as_str(), p.ended), ("Waiting", false));
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
