use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use futures::StreamExt as _;
use serde_json::{Value, json};

use super::{AppDiff, PARALLEL, cached, read_all, regroup, sides, summary};
use crate::smartdiff::SmartDiff;

/// One of Argo CD's managed resources, its states as the JSON text it sends.
fn res(group: &str, kind: &str, ns: &str, name: &str, live: Option<Value>, desired: Option<Value>) -> Value {
    let text = |v: &Option<Value>| v.as_ref().map(Value::to_string);
    json!({"group": group, "kind": kind, "namespace": ns, "name": name,
        "normalizedLiveState": text(&live), "predictedLiveState": text(&desired),
        "liveState": text(&live), "targetState": text(&desired)})
}

fn deploy(ns: &str, tag: &str, replicas: i64) -> Value {
    json!({"apiVersion": "apps/v1", "kind": "Deployment", "metadata": {"name": "web", "namespace": ns},
        "spec": {"replicas": replicas, "template": {
            "metadata": {"annotations": {"checksum/config": format!("{tag}-sum")}},
            "spec": {"containers": [{"name": "web", "image": format!("registry.example.com/shop/web:{tag}")}]}}},
        "status": {"replicas": replicas}})
}

fn config(ns: &str, name: &str, level: &str) -> Value {
    json!({"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": name, "namespace": ns},
        "data": {"LOG_LEVEL": level}})
}

/// An environment where a sync takes web from 1.15 to `tag`, its replicas from 3 to
/// `replicas`, and `LOG_LEVEL` from info to debug.
fn env(ns: &str, tag: &str, replicas: i64) -> AppDiff {
    AppDiff::of(&json!({"items": [
        res("apps", "Deployment", ns, "web", Some(deploy(ns, "1.15", 3)), Some(deploy(ns, tag, replicas))),
        res("", "ConfigMap", ns, "app-config", Some(config(ns, "app-config", "info")),
            Some(config(ns, "app-config", "debug"))),
    ]}))
}

fn texts(groups: &[super::Cross]) -> Vec<&str> {
    groups.iter().map(|g| g.text.as_str()).collect()
}

fn apps(groups: &[super::Cross], text: &str) -> Vec<usize> {
    groups.iter().find(|g| g.text == text).map_or_else(Vec::new, |g| g.apps.iter().map(|(a, _)| *a).collect())
}

#[test]
fn managed_resources_give_the_live_and_desired_sides() {
    let body = json!({"items": [
        res("apps", "Deployment", "shop", "web", Some(deploy("shop", "1.15", 3)), Some(deploy("shop", "1.16", 3))),
        // Created by the sync, pruned by it, the same, a hook, one with no name.
        res("", "Service", "shop", "web", None, Some(json!({"apiVersion": "v1", "kind": "Service"}))),
        res("", "ConfigMap", "shop", "old", Some(config("shop", "old", "info")), None),
        {"group": "", "kind": "ConfigMap", "namespace": "shop", "name": "same", "modified": false,
            "normalizedLiveState": "{}", "predictedLiveState": "{}"},
        {"kind": "Job", "name": "migrate", "hook": true, "targetState": "{}"},
        {"kind": "ConfigMap", "name": "", "targetState": "{}"},
    ]});
    let (live, desired, same, hooks) = sides(&body);
    let labels = |o: &crate::smartdiff::Objects| o.keys().map(crate::smartdiff::ObjectKey::label).collect::<Vec<_>>();
    assert_eq!(labels(&live), ["ConfigMap shop/old", "Deployment shop/web"]);
    assert_eq!(labels(&desired), ["Service shop/web", "Deployment shop/web"]);
    assert_eq!((same, hooks), (1, 1));
    // The status is left out: Argo CD doesn't sync it.
    assert!(live.values().all(|o| o.get("status").is_none()));
    // Without the normalized and predicted states, the plain ones.
    let plain = json!({"items": [{"group": "", "kind": "ConfigMap", "namespace": "a", "name": "c",
        "liveState": config("a", "c", "info").to_string(), "targetState": config("a", "c", "debug").to_string()}]});
    let d = AppDiff::of(&plain);
    assert_eq!(
        d.diff.groups.iter().map(crate::smartdiff::Group::text).collect::<Vec<_>>(),
        ["config key LOG_LEVEL changed"]
    );
    assert!(AppDiff::of(&json!({})).in_sync(), "no items: nothing to sync");
}

#[test]
fn a_secrets_values_never_show() {
    let secret = |v: &str| {
        json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "db", "namespace": "a"},
        "data": {"password": v}})
    };
    let body = json!({"items": [res("", "Secret", "a", "db", Some(secret("czNjcjN0")), Some(secret("bjN3")))]});
    let d = AppDiff::of(&body);
    let lines = format!("{:?}", d.diff.resources.iter().map(|r| r.lines(true)).collect::<Vec<_>>());
    assert!(!lines.contains("czNjcjN0") && !lines.contains("bjN3"), "{lines}");
    let groups = regroup(&[Some(&d.diff)], false);
    assert_eq!(texts(&groups), ["Secret db key password changed"]);
}

/// The same change in several Applications is one line with all of them; a change in only
/// some says which; an Application not read (its error) is in none.
#[test]
fn the_same_change_across_applications_is_shown_once() {
    let diffs = [env("dev", "1.16", 3), env("staging", "1.16", 5), env("prod", "1.16", 5), env("canary", "1.17", 3)];
    let mut read: Vec<Option<&SmartDiff>> = diffs.iter().map(|d| Some(&d.diff)).collect();
    read.insert(1, None);
    let groups = regroup(&read, false);
    assert_eq!(
        texts(&groups),
        [
            "image web 1.15 → 1.16",
            "image web 1.15 → 1.17",
            "replicas 3 → 5",
            "ConfigMap app-config key LOG_LEVEL changed"
        ],
        "images first, the one in the most Applications first"
    );
    assert_eq!(apps(&groups, "image web 1.15 → 1.16"), [0, 2, 3]);
    assert_eq!(apps(&groups, "image web 1.15 → 1.17"), [4]);
    assert_eq!(apps(&groups, "replicas 3 → 5"), [2, 3]);
    assert_eq!(apps(&groups, "ConfigMap app-config key LOG_LEVEL changed"), [0, 2, 3, 4]);
    // Each Application's resources where: the Deployment, its own in each namespace.
    let image = &groups[0];
    for (app, resources) in &image.apps {
        let diff = read[*app].unwrap();
        let names: Vec<String> = resources.iter().map(|r| diff.resources[*r].key.label()).collect();
        assert_eq!(names.len(), 1);
        assert!(names[0].starts_with("Deployment ") && names[0].ends_with("/web"), "{names:?}");
    }
    assert_eq!(
        summary(&groups, 4).unwrap(),
        "4 changes in 4 environments: image web 1.15 → 1.16 (3) · image web 1.15 → 1.17 (1) · replicas 3 → 5 (2) \
         · ConfigMap app-config key LOG_LEVEL changed (4)"
    );
    assert_eq!(summary(&[], 0), None);
    // The noise, by rule: the checksum annotation and the status, in every one of them.
    let noise = regroup(&read, true);
    assert!(texts(&noise).contains(&"noise: checksum annotation"), "{:?}", texts(&noise));
    assert_eq!(apps(&noise, "noise: checksum annotation"), [0, 2, 3, 4]);
}

#[test]
fn a_change_of_a_resources_own_is_the_same_only_in_a_resource_of_that_name() {
    let one = AppDiff::of(&json!({"items": [
        res("", "ConfigMap", "a", "app-config", Some(config("a", "app-config", "info")),
            Some(config("a", "app-config", "debug"))),
    ]}));
    let other = AppDiff::of(&json!({"items": [
        res("", "ConfigMap", "b", "worker-config", Some(config("b", "worker-config", "info")),
            Some(config("b", "worker-config", "debug"))),
    ]}));
    let groups = regroup(&[Some(&one.diff), Some(&other.diff)], false);
    assert_eq!(
        texts(&groups),
        ["ConfigMap app-config key LOG_LEVEL changed", "ConfigMap worker-config key LOG_LEVEL changed"]
    );
}

#[test]
fn a_configmap_regenerated_in_each_environment_is_one_change() {
    let regenerated = |ns: &str, old: &str, new: &str| {
        AppDiff::of(&json!({"items": [
            res("", "ConfigMap", ns, old, Some(config(ns, old, "info")), None),
            res("", "ConfigMap", ns, new, None, Some(config(ns, new, "debug"))),
        ]}))
    };
    let a = regenerated("a", "app-config-bbbbbbbbbb", "app-config-cccccccccc");
    let b = regenerated("b", "app-config-dddddddddd", "app-config-ffffffffff");
    let groups = regroup(&[Some(&a.diff), Some(&b.diff)], false);
    assert_eq!(apps(&groups, "ConfigMap app-config regenerated under a new name"), [0, 1], "{:?}", texts(&groups));
    assert_eq!(apps(&groups, "ConfigMap app-config key LOG_LEVEL changed"), [0, 1]);
}

#[test]
fn nothing_to_sync_and_noise_only_are_told_apart() {
    let same = deploy("a", "1.15", 3);
    let synced =
        AppDiff::of(&json!({"items": [res("apps", "Deployment", "a", "web", Some(same.clone()), Some(same))]}));
    assert!(synced.in_sync() && !synced.noise_only());
    assert_eq!(synced.same, 1);
    let stamped = |chart: &str| {
        let mut d = deploy("a", "1.15", 3);
        d["metadata"]["labels"] = json!({"helm.sh/chart": chart});
        d
    };
    let noise = AppDiff::of(&json!({"items": [
        res("apps", "Deployment", "a", "web", Some(stamped("web-1.1.0")), Some(stamped("web-1.2.0")))
    ]}));
    assert!(noise.noise_only() && !noise.in_sync());
    assert!(regroup(&[Some(&noise.diff)], false).is_empty());
}

/// A future that waits once before it is ready.
struct YieldOnce(bool);

impl Future for YieldOnce {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

#[test]
fn reads_run_a_few_at_a_time_and_one_failing_spares_the_others() {
    let (running, most) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let stream = read_all(20, |i| {
        let (running, most) = (running.clone(), most.clone());
        async move {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            YieldOnce(false).await;
            running.fetch_sub(1, Ordering::SeqCst);
            if i % 3 == 0 { Err(format!("app {i} failed")) } else { Ok(i) }
        }
    });
    let mut got: Vec<(usize, Result<usize, String>)> = futures::executor::block_on(stream.collect());
    got.sort_by_key(|(i, _)| *i);
    assert_eq!(got.len(), 20, "every Application answers, failed or not");
    for (i, r) in &got {
        assert_eq!(r.is_err(), i % 3 == 0);
    }
    assert_eq!(most.load(Ordering::SeqCst), PARALLEL);
}

#[test]
fn a_diff_is_kept_by_application_and_version() {
    let reads = AtomicUsize::new(0);
    // Counted when it runs: a read kept is never awaited.
    let read = || async {
        reads.fetch_add(1, Ordering::SeqCst);
        Ok::<_, String>(Arc::new(AppDiff::default()))
    };
    let key = |v: &str| ("argocd.example.com".to_string(), "argocd".to_string(), "kept-web".to_string(), v.to_string());
    let run = |k, f| futures::executor::block_on(cached(k, f));
    run(key("10"), read()).unwrap();
    run(key("10"), read()).unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 1, "the same version is kept");
    run(key("11"), read()).unwrap();
    run(key("10"), read()).unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 3, "a new version reads again, and replaces the old one");
    run(key(""), read()).unwrap();
    run(key(""), read()).unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 5, "no version known: always read");
    // A failed read isn't kept.
    let failed = futures::executor::block_on(cached(key("12"), async { Err::<Arc<AppDiff>, _>("down".to_string()) }));
    assert!(failed.is_err());
    run(key("12"), read()).unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 6);
}
