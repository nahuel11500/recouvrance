//! The pods whose logs the checked rows of a table stand for: a pod itself, or the pods a
//! workload or a Service selects. Read lean: only what following their logs needs.

use anyhow::Result;
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::Resource as _;
use kube::api::{DynamicObject, GetParams};
use kube::core::Request;
use kube::discovery::ApiResource;
use serde::{Deserialize, Serialize};

use super::{Kind, ar_of, lists, pod_containers, pod_selection, run};

/// A pod as the logs of checked rows follow it.
#[derive(Clone, Debug, PartialEq)]
pub struct LogPod {
    pub name: String,
    pub namespace: String,
    /// Running now: past the limit of pods followed, these go first.
    pub running: bool,
    /// Its `creationTimestamp` as written (RFC 3339 sorts as text): the newest go first.
    pub created: String,
    /// Its containers, the default one first (`pod_containers`).
    pub containers: Vec<String>,
}

/// The fields of a pod `LogPod` needs, with the names the API gives them: everything else
/// is skipped while parsing, never held. Written back as JSON, it reads as a pod to
/// `pod_containers`.
#[derive(Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct LeanPod {
    #[serde(default)]
    metadata: LeanMeta,
    #[serde(default)]
    spec: LeanSpec,
    #[serde(default)]
    status: LeanStatus,
}

#[derive(Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct LeanMeta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    namespace: String,
    #[serde(default)]
    creation_timestamp: Option<String>,
    /// Only the annotation naming the default container: others can be large.
    #[serde(default)]
    annotations: Option<DefaultContainer>,
}

#[derive(Deserialize, Serialize, Default)]
struct DefaultContainer {
    #[serde(rename = "kubectl.kubernetes.io/default-container", default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct LeanSpec {
    #[serde(default)]
    containers: Vec<LeanContainer>,
    #[serde(default)]
    init_containers: Vec<LeanContainer>,
    #[serde(default)]
    ephemeral_containers: Vec<LeanContainer>,
}

#[derive(Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
struct LeanContainer {
    #[serde(default)]
    name: String,
    /// `Always` on an init container makes it a sidecar.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    restart_policy: Option<String>,
}

#[derive(Deserialize, Serialize, Default)]
struct LeanStatus {
    #[serde(default)]
    phase: Option<String>,
}

impl LeanPod {
    fn into_log_pod(self) -> LogPod {
        let containers = serde_json::to_value(&self).map_or_default(|v| pod_containers(&v));
        LogPod {
            running: self.status.phase.as_deref() == Some("Running"),
            created: self.metadata.creation_timestamp.unwrap_or_default(),
            name: self.metadata.name,
            namespace: self.metadata.namespace,
            containers,
        }
    }
}

/// What finding a workload's pods needs of it: its selector.
#[derive(Deserialize, Default)]
struct LeanSelector {
    #[serde(default)]
    spec: SelectorSpec,
}

#[derive(Deserialize, Default)]
struct SelectorSpec {
    #[serde(default)]
    selector: serde_json::Value,
}

/// One object of `ar`, parsed into `T` (only the fields it has are kept).
async fn get_lean<T>(client: Client, ar: ApiResource, ns: String, name: String) -> Result<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    run(async move {
        let request =
            Request::new(DynamicObject::url_path(&ar, Some(ns.as_str()))).get(&name, &GetParams::default())?;
        Ok(client.request::<T>(request).await?)
    })
    .await
}

/// One pod, as its logs need it.
pub async fn log_pod(client: Client, ns: String, name: String) -> Result<LogPod> {
    let pod: LeanPod = get_lean(client, ar_of::<Pod>(), ns, name).await?;
    Ok(pod.into_log_pod())
}

/// The pods a workload or a Service selects (`pod_selection`), as their logs need them; none
/// when it selects none.
pub async fn log_pods_of(client: Client, kind: Kind, ns: String, name: String) -> Result<Vec<LogPod>> {
    let object: LeanSelector = get_lean(client.clone(), kind.ar.clone(), ns.clone(), name).await?;
    let lean = serde_json::json!({ "spec": { "selector": object.spec.selector } });
    let Some(selector) = pod_selection(&kind, &lean).list_selector() else { return Ok(Vec::new()) };
    let pods: Vec<LeanPod> = lists::list_lean_cached_by(client, ar_of::<Pod>(), Some(ns), selector).await?;
    Ok(pods.into_iter().map(LeanPod::into_log_pod).collect())
}

/// Whether the rows of `kind` have logs to read together: pods, and the workloads and
/// Services that select pods.
pub fn has_pod_logs(kind: &Kind) -> bool {
    kind.is("", "Pod")
        || kind.is("", "Service")
        || kind.is("batch", "Job")
        || (kind.ar.group == "apps"
            && matches!(kind.ar.kind.as_str(), "Deployment" | "StatefulSet" | "DaemonSet" | "ReplicaSet"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read lean, a pod still gives its containers as the pages list them, default first, and
    /// the annotations that are not the default container's are never held.
    #[test]
    fn a_lean_pod_keeps_what_its_logs_need() {
        let pod = serde_json::json!({
            "metadata": {
                "name": "web-1",
                "namespace": "shop",
                "creationTimestamp": "2026-11-20T10:00:00Z",
                "annotations": {"kubectl.kubernetes.io/default-container": "app", "big": "x".repeat(1000)},
                "labels": {"app": "web"}
            },
            "spec": {
                "containers": [{"name": "proxy", "image": "p"}, {"name": "app", "image": "a"}],
                "initContainers": [{"name": "agent", "restartPolicy": "Always"}, {"name": "migrate"}]
            },
            "status": {"phase": "Running", "podIP": "10.0.0.1"}
        });
        let lean: LeanPod = serde_json::from_value(pod.clone()).unwrap();
        let back = serde_json::to_string(&lean).unwrap();
        assert!(!back.contains("big") && !back.contains("image"));
        let got = lean.into_log_pod();
        assert_eq!(got.containers, pod_containers(&pod));
        assert_eq!(got.containers[0], "app");
        assert_eq!(
            (got.name.as_str(), got.namespace.as_str(), got.running, got.created.as_str()),
            ("web-1", "shop", true, "2026-11-20T10:00:00Z")
        );
        // A pending pod with nothing else set.
        let bare: LeanPod = serde_json::from_value(serde_json::json!({"metadata": {"name": "x"}})).unwrap();
        let bare = bare.into_log_pod();
        assert!(!bare.running && bare.containers.is_empty());
    }

    #[test]
    fn rows_with_logs() {
        for (group, kind) in [
            ("", "Pod"),
            ("", "Service"),
            ("apps", "Deployment"),
            ("apps", "StatefulSet"),
            ("apps", "DaemonSet"),
            ("apps", "ReplicaSet"),
            ("batch", "Job"),
        ] {
            assert!(has_pod_logs(&Kind::fake(group, kind)), "{kind}");
        }
        // A PVC's selector picks volumes; a CronJob's pods are its Jobs'.
        for (group, kind) in [("", "PersistentVolumeClaim"), ("batch", "CronJob"), ("", "ConfigMap"), ("x.io", "Pod")] {
            assert!(!has_pod_logs(&Kind::fake(group, kind)), "{kind}");
        }
    }
}
