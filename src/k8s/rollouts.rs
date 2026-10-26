//! A workload's rollouts: its revisions (a Deployment's ReplicaSets, a StatefulSet's or
//! DaemonSet's ControllerRevisions) and how its current rollout is going. Shared by the
//! Revisions tab and the agents' tools.

use anyhow::Result;
use k8s_openapi::api::apps::v1 as apps;
use kube::Client;
use serde_json::Value;

use super::{Kind, ar_of, controller_revisions, list_values, pod_selection};

/// The workload's revisions, ReplicaSets or ControllerRevisions shaped like them (the
/// revision annotation, `spec.template`), unsorted. Empty for other kinds.
pub async fn revisions(client: Client, kind: &Kind, ns: String, obj: &Value) -> Result<Vec<Value>> {
    let uid = obj.pointer("/metadata/uid").and_then(Value::as_str).unwrap_or_default().to_string();
    if kind.is("apps", "StatefulSet") || kind.is("apps", "DaemonSet") {
        return controller_revisions(client, ns, uid).await;
    }
    if !kind.is("apps", "Deployment") {
        return Ok(Vec::new());
    }
    let Some(Some(sel)) = pod_selection(kind, obj).list_selector() else { return Ok(Vec::new()) };
    let sets = list_values(client, ar_of::<apps::ReplicaSet>(), Some(ns), Some(sel)).await?;
    Ok(sets.into_iter().filter(|rs| owned_by(rs, &uid)).collect())
}

fn owned_by(obj: &Value, uid: &str) -> bool {
    obj.pointer("/metadata/ownerReferences")
        .and_then(Value::as_array)
        .is_some_and(|o| o.iter().any(|r| r["uid"].as_str() == Some(uid)))
}

/// A revision's number (0 when it has none).
pub fn number(rev: &Value) -> i64 {
    rev.pointer("/metadata/annotations/deployment.kubernetes.io~1revision")
        .and_then(Value::as_str)
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// The images of a revision's pod template, `container=image`.
pub fn images(rev: &Value) -> Vec<String> {
    rev.pointer("/spec/template/spec/containers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| format!("{}={}", c["name"].as_str().unwrap_or("?"), c["image"].as_str().unwrap_or("?")))
        .collect()
}

/// The revision to roll back to: `to` when given, else the newest one before the current
/// (the highest number). Errs when there is none.
pub fn undo_target(revs: &[Value], to: Option<i64>) -> Result<&Value, String> {
    let mut numbered: Vec<(i64, &Value)> = revs.iter().map(|r| (number(r), r)).filter(|(n, _)| *n > 0).collect();
    numbered.sort_by_key(|(n, _)| *n);
    let current = numbered.last().map(|(n, _)| *n);
    match to {
        Some(to) if Some(to) == current => Err(format!("revision {to} is the current one")),
        Some(to) => numbered.iter().find(|(n, _)| *n == to).map(|(_, r)| *r).ok_or(format!("no revision {to}")),
        None => numbered.iter().rev().nth(1).map(|(_, r)| *r).ok_or("no earlier revision to roll back to".into()),
    }
}

/// Whether the workload's rollout is done, and how it stands, like `kubectl rollout status`.
pub fn status(kind: &Kind, obj: &Value) -> (bool, String) {
    let int = |p: &str| obj.pointer(p).and_then(Value::as_i64);
    let generation = int("/metadata/generation").unwrap_or(0);
    let observed = int("/status/observedGeneration").unwrap_or(0);
    if observed < generation {
        return (false, "Waiting for the controller to see the latest change".into());
    }
    if kind.is("apps", "DaemonSet") {
        let desired = int("/status/desiredNumberScheduled").unwrap_or(0);
        let updated = int("/status/updatedNumberScheduled").unwrap_or(0);
        let available = int("/status/numberAvailable").unwrap_or(0);
        if updated < desired {
            return (false, format!("{updated} of {desired} pods updated"));
        }
        if available < desired {
            return (false, format!("{available} of {desired} updated pods available"));
        }
        return (true, format!("rolled out: {desired} pods"));
    }
    let desired = int("/spec/replicas").unwrap_or(1);
    let updated = int("/status/updatedReplicas").unwrap_or(0);
    let total = int("/status/replicas").unwrap_or(0);
    let available = int("/status/availableReplicas").or(int("/status/readyReplicas")).unwrap_or(0);
    if kind.is("apps", "Deployment") {
        let stuck = obj
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|c| c["type"] == "Progressing" && c["reason"] == "ProgressDeadlineExceeded");
        if let Some(c) = stuck {
            return (false, format!("stuck: {}", c["message"].as_str().unwrap_or("progress deadline exceeded")));
        }
    }
    if updated < desired {
        return (false, format!("{updated} of {desired} replicas updated"));
    }
    if total > updated {
        return (false, format!("{} old replicas pending termination", total - updated));
    }
    if available < updated {
        return (false, format!("{available} of {updated} updated replicas available"));
    }
    (true, format!("rolled out: {desired} replicas"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{number, status, undo_target};
    use crate::k8s::Kind;

    fn rev(n: i64) -> serde_json::Value {
        json!({"metadata": {"name": format!("web-{n}"), "annotations": {"deployment.kubernetes.io/revision": n.to_string()}}})
    }

    #[test]
    fn undo_goes_to_the_previous_revision_or_the_one_asked() {
        let revs = [rev(3), rev(1), rev(4)];
        assert_eq!(undo_target(&revs, None).map(number), Ok(3));
        assert_eq!(undo_target(&revs, Some(1)).map(number), Ok(1));
        assert!(undo_target(&revs, Some(4)).is_err(), "the current revision");
        assert!(undo_target(&revs, Some(2)).is_err(), "no such revision");
        assert!(undo_target(&[rev(1)], None).is_err(), "nothing before");
    }

    #[test]
    fn status_reads_like_kubectl_rollout_status() {
        let gvk = kube::api::GroupVersionKind::gvk("apps", "v1", "Deployment");
        let kind = Kind {
            ar: kube::discovery::ApiResource::from_gvk_with_plural(&gvk, "deployments"),
            namespaced: true,
            section: "",
            label: "Deployments".into(),
        };
        let dep = |updated: i64, total: i64, available: i64| {
            json!({"metadata": {"generation": 2}, "spec": {"replicas": 3},
                "status": {"observedGeneration": 2, "updatedReplicas": updated, "replicas": total, "availableReplicas": available}})
        };
        assert_eq!(status(&kind, &dep(1, 4, 3)), (false, "1 of 3 replicas updated".into()));
        assert_eq!(status(&kind, &dep(3, 4, 3)), (false, "1 old replicas pending termination".into()));
        assert_eq!(status(&kind, &dep(3, 3, 2)), (false, "2 of 3 updated replicas available".into()));
        assert!(status(&kind, &dep(3, 3, 3)).0);
        let unseen = json!({"metadata": {"generation": 3}, "status": {"observedGeneration": 2}});
        assert!(!status(&kind, &unseen).0);
    }
}
