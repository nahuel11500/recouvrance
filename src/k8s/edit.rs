//! Saving an object edited as YAML, what `kubectl edit` does: the whole object is sent back,
//! pinned to the version the edit started from. A change made on the cluster meanwhile (an
//! HPA scaling it, a controller or Argo CD writing to it) makes the API server refuse it with
//! a 409, instead of the edit silently undoing that change. The edit can then be moved onto
//! the latest version ([`rebase`]) and saved again.

use anyhow::{Context as _, Result};
use kube::Client;
use kube::api::DynamicObject;
use serde_json::{Map, Value};

use super::writes::dyn_api;
use super::{Kind, run};

/// Replace the object with `edited`, sent as of `base_version` (the version the edit started
/// from, whatever the text says). Returns the object as the server has it after, or as it
/// would have it with `dry_run`.
pub async fn save_edit(
    client: Client,
    kind: Kind,
    ns: Option<String>,
    mut edited: Value,
    base_version: Option<String>,
    dry_run: bool,
) -> Result<Value> {
    run(async move {
        pin_version(&mut edited, base_version.as_deref());
        let name = edited.pointer("/metadata/name").and_then(Value::as_str).context("no metadata.name")?.to_string();
        let obj: DynamicObject = serde_json::from_value(edited)?;
        let api = dyn_api(client, &kind, ns.as_deref());
        let pp = kube::api::PostParams { dry_run, field_manager: Some("recouvrance".into()) };
        Ok(serde_json::to_value(api.replace(&name, &pp, &obj).await?)?)
    })
    .await
}

/// Whether a write was refused because the object changed since the version it was sent as.
pub fn is_conflict(e: &anyhow::Error) -> bool {
    super::api_status(e) == Some(409)
}

/// Send `edited` as of `base`: its own resourceVersion (kept, removed or typed over in the
/// editor) would decide what a change meanwhile does. No base keeps what it says.
pub fn pin_version(edited: &mut Value, base: Option<&str>) {
    if let Some(base) = base
        && let Some(meta) = edited.get_mut("metadata").and_then(Value::as_object_mut)
    {
        meta.insert("resourceVersion".into(), Value::String(base.to_string()));
    }
}

/// The object's resourceVersion.
pub fn resource_version(v: &Value) -> Option<&str> {
    v.pointer("/metadata/resourceVersion").and_then(Value::as_str)
}

/// Who wrote to the object last, from its managedFields: the manager of the newest entry,
/// and when. None when it keeps none (or they have no time).
pub fn last_writer(v: &Value) -> Option<(String, jiff::Timestamp)> {
    crate::json::arr_at(v, "/metadata/managedFields")
        .iter()
        .filter_map(|f| Some((crate::json::str_at(f, "/manager")?.to_string(), crate::json::time_at(f, "/time")?)))
        .max_by_key(|(_, t)| *t)
}

/// The edit from `base` to `edited`, made on `live` instead: the fields the edit changed take
/// their edited value, every other field keeps what `live` has (what changed meanwhile
/// included). Lists are taken whole, as a JSON merge patch takes them. managedFields are
/// left out (the server keeps its own); the result carries `live`'s resourceVersion.
pub fn rebase(base: &Value, edited: &Value, live: &Value) -> Value {
    let lean = |v: &Value| {
        let mut v = v.clone();
        if let Some(meta) = v.get_mut("metadata").and_then(Value::as_object_mut) {
            meta.remove("managedFields");
            meta.remove("resourceVersion");
        }
        v
    };
    let mut out = lean(live);
    merge_apply(&mut out, &merge_diff(&lean(base), &lean(edited)));
    pin_version(&mut out, resource_version(live));
    out
}

/// The JSON merge patch (RFC 7386) that makes `from` into `to`. A field `to` sets to null
/// reads as removed: a merge patch can't say "set to null".
pub fn merge_diff(from: &Value, to: &Value) -> Value {
    let (Value::Object(f), Value::Object(t)) = (from, to) else { return to.clone() };
    let mut patch = Map::new();
    for k in f.keys().filter(|k| !t.contains_key(*k)) {
        patch.insert(k.clone(), Value::Null);
    }
    for (k, tv) in t {
        match f.get(k) {
            Some(fv) if fv == tv => {}
            Some(fv @ Value::Object(_)) if tv.is_object() => {
                patch.insert(k.clone(), merge_diff(fv, tv));
            }
            _ => {
                patch.insert(k.clone(), tv.clone());
            }
        }
    }
    Value::Object(patch)
}

/// Apply a JSON merge patch (RFC 7386) to `target`.
pub fn merge_apply(target: &mut Value, patch: &Value) {
    let Value::Object(p) = patch else {
        target.clone_from(patch);
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Map::new());
    }
    if let Value::Object(t) = target {
        for (k, v) in p {
            if v.is_null() {
                t.remove(k);
            } else {
                merge_apply(t.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::{Value, json};

    use super::{last_writer, merge_apply, merge_diff, pin_version, rebase};

    /// Values with no nulls (a merge patch reads null as "remove"), objects nested a few deep.
    fn value() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            any::<bool>().prop_map(Value::Bool),
            any::<i32>().prop_map(|n| json!(n)),
            "[a-z]{0,4}".prop_map(Value::String),
            prop::collection::vec("[a-z]{0,3}".prop_map(Value::String), 0..3).prop_map(Value::Array),
        ];
        leaf.prop_recursive(4, 32, 4, |inner| {
            prop::collection::btree_map("[a-d]", inner, 0..4).prop_map(|m| Value::Object(m.into_iter().collect()))
        })
    }

    fn object() -> impl Strategy<Value = Value> {
        prop::collection::btree_map("[a-d]", value(), 0..5).prop_map(|m| Value::Object(m.into_iter().collect()))
    }

    proptest! {
        /// The patch between two values makes the first into the second, whatever they hold.
        #[test]
        fn the_patch_makes_from_into_to(from in value(), to in value()) {
            let mut out = from.clone();
            merge_apply(&mut out, &merge_diff(&from, &to));
            prop_assert_eq!(out, to);
        }

        /// No edit keeps the live object as it is; an edit of an unchanged object is the edit.
        #[test]
        fn a_rebase_keeps_what_the_edit_did_not_touch(base in object(), edited in object(), live in object()) {
            prop_assert_eq!(rebase(&base, &base, &live), live);
            prop_assert_eq!(rebase(&base, &edited, &base), edited);
        }
    }

    /// A field changed meanwhile and one changed by the edit both stay; the version is the
    /// live one, so the save is checked against it.
    #[test]
    fn an_edit_moves_onto_the_latest_version() {
        let base =
            json!({"metadata": {"name": "a", "resourceVersion": "1"}, "spec": {"replicas": 2, "image": "web:1"}});
        let edited =
            json!({"metadata": {"name": "a", "resourceVersion": "1"}, "spec": {"replicas": 2, "image": "web:2"}});
        let live = json!({
            "metadata": {"name": "a", "resourceVersion": "7", "managedFields": [{"manager": "hpa"}]},
            "spec": {"replicas": 5, "image": "web:1"},
        });
        let out = rebase(&base, &edited, &live);
        assert_eq!(
            out,
            json!({"metadata": {"name": "a", "resourceVersion": "7"}, "spec": {"replicas": 5, "image": "web:2"}})
        );
    }

    /// The text's own resourceVersion, removed or typed over, never decides.
    #[test]
    fn the_version_sent_is_the_one_the_edit_started_from() {
        for meta in [json!({"name": "a"}), json!({"name": "a", "resourceVersion": "99"})] {
            let mut edited = json!({"metadata": meta});
            pin_version(&mut edited, Some("4"));
            assert_eq!(edited.pointer("/metadata/resourceVersion"), Some(&json!("4")));
        }
    }

    #[test]
    fn the_last_writer_is_the_newest_entry() {
        let v = json!({"metadata": {"managedFields": [
            {"manager": "kubectl", "time": "2026-01-01T00:00:00Z"},
            {"manager": "argocd-controller", "time": "2026-01-02T00:00:00Z"},
            {"manager": "no-time"},
        ]}});
        assert_eq!(last_writer(&v).map(|(m, _)| m).as_deref(), Some("argocd-controller"));
        assert_eq!(last_writer(&json!({"metadata": {}})), None);
    }
}
