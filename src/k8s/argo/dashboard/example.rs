//! A made-up service, `shop`, deployed to three made-up environments: no real data. The
//! developer tools' snapshots show its dashboard (`RECOUVRANCE_ACT=page:argo,argo-example`),
//! and the tests read its cards.
//!
//! - `example-staging`: synced and healthy, deployed by auto-sync;
//! - `example-prod`: out of sync (auto-sync off), Degraded: a worker crash-loops, another's
//!   pod is Pending, an autoscaler has no metric; the last deployment bumped two images;
//! - `example-sandbox`: its last sync failed (an invalid Deployment).

use jiff::Timestamp;
use serde_json::{Value, json};

use super::{Commit, Read};
use crate::history::DiffLine;
use crate::k8s::argo::diagnosis::Res;
use crate::k8s::argo::server::{Change, GitDiff, Login, ResourceDiff};

/// The environments, as Application names.
pub const ENVIRONMENTS: [&str; 3] = ["example-staging", "example-prod", "example-sandbox"];

const REGISTRY: &str = "registry.example.com/shop";
/// The tag of `api` and `worker` running; `worker-mail` stays on its own.
const TAG: &str = "1.5.0";
const MAIL_TAG: &str = "1.4.1";

fn at(now: Timestamp, secs_ago: i64) -> String {
    Timestamp::from_second(now.as_second() - secs_ago).unwrap_or(now).to_string()
}

fn image(component: &str, tag: &str) -> String {
    format!("{REGISTRY}/{component}:{tag}")
}

/// A deployment of the history, pinning `tag` for `api` and `worker` (kustomize images).
fn deployment(now: Timestamp, id: i64, secs_ago: i64, tag: &str, by: Value) -> Value {
    json!({
        "id": id,
        "revision": format!("{id:x}0c4f1e2d3b4a5968778695a4b3c2d1e0f9a8b7c6").chars().take(40).collect::<String>(),
        "deployedAt": at(now, secs_ago),
        "initiatedBy": by,
        "source": {
            "repoURL": "https://git.example.com/shop/deploy.git",
            "path": "overlays/env",
            "kustomize": {"images": [image("api", tag), image("worker", tag), image("worker-mail", MAIL_TAG)]},
        },
    })
}

/// The Application `env`, as Argo CD's API would send it.
pub fn app(env: &str, now: Timestamp) -> Value {
    let user = json!({"username": "dev-user"});
    let auto = json!({"automated": true});
    let prod = env == "example-prod";
    let sandbox = env == "example-sandbox";
    let history: Vec<Value> = if prod {
        vec![
            deployment(now, 11, 9 * 86400, "1.3.0", user.clone()),
            deployment(now, 12, 6 * 86400, "1.3.2", user.clone()),
            deployment(now, 13, 4 * 86400, "1.4.0", user.clone()),
            deployment(now, 14, 2 * 86400, "1.4.1", user.clone()),
            deployment(now, 15, 2 * 3600, "1.5.0", user.clone()),
        ]
    } else {
        (0..6).map(|i| deployment(now, 30 + i, (6 - i) * 86400, "1.5.0", auto.clone())).collect()
    };
    let resource = |kind: &str, group: &str, name: &str, sync: &str, health: Option<&str>| {
        let mut r = json!({"group": group, "version": "v1", "kind": kind, "namespace": "shop", "name": name,
            "status": sync});
        if let Some(h) = health {
            r["health"] = json!({"status": h});
        }
        r
    };
    let (sync, health) = if prod { ("OutOfSync", "Degraded") } else { ("Synced", "Healthy") };
    let mut resources = vec![
        resource("Deployment", "apps", "api", "Synced", Some("Healthy")),
        resource("Deployment", "apps", "worker-crawl", "Synced", Some(if prod { "Degraded" } else { "Healthy" })),
        resource("Deployment", "apps", "worker-mail", "Synced", Some(if prod { "Progressing" } else { "Healthy" })),
        resource("Service", "", "api", "Synced", Some("Healthy")),
        resource("ConfigMap", "", "shop-config", if prod { "OutOfSync" } else { "Synced" }, None),
    ];
    if prod {
        resources.push(resource("Service", "", "metrics", "OutOfSync", Some("Missing")));
        let mut old = resource("ConfigMap", "", "legacy-flags", "OutOfSync", None);
        old["requiresPruning"] = json!(true);
        resources.push(old);
        resources.push(resource("HorizontalPodAutoscaler", "autoscaling", "worker-crawl", "Synced", Some("Degraded")));
    }
    let step = |kind: &str, group: &str, name: &str, status: &str, message: &str| {
        json!({"group": group, "version": "v1", "kind": kind, "namespace": "shop", "name": name,
            "status": status, "message": message, "syncPhase": "Sync"})
    };
    let last_revision = history.last().map_or(Value::Null, |h| h["revision"].clone());
    let operation_state = if sandbox {
        let failed = "one or more objects failed to apply, reason: Deployment.apps \"api\" is invalid: \
             spec.template.spec.containers[0].image: Required value";
        let mut api = step("Deployment", "apps", "api", "SyncFailed", failed);
        api["syncPhase"] = json!("Sync");
        let started = at(now, 20 * 60 + 40);
        let finished = at(now, 20 * 60);
        let resources = vec![step("ConfigMap", "", "shop-config", "Synced", "configmap/shop-config configured"), api];
        json!({
            "phase": "Failed",
            "message": failed,
            "startedAt": started,
            "finishedAt": finished,
            "operation": {"initiatedBy": user, "sync": {"revision": "9f8e7d6c5b4a39281706f5e4d3c2b1a098765432"}},
            "syncResult": {"revision": "9f8e7d6c5b4a39281706f5e4d3c2b1a098765432", "resources": resources},
        })
    } else {
        let started = at(now, if prod { 2 * 3600 + 48 } else { 86400 + 31 });
        let finished = at(now, if prod { 2 * 3600 } else { 86400 });
        let by = if prod { user } else { auto };
        let resources = vec![
            step("ConfigMap", "", "shop-config", "Synced", "configmap/shop-config configured"),
            step("Deployment", "apps", "api", "Synced", "deployment.apps/api configured"),
            step("Deployment", "apps", "worker-crawl", "Synced", "deployment.apps/worker-crawl configured"),
            step("Deployment", "apps", "worker-mail", "Synced", "deployment.apps/worker-mail configured"),
        ];
        json!({
            "phase": "Succeeded",
            "message": "successfully synced (all tasks run)",
            "startedAt": started,
            "finishedAt": finished,
            "operation": {"initiatedBy": by, "sync": {}},
            "syncResult": {"revision": last_revision, "resources": resources},
        })
    };
    let policy = if prod { json!({}) } else { json!({"automated": {"prune": true}}) };
    let images = [image("api", TAG), image("worker", TAG), image("worker-mail", MAIL_TAG)];
    json!({
        "metadata": {"name": env, "namespace": "argocd", "resourceVersion": "4242",
            "labels": {"app.kubernetes.io/part-of": "shop"}},
        "spec": {
            "project": "shop",
            "destination": {"name": env, "namespace": "shop"},
            "source": {"repoURL": "https://git.example.com/shop/deploy.git", "path": "overlays/env"},
            "syncPolicy": policy,
        },
        "status": {
            "sync": {"status": sync, "revision": last_revision},
            "health": {"status": health},
            "history": history,
            "operationState": operation_state,
            "resources": resources,
            "summary": {"images": images},
        },
    })
}

fn node(kind: &str, group: &str, name: &str, uid: &str, parent: Option<&str>, health: Option<(&str, &str)>) -> Value {
    let mut n = json!({"group": group, "version": "v1", "kind": kind, "namespace": "shop", "name": name, "uid": uid});
    if let Some(p) = parent {
        n["parentRefs"] = json!([{"uid": p, "kind": "ReplicaSet", "namespace": "shop", "name": p}]);
    }
    if let Some((status, message)) = health {
        n["health"] = json!({"status": status, "message": message});
    }
    n
}

fn pod(name: &str, rs: &str, reason: &str, health: &str, message: &str, created: String) -> Value {
    let mut p = node("Pod", "", name, name, Some(rs), Some((health, message)));
    p["info"] = json!([{"name": "Status Reason", "value": reason}, {"name": "Containers", "value": "0/1"}]);
    p["createdAt"] = json!(created);
    p
}

/// The resource tree of `env`: two pods of `api`, eight of `worker-crawl` and two of
/// `worker-mail`, in trouble in `example-prod`.
pub fn tree(env: &str, now: Timestamp) -> Value {
    let prod = env == "example-prod";
    let mut nodes = vec![
        node("Deployment", "apps", "api", "d-api", None, Some(("Healthy", ""))),
        node("ReplicaSet", "apps", "api-6c9f", "rs-api", Some("d-api"), Some(("Healthy", ""))),
        node("Deployment", "apps", "worker-crawl", "d-crawl", None, Some(("Healthy", ""))),
        node("ReplicaSet", "apps", "worker-crawl-7d4b", "rs-crawl", Some("d-crawl"), Some(("Healthy", ""))),
        node("Deployment", "apps", "worker-mail", "d-mail", None, Some(("Healthy", ""))),
        node("ReplicaSet", "apps", "worker-mail-5f2a", "rs-mail", Some("d-mail"), Some(("Healthy", ""))),
        node("Service", "", "api", "s-api", None, Some(("Healthy", ""))),
    ];
    let ok = |name: &str, rs: &str| pod(name, rs, "Running", "Healthy", "", at(now, 3 * 86400));
    nodes.extend((0..2).map(|i| ok(&format!("api-6c9f-{i}"), "rs-api")));
    for i in 0..8 {
        let name = format!("worker-crawl-7d4b-{i}");
        nodes.push(if prod && i < 2 {
            pod(
                &name,
                "rs-crawl",
                "CrashLoopBackOff",
                "Degraded",
                "back-off 5m0s restarting failed container=worker",
                at(now, 12 * 60),
            )
        } else {
            ok(&name, "rs-crawl")
        });
    }
    nodes.push(ok("worker-mail-5f2a-0", "rs-mail"));
    nodes.push(if prod {
        pod(
            "worker-mail-5f2a-1",
            "rs-mail",
            "Pending",
            "Progressing",
            "0/6 nodes are available: insufficient memory",
            at(now, 4 * 60),
        )
    } else {
        ok("worker-mail-5f2a-1", "rs-mail")
    });
    if prod {
        nodes[2]["health"] = json!({"status": "Degraded", "message": "Deployment has 6 of 8 replicas available"});
        nodes.push(node(
            "HorizontalPodAutoscaler",
            "autoscaling",
            "worker-crawl",
            "hpa-crawl",
            None,
            Some(("Degraded", "No metric: the metrics API answered nothing for worker-crawl")),
        ));
    }
    json!({"nodes": nodes})
}

/// Argo CD's diff of `env`: in `example-prod`, a ConfigMap changed, a Service to create and a
/// ConfigMap to prune.
pub fn diff(env: &str) -> GitDiff {
    let res = |kind: &str, name: &str| Res {
        kind: kind.into(),
        namespace: "shop".into(),
        name: name.into(),
        version: "v1".into(),
        ..Default::default()
    };
    let changed = if env == "example-prod" {
        vec![
            ResourceDiff {
                res: res("ConfigMap", "shop-config"),
                change: Change::Modified,
                lines: vec![
                    DiffLine::Same("data:".into()),
                    DiffLine::Removed("  CRAWL_BATCH: \"50\"".into()),
                    DiffLine::Added("  CRAWL_BATCH: \"100\"".into()),
                ],
                more_lines: 0,
            },
            ResourceDiff {
                res: res("Service", "metrics"),
                change: Change::Created,
                lines: vec![
                    DiffLine::Added("kind: Service".into()),
                    DiffLine::Added("metadata: {name: metrics}".into()),
                ],
                more_lines: 0,
            },
            ResourceDiff {
                res: res("ConfigMap", "legacy-flags"),
                change: Change::Pruned,
                lines: vec![DiffLine::Removed("kind: ConfigMap".into())],
                more_lines: 0,
            },
        ]
    } else {
        Vec::new()
    };
    GitDiff { server: "argocd.example.com".into(), login: Login::Cli, changed, more: 0, same: 9, hooks: 0 }
}

/// Everything read for `env`, as the dashboard reads it.
pub fn read(env: &str, now: Timestamp) -> Read {
    let commit = Commit {
        author: Some("Dev User <dev-user@example.com>".into()),
        date: Some(Timestamp::from_second(now.as_second() - 3 * 3600).unwrap_or(now)),
        message: "Bump shop to 1.5.0".into(),
        tags: vec!["v1.5.0".into()],
    };
    Read {
        app: app(env, now),
        tree: Some(Ok(tree(env, now))),
        diff: Some(Ok(diff(env))),
        commits: vec![Some(Ok(commit))],
        note: None,
    }
}
