//! The external-secrets.io operator: `ExternalSecret`s and their stores that fail to sync (for
//! the Problems scan), and the `ExternalSecret` a Secret comes from (for its page). A cluster
//! without the operator's API has none of these kinds, so nothing here runs there.

use kube::Client;
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;

use crate::k8s::{self, Kind};
use crate::problems::{Problem, Severity};

pub const GROUP: &str = "external-secrets.io";

/// Longest reason shown for a failing object (provider messages can be paragraphs).
const REASON_MAX: usize = 200;

/// The Ready condition as (status, reason, message); None before the operator wrote one.
pub fn ready(obj: &Value) -> Option<(&str, &str, &str)> {
    let conds = obj.pointer("/status/conditions")?.as_array()?;
    let c = conds.iter().find(|c| c["type"].as_str() == Some("Ready"))?;
    let text = |k: &str| c[k].as_str().unwrap_or_default();
    Some((text("status"), text("reason"), text("message")))
}

/// What the Problems scan reads of one of the operator's objects: its name and age, and its
/// status's conditions and last sync. The rest (spec, managedFields) is skipped while its list
/// is parsed.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct LeanObject {
    metadata: LeanMeta,
    status: LeanStatus,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct LeanMeta {
    name: Option<String>,
    namespace: Option<String>,
    creation_timestamp: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct LeanStatus {
    #[serde(deserialize_with = "k8s::null_default")]
    conditions: Vec<LeanCondition>,
    /// Only whether it is set.
    refresh_time: Option<IgnoredAny>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LeanCondition {
    #[serde(rename = "type")]
    kind: Option<String>,
    status: Option<String>,
    reason: Option<String>,
    message: Option<String>,
}

impl LeanObject {
    /// Its Ready condition, as `ready` reads it of a whole object.
    fn ready(&self) -> Option<(&str, &str, &str)> {
        let c = self.status.conditions.iter().find(|c| c.kind.as_deref() == Some("Ready"))?;
        Some((
            c.status.as_deref().unwrap_or_default(),
            c.reason.as_deref().unwrap_or_default(),
            c.message.as_deref().unwrap_or_default(),
        ))
    }

    /// Whether an `ExternalSecret` never synced: the operator sets `status.refreshTime` on each
    /// sync that worked and never clears it, so without one its Secret was never written.
    fn never_synced(&self) -> bool {
        self.status.refresh_time.is_none()
    }

    /// The problem it is, as an object of `kind`; None when it looks healthy.
    fn problem(&self, kind: &Kind) -> Option<Problem> {
        let (severity, reason) = judge(&kind.ar.kind, self)?;
        Some(Problem {
            severity,
            kind: kind.kubectl_name(),
            label: kind.ar.kind.clone(),
            namespace: self.metadata.namespace.clone(),
            name: self.metadata.name.clone()?,
            reason,
            age: self.metadata.creation_timestamp.as_deref().and_then(|t| t.parse().ok()),
        })
    }
}

/// Judge an `ExternalSecret`, `SecretStore` or `ClusterSecretStore` from its object; None when
/// it looks healthy. An `ExternalSecret` not Ready is a warning (its Secret keeps the last
/// values), critical when it never synced (its Secret may not exist). A store not Ready is
/// critical: every `ExternalSecret` using it fails.
pub fn judge(kind: &str, obj: &LeanObject) -> Option<(Severity, String)> {
    let (status, reason, message) = obj.ready()?;
    // Ready is good when True; Unknown is the operator still deciding.
    let failing = crate::status::true_is_good("Ready") && status == "False";
    if !failing {
        return None;
    }
    let why = shorten(if message.trim().is_empty() { reason } else { message });
    let why = if why.is_empty() { "not ready".to_string() } else { why };
    match kind {
        "ExternalSecret" if obj.never_synced() => Some((Severity::Critical, format!("never synced: {why}"))),
        "ExternalSecret" => Some((Severity::Warning, why)),
        "SecretStore" | "ClusterSecretStore" => Some((Severity::Critical, why)),
        _ => None,
    }
}

/// The first line of `text`, at most [`REASON_MAX`] characters.
pub fn shorten(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default().trim();
    if line.chars().count() <= REASON_MAX {
        return line.to_string();
    }
    let cut: String = line.chars().take(REASON_MAX - 1).collect();
    format!("{}…", cut.trim_end())
}

/// Problems among the objects of one of the operator's kinds, read from their objects (the
/// conditions' messages are not in kubectl's columns), lean: one list from the API server's
/// cache, parsed as it streams in.
pub async fn problems(client: Client, kind: &Kind, namespace: Option<String>) -> anyhow::Result<Vec<Problem>> {
    let ns = if kind.namespaced { namespace } else { None };
    let items = k8s::lists::list_lean_cached::<LeanObject>(client, kind.ar.clone(), ns).await?;
    Ok(items.iter().filter_map(|obj| obj.problem(kind)).collect())
}

/// The `ExternalSecret` that owns a Secret, as (apiVersion, name), from its ownerReferences.
pub fn owner(secret: &Value) -> Option<(&str, &str)> {
    let owners = secret.pointer("/metadata/ownerReferences")?.as_array()?;
    owners.iter().find_map(|o| {
        let api_version = o["apiVersion"].as_str()?;
        let group = api_version.split_once('/').map(|(g, _)| g)?;
        (group == GROUP && o["kind"].as_str() == Some("ExternalSecret"))
            .then_some((api_version, o["name"].as_str()?))
            .filter(|(_, name)| !name.is_empty())
    })
}

/// The store an `ExternalSecret` reads from, as (kind, name). The kind defaults to `SecretStore`.
pub fn store_of(es: &Value) -> Option<(&str, &str)> {
    let r = es.pointer("/spec/secretStoreRef")?;
    let name = r["name"].as_str().filter(|n| !n.is_empty())?;
    Some((r["kind"].as_str().filter(|k| !k.is_empty()).unwrap_or("SecretStore"), name))
}

/// The Secret an `ExternalSecret` writes: `spec.target.name`, else its own name.
pub fn target_of(es: &Value) -> Option<&str> {
    es.pointer("/spec/target/name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .or_else(|| es.pointer("/metadata/name").and_then(Value::as_str))
}

/// The kubectl name of an operator kind, for links: `secretstores.external-secrets.io`.
pub fn kubectl_name(kind: &str) -> String {
    format!("{}s.{GROUP}", kind.to_lowercase())
}

/// The `ExternalSecret` a Secret comes from, when one owns it (one request). Read at the
/// version its owner reference names, else at the ones the operator serves today (a
/// version can stop being served after the reference was written).
pub async fn source_of(client: Client, namespace: &str, secret: &Value) -> Option<Value> {
    let (owner_version, name) = owner(secret)?;
    let mut versions = vec![owner_version.to_string()];
    for v in ["v1", "v1beta1"] {
        let v = format!("{GROUP}/{v}");
        if !versions.contains(&v) {
            versions.push(v);
        }
    }
    for v in versions {
        if let Some(es) =
            k8s::get_json(client.clone(), format!("/apis/{v}/namespaces/{namespace}/externalsecrets/{name}")).await
            && es["kind"].as_str() == Some("ExternalSecret")
        {
            return Some(es);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    /// An object as the scan's list reads it.
    fn lean(obj: &Value) -> LeanObject {
        serde_json::from_value(obj.clone()).unwrap()
    }

    fn es(status: &str, refresh: Value) -> Value {
        json!({
            "metadata": {"name": "app", "namespace": "prod"},
            "spec": {"secretStoreRef": {"name": "vault"}},
            "status": {
                "refreshTime": refresh,
                "conditions": [{"type": "Ready", "status": status, "reason": "SecretSyncedError",
                    "message": "could not get secret data from provider\nmore details"}],
            },
        })
    }

    #[test]
    fn judges_external_secrets() {
        let check = |obj: &Value| judge("ExternalSecret", &lean(obj));
        assert_eq!(check(&es("True", json!("2026-01-01T00:00:00Z"))), None);
        assert_eq!(
            check(&es("False", json!("2026-01-01T00:00:00Z"))),
            Some((Severity::Warning, "could not get secret data from provider".into()))
        );
        // Never synced: its Secret may not exist.
        let never = check(&es("False", Value::Null)).unwrap();
        assert_eq!(never.0, Severity::Critical);
        assert!(never.1.starts_with("never synced: could not get"));
        // Without a refreshTime at all.
        let mut fresh = es("False", Value::Null);
        fresh["status"].as_object_mut().unwrap().remove("refreshTime");
        assert_eq!(check(&fresh).unwrap().0, Severity::Critical);
        // Still being decided, or not yet seen by the operator.
        assert_eq!(check(&es("Unknown", Value::Null)), None);
        assert_eq!(check(&json!({"metadata": {"name": "x"}})), None);
    }

    #[test]
    fn judges_stores() {
        let store = |status: &str, message: &str| {
            let ready =
                json!({"type": "Ready", "status": status, "reason": "InvalidProviderConfig", "message": message});
            lean(&json!({"status": {"conditions": [ready]}}))
        };
        for kind in ["SecretStore", "ClusterSecretStore"] {
            assert_eq!(judge(kind, &store("True", "store validated")), None);
            assert_eq!(
                judge(kind, &store("False", "unable to validate store")),
                Some((Severity::Critical, "unable to validate store".into()))
            );
            // No message: the reason says why.
            assert_eq!(judge(kind, &store("False", "")), Some((Severity::Critical, "InvalidProviderConfig".into())));
        }
        assert_eq!(judge("PushSecret", &store("False", "x")), None);
    }

    #[test]
    fn finds_the_owner() {
        let secret = |api_version: &str, kind: &str| {
            json!({"metadata": {"ownerReferences": [
                {"apiVersion": "v1", "kind": "ConfigMap", "name": "other"},
                {"apiVersion": api_version, "kind": kind, "name": "app"},
            ]}})
        };
        assert_eq!(owner(&secret("external-secrets.io/v1", "ExternalSecret")), Some(("external-secrets.io/v1", "app")));
        assert_eq!(
            owner(&secret("external-secrets.io/v1beta1", "ExternalSecret")),
            Some(("external-secrets.io/v1beta1", "app"))
        );
        // Another operator's kind of the same name, or another kind of the group.
        assert_eq!(owner(&secret("example.com/v1", "ExternalSecret")), None);
        assert_eq!(owner(&secret("external-secrets.io/v1alpha1", "PushSecret")), None);
        assert_eq!(owner(&json!({"metadata": {"name": "plain"}})), None);
    }

    #[test]
    fn store_and_target() {
        let es = json!({"metadata": {"name": "app"}, "spec": {"secretStoreRef": {"name": "vault", "kind": "ClusterSecretStore"}}});
        assert_eq!(store_of(&es), Some(("ClusterSecretStore", "vault")));
        assert_eq!(target_of(&es), Some("app"));
        let es = json!({"metadata": {"name": "app"}, "spec": {"secretStoreRef": {"name": "vault"}, "target": {"name": "creds"}}});
        assert_eq!(store_of(&es), Some(("SecretStore", "vault")));
        assert_eq!(target_of(&es), Some("creds"));
        assert_eq!(kubectl_name("ClusterSecretStore"), "clustersecretstores.external-secrets.io");
    }

    proptest! {
        /// Any message, of any length and script, gives one short line.
        #[test]
        fn reasons_are_one_short_line(text in "\\PC{0,400}(\n\\PC{0,50})?") {
            let short = shorten(&text);
            prop_assert!(short.chars().count() <= REASON_MAX);
            prop_assert!(!short.contains('\n'));
        }

        /// Only a Ready condition that is False is a problem, whatever else the object says.
        #[test]
        fn only_ready_false_is_a_problem(
            status in "(True|False|Unknown|)",
            refresh in prop::option::of("[0-9TZ:-]{0,20}"),
            message in "\\PC{0,80}",
        ) {
            let obj = lean(&json!({"status": {"refreshTime": refresh,
                "conditions": [{"type": "Ready", "status": status, "message": message}]}}));
            for kind in ["ExternalSecret", "SecretStore", "ClusterSecretStore"] {
                prop_assert_eq!(judge(kind, &obj).is_some(), status == "False");
            }
        }

        /// Read lean, an object gives what its whole object gave before #842: its Ready
        /// condition, whether it ever synced, and its problem's namespace, name and age.
        #[test]
        fn read_lean_as_whole(
            conditions in prop::option::of(prop::collection::vec((
                prop::option::of("(Ready|Synced)"),
                prop::option::of("(True|False|Unknown|)"),
                prop::option::of("[A-Za-z]{0,12}"),
                prop::option::of("\\PC{0,30}(\n\\PC{0,10})?"),
            ), 0..3)),
            refresh in prop::option::of(prop::option::of("[0-9TZ:-]{1,20}")),
            name in prop::option::of("[a-z]{0,6}"),
            created in prop::option::of("(2026-01-01T00:00:00Z|soon|)"),
        ) {
            // `null` when None.
            let field = |v: Option<String>| v.map_or(Value::Null, Value::from);
            let conditions: Option<Vec<Value>> = conditions.map(|cs| {
                cs.into_iter()
                    .map(|(kind, status, reason, message)| {
                        json!({"type": field(kind), "status": field(status), "reason": field(reason),
                            "message": field(message), "lastTransitionTime": null})
                    })
                    .collect()
            });
            let mut status = json!({"conditions": conditions});
            if let Some(refresh) = refresh {
                status["refreshTime"] = field(refresh);
            }
            let obj = json!({
                "metadata": {"name": field(name), "namespace": "prod", "creationTimestamp": field(created),
                    "managedFields": [{"manager": "external-secrets", "fieldsV1": {"f:status": {}}}]},
                "spec": {"secretStoreRef": {"name": "vault"}, "data": [{"secretKey": "a", "remoteRef": {"key": "k"}}]},
                "status": status,
            });
            let read = lean(&obj);
            prop_assert_eq!(read.ready(), ready(&obj));
            prop_assert_eq!(read.never_synced(), obj.pointer("/status/refreshTime").is_none_or(Value::is_null));
            // What a whole object's problem took of it.
            let text = |p: &str| obj.pointer(p).and_then(Value::as_str);
            let found = read.problem(&Kind::fake(GROUP, "ExternalSecret"));
            let named = text("/metadata/name").is_some();
            prop_assert_eq!(found.is_some(), judge("ExternalSecret", &read).is_some() && named);
            if let Some(p) = found {
                prop_assert_eq!(p.namespace.as_deref(), text("/metadata/namespace"));
                prop_assert_eq!(Some(p.name.as_str()), text("/metadata/name"));
                let age = text("/metadata/creationTimestamp").and_then(|t| t.parse::<jiff::Timestamp>().ok());
                prop_assert_eq!(p.age, age);
            }
        }
    }
}
