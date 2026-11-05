use serde_json::{Value, json};

use super::*;

fn kind(group: &str, k: &str) -> Kind {
    Kind::fake(group, k)
}

fn raw(v: &Value) -> Box<RawValue> {
    serde_json::value::to_raw_value(v).unwrap()
}

fn cm(name: &str, uid: &str, rv: u64, value: &str) -> Box<RawValue> {
    raw(&json!({
        "metadata": {
            "name": name, "namespace": "default", "uid": uid, "resourceVersion": rv.to_string(),
            "creationTimestamp": "2020-01-01T00:00:00Z", "labels": {"v": value},
        },
        "data": {"k": value},
    }))
}

/// A recorder that keeps everything in memory.
fn recorder() -> &'static Recorder {
    Box::leak(Box::new(Recorder::new(None)))
}

#[test]
fn pods_and_events_are_not_recorded() {
    assert!(!recorded(&kind("", "Pod")));
    assert!(!recorded(&kind("events.k8s.io", "Event")));
    assert!(!recorded(&kind("metrics.k8s.io", "PodMetrics")));
    assert!(recorded(&kind("", "ConfigMap")));
    assert!(recorded(&kind("apps", "Deployment")));
}

#[test]
fn a_list_again_finds_what_was_deleted_meanwhile() {
    let rec = recorder();
    let tap = rec.tap("ctx", &kind("", "ConfigMap"), None, false);
    tap.begin_list();
    tap.listed(&cm("a", "ua", 1, "1"));
    tap.listed(&cm("b", "ub", 2, "1"));
    tap.end_list();
    assert!(rec.records_of("ctx").is_empty(), "the first list only notes what is there");
    std::thread::sleep(Duration::from_millis(5));
    tap.begin_list();
    tap.listed(&cm("a", "ua", 1, "1"));
    tap.end_list();
    let records = rec.records_of("ctx");
    assert_eq!(records.len(), 1);
    assert_eq!((records[0].what, records[0].name.as_str(), records[0].uid.as_str()), (What::Deleted, "b", "ub"));
}

#[test]
fn a_list_of_one_namespace_deletes_nothing_elsewhere() {
    let rec = recorder();
    let all = rec.tap("ctx", &kind("", "ConfigMap"), None, false);
    all.begin_list();
    all.listed(&cm("a", "ua", 1, "1"));
    all.end_list();
    std::thread::sleep(Duration::from_millis(5));
    let other = rec.tap("ctx", &kind("", "ConfigMap"), Some("other".into()), false);
    other.begin_list();
    other.end_list();
    assert!(rec.records_of("ctx").is_empty());
}

#[test]
fn two_watches_of_a_kind_record_a_change_once_by_context() {
    let rec = recorder();
    let (one, two) =
        (rec.tap("ctx", &kind("", "ConfigMap"), None, false), rec.tap("ctx", &kind("", "ConfigMap"), None, false));
    let elsewhere = rec.tap("other", &kind("", "ConfigMap"), None, false);
    for t in [&one, &two, &elsewhere] {
        t.begin_list();
        t.listed(&cm("a", "ua", 1, "1"));
        t.end_list();
    }
    one.event("MODIFIED", &cm("a", "ua", 2, "2"));
    two.event("MODIFIED", &cm("a", "ua", 2, "2"));
    assert_eq!(rec.records_of("ctx").len(), 1);
    // The same name on another cluster is another object.
    assert!(rec.records_of("other").is_empty());
    elsewhere.event("MODIFIED", &cm("a", "ua", 2, "2"));
    assert_eq!(rec.records_of("other").len(), 1);
}

#[test]
fn a_watch_of_whole_objects_takes_over_from_metadata() {
    let rec = recorder();
    let lists = rec.tap("ctx", &kind("", "ConfigMap"), None, false);
    let whole = rec.tap("ctx", &kind("", "ConfigMap"), None, true);
    whole.begin_list();
    whole.listed(&cm("a", "ua", 1, "1"));
    whole.end_list();
    lists.event("MODIFIED", &cm("a", "ua", 2, "2"));
    assert!(rec.records_of("ctx").is_empty(), "left to the whole watch");
    whole.event("MODIFIED", &cm("a", "ua", 2, "2"));
    let records = rec.records_of("ctx");
    assert_eq!(records.len(), 1);
    assert!(records[0].summary.starts_with("changed data.k, metadata.labels.v"), "{}", records[0].summary);
    drop(whole);
    // Compared on its metadata from now on: the first observation is the new baseline.
    lists.event("MODIFIED", &cm("a", "ua", 3, "3"));
    assert_eq!(rec.records_of("ctx").len(), 1);
    lists.event("MODIFIED", &cm("a", "ua", 4, "4"));
    assert_eq!(rec.records_of("ctx").len(), 2, "recorded again once the whole watch stopped");
}

fn secret(rv: u64, owner: bool) -> Box<RawValue> {
    let mut v = json!({
        "metadata": {
            "name": "scan", "namespace": "default", "uid": "us", "resourceVersion": rv.to_string(),
            "creationTimestamp": jiff::Timestamp::now().to_string(),
        },
        "data": {"k": "dg=="},
    });
    if owner {
        v["metadata"]["ownerReferences"] = json!([{"controller": true, "kind": "Job", "name": "scan", "uid": "j"}]);
    }
    raw(&v)
}

#[test]
fn a_creation_is_recorded_once_held_back_unless_a_controller_adopts_it() {
    let hold = Duration::from_millis(30);
    let rec: &'static Recorder = Box::leak(Box::new(Recorder { hold, ..Recorder::new(None) }));
    for full in [false, true] {
        let ctx = if full { "whole" } else { "meta" };
        let tap = rec.tap(ctx, &kind("", "Secret"), None, full);
        tap.begin_list();
        tap.end_list();
        // Made, adopted by its operator a moment later, then deleted: nothing to say.
        tap.event("ADDED", &secret(1, false));
        tap.event("MODIFIED", &secret(2, true));
        tap.event("DELETED", &secret(3, true));
        std::thread::sleep(hold * 2);
        rec.release();
        assert!(rec.records_of(ctx).is_empty(), "{:?}", rec.records_of(ctx));
    }
    let tap = rec.tap("ctx", &kind("", "Secret"), None, true);
    tap.begin_list();
    tap.end_list();
    tap.event("ADDED", &secret(1, false));
    assert!(rec.records_of("ctx").is_empty(), "held back");
    std::thread::sleep(hold * 2);
    rec.release();
    let whats: Vec<What> = rec.records_of("ctx").iter().map(|r| r.what).collect();
    assert_eq!(whats, [What::Created]);
    // Made and deleted before its creation was due: both, in order.
    let other = rec.tap("other", &kind("", "Secret"), None, true);
    other.begin_list();
    other.end_list();
    other.event("ADDED", &secret(1, false));
    other.event("DELETED", &secret(2, false));
    let whats: Vec<What> = rec.records_of("other").iter().map(|r| r.what).collect();
    assert_eq!(whats, [What::Created, What::Deleted]);
}

#[test]
fn a_record_is_a_history_entry_of_its_source() {
    let r = Record {
        time: jiff::Timestamp::from_second(1_700_000_000).unwrap(),
        resource: "configmaps".into(),
        kind: "ConfigMap".into(),
        namespace: Some("default".into()),
        name: "web".into(),
        uid: "u".into(),
        what: What::Updated,
        summary: "changed data.k".into(),
        before: Some(raw(&json!({"data": {"k": "1"}}))),
        after: Some(raw(&json!({"data": {"k": "2"}}))),
    };
    let c = r.change();
    assert_eq!((c.source, c.object.as_str()), (Source::Recorded, "ConfigMap web"));
    let short = c.diff.unwrap().short(5);
    let lines: Vec<&str> = short.lines().collect();
    assert_eq!(lines.len(), 2, "{short}");
    assert!(
        lines[0].starts_with("- ") && lines[0].contains('1') && lines[1].starts_with("+ ") && lines[1].contains('2')
    );
}
