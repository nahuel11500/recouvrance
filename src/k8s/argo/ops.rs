//! Operations on an Argo CD Application, written to it as argocd-server writes them: a sync
//! with options (some resources only, prune, dry run), a rollback to a past deployment, and
//! terminating the operation running. Argo CD's controller runs what is written to
//! `operation`; argocd-server's checks (no operation already running, no rollback while
//! auto-sync is on) are made here, on the Application as read right before the write.
//!
//! Nothing here sends anything: `k8s::argo_operation` reads the Application, asks [`patch`]
//! for the merge patch, and sends it with the caller's client (the write gate applies to the
//! window's).

use serde_json::{Map, Value, json};

use super::diagnosis::Res;
use super::{auto_sync, resources, short_revision, to_prune};

/// Who started an operation, as Argo CD records it in the Application's history.
pub const INITIATED_BY: &str = "recouvrance";
/// Resources named in a message, the rest counted.
const NAMED: usize = 20;

/// A sync, as the dialog or an agent asks for it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncRequest {
    /// Only these resources; empty syncs the whole Application (hooks run only then).
    pub resources: Vec<Res>,
    /// Delete what is no longer in git (of the resources picked, when some are).
    pub prune: bool,
    /// Argo CD applies nothing and reports what it would do.
    pub dry_run: bool,
}

/// What can be written to an Application's operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    Sync(SyncRequest),
    /// Back to the deployment of `status.history` with this id; None is the one before the
    /// newest, like `argocd app rollback` without an id.
    Rollback {
        id: Option<i64>,
        dry_run: bool,
    },
    /// Stop the operation running, like `argocd app terminate-op`.
    Terminate,
}

impl Op {
    pub fn dry_run(&self) -> bool {
        match self {
            Self::Sync(req) => req.dry_run,
            Self::Rollback { dry_run, .. } => *dry_run,
            Self::Terminate => false,
        }
    }
}

/// Whether an operation is running or waiting for the controller: `operation` is set until
/// the controller is done with it. argocd-server starts no other one then.
pub fn in_progress(app: &Value) -> bool {
    app.get("operation").is_some_and(|o| !o.is_null())
}

/// The resources out of sync that a sync can pick from, in Argo CD's order: hooks are not
/// applied on their own.
pub fn out_of_sync(app: &Value) -> Vec<Res> {
    resources(app).filter(|r| r["status"] == "OutOfSync" && r["hook"] != true).filter_map(Res::of).collect()
}

/// What a sync with prune deletes: every resource to prune, or those of `picked` when the
/// sync is of some resources only (Argo CD prunes nothing else then).
pub fn pruned(app: &Value, picked: &[Res]) -> Vec<Res> {
    resources(app)
        .filter(|r| to_prune(r))
        .filter_map(Res::of)
        .filter(|r| picked.is_empty() || picked.iter().any(|p| p.same(r)))
        .collect()
}

/// The merge patch that starts a sync (also shown as the kubectl equivalent). With no option,
/// what Argo CD's Sync button sends: the whole Application, hooks included.
pub fn sync_patch(req: &SyncRequest) -> Value {
    let mut sync = Map::new();
    sync.insert("syncStrategy".into(), json!({"hook": {}}));
    if req.prune {
        sync.insert("prune".into(), json!(true));
    }
    if req.dry_run {
        sync.insert("dryRun".into(), json!(true));
    }
    if !req.resources.is_empty() {
        let picked = req.resources.iter().map(resource_entry).collect();
        sync.insert("resources".into(), Value::Array(picked));
    }
    json!({"operation": {"initiatedBy": {"username": INITIATED_BY}, "sync": sync}})
}

/// A resource as `operation.sync.resources` names it (group and namespace left out when empty).
fn resource_entry(r: &Res) -> Value {
    let mut e = Map::new();
    if !r.group.is_empty() {
        e.insert("group".into(), json!(r.group));
    }
    e.insert("kind".into(), json!(r.kind));
    if !r.namespace.is_empty() {
        e.insert("namespace".into(), json!(r.namespace));
    }
    e.insert("name".into(), json!(r.name));
    Value::Object(e)
}

/// The merge patch that terminates the operation running: what argocd-server's
/// `TerminateOperation` writes. The Application kind keeps its status in the object (no
/// status subresource), so a patch of the object reaches it.
pub fn terminate_patch() -> Value {
    json!({"status": {"operationState": {"phase": "Terminating"}}})
}

/// The deployments of `status.history`, oldest first (by id, as Argo CD numbers them).
fn history(app: &Value) -> Vec<&Value> {
    let mut h: Vec<&Value> = app.pointer("/status/history").and_then(Value::as_array).into_iter().flatten().collect();
    h.sort_by_key(|e| e["id"].as_i64().unwrap_or(0));
    h
}

/// The deployment a rollback goes back to: the one with `id`, or the one before the newest.
pub fn rollback_target(app: &Value, id: Option<i64>) -> Result<&Value, String> {
    let h = history(app);
    match id {
        Some(id) => h
            .into_iter()
            .find(|e| e["id"].as_i64() == Some(id))
            .ok_or_else(|| format!("its history has no deployment {id} (Argo CD keeps the last 10 by default)")),
        None if h.len() >= 2 => Ok(h[h.len() - 2]),
        None => Err("its history has no deployment before the newest to roll back to".into()),
    }
}

/// A deployment's revision(s), short, for messages.
pub fn revision_text(entry: &Value) -> String {
    super::revisions(entry).unwrap_or_else(|| "-".into())
}

/// The merge patch that rolls back to `entry` (a deployment of `status.history`): what
/// argocd-server's Rollback writes. A sync of that deployment's revision(s) and source(s),
/// applied without hooks, with the Application's own sync options, and no prune.
pub fn rollback_patch(app: &Value, entry: &Value, dry_run: bool) -> Value {
    let mut sync = Map::new();
    let present = |v: &Value| !v.is_null() && v.as_str() != Some("") && v.as_array().is_none_or(|a| !a.is_empty());
    if present(&entry["revision"]) {
        sync.insert("revision".into(), entry["revision"].clone());
    }
    if present(&entry["revisions"]) {
        sync.insert("revisions".into(), entry["revisions"].clone());
    }
    // Several sources (`sources`), or the one (`source`): never both, as argocd-server does.
    if present(&entry["sources"]) {
        sync.insert("sources".into(), entry["sources"].clone());
    } else if entry["source"].as_object().is_some_and(|s| !s.is_empty()) {
        sync.insert("source".into(), entry["source"].clone());
    }
    sync.insert("syncStrategy".into(), json!({"apply": {}}));
    if let Some(options) = app.pointer("/spec/syncPolicy/syncOptions").filter(|o| present(o)) {
        sync.insert("syncOptions".into(), options.clone());
    }
    if dry_run {
        sync.insert("dryRun".into(), json!(true));
    }
    json!({"operation": {"initiatedBy": {"username": INITIATED_BY}, "sync": sync}})
}

/// Why argocd-server would refuse `op` on the Application as read now, if it would.
pub fn refusal(app: &Value, op: &Op) -> Option<String> {
    if app.pointer("/metadata/deletionTimestamp").is_some_and(|t| !t.is_null()) {
        return Some("the Application is being deleted".into());
    }
    match op {
        Op::Terminate => {
            let state = app.pointer("/status/operationState").is_some_and(|s| !s.is_null());
            if in_progress(app) && state { None } else { Some("no operation is in progress".to_string()) }
        }
        _ if in_progress(app) => {
            Some("another operation is already in progress: wait for it to end, or terminate it".into())
        }
        Op::Sync(req) => {
            let known: Vec<Res> = resources(app).filter_map(Res::of).collect();
            let unknown: Vec<String> =
                req.resources.iter().filter(|r| !known.iter().any(|k| k.same(r))).map(Res::label).collect();
            (!unknown.is_empty()).then(|| format!("not resources of the Application: {}", unknown.join(", ")))
        }
        Op::Rollback { id, .. } => {
            if auto_sync(app) {
                return Some(
                    "auto-sync is on: Argo CD refuses a rollback then (revert the change in git, or turn auto-sync off)"
                        .into(),
                );
            }
            let entry = match rollback_target(app, *id) {
                Ok(e) => e,
                Err(e) => return Some(e),
            };
            let source = entry["source"].as_object().is_some_and(|s| !s.is_empty())
                || entry["sources"].as_array().is_some_and(|s| !s.is_empty());
            if source {
                None
            } else {
                Some(
                    "that deployment does not record its source (Argo CD 0.11 or older): sync to its revision instead"
                        .to_string(),
                )
            }
        }
    }
}

/// The merge patch for `op` on the Application as read now, or why it is refused. It carries
/// the `resourceVersion` read: if the Application changes before it lands (an operation
/// started, auto-sync turned on), the API server refuses it instead.
pub fn patch(app: &Value, op: &Op) -> Result<Value, String> {
    if let Some(why) = refusal(app, op) {
        return Err(why);
    }
    let mut patch = match op {
        Op::Sync(req) => sync_patch(req),
        Op::Rollback { id, dry_run } => rollback_patch(app, rollback_target(app, *id)?, *dry_run),
        Op::Terminate => terminate_patch(),
    };
    if let Some(version) = app.pointer("/metadata/resourceVersion").and_then(Value::as_str) {
        patch["metadata"] = json!({"resourceVersion": version});
    }
    Ok(patch)
}

/// `Deployment shop/web, ConfigMap shop/a, 3 more`.
pub fn named(res: &[Res]) -> String {
    let mut out: Vec<String> = res.iter().take(NAMED).map(Res::label).collect();
    if res.len() > NAMED {
        out.push(format!("{} more", res.len() - NAMED));
    }
    out.join(", ")
}

/// What was started, said once the patch landed: for a prune, what it deletes.
pub fn started(app: &Value, op: &Op, name: &str) -> String {
    let what = match op {
        Op::Sync(req) if req.resources.is_empty() => "Sync".to_string(),
        Op::Sync(req) => format!("Sync of {}", crate::plural::count(req.resources.len(), "resource")),
        Op::Rollback { id, .. } => match rollback_target(app, *id) {
            Ok(e) => format!("Rollback to {}", deployment_label(e)),
            Err(_) => "Rollback".to_string(),
        },
        Op::Terminate => return format!("Terminating the operation of Application {name}"),
    };
    let mut s = format!("{what} of Application {name} started");
    if op.dry_run() {
        s.push_str(" (dry run: nothing is applied)");
    }
    if let Op::Sync(req) = op
        && req.prune
    {
        let gone = pruned(app, &req.resources);
        if gone.is_empty() {
            s.push_str("; nothing to prune");
        } else {
            let verb = if req.dry_run { "would delete" } else { "deletes" };
            s.push_str(&format!("; prune {verb} {}", named(&gone)));
        }
    }
    s
}

/// The result of a dry run once Argo CD is done with it: None while it runs, or when the
/// last operation was not a dry run.
pub fn dry_run_result(app: &Value) -> Option<String> {
    if in_progress(app) {
        return None;
    }
    let state = app.pointer("/status/operationState")?;
    let dry = state.pointer("/operation/sync/dryRun").and_then(Value::as_bool) == Some(true);
    if !dry {
        return None;
    }
    let phase = state["phase"].as_str().unwrap_or("Unknown");
    let mut out = match state["message"].as_str().filter(|m| !m.is_empty()) {
        Some(m) => format!("Dry run {phase}: {m}"),
        None => format!("Dry run {phase}"),
    };
    let results: Vec<&Value> =
        state.pointer("/syncResult/resources").and_then(Value::as_array).into_iter().flatten().collect();
    for r in results.iter().take(NAMED) {
        let Some(res) = Res::of(r) else { continue };
        let status = r["status"].as_str().or_else(|| r["hookPhase"].as_str()).unwrap_or_default();
        let message = r["message"].as_str().filter(|m| !m.is_empty()).map_or_default(|m| format!(": {m}"));
        out.push_str(&format!("\n  {} {status}{message}", res.label()));
    }
    if results.len() > NAMED {
        out.push_str(&format!("\n  … and {} more", results.len() - NAMED));
    }
    Some(out)
}

/// A deployment of the history, as a dialog names it: `deployment 4 (01234567)`.
pub fn deployment_label(entry: &Value) -> String {
    let rev = entry
        .get("revision")
        .and_then(Value::as_str)
        .filter(|r| !r.is_empty())
        .map_or_else(|| revision_text(entry), short_revision);
    format!("deployment {} ({rev})", entry["id"].as_i64().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn res(kind: &str, name: &str) -> Res {
        Res { kind: kind.into(), namespace: "shop".into(), name: name.into(), ..Default::default() }
    }

    fn app() -> Value {
        json!({
            "metadata": {"name": "web", "namespace": "argocd", "resourceVersion": "42"},
            "spec": {"syncPolicy": {"syncOptions": ["CreateNamespace=true"]}},
            "status": {
                "resources": [
                    {"kind": "ConfigMap", "namespace": "shop", "name": "a", "status": "Synced"},
                    {"group": "apps", "kind": "Deployment", "namespace": "shop", "name": "web", "status": "OutOfSync"},
                    {"kind": "ConfigMap", "namespace": "shop", "name": "old", "status": "OutOfSync", "requiresPruning": true},
                    {"kind": "ConfigMap", "namespace": "shop", "name": "gone", "status": "OutOfSync", "requiresPruning": true},
                    {"group": "batch", "kind": "Job", "namespace": "shop", "name": "migrate", "status": "OutOfSync", "hook": true}
                ],
                "history": [
                    {"id": 3, "revision": "c".repeat(40), "source": {"repoURL": "https://git.example/r", "path": "web"}},
                    {"id": 1, "revision": "a".repeat(40), "source": {"repoURL": "https://git.example/r", "path": "web"}},
                    {"id": 2, "revision": "b".repeat(40), "source": {"repoURL": "https://git.example/r", "path": "web"}}
                ]
            }
        })
    }

    #[test]
    fn a_sync_with_no_option_is_argo_cd_s_sync_button() {
        let p = sync_patch(&SyncRequest::default());
        assert_eq!(
            p,
            json!({"operation": {"initiatedBy": {"username": "recouvrance"}, "sync": {"syncStrategy": {"hook": {}}}}})
        );
    }

    #[test]
    fn a_sync_with_options_names_them() {
        let req = SyncRequest {
            resources: vec![Res { group: "apps".into(), ..res("Deployment", "web") }, res("ConfigMap", "old")],
            prune: true,
            dry_run: true,
        };
        let p = patch(&app(), &Op::Sync(req)).unwrap();
        let sync = &p["operation"]["sync"];
        assert_eq!(sync["prune"], true);
        assert_eq!(sync["dryRun"], true);
        assert_eq!(
            sync["resources"],
            json!([
                {"group": "apps", "kind": "Deployment", "namespace": "shop", "name": "web"},
                {"kind": "ConfigMap", "namespace": "shop", "name": "old"}
            ])
        );
        // Guarded by the version read: a change meanwhile makes the API server refuse it.
        assert_eq!(p["metadata"]["resourceVersion"], "42");
    }

    #[test]
    fn the_resources_offered_are_out_of_sync_and_not_hooks() {
        let names: Vec<String> = out_of_sync(&app()).into_iter().map(|r| r.name).collect();
        assert_eq!(names, ["web", "old", "gone"]);
    }

    #[test]
    fn prune_deletes_only_what_is_picked() {
        let names = |r: Vec<Res>| r.into_iter().map(|r| r.name).collect::<Vec<_>>();
        assert_eq!(names(pruned(&app(), &[])), ["old", "gone"]);
        assert_eq!(names(pruned(&app(), &[res("ConfigMap", "gone")])), ["gone"]);
        let web = Res { group: "apps".into(), ..res("Deployment", "web") };
        assert!(pruned(&app(), &[web]).is_empty());
        let msg = started(&app(), &Op::Sync(SyncRequest { prune: true, ..Default::default() }), "web");
        assert!(msg.contains("prune deletes ConfigMap shop/old, ConfigMap shop/gone"), "{msg}");
    }

    #[test]
    fn a_resource_the_application_does_not_have_is_refused() {
        let req = SyncRequest { resources: vec![res("Secret", "nope")], ..Default::default() };
        let why = patch(&app(), &Op::Sync(req)).unwrap_err();
        assert!(why.contains("Secret shop/nope"), "{why}");
    }

    #[test]
    fn nothing_starts_while_an_operation_runs() {
        let mut busy = app();
        busy["operation"] = json!({"sync": {}});
        for op in [Op::Sync(SyncRequest::default()), Op::Rollback { id: None, dry_run: false }] {
            assert!(patch(&busy, &op).unwrap_err().contains("in progress"));
        }
        // Terminate needs one, with its state.
        assert!(patch(&app(), &Op::Terminate).unwrap_err().contains("no operation"));
        assert!(patch(&busy, &Op::Terminate).unwrap_err().contains("no operation"));
        busy["status"]["operationState"] = json!({"phase": "Running"});
        let p = patch(&busy, &Op::Terminate).unwrap();
        assert_eq!(p["status"]["operationState"], json!({"phase": "Terminating"}));
        assert_eq!(p["metadata"]["resourceVersion"], "42");
        assert!(p.get("operation").is_none());
    }

    #[test]
    fn a_rollback_is_a_sync_of_the_deployment_s_revision_and_source() {
        let p = patch(&app(), &Op::Rollback { id: None, dry_run: false }).unwrap();
        // The one before the newest (ids 1, 2, 3, in any order in the history).
        let sync = &p["operation"]["sync"];
        assert_eq!(sync["revision"], "b".repeat(40));
        assert_eq!(sync["source"], json!({"repoURL": "https://git.example/r", "path": "web"}));
        assert_eq!(sync["syncStrategy"], json!({"apply": {}}));
        assert_eq!(sync["syncOptions"], json!(["CreateNamespace=true"]));
        assert!(sync.get("prune").is_none() && sync.get("dryRun").is_none() && sync.get("sources").is_none());
        assert_eq!(p["operation"]["initiatedBy"]["username"], "recouvrance");
        let p = patch(&app(), &Op::Rollback { id: Some(1), dry_run: true }).unwrap();
        assert_eq!(p["operation"]["sync"]["revision"], "a".repeat(40));
        assert_eq!(p["operation"]["sync"]["dryRun"], true);
        assert!(patch(&app(), &Op::Rollback { id: Some(9), dry_run: false }).unwrap_err().contains("no deployment 9"));
    }

    #[test]
    fn a_rollback_of_several_sources_sends_them_all() {
        let mut a = app();
        a["status"]["history"] = json!([
            {"id": 1, "revisions": ["r1", "1.0.0"], "sources": [{"repoURL": "x"}, {"chart": "c"}]},
            {"id": 2, "revisions": ["r2", "2.0.0"], "sources": [{"repoURL": "x"}, {"chart": "c"}]}
        ]);
        let p = patch(&a, &Op::Rollback { id: None, dry_run: false }).unwrap();
        let sync = &p["operation"]["sync"];
        assert_eq!(sync["revisions"], json!(["r1", "1.0.0"]));
        assert_eq!(sync["sources"], json!([{"repoURL": "x"}, {"chart": "c"}]));
        assert!(sync.get("source").is_none() && sync.get("revision").is_none());
    }

    #[test]
    fn no_rollback_while_auto_sync_is_on() {
        let mut a = app();
        a["spec"]["syncPolicy"]["automated"] = json!({"prune": true});
        let why = patch(&a, &Op::Rollback { id: None, dry_run: false }).unwrap_err();
        assert!(why.contains("auto-sync is on"), "{why}");
        // Turned off with `enabled: false` (newer Argo CD): allowed.
        a["spec"]["syncPolicy"]["automated"]["enabled"] = json!(false);
        assert!(patch(&a, &Op::Rollback { id: None, dry_run: false }).is_ok());
        // Nothing before the newest, or no source recorded: refused.
        let mut one = app();
        one["status"]["history"] = json!([{"id": 1, "revision": "x", "source": {"repoURL": "x"}}]);
        assert!(patch(&one, &Op::Rollback { id: None, dry_run: false }).is_err());
        one["status"]["history"] = json!([{"id": 1, "revision": "x"}, {"id": 2, "revision": "y"}]);
        assert!(patch(&one, &Op::Rollback { id: None, dry_run: false }).unwrap_err().contains("source"));
    }

    #[test]
    fn nothing_starts_on_an_application_being_deleted() {
        let mut a = app();
        a["metadata"]["deletionTimestamp"] = json!("2025-01-01T00:00:00Z");
        assert!(patch(&a, &Op::Sync(SyncRequest::default())).unwrap_err().contains("deleted"));
    }

    #[test]
    fn a_dry_run_s_result_is_read_once_it_ended() {
        let mut a = app();
        a["operation"] = json!({"sync": {"dryRun": true}});
        a["status"]["operationState"] = json!({"phase": "Running", "operation": {"sync": {"dryRun": true}}});
        assert_eq!(dry_run_result(&a), None);
        a["operation"] = Value::Null;
        a["status"]["operationState"] = json!({
            "phase": "Succeeded", "message": "successfully synced (no more tasks)",
            "operation": {"sync": {"dryRun": true}},
            "syncResult": {"resources": [
                {"kind": "ConfigMap", "namespace": "shop", "name": "old", "status": "Pruned", "message": "pruned (dry run)"}
            ]}
        });
        let out = dry_run_result(&a).unwrap();
        assert!(out.starts_with("Dry run Succeeded: successfully synced"), "{out}");
        assert!(out.contains("ConfigMap shop/old Pruned: pruned (dry run)"), "{out}");
        // A real sync's result is not a dry run's.
        a["status"]["operationState"]["operation"]["sync"]["dryRun"] = json!(false);
        assert_eq!(dry_run_result(&a), None);
    }

    proptest! {
        /// For every choice of resources, prune never reaches beyond what is picked and
        /// flagged to prune, and the patch names exactly the resources picked.
        #[test]
        fn prune_stays_within_the_resources_picked(picks in proptest::collection::vec(any::<bool>(), 5)) {
            let a = app();
            let all: Vec<Res> = resources(&a).filter_map(Res::of).collect();
            let picked: Vec<Res> = all.iter().zip(&picks).filter(|(_, p)| **p).map(|(r, _)| r.clone()).collect();
            let gone = pruned(&a, &picked);
            for g in &gone {
                prop_assert!(picked.is_empty() || picked.iter().any(|p| p.same(g)));
                prop_assert!(g.name == "old" || g.name == "gone");
            }
            let req = SyncRequest { resources: picked.clone(), prune: true, dry_run: false };
            let p = patch(&a, &Op::Sync(req)).unwrap();
            let sent = p["operation"]["sync"]["resources"].as_array().map_or(0, Vec::len);
            prop_assert_eq!(sent, picked.len());
        }
    }
}
