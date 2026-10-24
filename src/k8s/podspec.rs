//! A pod's requests and limits as the scheduler counts them, in one place.
//!
//! Pages, tables, the cluster overview and the agents' tools each used to add up
//! `spec.containers` their own way, and each missed something different: sidecars (init
//! containers that keep running), regular init containers, overhead, pod-level resources.
//! They all go through [`effective`] now, from a pod's JSON or from a lean parsed spec.

use std::collections::HashMap;

use serde::Deserialize;

/// One resource (`cpu`, `memory`, …) of one pod.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Effective {
    /// What the scheduler reserves; 0 when nothing requests it.
    pub request: f64,
    /// Some container or the pod requests it.
    pub requested: bool,
    /// The limits added up (a container without one counts 0).
    pub limit: f64,
    /// Some container has no limit (and the pod has none): the pod may use what the node has.
    pub unlimited: bool,
}

impl Effective {
    /// The request, when there is one.
    pub fn request(&self) -> Option<f64> {
        self.requested.then_some(self.request)
    }

    /// The limit, unless some container is unlimited.
    pub fn limit(&self) -> Option<f64> {
        (!self.unlimited).then_some(self.limit)
    }
}

/// A container's request and limit of one resource, and what part it plays.
#[derive(Clone, Copy, Debug)]
pub struct Part {
    pub role: Role,
    pub request: Option<f64>,
    pub limit: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Container,
    /// An init container with `restartPolicy: Always`: runs alongside the containers.
    Sidecar,
    /// A regular init container: runs alone (with the sidecars started before it), first.
    Init,
}

/// What a sum is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Count {
    /// What the scheduler reserves on a node: init containers count, as they run first.
    Scheduled,
    /// What the running pod is held to, to set its usage against: regular init containers
    /// have finished by then.
    Running,
}

/// `parts` in spec order (init containers first, then containers); `pod` the pod-level
/// resources (they win when set), `overhead` the RuntimeClass overhead. Per pod, the larger
/// of the containers plus sidecars, and of each init container plus the sidecars started
/// before it; plus overhead.
pub fn effective(parts: &[Part], pod: (Option<f64>, Option<f64>), overhead: f64, count: Count) -> Effective {
    let kept: Vec<Part> = parts.iter().filter(|p| count == Count::Scheduled || p.role != Role::Init).copied().collect();
    let parts = kept.as_slice();
    let amount = |of: fn(&Part) -> Option<f64>| {
        let (mut sidecars, mut running, mut peak) = (0.0, 0.0, 0.0f64);
        for p in parts {
            let v = of(p).unwrap_or(0.0);
            match p.role {
                Role::Sidecar => sidecars += v,
                Role::Init => peak = peak.max(sidecars + v),
                Role::Container => running += v,
            }
        }
        (running + sidecars).max(peak)
    };
    let requested = pod.0.is_some() || parts.iter().any(|p| p.request.is_some());
    let request = pod.0.unwrap_or_else(|| amount(|p| p.request)) + overhead;
    let unlimited = pod.1.is_none() && (parts.is_empty() || parts.iter().any(|p| p.limit.is_none()));
    let limit = pod.1.unwrap_or_else(|| amount(|p| p.limit)) + overhead;
    Effective { request: if requested { request } else { 0.0 }, requested, limit, unlimited }
}

/// How to read a quantity of `resource`.
pub fn parser(resource: &str) -> fn(&str) -> f64 {
    match resource {
        "cpu" => super::parse_cpu,
        "memory" | "ephemeral-storage" | "storage" => super::parse_bytes,
        // GPUs and other extended resources are counts.
        _ => |q| q.parse::<f64>().unwrap_or_else(|_| super::parse_bytes(q)),
    }
}

/// One resource of a pod given as JSON (a `Pod` object).
pub fn of_value(pod: &serde_json::Value, resource: &str, count: Count) -> Effective {
    let parse = parser(resource);
    let key = resource.replace('/', "~1");
    let get = |v: &serde_json::Value, which: &str| {
        v.pointer(&format!("/resources/{which}/{key}")).and_then(|q| q.as_str()).map(parse)
    };
    let list = |path: &str| pod.pointer(path).and_then(|c| c.as_array()).map_or(&[][..], Vec::as_slice);
    let inits = list("/spec/initContainers").iter().map(|c| {
        let role = if c["restartPolicy"].as_str() == Some("Always") { Role::Sidecar } else { Role::Init };
        Part { role, request: get(c, "requests"), limit: get(c, "limits") }
    });
    let containers = list("/spec/containers").iter().map(|c| Part {
        role: Role::Container,
        request: get(c, "requests"),
        limit: get(c, "limits"),
    });
    let parts: Vec<Part> = inits.chain(containers).collect();
    let spec = pod.get("spec").unwrap_or(&serde_json::Value::Null);
    let overhead = spec.pointer(&format!("/overhead/{key}")).and_then(|q| q.as_str()).map_or(0.0, parse);
    effective(&parts, (get(spec, "requests"), get(spec, "limits")), overhead, count)
}

/// Requests and limits of `resource` added up over pods, as charts draw them against their
/// usage: no request when none of them requests it, no limit when one of them is unlimited.
pub fn total<'a>(pods: impl IntoIterator<Item = &'a serde_json::Value>, resource: &str) -> (Option<f64>, Option<f64>) {
    let (mut request, mut limit) = (None::<f64>, Some(0.0));
    for p in pods {
        let e = of_value(p, resource, Count::Running);
        if let Some(r) = e.request() {
            *request.get_or_insert(0.0) += r;
        }
        limit = limit.zip(e.limit()).map(|(a, b)| a + b);
    }
    (request, limit.filter(|_| request.is_some() || limit.is_some_and(|l| l > 0.0)))
}

/// The part of a pod spec these sums read, for lean lists (nothing else is parsed).
#[derive(Deserialize, Default, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct LeanSpec {
    pub containers: Vec<LeanContainer>,
    pub init_containers: Vec<LeanContainer>,
    pub overhead: HashMap<String, String>,
    pub resources: LeanResources,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default, rename_all = "camelCase")]
pub struct LeanContainer {
    pub resources: LeanResources,
    pub restart_policy: Option<String>,
}

#[derive(Deserialize, Default, Clone)]
#[serde(default)]
pub struct LeanResources {
    pub requests: HashMap<String, String>,
    pub limits: HashMap<String, String>,
}

impl LeanSpec {
    /// One resource of this pod.
    pub fn effective(&self, resource: &str, count: Count) -> Effective {
        let parse = parser(resource);
        let get = |m: &HashMap<String, String>| m.get(resource).map(|q| parse(q));
        let part = |c: &LeanContainer, role| Part {
            role,
            request: get(&c.resources.requests),
            limit: get(&c.resources.limits),
        };
        let inits = self
            .init_containers
            .iter()
            .map(|c| part(c, if c.restart_policy.as_deref() == Some("Always") { Role::Sidecar } else { Role::Init }));
        let parts: Vec<Part> = inits.chain(self.containers.iter().map(|c| part(c, Role::Container))).collect();
        let overhead = self.overhead.get(resource).map_or(0.0, |q| parse(q));
        effective(&parts, (get(&self.resources.requests), get(&self.resources.limits)), overhead, count)
    }

    /// Every resource a container of this pod requests or limits (cpu, memory, GPUs…).
    pub fn resource_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .containers
            .iter()
            .chain(&self.init_containers)
            .flat_map(|c| c.resources.requests.keys().chain(c.resources.limits.keys()))
            .cloned()
            .collect();
        names.sort();
        names.dedup();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::{Count, LeanSpec, effective, of_value, total};
    use serde_json::json;

    fn pod(spec: serde_json::Value) -> serde_json::Value {
        json!({ "spec": spec })
    }

    #[test]
    fn containers_add_up() {
        let p = pod(json!({"containers": [
            {"resources": {"requests": {"cpu": "100m"}, "limits": {"cpu": "200m"}}},
            {"resources": {"requests": {"cpu": "50m"}, "limits": {"cpu": "100m"}}},
        ]}));
        let e = of_value(&p, "cpu", Count::Scheduled);
        assert_eq!((e.request(), e.limit()), (Some(150.0), Some(300.0)));
    }

    #[test]
    fn sidecars_run_alongside_and_init_containers_run_alone() {
        let p = pod(json!({
            "initContainers": [
                {"restartPolicy": "Always", "resources": {"requests": {"cpu": "100m"}, "limits": {"cpu": "100m"}}},
                {"resources": {"requests": {"cpu": "1"}, "limits": {"cpu": "1"}}},
            ],
            "containers": [{"resources": {"requests": {"cpu": "200m"}, "limits": {"cpu": "200m"}}}],
        }));
        // Containers + sidecar = 300m; the init container + the sidecar before it = 1100m.
        let e = of_value(&p, "cpu", Count::Scheduled);
        assert_eq!((e.request(), e.limit()), (Some(1100.0), Some(1100.0)));
        // Once it runs, the init container has finished.
        let e = of_value(&p, "cpu", Count::Running);
        assert_eq!((e.request(), e.limit()), (Some(300.0), Some(300.0)));
    }

    #[test]
    fn overhead_and_pod_level_resources() {
        let p = pod(json!({
            "overhead": {"memory": "10Mi"},
            "resources": {"limits": {"memory": "1Gi"}},
            "containers": [{"resources": {"requests": {"memory": "100Mi"}}}],
        }));
        let mi = 1024.0 * 1024.0;
        let e = of_value(&p, "memory", Count::Scheduled);
        assert_eq!(e.request(), Some(110.0 * mi));
        // The pod-level limit bounds the pod, though its container has none.
        assert_eq!(e.limit(), Some(1034.0 * mi));
    }

    #[test]
    fn a_container_without_a_limit_leaves_the_pod_unlimited() {
        let p = pod(json!({"containers": [
            {"resources": {"limits": {"cpu": "1"}}},
            {"resources": {}},
        ]}));
        let e = of_value(&p, "cpu", Count::Scheduled);
        assert_eq!((e.request(), e.limit()), (None, None));
        // Added up for a node's total, the missing limit counts 0.
        assert_eq!(e.limit, 1000.0);
        assert_eq!(effective(&[], (None, None), 0.0, Count::Scheduled).limit(), None);
    }

    #[test]
    fn the_lean_spec_counts_the_same() {
        let p = pod(json!({
            "initContainers": [{"restartPolicy": "Always", "resources": {"requests": {"cpu": "100m", "nvidia.com/gpu": "1"}}}],
            "containers": [{"resources": {"requests": {"cpu": "200m"}, "limits": {"nvidia.com/gpu": "2"}}}],
            "overhead": {"cpu": "10m"},
        }));
        let lean: LeanSpec = serde_json::from_value(p["spec"].clone()).unwrap();
        for r in ["cpu", "memory", "nvidia.com/gpu"] {
            for c in [Count::Scheduled, Count::Running] {
                assert_eq!(lean.effective(r, c), of_value(&p, r, c), "{r}");
            }
        }
        assert_eq!(lean.resource_names(), vec!["cpu", "nvidia.com/gpu"]);
    }

    #[test]
    fn totals_over_pods() {
        let limited =
            pod(json!({"containers": [{"resources": {"requests": {"cpu": "100m"}, "limits": {"cpu": "1"}}}]}));
        let unlimited = pod(json!({"containers": [{"resources": {"requests": {"cpu": "50m"}}}]}));
        assert_eq!(total([&limited, &limited], "cpu"), (Some(200.0), Some(2000.0)));
        assert_eq!(total([&limited, &unlimited], "cpu"), (Some(150.0), None));
        assert_eq!(total([&pod(json!({"containers": [{}]}))], "cpu"), (None, None));
    }
}
