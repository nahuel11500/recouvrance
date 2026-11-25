use jiff::Timestamp;
use proptest::prelude::*;
use serde_json::{Value, json};

use super::example::{self, ENVIRONMENTS};
use super::{
    Cards, DiffSummary, Image, Read, deployed, diff_from_git, diff_from_status, history, image_changes, last_sync,
    managed_path, problems_from_status, problems_from_tree, remember, revision_path, source_images, split_image, state,
    tree_path, versions, who,
};
use crate::k8s::argo::server::Login;
use crate::status::Tone;

fn now() -> Timestamp {
    Timestamp::from_second(1_800_000_000).unwrap()
}

fn app(env: &str) -> Value {
    example::app(env, now())
}

#[test]
fn each_read_is_the_path_argo_cd_routes() {
    assert_eq!(tree_path("team a", "web"), "/api/v1/applications/web/resource-tree?appNamespace=team%20a");
    assert_eq!(managed_path("argocd", "web"), "/api/v1/applications/web/managed-resources?appNamespace=argocd");
    assert_eq!(
        revision_path("argocd", "web", "abc123", None, Some(7)),
        "/api/v1/applications/web/revisions/abc123/metadata?appNamespace=argocd"
    );
    // Several sources: which one, and the deployment it was part of.
    assert_eq!(
        revision_path("argocd", "web", "v1.2/rc", Some(1), Some(7)),
        "/api/v1/applications/web/revisions/v1.2%2Frc/metadata?appNamespace=argocd&sourceIndex=1&versionId=7"
    );
}

#[test]
fn an_image_splits_into_its_repository_and_tag() {
    let split = split_image;
    assert_eq!(split("registry.example.com/shop/api:1.5.0"), ("registry.example.com/shop/api".into(), "1.5.0".into()));
    // A registry's port is not a tag.
    assert_eq!(split("registry:5000/team/web"), ("registry:5000/team/web".into(), "latest".into()));
    assert_eq!(split("registry:5000/team/web:2"), ("registry:5000/team/web".into(), "2".into()));
    assert_eq!(split("nginx@sha256:0123456789abcdef0123"), ("nginx".into(), "@0123456789ab".into()));
    assert_eq!(split("nginx:1.27@sha256:0123456789abcdef"), ("nginx".into(), "1.27".into()));
}

#[test]
fn an_image_that_changed_says_from_what() {
    let current = ["r.example/a/api:2".to_string(), "r.example/a/worker:1".into(), "r.example/a/new:1".into()];
    let before = ["r.example/a/api:1".to_string(), "r.example/a/worker:1".into()];
    let images = image_changes(&current, Some(&before));
    let image = |c: &str, tag: &str, from: Option<&str>| Image {
        component: c.into(),
        repo: format!("r.example/a/{c}"),
        tag: tag.into(),
        from: from.map(String::from),
    };
    assert_eq!(images, [image("api", "2", Some("1")), image("new", "1", None), image("worker", "1", None)]);
    // Nothing known of before: nothing said changed.
    assert!(image_changes(&current, None).iter().all(|i| i.from.is_none()));
}

#[test]
fn a_source_pins_images_by_kustomize_or_helm() {
    let kustomize = json!({"kustomize": {"images": ["r.example/api:2", "worker=r.example/worker:3"]}});
    assert_eq!(source_images(&kustomize), ["r.example/api:2", "r.example/worker:3"]);
    let helm = json!({"helm": {"parameters": [
        {"name": "api.image.repository", "value": "r.example/api"},
        {"name": "api.image.tag", "value": "2"},
        {"name": "replicas", "value": "3"},
        {"name": "lonely.repository", "value": "r.example/x"},
    ]}});
    assert_eq!(source_images(&helm), ["r.example/api:2"]);
}

#[test]
fn versions_say_what_changed_who_released_and_where_a_rollback_goes() {
    let prod = versions(&app("example-prod"), None);
    let changed: Vec<(&str, Option<&str>)> =
        prod.images.iter().map(|i| (i.component.as_str(), i.from.as_deref())).collect();
    // api and worker went 1.4.1 → 1.5.0; worker-mail stayed.
    assert_eq!(changed, [("api", Some("1.4.1")), ("worker", Some("1.4.1")), ("worker-mail", None)]);
    assert_eq!(prod.by.as_deref(), Some("dev-user"));
    assert_eq!(prod.previous.as_deref(), Some("api 1.4.1, worker 1.4.1"));
    // Auto-sync off: a rollback goes to the deployment before the newest.
    assert_eq!((prod.rollback_to, prod.auto_sync), (Some(14), false));
    let staging = versions(&app("example-staging"), None);
    assert_eq!(staging.by.as_deref(), Some("autosync"));
    assert!(staging.images.iter().all(|i| i.from.is_none()));
    // Auto-sync on: Argo CD refuses a rollback, none is offered.
    assert_eq!((staging.rollback_to, staging.auto_sync), (None, true));
    assert!(staging.previous.as_deref().is_some_and(|p| p.starts_with("revision ")));
    // The images seen running under the deployment before win over what its sources pinned.
    let seen = ["registry.example.com/shop/worker-mail:1.3.0".to_string()];
    let prod = versions(&app("example-prod"), Some(&seen));
    let mail = prod.images.iter().find(|i| i.component == "worker-mail").unwrap();
    assert_eq!(mail.from.as_deref(), Some("1.3.0"));
}

#[test]
fn who_started_it() {
    assert_eq!(who(&json!({"automated": true, "username": "x"})).as_deref(), Some("autosync"));
    assert_eq!(who(&json!({"username": "dev-user"})).as_deref(), Some("dev-user"));
    assert_eq!(who(&json!({})), None);
}

#[test]
fn images_seen_are_kept_by_deployment() {
    let mut seen = Vec::new();
    remember(&mut seen, 1, &["a:1".to_string()]);
    // A deployment seen already keeps what was first seen under it.
    remember(&mut seen, 1, &["a:9".to_string()]);
    remember(&mut seen, 2, &[]);
    assert_eq!(seen, [(1, vec!["a:1".to_string()])]);
    for id in 2..10 {
        remember(&mut seen, id, &[format!("a:{id}")]);
    }
    assert_eq!(seen.len(), 5);
    assert_eq!(seen[0].0, 5);
}

#[test]
fn the_state_says_a_failed_or_running_sync_over_the_comparison() {
    let prod = state(&app("example-prod"), None);
    assert_eq!((prod.sync.as_str(), prod.health.as_str(), prod.tone), ("OutOfSync", "Degraded", Tone::Bad));
    // The Deployment and its autoscaler share a name: said once.
    assert_eq!(prod.degraded, ["worker-crawl"]);
    let sandbox = state(&app("example-sandbox"), None);
    assert_eq!((sandbox.sync.as_str(), sandbox.tone), ("Sync Failed", Tone::Bad));
    let mut running = app("example-staging");
    running["operation"] = json!({"sync": {}});
    assert_eq!(state(&running, None).sync, "Syncing");
    let staging = state(&app("example-staging"), None);
    assert_eq!((staging.sync.as_str(), staging.tone), ("Synced", Tone::Good));
    assert!(staging.degraded.is_empty());
    // Argo CD 3 keeps no health in the status: the tree's top objects say it.
    let mut lean = app("example-prod");
    for r in lean["status"]["resources"].as_array_mut().unwrap() {
        r.as_object_mut().unwrap().remove("health");
    }
    let tree = example::tree("example-prod", now());
    assert_eq!(state(&lean, Some(&tree)).degraded, ["worker-crawl"]);
}

#[test]
fn problems_are_grouped_by_workload_from_the_tree() {
    let p = problems_from_tree(&app("example-prod"), &example::tree("example-prod", now()));
    let rows: Vec<_> =
        p.rows.iter().map(|r| (r.status.as_str(), r.res.kind.as_str(), r.res.name.as_str(), r.pods)).collect();
    assert_eq!(
        rows,
        [
            ("CrashLoopBackOff", "Deployment", "worker-crawl", Some((2, 8))),
            ("Degraded", "HorizontalPodAutoscaler", "worker-crawl", None),
            ("Pending", "Deployment", "worker-mail", Some((1, 2))),
        ]
    );
    assert_eq!((p.pods.len(), p.troubled), (12, 3));
    assert_eq!(p.rows[0].tone, Tone::Bad);
    assert_eq!(p.rows[2].tone, Tone::Busy);
    // Since the oldest pod in trouble, and why from the pod's own message.
    assert_eq!(p.rows[0].since, Some(Timestamp::from_second(now().as_second() - 12 * 60).unwrap()));
    assert!(p.rows[0].why.as_deref().is_some_and(|w| w.contains("back-off")));
    assert!(p.rows[1].why.as_deref().is_some_and(|w| w.starts_with("No metric")));
    let fine = problems_from_tree(&app("example-staging"), &example::tree("example-staging", now()));
    assert!(fine.rows.is_empty() && fine.troubled == 0 && fine.pods.len() == 12);
}

#[test]
fn a_pod_running_but_not_ready_is_in_trouble() {
    let tree = json!({"nodes": [
        {"kind": "Pod", "name": "p", "namespace": "n", "uid": "p",
            "info": [{"name": "Status Reason", "value": "Running"}], "health": {"status": "Progressing"}},
        {"kind": "Pod", "name": "q", "namespace": "n", "uid": "q",
            "info": [{"name": "Status Reason", "value": "Completed"}], "health": {"status": "Healthy"}},
    ]});
    let p = problems_from_tree(&json!({}), &tree);
    assert_eq!(p.rows.len(), 1);
    assert_eq!(
        (p.rows[0].status.as_str(), p.rows[0].res.name.as_str(), p.rows[0].pods),
        ("Not ready", "p", Some((1, 1)))
    );
}

#[test]
fn without_a_tree_problems_come_from_the_status() {
    let p = problems_from_status(&app("example-prod"));
    let rows: Vec<(&str, &str)> = p.rows.iter().map(|r| (r.status.as_str(), r.res.name.as_str())).collect();
    // Bad first; Missing is a problem with auto-sync on only (prod syncs by hand).
    assert_eq!(rows, [("Degraded", "worker-crawl"), ("Degraded", "worker-crawl"), ("Progressing", "worker-mail")]);
    assert!(!p.from_tree);
}

#[test]
fn the_last_sync_says_its_steps_and_how_long_it_took() {
    let s = last_sync(&app("example-sandbox")).unwrap();
    assert_eq!((s.phase.as_str(), s.tone, s.running), ("Failed", Tone::Bad, false));
    assert_eq!(s.seconds(now()), Some(40));
    assert_eq!(s.by.as_deref(), Some("dev-user"));
    assert_eq!(s.steps.len(), 2);
    assert_eq!((s.steps[1].result(), crate::k8s::argo::steps::tone(&s.steps[1])), ("SyncFailed", Tone::Bad));
    assert_eq!(s.summary.cause.len(), 1, "the failed step leads");
    assert_eq!(s.done, 2);
    // A sync running: its hook still running is not done, and it lasts until now.
    let mut running = app("example-staging");
    running["status"]["operationState"] = json!({
        "phase": "Running",
        "startedAt": Timestamp::from_second(now().as_second() - 75).unwrap().to_string(),
        "message": "waiting for completion of hook batch/Job/db-migrate\nand more",
        "operation": {"initiatedBy": {"automated": true}},
        "syncResult": {"resources": [
            {"kind": "Job", "group": "batch", "name": "db-migrate", "hookType": "PreSync", "hookPhase": "Running"},
            {"kind": "ConfigMap", "name": "c", "status": "Synced"},
        ]},
    });
    let s = last_sync(&running).unwrap();
    assert!(s.running);
    assert_eq!((s.done, s.steps.len(), s.seconds(now())), (1, 2, Some(75)));
    assert!(s.steps[0].is_hook());
    assert_eq!(s.summary.current.as_deref(), Some("PreSync"));
    assert_eq!(s.message.as_deref(), Some("waiting for completion of hook batch/Job/db-migrate…"));
    assert_eq!(s.by.as_deref(), Some("autosync"));
}

#[test]
fn the_diff_is_counted_by_kind_of_change() {
    let d = diff_from_git(&example::diff("example-prod"));
    assert_eq!((d.total(), d.text().as_str()), (7, "~ 5 changed, + 1 added, – 1 pruned"));
    assert!(d.from_argo);
    assert_eq!(diff_from_git(&example::diff("example-staging")).total(), 0);
    // From the status alone: a resource without health may or may not be live, counted changed.
    let d = diff_from_status(&app("example-prod"));
    assert_eq!((d.unknown.len(), d.added.len(), d.pruned.len()), (1, 1, 1));
    assert_eq!(d.text(), "~ 1 changed, + 1 added, – 1 pruned");
    assert_eq!(DiffSummary::default().text(), "");
}

#[test]
fn history_shows_deployments_and_a_failed_sync_after_them() {
    let h = history(&app("example-sandbox"), 10);
    let oks: Vec<bool> = h.dots.iter().map(|d| d.ok).collect();
    assert_eq!(oks, [true, true, true, true, true, true, false]);
    assert_eq!((h.succeeded, h.failed), (6, 1));
    // The last few only.
    let h = history(&app("example-sandbox"), 3);
    assert_eq!((h.dots.len(), h.succeeded, h.failed), (3, 2, 1));
    let h = history(&app("example-prod"), 10);
    assert_eq!((h.succeeded, h.failed), (5, 0));
    assert_eq!(h.dots.last().and_then(|d| d.id), Some(15));
}

#[test]
fn every_example_makes_its_cards() {
    for env in ENVIRONMENTS {
        let cards = Cards::of(example::read(env, now()), None, now());
        assert_eq!(cards.summary.name, env);
        assert!(cards.problems.from_tree);
        assert_eq!(cards.deployed.len(), 1);
        assert!(cards.deployed[0].1.as_ref().is_some_and(Result::is_ok), "{env}");
    }
    let prod = Cards::of(example::read("example-prod", now()), None, now());
    assert!(!prod.findings.is_empty());
    assert_eq!(prod.diff.total(), 7);
}

/// The tree holds only live objects: what auto-sync should have deployed and isn't live is
/// said from the status, Missing, whether the status says so or only that it's out of sync.
#[test]
fn what_is_missing_is_a_problem_with_a_tree() {
    let mut staging = app("example-staging");
    let resources = staging["status"]["resources"].as_array_mut().unwrap();
    resources.push(json!({"kind": "Service", "namespace": "shop", "name": "metrics", "status": "OutOfSync",
        "health": {"status": "Missing"}}));
    // Argo CD 3: no health in the status, out of sync, not in the tree.
    resources.push(json!({"kind": "ConfigMap", "namespace": "shop", "name": "flags", "status": "OutOfSync"}));
    // Out of sync but live: it differs, it isn't missing.
    resources.push(json!({"group": "apps", "kind": "Deployment", "namespace": "shop", "name": "api",
        "status": "OutOfSync"}));
    // To prune: no longer in git, not missing.
    resources.push(json!({"kind": "ConfigMap", "namespace": "shop", "name": "old", "status": "OutOfSync",
        "requiresPruning": true}));
    let tree = example::tree("example-staging", now());
    let p = problems_from_tree(&staging, &tree);
    let rows: Vec<(&str, &str)> = p.rows.iter().map(|r| (r.status.as_str(), r.res.name.as_str())).collect();
    assert_eq!(rows, [("Missing", "flags"), ("Missing", "metrics")]);
    assert!(p.rows.iter().all(|r| r.tone == Tone::Bad && r.why.is_some()));
    // Synced by hand, what isn't deployed yet is expected.
    staging["spec"]["syncPolicy"] = json!({});
    assert!(problems_from_tree(&staging, &tree).rows.is_empty());
}

/// Argo CD 3 keeps no health in the status: with no tree, finding nothing isn't "nothing
/// wrong" unless the Application is Healthy.
#[test]
fn a_status_without_health_says_nothing_either_way() {
    let mut prod = app("example-prod");
    for r in prod["status"]["resources"].as_array_mut().unwrap() {
        r.as_object_mut().unwrap().remove("health");
    }
    assert!(problems_from_status(&prod).rows.is_empty());
    let read = |app: Value| Read {
        app,
        tree: None,
        diff: None,
        managed: None,
        commits: Vec::new(),
        note: Some("no server".into()),
    };
    let cards = Cards::of(read(prod), None, now());
    assert!(cards.problems.rows.is_empty() && !cards.clean_is_known(), "Degraded, nothing found: unknown");
    let cards = Cards::of(read(app("example-staging")), None, now());
    assert!(cards.clean_is_known(), "Healthy");
    let cards = Cards::of(example::read("example-prod", now()), None, now());
    assert!(cards.clean_is_known(), "from the tree");
}

#[test]
fn one_repository_at_several_tags_is_no_change() {
    let both = ["r.example/a/worker:1".to_string(), "r.example/a/worker:2".into()];
    // Two components on one image at two tags, both there before: nothing changed.
    assert!(image_changes(&both, Some(&both)).iter().all(|i| i.from.is_none()));
    // One of them moved to a tag not there before: that one changed.
    let now = ["r.example/a/worker:1".to_string(), "r.example/a/worker:3".into()];
    let images = image_changes(&now, Some(&both));
    let from: Vec<(&str, Option<&str>)> = images.iter().map(|i| (i.tag.as_str(), i.from.as_deref())).collect();
    assert_eq!(from, [("1", None), ("3", Some("1"))]);
}

#[test]
fn deployed_names_each_source_with_its_revision() {
    let mut a = app("example-staging");
    a["status"]["history"] = json!([{
        "id": 4,
        "revisions": ["abc123", "", "1.2.0"],
        "sources": [
            {"repoURL": "https://git.example.com/a.git"},
            {"repoURL": "https://git.example.com/b.git"},
            {"repoURL": "https://charts.example.com", "chart": "web"},
        ],
    }]);
    let (sources, version) = deployed(&a);
    let got: Vec<(Option<usize>, &str, Option<&str>)> =
        sources.iter().map(|d| (d.index, d.revision.as_str(), d.chart.as_deref())).collect();
    // A source without a revision is left out; the others keep their index.
    assert_eq!(got, [(Some(0), "abc123", None), (Some(2), "1.2.0", Some("web"))]);
    assert_eq!(version, Some(4));
    // Nothing deployed yet: the revision compared with git.
    let mut fresh = app("example-staging");
    fresh["status"]["history"] = json!([]);
    let (sources, version) = deployed(&fresh);
    assert_eq!((sources.len(), version), (1, None));
    assert_eq!(sources[0].repo, "https://git.example.com/shop/deploy.git");
}

/// Argo CD's API sends `"source": {"repoURL": ""}` next to `sources` on an Application with
/// several: the chart's version is still known as one, and no commit is asked for it (#1071).
#[test]
fn a_chart_version_among_several_sources_is_never_asked_for_its_commit() {
    let sources = json!([
        {"repoURL": "https://git.example.com/config.git", "targetRevision": "HEAD", "ref": "configs"},
        {"repoURL": "https://charts.example.com", "targetRevision": "0.35.0", "chart": "web"},
    ]);
    let mut a = app("example-staging");
    a["status"]["history"] = json!([{
        "id": 3, "revisions": ["abc123", "0.35.0"], "source": {"repoURL": ""}, "sources": sources,
    }]);
    let (got, _) = deployed(&a);
    let charts: Vec<(&str, Option<&str>)> = got.iter().map(|d| (d.revision.as_str(), d.chart.as_deref())).collect();
    assert_eq!(charts, [("abc123", None), ("0.35.0", Some("web"))]);
    // Nothing deployed yet: the spec has the same empty `source`.
    let mut fresh = app("example-staging");
    fresh["status"]["history"] = json!([]);
    fresh["spec"]["source"] = json!({"repoURL": ""});
    fresh["spec"]["sources"] = sources;
    fresh["status"]["sync"] = json!({"revisions": ["abc123", "0.35.0"]});
    let (got, _) = deployed(&fresh);
    assert_eq!(got.iter().map(|d| d.chart.as_deref()).collect::<Vec<_>>(), [None, Some("web")]);

    let asked = std::sync::Mutex::new(Vec::new());
    let get = |path: String| {
        asked.lock().unwrap().push(path);
        async { Ok::<_, String>(json!({"message": "a commit"})) }
    };
    let diff_of = |v: &Value| crate::k8s::argo::server::diffs(v, "argocd.example.com".into(), Login::Cli);
    let read = futures::executor::block_on(super::extras("argocd.example.com", "argocd", "web-1071", a, get, diff_of));
    let revisions: Vec<String> =
        asked.into_inner().unwrap().into_iter().filter(|p| p.contains("/revisions/")).collect();
    assert_eq!(revisions, [revision_path("argocd", "web-1071", "abc123", Some(0), Some(3))]);
    assert!(read.commits[0].is_some() && read.commits[1].is_none());
}

#[test]
fn a_terminating_sync_is_syncing_and_no_comparison_is_unknown() {
    let mut a = app("example-staging");
    a["status"]["operationState"]["phase"] = json!("Terminating");
    assert_eq!(state(&a, None).sync, "Syncing");
    let mut a = app("example-staging");
    a["status"].as_object_mut().unwrap().remove("sync");
    let s = state(&a, None);
    assert_eq!(s.sync, "Unknown");
    assert_ne!(s.tone, Tone::Good);
}

proptest! {
    /// Whatever the tree holds, every pod is counted once, and those in trouble are counted
    /// in exactly one row.
    #[test]
    fn every_pod_in_trouble_is_in_one_row(
        pods in proptest::collection::vec((0usize..3, 0usize..4), 0..20),
    ) {
        let reasons = ["Running", "CrashLoopBackOff", "Pending", "Completed"];
        let mut nodes = vec![
            json!({"kind": "Deployment", "group": "apps", "name": "d0", "uid": "d0"}),
            json!({"kind": "Deployment", "group": "apps", "name": "d1", "uid": "d1"}),
        ];
        for (i, (owner, reason)) in pods.iter().enumerate() {
            let mut pod = json!({"kind": "Pod", "name": format!("p{i}"), "uid": format!("p{i}"),
                "info": [{"name": "Status Reason", "value": reasons[*reason]}]});
            // Owner 2: a pod of no known owner.
            if *owner < 2 {
                pod["parentRefs"] = json!([{"uid": format!("d{owner}")}]);
            }
            nodes.push(pod);
        }
        let p = problems_from_tree(&json!({}), &json!({"nodes": nodes}));
        let in_trouble = pods.iter().filter(|(_, r)| matches!(*r, 1 | 2)).count();
        prop_assert_eq!(p.pods.len(), pods.len());
        prop_assert_eq!(p.troubled, in_trouble);
        let counted: usize = p.rows.iter().filter_map(|r| r.pods).map(|(t, _)| t).sum();
        prop_assert_eq!(counted, in_trouble);
    }
}

/// An Application as the list holds it (no resources, no tree): its cards say what the list
/// knows, and why it wasn't read in one line, never a page of error.
#[test]
fn a_list_copy_gives_cards_with_one_line_why() {
    let full = app("example-prod");
    let lean = crate::k8s::argo::api::lean(&full);
    let read = Read::held(lean, "\n  rpc error: code = Unauthenticated desc = token is expired\nstack…");
    assert_eq!(read.note.as_deref(), Some("rpc error: code = Unauthenticated desc = token is expired"));
    assert!(read.tree.is_none() && read.diff.is_none() && read.commits.is_empty());
    let cards = Cards::of(read, None, now());
    assert_eq!(cards.summary.name, "example-prod");
    assert_eq!(cards.summary.sync, full["status"]["sync"]["status"].as_str().unwrap());
    assert!(!cards.clean_is_known() || cards.summary.health == "Healthy");
}

/// An Application whose newest deployment is #14 at `e599d6d8` (6 days ago), and whose last
/// operation is `op` started `mins` minutes ago.
fn after_deployment(op: Value, mins: i64) -> Value {
    let at = |m: i64| Timestamp::from_second(now().as_second() - m * 60).unwrap().to_string();
    let mut state = op;
    state["startedAt"] = json!(at(mins + 1));
    state["finishedAt"] = json!(at(mins));
    json!({
        "metadata": {"name": "shop-prod-a-workload", "namespace": "argocd"},
        "spec": {"source": {"repoURL": "https://git.example.com/deploy.git"}},
        "status": {
            "sync": {"status": "OutOfSync", "revision": "cd3e16f6aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
            "history": [
                {"id": 13, "revision": "1cca9ca6aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "deployedAt": at(8 * 1440)},
                {"id": 14, "revision": "e599d6d8aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "deployedAt": at(6 * 1440)},
            ],
            "resources": [
                {"kind": "Deployment", "name": "api", "status": "Synced"},
                {"kind": "Deployment", "name": "web", "status": "OutOfSync"},
                {"kind": "ConfigMap", "name": "api", "status": "Synced"},
                {"kind": "Job", "name": "migrate", "hook": true},
            ],
            "operationState": state,
        },
    })
}

fn selective(phase: &str) -> Value {
    json!({
        "phase": phase,
        "operation": {
            "initiatedBy": {"username": "jdoe"},
            "sync": {"revision": "ebf7503baaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "resources": [
                {"kind": "Deployment", "name": "api"}, {"kind": "ConfigMap", "name": "api"}
            ]},
        },
        "syncResult": {"revision": "ebf7503baaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"},
    })
}

/// A selective sync after the newest deployment is said where the deployment is: in the
/// Why, the Last sync and the roll back's confirmation (#1067).
#[test]
fn a_selective_sync_after_the_newest_deployment_is_said() {
    let app = after_deployment(selective("Succeeded"), 5 * 1440);
    let p = super::partial(&app).expect("a selective sync");
    assert_eq!((p.resources, p.total, p.revision.as_str(), p.after), (2, 3, "ebf7503", Some(14)));
    assert_eq!(p.by.as_deref(), Some("jdoe"));
    assert!(p.text().starts_with("2 of 3 resources at ebf7503 (selective sync, "), "{}", p.text());
    let last = last_sync(&app).unwrap();
    assert_eq!(last.selective, Some((2, 3)));
    let why = crate::k8s::argo::diagnosis::Diagnosis::of(&app, now());
    let said: Vec<String> = why.findings.iter().filter_map(|f| f.message.clone()).collect();
    assert!(
        said.iter()
            .any(|m| m.contains("2 of 3 resources at ebf7503") && m.contains("(e599d6d)") && m.contains("cd3e16f")),
        "{said:?}"
    );
    let undoes = super::rollback_undoes(&app).unwrap();
    assert!(undoes.contains("after deployment #14") && undoes.contains("Deployment/api, ConfigMap/api"), "{undoes}");
}

/// A whole sync, a dry run, one still running, or one older than the newest deployment is
/// not a selective sync to say.
#[test]
fn only_a_finished_selective_sync_newer_than_the_history_counts() {
    let mut whole = selective("Succeeded");
    whole["operation"]["sync"].as_object_mut().unwrap().remove("resources");
    assert_eq!(super::partial(&after_deployment(whole, 60)), None);
    let mut dry = selective("Succeeded");
    dry["operation"]["sync"]["dryRun"] = json!(true);
    assert_eq!(super::partial(&after_deployment(dry, 60)), None);
    assert_eq!(super::partial(&after_deployment(selective("Running"), 60)), None);
    assert_eq!(super::partial(&after_deployment(selective("Succeeded"), 7 * 1440)), None, "older than #14");
    assert!(super::partial(&after_deployment(selective("Failed"), 60)).is_some(), "a failed one may have applied some");
    assert_eq!(last_sync(&after_deployment(selective("Running"), 60)).unwrap().selective, Some((2, 3)));
}

/// The Sync diff says what changes, images first, each change once with where: a field added
/// on several resources of a kind is one line per value until grouped by kind; a Secret's
/// values never show.
#[test]
fn the_sync_diff_says_what_changes_images_first() {
    let cards = Cards::of(example::read("example-prod", now()), None, now());
    let smart = cards.smart.as_ref().expect("the managed resources were read");
    let lines: Vec<String> = smart.diff.groups.iter().map(crate::smartdiff::Group::text).collect();
    assert!(lines[0].starts_with("image api 1.4.1 → 1.5.0"), "{lines:?}");
    assert!(lines[1].starts_with("image worker 1.4.1 → 1.5.0"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "Secret key SMTP_HOST changed"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "1 resource added") && lines.iter().any(|l| l == "1 resource pruned"));
    let secret = smart.diff.resources.iter().find(|r| r.key.kind == "Secret").unwrap();
    let text = format!("{:?}", secret.lines(true));
    assert!(!text.contains("++++"), "a Secret's values are masked: {text}");
    assert!(Cards::of(example::read("example-staging", now()), None, now()).smart.is_some_and(|s| s.in_sync()));
}

/// The Sync diff groups as the grouped diff does: one annotation added on three Deployments
/// is one line, images first.
#[test]
fn the_sync_diff_groups_a_field_by_kind() {
    let cards = Cards::of(example::read("example-prod", now()), None, now());
    let lines: Vec<&str> = cards.changes.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(
        lines,
        [
            "image api 1.4.1 → 1.5.0",
            "image worker 1.4.1 → 1.5.0",
            "ConfigMap shop-config key CRAWL_BATCH changed",
            "Secret shop-secrets key SMTP_HOST changed",
            "Service metrics added",
            "ConfigMap legacy-flags pruned (with prune)",
            "3 Deployments: annotation example.io/owner added (3 values)",
        ]
    );
}
