//! Argo CD Applications that need attention, shown under the cluster they deploy to: Degraded,
//! a failed sync, an error Argo CD reports, a sync running for long, and resources Missing
//! while auto-sync is on. The Applications are read on their hub, often not a cluster open.

use kube::Client;

use super::{OnHub, Problem, Scan, Severity};
use crate::k8s;
use crate::k8s::argo::{self, LeanApp};

/// What a row says the kind is.
pub const LABEL: &str = "Argo CD Application";

/// The longest message of Argo CD's kept in a reason.
const MESSAGE_CHARS: usize = 200;

/// The check's name among a scan's checks.
pub const CHECK: &str = "Argo CD Applications";

/// "what: message", the message on one line and cut to `MESSAGE_CHARS`.
fn with_message(what: &str, message: Option<&str>) -> String {
    let Some(message) = message.map(str::trim).filter(|m| !m.is_empty()) else { return what.to_string() };
    let line = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let short = if line.chars().count() > MESSAGE_CHARS {
        format!("{}…", line.chars().take(MESSAGE_CHARS - 1).collect::<String>())
    } else {
        line
    };
    format!("{what}: {short}")
}

/// Whether an Application needs attention, and why: Degraded is critical; a failed sync, a
/// condition whose type ends in `Error` (`ComparisonError`, `SyncError`, `InvalidSpecError`...),
/// a sync running for more than 10 minutes and Missing with auto-sync on are warnings. Missing
/// with auto-sync off is how an Application synced by hand shows what it has not deployed yet,
/// and out of sync is how it waits for its next sync: neither counts then (out of sync never
/// does). One being deleted is the stuck-deletion check's. None: nothing to look at.
pub fn judge(app: &LeanApp, now: jiff::Timestamp) -> Option<(Severity, String)> {
    if app.metadata.deleted.is_some() {
        return None;
    }
    // Not looked at by Argo CD yet.
    let status = app.status.as_ref()?;
    let mut worst: Option<Severity> = None;
    let mut why: Vec<String> = Vec::new();
    let mut add = |severity: Severity, text: String| {
        worst = Some(worst.map_or(severity, |w| w.min(severity)));
        why.push(text);
    };
    let health = status.health.as_ref();
    let message = health.and_then(|h| h.message.as_deref());
    match health.and_then(|h| h.status.as_deref()) {
        Some("Degraded") => add(Severity::Critical, with_message("Degraded", message)),
        Some("Missing") if app.automated() => add(Severity::Warning, with_message("Missing", message)),
        _ => {}
    }
    if let Some(op) = &status.operation {
        let phase = op.phase.as_deref().unwrap_or_default();
        if argo::operation_tone(phase) == crate::status::Tone::Bad {
            add(Severity::Warning, with_message("last sync failed", op.message.as_deref()));
        } else if matches!(phase, "Running" | "Terminating") {
            let started = op.started.as_deref().and_then(|t| t.parse::<jiff::Timestamp>().ok());
            if let Some(secs) = started.map(|t| now.as_second() - t.as_second())
                && secs >= argo::diagnosis::STUCK_AFTER
            {
                add(Severity::Warning, format!("sync running for {}", k8s::duration_text(secs)));
            }
        }
    }
    for c in status.conditions.iter().flatten() {
        if let Some(kind) = c.kind.as_deref().filter(|k| k.ends_with("Error")) {
            add(Severity::Warning, with_message(kind, c.message.as_deref()));
        }
    }
    Some((worst?, why.join("; ")))
}

/// The Applications that need attention among those `hub` holds that deploy to the cluster of
/// `context`, in `namespaces` (all when empty), each under the namespace it deploys into.
fn found_on(apps: &[LeanApp], hub: &str, context: &str, namespaces: &[String], now: jiff::Timestamp) -> Vec<Problem> {
    let targets = argo::targets();
    let (hub_target, here) = (argo::target(&targets, hub), argo::target(&targets, context));
    apps.iter()
        .filter(|a| a.deploys_to(&hub_target, &here))
        .filter(|a| a.dest_namespace().is_none_or(|ns| namespaces.is_empty() || namespaces.iter().any(|n| n == ns)))
        .filter_map(|a| {
            let (severity, mut reason) = judge(a, now)?;
            // The row is under the cluster it deploys to: say where Argo CD is when elsewhere.
            if hub != context {
                reason.push_str(&format!(" (Argo CD on {hub})"));
            }
            Some(Problem {
                severity,
                kind: argo::application_kind().kubectl_name(),
                label: LABEL.into(),
                namespace: a.dest_namespace().map(String::from),
                name: a.metadata.name.clone(),
                reason,
                age: a.metadata.created.as_deref().and_then(|t| t.parse().ok()),
                on_hub: Some(OnHub { context: hub.to_string(), namespace: a.metadata.namespace.clone() }),
            })
        })
        .collect()
}

/// The check of a scan of `context` (`client`: its own; `serves`: it serves Applications): the
/// Applications of every hub known (see `argo::problem_hubs`) that deploy there. A hub that
/// can't be read is named, and doesn't hide the others.
pub async fn scan(context: String, client: Client, serves: bool, namespaces: Vec<String>) -> Scan {
    let hubs = argo::problem_hubs(&context, &client, serves);
    let lists = futures::future::join_all(hubs.iter().map(argo::lean_applications)).await;
    let now = jiff::Timestamp::now();
    let mut scan = Scan::default();
    for (hub, list) in hubs.iter().zip(lists) {
        match list {
            Ok(apps) => scan.problems.extend(found_on(&apps, &hub.context, &context, &namespaces, now)),
            Err(e) => scan.skipped.push((format!("{CHECK} on {}", hub.context), e)),
        }
    }
    scan
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::{Value, json};

    use super::*;

    fn app(v: &Value) -> LeanApp {
        serde_json::from_value(v.clone()).unwrap()
    }

    fn ago(secs: i64) -> String {
        jiff::Timestamp::from_second(jiff::Timestamp::now().as_second() - secs).unwrap().to_string()
    }

    /// An Application with auto-sync on or off, the given health, operation and conditions.
    fn of(automated: bool, health: &str, operation: Option<Value>, conditions: &[(&str, &str)]) -> Value {
        let mut v = json!({
            "metadata": {"name": "web", "namespace": "argocd"},
            "spec": {"destination": {"server": "https://kubernetes.default.svc", "namespace": "web"}},
            "status": {
                "sync": {"status": "Synced"},
                "health": {"status": health},
                "conditions": conditions.iter().map(|(t, m)| json!({"type": t, "message": m})).collect::<Vec<_>>(),
            },
        });
        if automated {
            v["spec"]["syncPolicy"] = json!({"automated": {"prune": true}});
        }
        if let Some(op) = operation {
            v["status"]["operationState"] = op;
        }
        v
    }

    /// A rule's case: its name, the Application, and the verdict wanted.
    type Case = (&'static str, Value, Option<(Severity, &'static str)>);

    #[test]
    fn the_rules() {
        let now = jiff::Timestamp::now();
        let failed = Some(json!({"phase": "Failed", "message": "one or more objects failed to apply"}));
        let error = Some(json!({"phase": "Error", "message": "rpc error"}));
        let long = Some(json!({"phase": "Running", "startedAt": ago(25 * 60 + 30)}));
        let short = Some(json!({"phase": "Running", "startedAt": ago(60)}));
        let ok = Some(json!({"phase": "Succeeded"}));
        let cases: Vec<Case> = vec![
            ("healthy", of(true, "Healthy", ok, &[]), None),
            ("degraded", of(false, "Degraded", None, &[]), Some((Severity::Critical, "Degraded"))),
            ("missing, auto-sync on", of(true, "Missing", None, &[]), Some((Severity::Warning, "Missing"))),
            ("missing, synced by hand", of(false, "Missing", None, &[]), None),
            ("progressing", of(true, "Progressing", None, &[]), None),
            ("suspended", of(false, "Suspended", None, &[]), None),
            (
                "failed sync",
                of(false, "Healthy", failed, &[]),
                Some((Severity::Warning, "last sync failed: one or more objects failed to apply")),
            ),
            ("sync error", of(true, "Healthy", error, &[]), Some((Severity::Warning, "last sync failed: rpc error"))),
            (
                "sync running long",
                of(true, "Progressing", long, &[]),
                Some((Severity::Warning, "sync running for 25m")),
            ),
            ("sync just started", of(true, "Progressing", short, &[]), None),
            (
                "comparison error",
                of(false, "Healthy", None, &[("ComparisonError", "repo not found")]),
                Some((Severity::Warning, "ComparisonError: repo not found")),
            ),
            ("a warning condition", of(false, "Healthy", None, &[("SyncWarning", "x")]), None),
            ("an orphaned resource", of(false, "Healthy", None, &[("OrphanedResourceWarning", "x")]), None),
            (
                "degraded and an error",
                of(true, "Degraded", None, &[("InvalidSpecError", "bad\n  path")]),
                Some((Severity::Critical, "Degraded; InvalidSpecError: bad path")),
            ),
        ];
        for (name, v, want) in cases {
            let got = judge(&app(&v), now);
            assert_eq!(got.as_ref().map(|(s, r)| (*s, r.as_str())), want, "{name}");
        }
        // Auto-sync turned off with `enabled: false` is off.
        let mut v = of(true, "Missing", None, &[]);
        v["spec"]["syncPolicy"]["automated"]["enabled"] = json!(false);
        assert_eq!(judge(&app(&v), now), None);
        // Being deleted: the stuck-deletion check says it.
        let mut v = of(true, "Degraded", None, &[]);
        v["metadata"]["deletionTimestamp"] = json!(ago(60));
        assert_eq!(judge(&app(&v), now), None);
        // Not looked at yet.
        assert_eq!(judge(&app(&json!({"metadata": {"name": "new"}, "spec": {}})), now), None);
    }

    #[test]
    fn long_messages_are_cut() {
        let long = "x".repeat(500);
        let reason = with_message("Degraded", Some(&long));
        assert_eq!(reason.chars().count(), "Degraded: ".len() + MESSAGE_CHARS);
        assert!(reason.ends_with('…'));
    }

    #[test]
    fn rows_go_under_the_cluster_and_namespaces_they_deploy_to() {
        let now = jiff::Timestamp::now();
        let mut elsewhere = of(true, "Degraded", None, &[]);
        elsewhere["metadata"]["name"] = json!("other");
        elsewhere["spec"]["destination"] = json!({"server": "https://other.example:6443", "namespace": "web"});
        let mut api = of(true, "Degraded", None, &[]);
        api["metadata"]["name"] = json!("api");
        api["spec"]["destination"]["namespace"] = json!("api");
        let apps = [app(&of(true, "Degraded", None, &[])), app(&elsewhere), app(&api)];
        let found = found_on(&apps, "hub", "hub", &[], now);
        let names: Vec<(&str, Option<&str>)> =
            found.iter().map(|p| (p.name.as_str(), p.namespace.as_deref())).collect();
        assert_eq!(names, [("web", Some("web")), ("api", Some("api"))]);
        assert_eq!(found[0].on_hub, Some(OnHub { context: "hub".into(), namespace: "argocd".into() }));
        assert_eq!(found[0].label, LABEL);
        assert_eq!(found[0].reason, "Degraded");
        let found = found_on(&apps, "hub", "hub", &["api".into()], now);
        assert_eq!(found.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["api"]);
    }

    /// How auto-sync may be off: no sync policy, none automated, or turned off.
    fn auto_sync_off() -> impl Strategy<Value = Option<Value>> {
        prop::sample::select(vec![
            None,
            Some(json!({})),
            Some(json!({"automated": null})),
            Some(json!({"automated": {"enabled": false, "prune": true}})),
            Some(json!({"syncOptions": ["CreateNamespace=true"]})),
        ])
    }

    proptest! {
        /// Out of sync never counts on an Application with auto-sync off: it is judged as if it
        /// were synced, and one otherwise fine (Missing included) is never a problem.
        #[test]
        fn out_of_sync_by_hand_never_counts(
            policy in auto_sync_off(),
            health in prop::sample::select(vec!["Healthy", "Missing", "Progressing", "Suspended", "Unknown", "Degraded"]),
            phase in prop::option::of(prop::sample::select(vec!["Succeeded", "Running", "Failed", "Error", "Terminating"])),
            started in 0..7200i64,
            condition in prop::option::of(prop::sample::select(vec!["SyncWarning", "OrphanedResourceWarning", "ComparisonError", "SharedResourceWarning"])),
        ) {
            let now = jiff::Timestamp::now();
            let operation = phase.map(|p| json!({"phase": p, "startedAt": ago(started), "message": "m"}));
            let conditions: Vec<(&str, &str)> = condition.iter().map(|c| (*c, "m")).collect();
            let mut v = of(false, health, operation, &conditions);
            if let Some(policy) = policy {
                v["spec"]["syncPolicy"] = policy;
            }
            let synced = judge(&app(&v), now);
            v["status"]["sync"] = json!({"status": "OutOfSync"});
            let out_of_sync = judge(&app(&v), now);
            prop_assert_eq!(&out_of_sync, &synced);
            let failed = matches!(phase, Some("Failed" | "Error"));
            // A second's leeway: the clock moves between `ago` and `now`.
            let stuck = matches!(phase, Some("Running" | "Terminating")) && started + 1 >= argo::diagnosis::STUCK_AFTER;
            let error = condition.is_some_and(|c| c.ends_with("Error"));
            let troubled = health == "Degraded" || failed || stuck || error;
            if !troubled {
                prop_assert_eq!(out_of_sync, None);
            }
        }
    }
}
