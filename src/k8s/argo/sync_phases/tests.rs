use jiff::Timestamp;
use serde_json::{Value, json};

use super::*;
use crate::k8s::argo::journal::Step;

fn now() -> Timestamp {
    "2026-01-10T12:00:00Z".parse().unwrap()
}

fn ago(secs: i64) -> String {
    Timestamp::from_second(now().as_second() - secs).unwrap().to_string()
}

/// A deployment of the history that took `secs`.
fn deployment(id: i64, secs: i64) -> Value {
    let end = 86400 * (20 - id);
    json!({"id": id, "revision": "abc", "deployStartedAt": ago(end + secs), "deployedAt": ago(end)})
}

fn app(phase: &str, message: &str, resources: Value, history: Vec<Value>) -> Value {
    json!({
        "spec": {"destination": {"namespace": "shop"}},
        "status": {
            "history": history,
            "operationState": {
                "phase": phase,
                "message": message,
                "startedAt": ago(252),
                "operation": {"initiatedBy": {"username": "dev-user"}, "sync": {}},
                "syncResult": {"resources": resources},
            },
        },
    })
}

fn steps_of(app: &Value) -> Vec<Step> {
    app.pointer("/status/operationState/syncResult/resources")
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |r| r.iter().map(Step::of).collect())
}

fn hook(name: &str, phase: &str, result: &str) -> Value {
    json!({"group": "batch", "kind": "Job", "namespace": "shop", "name": name, "hookType": phase,
        "syncPhase": phase, "hookPhase": result, "status": "Synced"})
}

fn res(kind: &str, group: &str, name: &str, status: &str) -> Value {
    let message = if status == "SyncFailed" { "the server rejected it" } else { "configured" };
    json!({"group": group, "kind": kind, "namespace": "shop", "name": name, "status": status, "syncPhase": "Sync",
        "message": message})
}

fn node(kind: &str, group: &str, name: &str, uid: &str, parent: Option<&str>, health: &str, message: &str) -> Value {
    let mut n = json!({"group": group, "kind": kind, "namespace": "shop", "name": name, "uid": uid,
        "health": {"status": health, "message": message}});
    if let Some(p) = parent {
        n["parentRefs"] = json!([{"uid": p}]);
    }
    n
}

fn pod(name: &str, rs: &str, reason: &str, health: &str) -> Value {
    let mut p = node("Pod", "", name, name, Some(rs), health, "");
    p["info"] = json!([{"name": "Status Reason", "value": reason}]);
    p
}

/// `crawl` rolling out: 3 of 5 ready, two pods starting; `export` crash-looping; `api` healthy.
fn tree() -> Value {
    json!({"nodes": [
        node("Deployment", "apps", "crawl", "d-crawl", None, "Progressing",
            "Waiting for rollout to finish: 3 of 5 updated replicas are available..."),
        node("ReplicaSet", "apps", "crawl-1", "rs-crawl", Some("d-crawl"), "Progressing", ""),
        pod("crawl-1-a", "rs-crawl", "Running", "Healthy"),
        pod("crawl-1-b", "rs-crawl", "ContainerCreating", "Progressing"),
        pod("crawl-1-c", "rs-crawl", "ContainerCreating", "Progressing"),
        node("Deployment", "apps", "export", "d-export", None, "Degraded", "Deployment exceeded its progress deadline"),
        node("ReplicaSet", "apps", "export-1", "rs-export", Some("d-export"), "Degraded", ""),
        pod("export-1-a", "rs-export", "CrashLoopBackOff", "Degraded"),
        pod("export-1-b", "rs-export", "CrashLoopBackOff", "Degraded"),
        pod("export-1-c", "rs-export", "Running", "Healthy"),
        node("Deployment", "apps", "api", "d-api", None, "Healthy", ""),
    ]})
}

fn states(p: &SyncPhases) -> Vec<PhaseState> {
    p.phases.iter().map(|p| p.state).collect()
}

#[test]
fn what_a_sync_waits_for_is_read_from_its_message() {
    let w = waiting("waiting for healthy state of apps/Deployment/web and 2 more resources").unwrap();
    assert_eq!(
        (w.what.as_str(), w.res.group.as_str(), w.res.kind.as_str()),
        ("healthy state of", "apps", "Deployment")
    );
    assert_eq!(w.more_text().as_deref(), Some("and 2 more resources"));
    // The core group is empty.
    let w = waiting("waiting for completion of hook /Pod/migrate").unwrap();
    assert_eq!((w.what.as_str(), w.res.group.as_str(), w.res.kind.as_str()), ("completion of hook", "", "Pod"));
    assert_eq!((w.res.name.as_str(), w.more), ("migrate", 0));
    let w = waiting("Waiting for deletion of batch/Job/old-job").unwrap();
    assert_eq!((w.what.as_str(), w.res.name.as_str()), ("deletion of", "old-job"));
    // Anything else says nothing.
    for m in
        ["successfully synced (all tasks run)", "waiting for something else", "waiting for healthy state of web", ""]
    {
        assert_eq!(waiting(m), None, "{m}");
    }
}

#[test]
fn replicas_are_read_from_a_workloads_health() {
    for (m, want) in [
        ("Waiting for rollout to finish: 3 of 5 updated replicas are available...", Some((3, 5))),
        ("Waiting for rollout to finish: 2 out of 5 new replicas have been updated...", Some((2, 5))),
        ("Waiting for daemon set \"logs\" rollout to finish: 1 out of 3 new pods have been updated...", Some((1, 3))),
        ("Deployment has 6 of 8 replicas available", Some((6, 8))),
        ("Waiting for 1 pods to be ready...", None),
        ("Waiting for rollout to finish: 1 old replicas are pending termination...", None),
        ("5 of 3", None),
        ("", None),
    ] {
        assert_eq!(replicas(m), want, "{m}");
    }
}

#[test]
fn the_usual_duration_is_the_median_of_the_last_ten_deployments() {
    assert_eq!(median(vec![]), None);
    assert_eq!(median(vec![5, 1, 3]), Some(3));
    assert_eq!(median(vec![4, 1, 3, 2]), Some(2));
    // Two old slow ones left out: the last ten took 300 s, but one 600 s.
    let mut history = vec![deployment(1, 3000), deployment(2, 3000)];
    history.extend((3..12).map(|i| deployment(i, 300)));
    history.push(deployment(12, 600));
    // One without a start is left out.
    history.push(json!({"id": 13, "deployedAt": ago(10)}));
    let a = app("Running", "", json!([]), history);
    assert_eq!(usual(&a), Some(300));
    assert_eq!(usual(&app("Running", "", json!([]), vec![])), None);
}

#[test]
fn durations_read_to_the_second_or_about() {
    assert_eq!(clock(42), "42 s");
    assert_eq!(clock(252), "4 min 12 s");
    assert_eq!(clock(300), "5 min");
    assert_eq!(clock(3900), "1 h 5 min");
    assert_eq!(clock(-3), "0 s");
    assert_eq!(about(40), "~40 s");
    assert_eq!(about(290), "~5 min");
    assert_eq!(about(4200), "~1 h 10 min");
    assert_eq!(about(7200), "~2 h");
}

#[test]
fn a_sync_waiting_for_a_workload_is_in_its_rollout() {
    let resources = json!([
        hook("migrate", "PreSync", "Succeeded"),
        res("ConfigMap", "", "config", "Synced"),
        res("Deployment", "apps", "crawl", "Synced"),
        res("Deployment", "apps", "export", "Synced"),
    ]);
    let history = (1..6).map(|i| deployment(i, 300)).collect();
    let a =
        app("Running", "waiting for healthy state of apps/Deployment/crawl and 1 more resources", resources, history);
    let t = tree();
    let p = of(&a, &steps_of(&a), Some(&t)).unwrap();
    assert_eq!(p.outcome, Outcome::Syncing);
    assert_eq!(p.by.as_deref(), Some("dev-user"));
    // A crash-looping workload is a problem while the rollout runs.
    assert_eq!(states(&p), [PhaseState::Done, PhaseState::Done, PhaseState::Problem, PhaseState::Pending]);
    assert_eq!(p.current(), Some(PhaseName::Rollout));
    let w = p.waiting.as_ref().unwrap();
    assert_eq!((w.res.name.as_str(), w.res.namespace.as_str()), ("crawl", "shop"));
    let sync = p.phase(PhaseName::Sync).unwrap();
    assert_eq!(sync.count(), "3 Synced · 0 SyncFailed");
    // The broken first, each with its replicas and its pods' reasons; the healthy one left out.
    let rollout = p.phase(PhaseName::Rollout).unwrap();
    let names: Vec<&str> = rollout.workloads.iter().map(|w| w.res.name.as_str()).collect();
    assert_eq!(names, ["export", "crawl"]);
    let (export, crawl) = (&rollout.workloads[0], &rollout.workloads[1]);
    assert_eq!((export.tone, export.ready), (Tone::Bad, Some((1, 3))));
    assert_eq!(export.reasons.iter().map(Reason::text).collect::<Vec<_>>(), ["2 CrashLoopBackOff"]);
    assert_eq!(export.pod.as_ref().map(|r| r.name.as_str()), Some("export-1-a"));
    assert_eq!((crawl.tone, crawl.ready), (Tone::Busy, Some((3, 5))));
    assert_eq!(crawl.reasons[0].text(), "2 ContainerCreating");
    assert_eq!(crawl.reasons[0].tone, Tone::Busy);
    // 4 min 12 s of the usual 5 min.
    assert_eq!(p.duration_text(now()).as_deref(), Some("4 min 12 s, usually ~5 min"));
    assert!(!p.slow(now()));
    assert!((p.share(now()).unwrap() - 0.84).abs() < 0.01);
}

#[test]
fn a_slow_sync_is_said_and_its_bar_stays_full() {
    let history = (1..4).map(|i| deployment(i, 100)).collect();
    let a = app("Running", "", json!([]), history);
    let p = of(&a, &steps_of(&a), None).unwrap();
    assert!(p.slow(now()));
    assert_eq!(p.share(now()), Some(1.));
    // No history: no usual duration, no bar.
    let a = app("Running", "", json!([]), vec![]);
    let p = of(&a, &steps_of(&a), None).unwrap();
    assert_eq!((p.share(now()), p.slow(now())), (None, false));
    assert_eq!(p.duration_text(now()).as_deref(), Some("4 min 12 s"));
}

#[test]
fn a_sync_waiting_for_a_hook_is_in_the_hooks_phase() {
    let pre = json!([hook("migrate", "PreSync", "Running")]);
    let a = app("Running", "waiting for completion of hook batch/Job/migrate", pre, vec![]);
    let p = of(&a, &steps_of(&a), None).unwrap();
    assert_eq!(states(&p), [PhaseState::Running, PhaseState::Pending, PhaseState::Pending, PhaseState::Pending]);
    let h = &p.phase(PhaseName::PreSync).unwrap().hooks[0];
    assert_eq!((h.status.as_str(), h.tone), ("Running", Tone::Busy));

    let post = json!([
        hook("migrate", "PreSync", "Succeeded"),
        res("Deployment", "apps", "api", "Synced"),
        hook("notify", "PostSync", "Running"),
    ]);
    let a = app("Running", "waiting for completion of hook batch/Job/notify", post, vec![]);
    let p = of(&a, &steps_of(&a), None).unwrap();
    assert_eq!(states(&p), [PhaseState::Done, PhaseState::Done, PhaseState::Done, PhaseState::Running]);
    assert_eq!(p.current(), Some(PhaseName::PostSync));
    // A hook that succeeded reads as good.
    assert_eq!(p.phase(PhaseName::PreSync).unwrap().hooks[0].tone, Tone::Good);
}

#[test]
fn a_hook_not_started_is_pending() {
    let a = app(
        "Running",
        "",
        json!([{"kind": "Job", "group": "batch", "name": "seed", "hookType": "PostSync",
        "syncPhase": "PostSync"}]),
        vec![],
    );
    let p = of(&a, &steps_of(&a), None).unwrap();
    let h = &p.phase(PhaseName::PostSync).unwrap().hooks[0];
    assert_eq!((h.status.as_str(), h.tone), ("pending", Tone::Busy));
}

#[test]
fn a_failed_resource_stops_the_sync_at_its_phase() {
    let resources = json!([
        hook("migrate", "PreSync", "Succeeded"),
        res("ConfigMap", "", "config", "Synced"),
        res("Deployment", "apps", "api", "SyncFailed"),
    ]);
    let a = app("Failed", "one or more objects failed to apply", resources, vec![]);
    let p = of(&a, &steps_of(&a), None).unwrap();
    assert_eq!(p.outcome, Outcome::Failed);
    assert_eq!(states(&p), [PhaseState::Done, PhaseState::Problem, PhaseState::NotRun, PhaseState::NotRun]);
    let sync = p.phase(PhaseName::Sync).unwrap();
    assert_eq!(sync.count(), "1 Synced · 1 SyncFailed");
    assert_eq!(sync.failed[0].res.name, "api");
    assert_eq!(sync.failed[0].message.as_deref(), Some("the server rejected it"));
    assert_eq!(p.message.as_deref(), Some("one or more objects failed to apply"));
}

#[test]
fn a_failed_hook_stops_the_sync_before_the_resources() {
    let a = app("Failed", "job failed", json!([hook("migrate", "PreSync", "Failed")]), vec![]);
    let p = of(&a, &steps_of(&a), None).unwrap();
    assert_eq!(states(&p), [PhaseState::Problem, PhaseState::NotRun, PhaseState::NotRun, PhaseState::NotRun]);
}

#[test]
fn a_sync_stopped_by_a_timeout_is_blamed_on_the_workload_not_healthy() {
    let resources = json!([res("Deployment", "apps", "export", "Synced")]);
    let a = app("Failed", "Operation terminated, triggered by controller sync timeout", resources, vec![]);
    let t = tree();
    let p = of(&a, &steps_of(&a), Some(&t)).unwrap();
    assert_eq!(states(&p), [PhaseState::Done, PhaseState::Done, PhaseState::Problem, PhaseState::NotRun]);
    assert_eq!(p.current(), Some(PhaseName::Rollout));
    assert_eq!(p.waiting, None);
}

#[test]
fn a_sync_that_failed_before_anything_ran_is_blamed_on_sync() {
    let a = app("Error", "ComparisonError: rpc error", json!([]), vec![]);
    let p = of(&a, &steps_of(&a), None).unwrap();
    assert_eq!(states(&p), [PhaseState::Done, PhaseState::Problem, PhaseState::NotRun, PhaseState::NotRun]);
}

#[test]
fn a_sync_that_succeeded_is_done_and_says_the_workloads_broken_since() {
    let resources = json!([hook("migrate", "PreSync", "Succeeded"), res("Deployment", "apps", "api", "Synced")]);
    let a = app("Succeeded", "successfully synced (all tasks run)", resources, vec![]);
    let p = of(&a, &steps_of(&a), None).unwrap();
    assert!(p.phases.iter().all(|p| p.state == PhaseState::Done));
    assert_eq!(p.current(), None);
    let t = tree();
    let p = of(&a, &steps_of(&a), Some(&t)).unwrap();
    assert_eq!(p.phase(PhaseName::Rollout).unwrap().state, PhaseState::Problem);
}

#[test]
fn without_the_tree_the_status_says_the_workloads_not_healthy() {
    let mut a = app("Running", "", json!([]), vec![]);
    a["status"]["resources"] = json!([
        {"group": "apps", "kind": "StatefulSet", "namespace": "shop", "name": "db",
         "health": {"status": "Progressing", "message": "Waiting for partitioned roll out to finish: 1 out of 3 new pods have been updated..."}},
        {"group": "apps", "kind": "Deployment", "namespace": "shop", "name": "api", "health": {"status": "Healthy"}},
        {"group": "", "kind": "Service", "namespace": "shop", "name": "api", "health": {"status": "Progressing"}},
    ]);
    let p = of(&a, &steps_of(&a), None).unwrap();
    let w = &p.phase(PhaseName::Rollout).unwrap().workloads;
    assert_eq!(w.len(), 1);
    assert_eq!((w[0].res.name.as_str(), w[0].ready, w[0].tone), ("db", Some((1, 3)), Tone::Busy));
}

#[test]
fn an_application_never_synced_has_no_dialog() {
    assert_eq!(of(&json!({"status": {}}), &[], None), None);
}

#[test]
fn the_examples_failed_sync_names_its_resource() {
    let now = Timestamp::now();
    let read = crate::k8s::argo::dashboard::example::read("example-sandbox", now);
    let cards = crate::k8s::argo::dashboard::Cards::of(read, None, now);
    let p = cards.sync_phases.as_ref().unwrap();
    assert_eq!(p.outcome, Outcome::Failed);
    assert_eq!(p.phase(PhaseName::Sync).unwrap().state, PhaseState::Problem);
    assert_eq!(p.phase(PhaseName::Sync).unwrap().failed[0].res.name, "api");
}
