use std::time::{Duration, Instant};

use proptest::prelude::*;
use serde_json::{Value, json};

use super::*;

fn ts(secs: i64) -> String {
    jiff::Timestamp::from_second(1_700_000_000 + secs).unwrap().to_string()
}

fn writer(manager: &str, subresource: &str, secs: i64, fields: Value) -> Value {
    let sub = if subresource.is_empty() { Value::Null } else { subresource.into() };
    json!({"manager": manager, "operation": "Update", "subresource": sub, "time": ts(secs), "fieldsV1": fields})
}

fn object(rv: u64, generation: i64, labels: Value, data: Value, writers: Vec<Value>) -> Value {
    json!({
        "apiVersion": "v1", "kind": "ConfigMap",
        "metadata": {
            "name": "web", "namespace": "default", "uid": "u1", "resourceVersion": rv.to_string(),
            "generation": generation, "creationTimestamp": ts(0), "labels": labels, "managedFields": writers,
        },
        "data": data,
        // Differs on every version: never compared.
        "status": {"observed": rv},
    })
}

fn info(secret: bool) -> KindInfo {
    let (resource, kind) = if secret { ("secrets", "Secret") } else { ("configmaps", "ConfigMap") };
    KindInfo { resource: resource.into(), kind: kind.into(), secret }
}

fn state() -> KindState {
    KindState::new(jiff::Timestamp::from_second(1_800_000_000).unwrap())
}

/// Observe `obj` as a watch of metadata (`full` false) or of whole objects would.
fn see(state: &mut KindState, info: &KindInfo, full: bool, obj: &Value, how: How, at: Instant) -> Option<Record> {
    let raw = serde_json::value::to_raw_value(obj).unwrap();
    let head: Head<'_> = serde_json::from_str(raw.get()).unwrap();
    let whole = full.then(|| obj.clone());
    let obs = Observed::new(head, whole, info).unwrap();
    observe(state, info, full, obs, how, jiff::Timestamp::now(), at)
}

fn spec_fields() -> Value {
    json!({"f:data": {"f:a": {}}})
}

fn status_fields() -> Value {
    json!({"f:status": {"f:observed": {}}})
}

#[test]
fn status_and_resource_version_churn_is_not_a_change() {
    let (mut st, info, at) = (state(), info(false), Instant::now());
    let w = |status_secs| {
        vec![writer("kubectl", "", 0, spec_fields()), writer("ctl", "status", status_secs, status_fields())]
    };
    let v1 = object(1, 1, json!({"app": "web"}), json!({"a": "1"}), w(0));
    assert!(see(&mut st, &info, false, &v1, How::Listed, at).is_none(), "listed first: seeded");
    let v2 = object(2, 1, json!({"app": "web"}), json!({"a": "1"}), w(60));
    assert!(see(&mut st, &info, false, &v2, How::Modified, at).is_none());
    // Whole objects: their status is left out of the comparison.
    let mut whole = state();
    see(&mut whole, &info, true, &v1, How::Listed, at);
    assert!(see(&mut whole, &info, true, &v2, How::Modified, at).is_none());
}

#[test]
fn a_label_change_is_recorded_with_who_and_a_diff() {
    let (mut st, info, at) = (state(), info(false), Instant::now());
    let v1 = object(1, 1, json!({"app": "web"}), json!({}), vec![writer("kubectl", "", 0, spec_fields())]);
    see(&mut st, &info, false, &v1, How::Listed, at);
    let labels = json!({"f:metadata": {"f:labels": {"f:tier": {}}}});
    let v2 = object(
        2,
        1,
        json!({"app": "web", "tier": "front"}),
        json!({}),
        vec![writer("kubectl", "", 0, spec_fields()), writer("kubectl-label", "", 30, labels)],
    );
    let r = see(&mut st, &info, false, &v2, How::Modified, at).expect("a record");
    assert_eq!(r.what, What::Updated);
    assert_eq!(r.summary, "labels changed: tier by kubectl-label");
    assert_eq!(r.time.to_string(), ts(30));
    let d = r.diff().expect("a diff");
    assert!(d.lines().iter().any(|l| matches!(l, history::DiffLine::Added(t) if t.contains("tier: front"))));
}

#[test]
fn a_spec_write_is_recorded_a_status_write_bumping_the_generation_is_not() {
    let (mut st, info, at) = (state(), info(false), Instant::now());
    let v1 = object(1, 1, json!({}), json!({}), vec![writer("helm", "", 0, spec_fields())]);
    see(&mut st, &info, false, &v1, How::Listed, at);
    let edit = json!({"f:spec": {"f:replicas": {}}});
    let v2 = object(
        2,
        2,
        json!({}),
        json!({}),
        vec![writer("helm", "", 0, spec_fields()), writer("kubectl-edit", "", 10, edit)],
    );
    let r = see(&mut st, &info, false, &v2, How::Modified, at).expect("a record");
    assert_eq!(r.summary, "spec changed (generation 1 → 2) by kubectl-edit");
    // A custom resource without a status subresource: its operator's status write bumps the
    // generation too.
    let v3 = object(
        3,
        3,
        json!({}),
        json!({}),
        vec![
            writer("helm", "", 0, spec_fields()),
            writer("kubectl-edit", "", 10, json!({"f:spec": {"f:replicas": {}}})),
            writer("operator", "", 20, status_fields()),
        ],
    );
    assert!(see(&mut st, &info, false, &v3, How::Modified, at).is_none());
    // A spec write in the same second as the writer's last one shows in the generation alone.
    let v4 = object(
        4,
        4,
        json!({}),
        json!({}),
        vec![
            writer("helm", "", 0, spec_fields()),
            writer("kubectl-edit", "", 10, json!({"f:spec": {"f:replicas": {}}})),
            writer("operator", "", 20, status_fields()),
        ],
    );
    assert_eq!(see(&mut st, &info, false, &v4, How::Modified, at).unwrap().summary, "spec changed (generation 3 → 4)");
}

#[test]
fn an_older_observation_is_ignored() {
    let (mut st, info, at) = (state(), info(false), Instant::now());
    let w = |secs| vec![writer("kubectl", "", secs, spec_fields())];
    see(&mut st, &info, false, &object(5, 1, json!({"v": "5"}), json!({}), w(5)), How::Listed, at);
    // A list behind the watch: version 4 came before 5.
    assert!(see(&mut st, &info, false, &object(4, 1, json!({"v": "4"}), json!({}), w(4)), How::Listed, at).is_none());
    assert!(see(&mut st, &info, false, &object(6, 1, json!({"v": "6"}), json!({}), w(6)), How::Modified, at).is_some());
}

#[test]
fn creations_and_deletions() {
    let (mut st, info, at) = (state(), info(false), Instant::now());
    let v1 = object(1, 1, json!({"app": "web"}), json!({}), vec![writer("kubectl-create", "", 0, spec_fields())]);
    let r = see(&mut st, &info, false, &v1, How::Added, at).expect("created");
    assert_eq!((r.what, r.summary.as_str(), r.uid.as_str()), (What::Created, "created by kubectl-create", "u1"));
    let r =
        see(&mut st, &info, false, &object(2, 1, json!({"app": "web"}), json!({}), vec![]), How::Deleted, at).unwrap();
    assert_eq!((r.what, r.name.as_str(), r.uid.as_str()), (What::Deleted, "web", "u1"));
    assert!(st.objects.is_empty());
    // What a controller made (a Job of a CronJob) comes and goes without a record.
    let mut owned = object(3, 1, json!({}), json!({}), vec![]);
    owned["metadata"]["ownerReferences"] = json!([{"controller": true, "kind": "CronJob", "name": "x", "uid": "c"}]);
    assert!(see(&mut st, &info, false, &owned, How::Added, at).is_none());
    assert!(see(&mut st, &info, false, &owned, How::Deleted, at).is_none());
}

#[test]
fn a_secret_s_values_are_never_kept_only_which_keys_changed() {
    let (mut st, info, at) = (state(), info(true), Instant::now());
    let v1 = object(1, 1, json!({}), json!({"alpha": "b25l", "beta": "dHdv"}), vec![]);
    see(&mut st, &info, true, &v1, How::Listed, at);
    let mut v2 = object(2, 1, json!({}), json!({"alpha": "dGhyZWU=", "beta": "dHdv"}), vec![]);
    v2["metadata"]["annotations"] =
        json!({"kubectl.kubernetes.io/last-applied-configuration": "{\"data\":{\"alpha\":\"dGhyZWU=\"}}"});
    let r = see(&mut st, &info, true, &v2, How::Modified, at).expect("a record");
    assert_eq!(r.summary, "changed data.alpha");
    let text = serde_json::to_string(&r).unwrap();
    for value in ["b25l", "dGhyZWU=", "dHdv", "#"] {
        assert!(!text.contains(value), "{value} in {text}");
    }
    let after = r.after.as_ref().unwrap().get();
    assert!(after.contains(r#""alpha":"(hidden, changed)""#) && after.contains(r#""beta":"(hidden)""#), "{after}");
}

#[test]
fn a_whole_object_s_diff_names_the_fields() {
    let (mut st, info, at) = (state(), info(false), Instant::now());
    see(&mut st, &info, true, &object(1, 1, json!({}), json!({"a": "1", "b": "2"}), vec![]), How::Listed, at);
    let w = vec![writer("kubectl-edit", "", 40, spec_fields())];
    let r =
        see(&mut st, &info, true, &object(2, 1, json!({}), json!({"a": "3", "b": "2"}), w), How::Modified, at).unwrap();
    assert_eq!(r.summary, "changed data.a by kubectl-edit");
    let lines = r.diff().unwrap().lines();
    let added = |l: &history::DiffLine| matches!(l, history::DiffLine::Added(t) if t.trim_start().starts_with("a:") && t.contains('3'));
    assert!(lines.iter().any(added), "{lines:?}");
    let status = |l: &history::DiffLine| matches!(l, history::DiffLine::Added(t) | history::DiffLine::Removed(t) if t.contains("observed"));
    assert!(!lines.iter().any(status), "{lines:?}");
}

#[test]
fn an_object_written_all_the_time_is_recorded_ten_times_an_hour() {
    let (mut st, info, start) = (state(), info(false), Instant::now());
    see(&mut st, &info, false, &object(1, 1, json!({"n": "0"}), json!({}), vec![]), How::Listed, start);
    let mut recorded = 0;
    for i in 2..30u64 {
        let v = object(i, 1, json!({"n": i.to_string()}), json!({}), vec![]);
        recorded += usize::from(see(&mut st, &info, false, &v, How::Modified, start).is_some());
    }
    assert_eq!(recorded, usize::from(BURST));
    // An hour later: recorded again, saying how many were not.
    let later = start + Duration::from_secs(3601);
    let r = see(&mut st, &info, false, &object(40, 1, json!({"n": "40"}), json!({}), vec![]), How::Modified, later)
        .unwrap();
    assert!(r.summary.ends_with("(18 changes before it not recorded: too many)"), "{}", r.summary);
}

proptest! {
    /// For any mix of writes, a record is made exactly when something outside the status
    /// changed (labels here), never for status and resourceVersion churn.
    #[test]
    fn records_only_what_changed(steps in prop::collection::vec(any::<bool>(), 1..30)) {
        let (mut st, info) = (state(), info(false));
        let start = Instant::now();
        let mut labels = 0u32;
        let mut status = 0i64;
        let version = |rv: u64, labels: u32, status: i64| {
            object(rv, 1, json!({"n": labels.to_string()}), json!({}), vec![writer("ctl", "status", status, status_fields())])
        };
        see(&mut st, &info, false, &version(1, 0, 0), How::Listed, start);
        for (i, change) in steps.into_iter().enumerate() {
            if change { labels += 1 } else { status += 1 }
            // Hours apart: the hourly bound never applies.
            let at = start + Duration::from_secs(3601 * (i as u64 + 1));
            let r = see(&mut st, &info, false, &version(i as u64 + 2, labels, status), How::Modified, at);
            prop_assert_eq!(r.is_some(), change);
        }
    }
}
