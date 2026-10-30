use serde_json::{Value, json};

use super::*;

fn kind(group: &str, k: &str) -> Kind {
    Kind {
        ar: kube::discovery::ApiResource {
            group: group.into(),
            version: "v1".into(),
            api_version: if group.is_empty() { "v1".into() } else { format!("{group}/v1") },
            kind: k.into(),
            plural: format!("{}s", k.to_lowercase()),
        },
        namespaced: true,
        section: "",
        label: k.into(),
    }
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
    std::thread::sleep(std::time::Duration::from_millis(5));
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
    std::thread::sleep(std::time::Duration::from_millis(5));
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
