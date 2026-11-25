//! A made-up sync journal, no real data: the developer tools' snapshots show it
//! (`RECOUVRANCE_ACT=page:argo,argo-journal-example`).
//!
//! - `shop-prod`: a sync that timed out after 30 min, 440 steps: 437 applied, then the
//!   PostSync wave never came;
//! - `shop-staging`: a sync running, its PreSync migration still running;
//! - `shop-sandbox`: a sync that failed on an invalid Deployment.

use super::{Loaded, Operation, Record, Seen, Step};

const INSTANCE: &str = "argocd.example.com";

fn step(kind: &str, group: &str, name: &str, status: &str, message: &str, phase: &str) -> Step {
    Step {
        group: group.into(),
        kind: kind.into(),
        namespace: "shop".into(),
        name: name.into(),
        status: status.into(),
        message: message.into(),
        sync_phase: phase.into(),
        ..Default::default()
    }
}

fn hook(name: &str, hook_phase: &str, hook_type: &str, message: &str) -> Step {
    Step {
        group: "batch".into(),
        kind: "Job".into(),
        namespace: "shop".into(),
        name: name.into(),
        hook_phase: hook_phase.into(),
        hook_type: hook_type.into(),
        sync_phase: hook_type.into(),
        message: message.into(),
        ..Default::default()
    }
}

fn at(now: i64, ago: i64) -> String {
    jiff::Timestamp::from_second(now - ago).unwrap_or(jiff::Timestamp::UNIX_EPOCH).to_string()
}

fn record(now: i64, app: &str, started_ago: i64, finished_ago: Option<i64>, phase: &str, message: &str) -> Record {
    Record {
        seen: now - finished_ago.unwrap_or(0),
        instance: INSTANCE.into(),
        namespace: "argocd".into(),
        name: app.into(),
        started: at(now, started_ago),
        finished: finished_ago.map(|f| at(now, f)),
        phase: phase.into(),
        message: message.into(),
        by: Some("dev-user".into()),
        revisions: vec!["9f8e7d6c5b4a39281706f5e4d3c2b1a098765432".into()],
        ..Default::default()
    }
}

/// The applied steps of a big Application: `n` of each kind, `kind/name serverside-applied`.
fn applied(kinds: &[(&str, &str, usize)]) -> Vec<Step> {
    kinds
        .iter()
        .flat_map(|&(kind, group, n)| {
            (0..n).map(move |i| {
                let name = format!("{}-{i}", kind.to_lowercase());
                let lower = kind.to_lowercase();
                let what = if group.is_empty() { lower } else { format!("{lower}.{group}") };
                step(kind, group, &name, "Synced", &format!("{what}/{name} serverside-applied"), "Sync")
            })
        })
        .collect()
}

/// The journal, as of `now` (seconds).
pub fn loaded(now: i64) -> Loaded {
    // Timed out: the PreSync hook and 436 steps applied, the PostSync wave never ran.
    let mut timeout = record(
        now,
        "shop-prod",
        34 * 60,
        Some(4 * 60),
        "Failed",
        "Operation terminated, triggered by controller sync timeout (retried 5 times).",
    );
    timeout.resources = vec![hook("db-migrate", "Succeeded", "PreSync", "Reached expected number of succeeded pods")];
    timeout.resources.extend(applied(&[
        ("ConfigMap", "", 120),
        ("Secret", "", 60),
        ("ExternalSecret", "external-secrets.io", 50),
        ("Service", "", 40),
        ("Deployment", "apps", 38),
        ("ServiceAccount", "", 30),
        ("Role", "rbac.authorization.k8s.io", 30),
        ("RoleBinding", "rbac.authorization.k8s.io", 30),
        ("NetworkPolicy", "networking.k8s.io", 20),
        ("PodDisruptionBudget", "policy", 18),
    ]));
    timeout.resources.push(step("StatefulSet", "apps", "search", "", "", "Sync"));
    timeout.resources.push(step("Deployment", "apps", "search-proxy", "", "", "Sync"));
    timeout.resources.push(hook("smoke-test", "", "PostSync", ""));
    let mut seen: Vec<Seen> =
        timeout.resources.iter().map(|_| Seen { first: now - 33 * 60, settled: now - 32 * 60 }).collect();
    let n = seen.len();
    for s in &mut seen[n - 3..] {
        *s = Seen { first: now - 32 * 60, settled: now - 32 * 60 };
    }
    let timeout = Operation { last: timeout, states: 9, seen };

    // Running: its PreSync migration for 2 min, the rest waiting.
    let mut running =
        record(now, "shop-staging", 150, None, "Running", "waiting for completion of hook batch/Job/db-migrate");
    running.resources = vec![hook("db-migrate", "Running", "PreSync", "")];
    running.resources.extend(
        applied(&[("ConfigMap", "", 30), ("Deployment", "apps", 15), ("Service", "", 10)]).into_iter().map(|s| Step {
            status: String::new(),
            message: String::new(),
            ..s
        }),
    );
    let seen = running.resources.iter().map(|_| Seen { first: now - 140, settled: now - 140 }).collect();
    let running = Operation { last: running, states: 3, seen };

    // Failed: an invalid Deployment.
    let failed = "one or more objects failed to apply, reason: Deployment.apps \"api\" is invalid: \
                  spec.template.spec.containers[0].image: Required value";
    let mut sandbox = record(now, "shop-sandbox", 3 * 3600 + 40, Some(3 * 3600), "Failed", failed);
    sandbox.resources = vec![
        step("ConfigMap", "", "shop-config", "Synced", "configmap/shop-config configured", "Sync"),
        step("Deployment", "apps", "api", "SyncFailed", failed, "Sync"),
    ];
    let seen = vec![Seen { first: now - 3 * 3600 - 30, settled: now - 3 * 3600 - 30 }; 2];
    let sandbox = Operation { last: sandbox, states: 2, seen };
    Loaded { ops: vec![running, timeout, sandbox], bad: 0 }
}
