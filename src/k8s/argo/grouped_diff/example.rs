//! Made-up production environments of a made-up service, `shop`, and what a sync of each would
//! change: no real data. The developer tools' snapshots show their grouped diff
//! (`RECOUVRANCE_ACT=page:argo,argo-example-diff`), and the tests read it.
//!
//! In each: an image bumped on the `web` Deployment; an annotation `example.io/role` added on
//! 36 Deployments, each with its own value; a PrometheusRule's alert added, naming the
//! environment and its client (its threshold other in `shop-prod-initech`).

use serde_json::{Value, json};

use super::{AppDiff, Names};

/// The environments: their name, client and the threshold of their rule.
pub const ENVIRONMENTS: [(&str, &str, u32); 3] =
    [("shop-prod-acme", "acme", 5), ("shop-prod-globex", "globex", 5), ("shop-prod-initech", "initech", 9)];
/// Deployments annotated in each.
pub const ANNOTATED: usize = 36;

/// One of Argo CD's managed resources, its states as the JSON text it sends.
pub fn item(group: &str, kind: &str, ns: &str, name: &str, live: Option<Value>, desired: Option<Value>) -> Value {
    let text = |v: &Option<Value>| v.as_ref().map(Value::to_string);
    json!({"group": group, "kind": kind, "namespace": ns, "name": name,
        "normalizedLiveState": text(&live), "predictedLiveState": text(&desired), "modified": true})
}

fn deployment(ns: &str, name: &str, image: &str, role: Option<&str>) -> Value {
    let mut d = json!({"apiVersion": "apps/v1", "kind": "Deployment",
        "metadata": {"name": name, "namespace": ns, "annotations": {"owner": "shop"}},
        "spec": {"replicas": 2, "template": {"spec": {"containers": [{"name": name, "image": image}]}}}});
    if let Some(r) = role {
        d["metadata"]["annotations"]["example.io/role"] = json!(r);
    }
    d
}

fn rule(ns: &str, client: &str, threshold: Option<u32>) -> Value {
    let rules: Vec<Value> = threshold
        .map(|t| {
            json!({"alert": "QueueStuck",
                "expr": format!("queue_depth{{namespace=\"{ns}\", client=\"{client}\"}} > {t}"),
                "labels": {"severity": "page", "client": client}})
        })
        .into_iter()
        .collect();
    json!({"apiVersion": "monitoring.coreos.com/v1", "kind": "PrometheusRule",
        "metadata": {"name": "shop-rules", "namespace": ns},
        "spec": {"groups": [{"name": "shop", "rules": rules}]}})
}

/// Argo CD's managed resources of the environment `ns` (of `client`, its rule's `threshold`).
pub fn managed(ns: &str, client: &str, threshold: u32) -> Value {
    let image = |tag: &str| format!("registry.example.com/shop/web:{tag}");
    let mut items = vec![item(
        "apps",
        "Deployment",
        ns,
        "web",
        Some(deployment(ns, "web", &image("1.15.0"), None)),
        Some(deployment(ns, "web", &image("1.16.0"), None)),
    )];
    for i in 0..ANNOTATED {
        let name = format!("worker-{i:02}");
        let role = format!("Queue worker {i:02}: takes the jobs of its queue");
        let img = image("1.16.0");
        let (live, desired) = (deployment(ns, &name, &img, None), deployment(ns, &name, &img, Some(&role)));
        items.push(item("apps", "Deployment", ns, &name, Some(live), Some(desired)));
    }
    let (live, desired) = (rule(ns, client, None), rule(ns, client, Some(threshold)));
    items.push(item("monitoring.coreos.com", "PrometheusRule", ns, "shop-rules", Some(live), Some(desired)));
    json!({"items": items})
}

/// Each environment's name, its own names and what a sync of it would change.
pub fn diffs() -> Vec<(String, Names, AppDiff)> {
    ENVIRONMENTS
        .iter()
        .map(|(ns, client, threshold)| {
            let names = Names::new([ns.to_string(), format!("{ns}-workload")], Some(client.to_string()));
            (format!("{ns}-workload"), names, AppDiff::of(&managed(ns, client, *threshold)))
        })
        .collect()
}
