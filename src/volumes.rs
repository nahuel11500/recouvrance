//! How full volumes are: a PVC's own use from the kubelet of a node that mounts it (the PVC
//! page), and the PVCs filling up from the cluster's Prometheus (the Problems scan).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use kube::Client;
use serde_json::Value;

use crate::problems::{Problem, Severity};

/// A claim is filling up from this share of its bytes or inodes used.
pub const FULL_FROM: f64 = 0.9;

/// Critical from this share: 3% left, where kube-prometheus-stack's own alert turns critical.
pub const CRITICAL_FROM: f64 = 0.97;

/// A volume's use, as its kubelet measures it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Usage {
    pub used: f64,
    pub capacity: f64,
    pub available: f64,
    /// (used, total); None when the filesystem doesn't count them.
    pub inodes: Option<(f64, f64)>,
}

impl Usage {
    pub fn bytes_ratio(&self) -> Option<f64> {
        (self.capacity > 0.0).then(|| self.used / self.capacity)
    }

    pub fn inodes_ratio(&self) -> Option<f64> {
        self.inodes.and_then(|(used, total)| (total > 0.0).then(|| used / total))
    }
}

/// A claim's use in a kubelet's stats summary (`pods[].volume[]` with its `pvcRef`). None when
/// no pod on that node mounts it, or the kubelet doesn't measure it (a raw block volume).
pub fn of_claim(stats: &Value, namespace: &str, claim: &str) -> Option<Usage> {
    let pods = stats.get("pods")?.as_array()?;
    let volume = pods
        .iter()
        .flat_map(|p| p.get("volume").and_then(Value::as_array).map_or_default(Vec::as_slice))
        .find(|v| {
            v.pointer("/pvcRef/name").and_then(Value::as_str) == Some(claim)
                && v.pointer("/pvcRef/namespace").and_then(Value::as_str) == Some(namespace)
        })?;
    let n = |key: &str| volume.get(key).and_then(Value::as_f64);
    let (used, capacity) = (n("usedBytes")?, n("capacityBytes")?);
    let inodes = n("inodesUsed").zip(n("inodes"));
    Some(Usage { used, capacity, available: n("availableBytes").unwrap_or((capacity - used).max(0.0)), inodes })
}

/// Whether a pod runs on a node, whose kubelet then measures the volumes it mounts.
pub fn running(pod: &Value) -> bool {
    pod.pointer("/status/phase").and_then(Value::as_str) == Some("Running")
        && pod.pointer("/spec/nodeName").and_then(Value::as_str).is_some_and(|n| !n.is_empty())
}

/// What the PVC page says of its claim's use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OnPage {
    /// No running pod mounts it: no kubelet measures it.
    NotMounted,
    /// Mounted, but its kubelet's stats could not be read (no `nodes/proxy`, too slow) or
    /// don't have it.
    Unread,
    Read(Usage),
}

/// A claim's use for its page, from its pods (those that mount it) and the stats of the node
/// of one of them.
pub fn on_page(claim: &Value, pods: &[Value], stats: Option<&Value>) -> OnPage {
    if !pods.iter().any(running) {
        return OnPage::NotMounted;
    }
    let name = |p: &str| claim.pointer(p).and_then(Value::as_str).unwrap_or_default();
    stats
        .and_then(|s| of_claim(s, name("/metadata/namespace"), name("/metadata/name")))
        .map_or(OnPage::Unread, OnPage::Read)
}

/// "92%", rounded down: 89.6% doesn't read as 90%.
pub fn percent(ratio: f64) -> String {
    format!("{:.0}%", (ratio * 100.0).floor())
}

/// The PromQL for the claims filling up: one series per claim and measure over `FULL_FROM`,
/// its `full` label saying which (bytes or inodes). `max`: several Prometheus replicas behind
/// one query (Thanos) and a claim mounted on several nodes count once.
pub fn query() -> String {
    let over = |used: &str, total: &str, what: &str| {
        format!(
            r#"label_replace(max by (namespace, persistentvolumeclaim) ({used} / {total}) >= {FULL_FROM}, "full", "{what}", "", "")"#
        )
    };
    format!(
        "{} or {}",
        over("kubelet_volume_stats_used_bytes", "kubelet_volume_stats_capacity_bytes", "bytes"),
        over("kubelet_volume_stats_inodes_used", "kubelet_volume_stats_inodes", "inodes"),
    )
}

/// A claim filling up: the share of its bytes and of its inodes used, each when over `FULL_FROM`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Filling {
    pub namespace: String,
    pub claim: String,
    pub bytes: Option<f64>,
    pub inodes: Option<f64>,
}

impl Filling {
    fn worst(&self) -> f64 {
        self.bytes.unwrap_or(0.0).max(self.inodes.unwrap_or(0.0))
    }
}

/// The claims of `query`'s answer, one each, by namespace and name. A series without a claim
/// or a measure, or under `FULL_FROM`, is left out.
pub fn filling(samples: &[(BTreeMap<String, String>, f64)]) -> Vec<Filling> {
    let mut by_claim: BTreeMap<(&str, &str), Filling> = BTreeMap::new();
    for (labels, value) in samples {
        let label = |k: &str| labels.get(k).map(String::as_str).filter(|v| !v.is_empty());
        let (Some(ns), Some(claim)) = (label("namespace"), label("persistentvolumeclaim")) else { continue };
        if !value.is_finite() || *value < FULL_FROM {
            continue;
        }
        let f = by_claim.entry((ns, claim)).or_insert_with(|| Filling {
            namespace: ns.to_string(),
            claim: claim.to_string(),
            ..Filling::default()
        });
        let slot = match label("full") {
            Some("bytes") => &mut f.bytes,
            Some("inodes") => &mut f.inodes,
            _ => continue,
        };
        *slot = Some(slot.map_or(*value, |v| v.max(*value)));
    }
    by_claim.into_values().filter(|f| f.bytes.is_some() || f.inodes.is_some()).collect()
}

/// The claims filling up in `namespaces` (every one when empty), as problems. Those
/// `alerted` already names (a firing `KubePersistentVolumeFillingUp`) are left to their alert,
/// so a claim isn't listed twice.
pub fn problems(filling: &[Filling], namespaces: &[String], alerted: impl Fn(&str, &str) -> bool) -> Vec<Problem> {
    filling
        .iter()
        .filter(|f| namespaces.is_empty() || namespaces.contains(&f.namespace))
        .filter(|f| !alerted(&f.namespace, &f.claim))
        .map(|f| {
            let reason = match (f.bytes, f.inodes) {
                (Some(b), Some(i)) => format!("{} of its space and {} of its inodes used", percent(b), percent(i)),
                (Some(b), None) => format!("{} of its space used", percent(b)),
                (None, Some(i)) => format!("{} of its inodes used", percent(i)),
                (None, None) => "filling up".into(),
            };
            Problem {
                severity: if f.worst() >= CRITICAL_FROM { Severity::Critical } else { Severity::Warning },
                kind: "persistentvolumeclaims".into(),
                label: "PersistentVolumeClaim".into(),
                namespace: Some(f.namespace.clone()),
                name: f.claim.clone(),
                reason,
                age: None,
                on_argo: None,
            }
        })
        .collect()
}

/// The claims filling up in a cluster, as one scan read them, and why they could not be
/// read when its Prometheus was found but did not answer ("failed: …").
#[derive(Debug, Default)]
pub struct Volumes {
    pub filling: Vec<Filling>,
    pub error: Option<String>,
}

/// An answer is asked again after this long, like the alerts: the scans of several scopes
/// of one cluster (two windows, a namespace and the whole cluster) share it meanwhile.
const FRESH: Duration = Duration::from_secs(30);

/// An answer, and when it came.
type Kept = (Instant, Arc<Volumes>);

/// The last answer of each cluster, by context.
static LAST: LazyLock<Mutex<HashMap<String, Kept>>> = LazyLock::new(Default::default);

/// The claims filling up in the cluster of `context`: one instant query to its Prometheus,
/// nothing to the API server's lists. Nothing to say without Prometheus (or with metrics off)
/// or without the kubelet's volume metrics.
pub async fn read(context: &str, client: Client) -> Arc<Volumes> {
    let kept = LAST.lock().unwrap().get(context).filter(|(at, _)| at.elapsed() < FRESH).map(|(_, v)| v.clone());
    if let Some(volumes) = kept {
        return volumes;
    }
    let volumes = Arc::new(ask(context, client).await);
    LAST.lock().unwrap().insert(context.to_string(), (Instant::now(), volumes.clone()));
    volumes
}

/// The query itself, answered now.
async fn ask(context: &str, client: Client) -> Volumes {
    let Ok(Some(target)) = crate::prom::target(context, client.clone()).await else { return Volumes::default() };
    match crate::prom::instant(client, target, query()).await {
        Ok(samples) => Volumes { filling: filling(&samples), error: None },
        // Something else on that service: no Prometheus to ask.
        Err(e) if crate::alerts::unsupported(&e) => Volumes::default(),
        Err(e) => Volumes { filling: Vec::new(), error: Some(crate::k8s::lists::load_error(&e)) },
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;

    fn sample(ns: &str, claim: &str, full: &str, value: f64) -> (BTreeMap<String, String>, f64) {
        let labels = [("namespace", ns), ("persistentvolumeclaim", claim), ("full", full)];
        (labels.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| ((*k).into(), (*v).into())).collect(), value)
    }

    #[test]
    fn reads_a_claims_use_from_the_kubelet() {
        let stats = json!({"pods": [
            {"podRef": {"name": "other", "namespace": "b"}, "volume": [
                // The same claim name in another namespace is another claim.
                {"name": "data", "pvcRef": {"name": "data", "namespace": "b"},
                 "usedBytes": 1.0, "capacityBytes": 2.0, "availableBytes": 1.0},
            ]},
            {"podRef": {"name": "db-0", "namespace": "a"}, "volume": [
                {"name": "tmp", "usedBytes": 5.0, "capacityBytes": 10.0},
                {"name": "data", "pvcRef": {"name": "data", "namespace": "a"}, "usedBytes": 90.0,
                 "capacityBytes": 100.0, "availableBytes": 8.0, "inodes": 1000.0, "inodesUsed": 50.0},
            ]},
        ]});
        let u = of_claim(&stats, "a", "data").unwrap();
        assert_eq!(u, Usage { used: 90.0, capacity: 100.0, available: 8.0, inodes: Some((50.0, 1000.0)) });
        assert_eq!(u.bytes_ratio(), Some(0.9));
        assert_eq!(u.inodes_ratio(), Some(0.05));
        assert_eq!(of_claim(&stats, "b", "data").unwrap().inodes, None);
        assert_eq!(of_claim(&stats, "a", "missing"), None);
        assert_eq!(of_claim(&json!({}), "a", "data"), None);
    }

    #[test]
    fn the_page_reads_a_claim_only_while_a_running_pod_mounts_it() {
        let claim = json!({"metadata": {"name": "data", "namespace": "a"}});
        let pod = |phase: &str, node: &str| json!({"spec": {"nodeName": node}, "status": {"phase": phase}});
        let stats = json!({"pods": [{"volume": [
            {"pvcRef": {"name": "data", "namespace": "a"}, "usedBytes": 1.0, "capacityBytes": 4.0},
        ]}]});
        // Stats kept from an earlier read say nothing once no pod runs with it.
        assert_eq!(on_page(&claim, &[], Some(&stats)), OnPage::NotMounted);
        assert_eq!(on_page(&claim, &[pod("Pending", ""), pod("Succeeded", "n1")], Some(&stats)), OnPage::NotMounted);
        let pods = [pod("Pending", ""), pod("Running", "n1")];
        assert_eq!(on_page(&claim, &pods, None), OnPage::Unread);
        assert_eq!(on_page(&claim, &pods, Some(&json!({"pods": []}))), OnPage::Unread);
        let OnPage::Read(u) = on_page(&claim, &pods, Some(&stats)) else { panic!("not read") };
        assert_eq!((u.used, u.available, u.bytes_ratio()), (1.0, 3.0, Some(0.25)));
    }

    #[test]
    fn a_block_volume_without_its_figures_has_no_use() {
        let stats = json!({"pods": [{"volume": [{"pvcRef": {"name": "raw", "namespace": "a"}}]}]});
        assert_eq!(of_claim(&stats, "a", "raw"), None);
        let empty = Usage { used: 0.0, capacity: 0.0, available: 0.0, inodes: Some((0.0, 0.0)) };
        assert_eq!((empty.bytes_ratio(), empty.inodes_ratio()), (None, None));
    }

    #[test]
    fn percents_round_down() {
        assert_eq!(percent(0.899), "89%");
        assert_eq!(percent(0.9), "90%");
        assert_eq!(percent(1.0), "100%");
    }

    #[test]
    fn the_query_asks_for_bytes_and_inodes_over_the_threshold() {
        let q = query();
        assert!(q.contains("kubelet_volume_stats_used_bytes / kubelet_volume_stats_capacity_bytes) >= 0.9"));
        assert!(q.contains("kubelet_volume_stats_inodes_used / kubelet_volume_stats_inodes) >= 0.9"));
        assert!(q.contains(r#""full", "bytes""#) && q.contains(r#""full", "inodes""#));
    }

    #[test]
    fn one_finding_per_claim_with_both_measures() {
        let found = filling(&[
            sample("a", "data", "bytes", 0.95),
            sample("a", "data", "inodes", 0.91),
            sample("b", "data", "inodes", 0.99),
            // Under the threshold, without a claim, or not a number: left out.
            sample("a", "logs", "bytes", 0.5),
            sample("a", "", "bytes", 0.95),
            sample("a", "nan", "bytes", f64::NAN),
            sample("a", "what", "", 0.95),
        ]);
        assert_eq!(
            found,
            vec![
                Filling { namespace: "a".into(), claim: "data".into(), bytes: Some(0.95), inodes: Some(0.91) },
                Filling { namespace: "b".into(), claim: "data".into(), bytes: None, inodes: Some(0.99) },
            ]
        );
        let p = problems(&found, &[], |_, _| false);
        assert_eq!(p[0].reason, "95% of its space and 91% of its inodes used");
        assert_eq!((p[0].severity, p[1].severity), (Severity::Warning, Severity::Critical));
        assert_eq!(p[1].reason, "99% of its inodes used");
        assert_eq!((p[0].kind.as_str(), p[0].namespace.as_deref()), ("persistentvolumeclaims", Some("a")));
    }

    #[test]
    fn a_claim_its_alert_names_and_other_namespaces_are_left_out() {
        let found = filling(&[sample("a", "data", "bytes", 0.95), sample("b", "data", "bytes", 0.95)]);
        let names = |p: Vec<Problem>| p.into_iter().map(|p| format!("{}/{}", p.namespace.unwrap(), p.name)).collect();
        let shown: Vec<String> = names(problems(&found, &[], |ns, claim| (ns, claim) == ("a", "data")));
        assert_eq!(shown, ["b/data"]);
        let shown: Vec<String> = names(problems(&found, &["a".into()], |_, _| false));
        assert_eq!(shown, ["a/data"]);
    }

    proptest! {
        /// Every claim over the threshold in bytes or inodes is listed once, with its worst
        /// measure deciding how bad it is; none under it is.
        #[test]
        fn every_claim_over_the_threshold_is_listed_once(
            series in proptest::collection::vec((0..3usize, 0..3usize, any::<bool>(), 0.0..1.2f64), 0..30),
        ) {
            let samples: Vec<_> = series
                .iter()
                .map(|(ns, claim, bytes, v)| {
                    sample(&format!("ns{ns}"), &format!("c{claim}"), if *bytes { "bytes" } else { "inodes" }, *v)
                })
                .collect();
            let found = problems(&filling(&samples), &[], |_, _| false);
            let mut worst: BTreeMap<(String, String), f64> = BTreeMap::new();
            for (labels, v) in &samples {
                let key = (labels["namespace"].clone(), labels["persistentvolumeclaim"].clone());
                let w = worst.entry(key).or_insert(0.0);
                *w = w.max(*v);
            }
            let over: Vec<_> = worst.iter().filter(|(_, v)| **v >= FULL_FROM).collect();
            prop_assert_eq!(found.len(), over.len());
            for ((ns, claim), v) in over {
                let p = found.iter().find(|p| p.namespace.as_deref() == Some(ns) && &p.name == claim).unwrap();
                let critical = *v >= CRITICAL_FROM;
                prop_assert_eq!(p.severity, if critical { Severity::Critical } else { Severity::Warning });
            }
        }
    }
}
