//! Local storage managed by TopoLVM: what each volume group (device class) of a node has
//! left, and what its volumes take.
//!
//! TopoLVM writes the free bytes of each device class on the node, in the annotation
//! `capacity.topolvm.io/<device class>`, and makes one cluster-scoped `LogicalVolume` per
//! volume, with the node, the device class and the size. Used is the sum of the node's
//! volumes, and the total is free plus used.

use std::collections::BTreeMap;

use anyhow::Result;
use kube::Client;
use kube::discovery::ApiResource;
use serde::Deserialize;
use serde_json::Value;

use super::{api_status, lists};

/// The annotation prefixes of the free bytes, the current one first (older installs used
/// the `topolvm.cybozu.com` group).
const CAPACITY_PREFIXES: [&str; 2] = ["capacity.topolvm.io/", "capacity.topolvm.cybozu.com/"];
/// The API groups of `LogicalVolume`, in the same order.
const GROUPS: [&str; 2] = ["topolvm.io", "topolvm.cybozu.com"];
/// The annotation TopoLVM adds for the default device class, next to the class's own.
const DEFAULT_ALIAS: &str = "00default";

/// One volume on a node: its device class ("" for the default one) and size in bytes.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalVolume {
    pub device_class: String,
    pub bytes: f64,
}

/// What one device class of a node holds; its total is free plus used. `free` is unknown
/// for a class that has volumes but no annotation; `used` is unknown when the volumes
/// could not be read.
#[derive(Clone, Debug, PartialEq)]
pub struct LocalClass {
    pub name: String,
    pub free: Option<f64>,
    pub used: Option<f64>,
}

/// The free bytes by device class from a node's annotations, under the first prefix that
/// has any. A value that does not read as a byte count is unknown (None), not 0.
fn free_by_class(node: &Value) -> BTreeMap<String, Option<f64>> {
    let Some(annotations) = node.pointer("/metadata/annotations").and_then(Value::as_object) else {
        return BTreeMap::new();
    };
    CAPACITY_PREFIXES
        .iter()
        .map(|prefix| {
            annotations
                .iter()
                .filter_map(|(k, v)| {
                    let class = k.strip_prefix(prefix).filter(|c| !c.is_empty())?;
                    let free = v.as_str().and_then(|s| s.trim().parse::<u64>().ok()).map(|b| b as f64);
                    Some((class.to_string(), free))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .find(|m| !m.is_empty())
        .unwrap_or_default()
}

/// Whether TopoLVM manages local storage on this node (it has a capacity annotation).
pub fn has_local_storage(node: &Value) -> bool {
    !free_by_class(node).is_empty()
}

/// Free and used bytes of each device class of a node, by name; empty when the
/// node has no capacity annotation. `volumes` are the node's LogicalVolumes, None when
/// they could not be read (then only the free space is known).
///
/// `00default` is not a class of its own: TopoLVM writes it next to the default class's
/// annotation, with the same value. So it is shown only when it is the only annotation,
/// and otherwise only tells which class is the default one, to count volumes that name
/// no class: the only class when there is one, else the one class with the same free
/// bytes. When that is not one class (two classes with the same free bytes), those
/// volumes are counted under "default" with free unknown, rather than in a class picked
/// at random or with the default's free space a second time.
pub fn local_storage(node: &Value, volumes: Option<&[LocalVolume]>) -> Vec<LocalClass> {
    let mut free = free_by_class(node);
    if free.is_empty() {
        return Vec::new();
    }
    let alias = if free.len() > 1 { free.remove(DEFAULT_ALIAS).flatten() } else { None };
    let default = if free.len() == 1 {
        free.keys().next().cloned()
    } else {
        let mut same = free.iter().filter(|(_, f)| alias.is_some() && **f == alias);
        match (same.next(), same.next()) {
            (Some((name, _)), None) => Some(name.clone()),
            _ => None,
        }
    };
    let mut used: BTreeMap<String, f64> = BTreeMap::new();
    for v in volumes.into_iter().flatten() {
        // Unresolved, they go under "default": a class really named so is most likely it.
        let class = match v.device_class.as_str() {
            "" => default.clone().unwrap_or_else(|| "default".to_string()),
            c => c.to_string(),
        };
        *used.entry(class).or_default() += v.bytes;
    }
    let mut names: Vec<String> = free.keys().chain(used.keys()).cloned().collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| LocalClass {
            free: free.get(&name).copied().flatten(),
            used: volumes.map(|_| used.get(&name).copied().unwrap_or(0.0)),
            name,
        })
        .collect()
}

#[derive(Deserialize)]
struct LeanVolume {
    #[serde(default)]
    spec: Option<LeanSpec>,
    #[serde(default)]
    status: Option<LeanStatus>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeanSpec {
    #[serde(default)]
    node_name: Option<String>,
    #[serde(default)]
    device_class: Option<String>,
    #[serde(default)]
    size: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeanStatus {
    #[serde(default)]
    current_size: Option<String>,
}

/// The volumes of `node` among all LogicalVolumes, sized by their current size (what the
/// volume group holds), else the size asked for.
fn volumes_of(all: Vec<LeanVolume>, node: &str) -> Vec<LocalVolume> {
    all.into_iter()
        .filter_map(|v| {
            let spec = v.spec?;
            if spec.node_name.as_deref() != Some(node) {
                return None;
            }
            let size = v.status.and_then(|s| s.current_size).filter(|s| !s.is_empty()).or(spec.size)?;
            Some(LocalVolume { device_class: spec.device_class.unwrap_or_default(), bytes: super::parse_bytes(&size) })
        })
        .collect()
}

/// The LogicalVolumes on `node`, from `topolvm.io/v1`, else `topolvm.cybozu.com/v1` where
/// the first is not served. The kind is cluster-scoped and has no field to select a node
/// by, so it lists them all, keeping only the fields read.
pub async fn logical_volumes(client: Client, node: String) -> Result<Vec<LocalVolume>> {
    let mut last = None;
    for group in GROUPS {
        let ar = ApiResource {
            group: group.into(),
            version: "v1".into(),
            api_version: format!("{group}/v1"),
            kind: "LogicalVolume".into(),
            plural: "logicalvolumes".into(),
        };
        match lists::list_lean_cached::<LeanVolume>(client.clone(), ar, None).await {
            Ok(all) => return Ok(volumes_of(all, &node)),
            Err(e) if api_status(&e) == Some(404) => last = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("no LogicalVolume API")))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::{Value, json};

    use super::{LocalClass, LocalVolume, local_storage, volumes_of};

    fn node(annotations: &[(&str, &str)]) -> Value {
        let map: serde_json::Map<String, Value> =
            annotations.iter().map(|(k, v)| (k.to_string(), Value::String(v.to_string()))).collect();
        json!({"metadata": {"name": "n1", "annotations": map}})
    }

    fn lv(class: &str, bytes: f64) -> LocalVolume {
        LocalVolume { device_class: class.into(), bytes }
    }

    const TI: f64 = 1024.0 * 1024.0 * 1024.0 * 1024.0;

    #[test]
    fn the_default_alias_is_not_counted_twice() {
        let n = node(&[
            ("capacity.topolvm.io/00default", "1099511627776"),
            ("capacity.topolvm.io/md0cryptlvm", "1099511627776"),
            ("other/annotation", "x"),
        ]);
        // Volumes that name no class are in the default class.
        let vols = [lv("", 2.0 * TI), lv("md0cryptlvm", TI)];
        let rows = local_storage(&n, Some(&vols));
        assert_eq!(rows, vec![LocalClass { name: "md0cryptlvm".into(), free: Some(TI), used: Some(3.0 * TI) }]);
    }

    #[test]
    fn free_only_when_the_volumes_are_unknown() {
        let n = node(&[("capacity.topolvm.io/00default", "100"), ("capacity.topolvm.io/ssd", "100")]);
        let rows = local_storage(&n, None);
        assert_eq!(rows, vec![LocalClass { name: "ssd".into(), free: Some(100.0), used: None }]);
    }

    #[test]
    fn nothing_without_the_annotation() {
        assert!(local_storage(&node(&[("foo", "1")]), Some(&[lv("", 5.0)])).is_empty());
        assert!(local_storage(&json!({"metadata": {}}), None).is_empty());
    }

    #[test]
    fn several_classes_find_the_default_by_its_free_bytes() {
        let n = node(&[
            ("capacity.topolvm.io/00default", "30"),
            ("capacity.topolvm.io/hdd", "30"),
            ("capacity.topolvm.io/ssd", "10"),
        ]);
        let rows = local_storage(&n, Some(&[lv("", 5.0), lv("ssd", 1.0)]));
        assert_eq!(
            rows,
            vec![
                LocalClass { name: "hdd".into(), free: Some(30.0), used: Some(5.0) },
                LocalClass { name: "ssd".into(), free: Some(10.0), used: Some(1.0) },
            ]
        );
        // Two classes with the default's free bytes: its volumes are not put in either.
        let tie = node(&[
            ("capacity.topolvm.io/00default", "30"),
            ("capacity.topolvm.io/hdd", "30"),
            ("capacity.topolvm.io/ssd", "30"),
        ]);
        let rows = local_storage(&tie, Some(&[lv("", 5.0)]));
        assert_eq!(rows[0], LocalClass { name: "default".into(), free: None, used: Some(5.0) });
        assert_eq!(rows[1].used, Some(0.0));
    }

    #[test]
    fn the_alias_alone_and_older_installs() {
        let rows = local_storage(&node(&[("capacity.topolvm.io/00default", "7")]), Some(&[lv("", 3.0)]));
        assert_eq!(rows, vec![LocalClass { name: "00default".into(), free: Some(7.0), used: Some(3.0) }]);
        let old = node(&[("capacity.topolvm.cybozu.com/00default", "7"), ("capacity.topolvm.cybozu.com/vg", "7")]);
        assert_eq!(local_storage(&old, None)[0].name, "vg");
        // A class with volumes but no annotation (removed from lvmd's config) keeps its used.
        let gone = local_storage(&node(&[("capacity.topolvm.io/vg", "7")]), Some(&[lv("old", 2.0)]));
        assert_eq!(gone[0], LocalClass { name: "old".into(), free: None, used: Some(2.0) });
        // A value that is not a byte count is unknown, not 0.
        assert_eq!(local_storage(&node(&[("capacity.topolvm.io/vg", "lots")]), None)[0].free, None);
    }

    #[test]
    fn volumes_of_the_node_by_current_size() {
        let list = json!([
            {"spec": {"nodeName": "n1", "size": "50Gi"}, "status": {"currentSize": "60Gi"}},
            {"spec": {"nodeName": "n1", "size": "1536Mi", "deviceClass": "ssd"}},
            {"spec": {"nodeName": "n1", "size": "1Gi", "deviceClass": null}, "status": {"currentSize": null}},
            {"spec": {"nodeName": "n2", "size": "1Ti"}},
            {"metadata": {"name": "no-spec"}},
        ]);
        let vols = volumes_of(serde_json::from_value(list).unwrap(), "n1");
        let gi = 1024.0 * 1024.0 * 1024.0;
        assert_eq!(vols, vec![lv("", 60.0 * gi), lv("ssd", 1.5 * gi), lv("", gi)]);
    }

    fn class() -> impl Strategy<Value = String> {
        prop_oneof![Just("00default".to_string()), Just("".to_string()), "[a-c]{1,2}"]
    }

    proptest! {
        /// For any annotations and volumes: `00default` never shows next to another class,
        /// no class shows twice, the free bytes shown are the real classes' (the alias is
        /// never added on top), and every volume is counted once, so that free + used is
        /// the whole volume group.
        #[test]
        fn rows_add_up(
            free in proptest::collection::btree_map(class(), 0u64..100, 0..5),
            vols in proptest::collection::vec((class(), 0u32..100), 0..8),
            known in any::<bool>(),
        ) {
            let annotations: Vec<(String, String)> = free
                .iter()
                .map(|(c, f)| (format!("capacity.topolvm.io/{c}"), f.to_string()))
                .collect();
            let refs: Vec<(&str, &str)> = annotations.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let n = node(&refs);
            let vols: Vec<LocalVolume> = vols.into_iter().map(|(c, b)| lv(&c, b as f64)).collect();
            let rows = local_storage(&n, known.then_some(vols.as_slice()));
            let classes: Vec<&String> = free.keys().filter(|c| !c.is_empty()).collect();
            if classes.is_empty() {
                prop_assert!(rows.is_empty());
                return Ok(());
            }
            let mut names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
            names.dedup();
            prop_assert_eq!(names.len(), rows.len());
            if classes.len() > 1 {
                prop_assert!(rows.iter().all(|r| r.name != "00default" || r.free.is_none()));
            }
            let real: f64 = if classes.len() == 1 {
                free[classes[0]] as f64
            } else {
                classes.iter().filter(|c| c.as_str() != "00default").map(|c| free[*c] as f64).sum()
            };
            let shown: f64 = rows.iter().filter_map(|r| r.free).sum();
            prop_assert_eq!(shown, real);
            let used: f64 = rows.iter().filter_map(|r| r.used).sum();
            prop_assert_eq!(used, if known { vols.iter().map(|v| v.bytes).sum() } else { 0.0 });
            for r in &rows {
                prop_assert_eq!(r.used.is_some(), known);
            }
        }
    }
}
