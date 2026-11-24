//! What a detail page fetches next to the object: pods, revisions, references, usage history.

use std::sync::Arc;

use kube::Client;
use serde_json::Value;

use crate::k8s;
use crate::k8s::ObjectRef;
use crate::overview::Related;

/// A page's pods: their changes go at most this often (each one works out the table again).
const PODS_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// A pod's containers as its tabs show them (`k8s::spec_containers`), with what each is.
pub(super) fn container_names(value: &Value) -> Vec<(String, k8s::ContainerKind)> {
    k8s::spec_containers(&value["spec"]).iter().map(|c| (c.name.to_string(), c.kind)).collect()
}

/// Names of the workload pods the charts measure (see `prom::chart_pods`).
fn chart_pod_names(pods: &[Value]) -> Vec<String> {
    crate::prom::chart_pods(pods)
        .0
        .into_iter()
        .filter_map(|p| p.pointer("/metadata/name").and_then(|v| v.as_str()).map(String::from))
        .collect()
}

/// Everything the overview links to or measures for one object.
/// CPU (millicores) and memory (bytes) of a pod, workload (its pods) or node from Prometheus.
pub(super) async fn history_for(
    client: Client,
    context: &str,
    kind: &k8s::Kind,
    ns: Option<&str>,
    obj: &Value,
    pods: &[Value],
    minutes: i64,
) -> Option<(crate::prom::Series, crate::prom::Series)> {
    let target = crate::prom::target(context, client.clone()).await.ok()??;
    let name = obj.pointer("/metadata/name").and_then(|v| v.as_str())?;
    let (cpu, mem) = if kind.is("", "Node") {
        crate::prom::node_usage(client, target, name, minutes).await.ok()?
    } else {
        let names: Vec<String> = if kind.is("", "Pod") { vec![name.to_string()] } else { chart_pod_names(pods) };
        if names.is_empty() {
            return None;
        }
        crate::prom::pod_usage(client, target, ns?, &names, minutes).await.ok()?
    };
    // Cores to millicores, like the rest of the UI.
    Some((cpu.into_iter().map(|(t, v)| (t, v * 1000.0)).collect(), mem))
}

/// A pod's CPU and memory history per container (empty for other kinds or without Prometheus).
pub(super) async fn container_history_for(
    client: Client,
    context: &str,
    kind: &k8s::Kind,
    ns: Option<&str>,
    obj: &Value,
    minutes: i64,
) -> std::collections::HashMap<String, (crate::prom::Series, crate::prom::Series)> {
    if !kind.is("", "Pod") {
        return Default::default();
    }
    let (Some(ns), Some(name)) = (ns, obj.pointer("/metadata/name").and_then(|v| v.as_str())) else {
        return Default::default();
    };
    let Ok(Some(target)) = crate::prom::target(context, client.clone()).await else { return Default::default() };
    crate::prom::container_usage(client, target, ns, name, minutes).await.unwrap_or_default()
}

/// GPU history for the same object as `history_for` (None without GPU metrics).
pub(super) async fn gpu_history_for(
    client: Client,
    context: &str,
    kind: &k8s::Kind,
    ns: Option<&str>,
    obj: &Value,
    pods: &[Value],
    minutes: i64,
) -> Option<crate::prom::GpuUsage> {
    let target = crate::prom::target(context, client.clone()).await.ok()??;
    let name = obj.pointer("/metadata/name").and_then(|v| v.as_str())?;
    if kind.is("", "Node") {
        return crate::prom::gpu_usage(client, target, crate::prom::GpuScope::Node(name), minutes).await;
    }
    let names: Vec<String> = if kind.is("", "Pod") { vec![name.to_string()] } else { chart_pod_names(pods) };
    if names.is_empty() {
        return None;
    }
    crate::prom::gpu_usage(client, target, crate::prom::GpuScope::Pods { ns: ns?, pods: &names }, minutes).await
}

/// What the page reads next to its object, at the usage's pace (see `load_related`): the usage
/// of the pods it lists (metrics-server; a node's or a pod's kubelet) every time, and the
/// objects it links to when `links` asks. Its pods are not listed here: their watch keeps them
/// (`pods_of`), and `pods` are those the page holds.
#[allow(clippy::too_many_arguments)]
pub(super) async fn fetch_related(
    client: Client,
    context: &str,
    kind: &k8s::Kind,
    ns: Option<String>,
    obj: &Value,
    clusters: Vec<(String, Client)>,
    pods: &[Value],
    links: bool,
) -> Related {
    let objects = async {
        if links {
            fetch_objects(client.clone(), context, kind, ns.clone(), obj, clusters, pods).await
        } else {
            Related::default()
        }
    };
    let (mut related, usage, stats) = futures::join!(
        objects,
        usage_of(client.clone(), context, kind, ns.as_deref(), obj, pods),
        kubelet_stats(client.clone(), kind, obj),
    );
    if let Some((usage, read)) = usage {
        (related.usage, related.usage_read) = (usage, Some(read));
    }
    related.node_stats = stats.map(Arc::new);
    related
}

/// The usage of the pods a page lists, by pod and container, with when metrics-server measured
/// it. Every pods table has CPU and Memory columns: whatever page lists the pods (a workload, a
/// Service, a PDB, a ServiceAccount…), their usage is read here, in one place; a pod's page reads
/// its namespace's. Metrics are optional (no metrics-server): no usage is not an error to show.
async fn usage_of(
    client: Client,
    context: &str,
    kind: &k8s::Kind,
    ns: Option<&str>,
    obj: &Value,
    pods: &[Value],
) -> Option<(k8s::ContainerUsage, k8s::Reading)> {
    if !crate::settings::metrics_on(context) {
        return None;
    }
    let (ns, selector) = if kind.is("", "Pod") { (ns?.to_string(), None) } else { usage_scope(kind, obj, pods)? };
    k8s::container_usage_read(client, ns, selector).await.ok()
}

/// The kubelet's stats for a node's page, or a pod's node (its ephemeral storage). A busy kubelet
/// can take tens of seconds: read alongside the rest, not before the usage and the other sections.
async fn kubelet_stats(client: Client, kind: &k8s::Kind, obj: &Value) -> Option<Value> {
    let node = if kind.is("", "Node") {
        obj.pointer("/metadata/name")
    } else if kind.is("", "Pod") {
        obj.pointer("/spec/nodeName")
    } else {
        None
    };
    k8s::node_stats(client, node.and_then(Value::as_str)?.to_string()).await
}

/// Which pods a page lists, as their watch picks them: in a namespace (or in every one), by
/// labels or fields; a PVC's are those of its namespace that mount it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct PodsOf {
    pub namespace: Option<String>,
    pub labels: Option<String>,
    pub fields: Option<String>,
    /// Only the pods that mount this claim: the API server can't select them.
    pub claim: Option<String>,
}

/// The pods a page lists, picked as they always were: a node's (`spec.nodeName`), a namespace's
/// (every one), a ServiceAccount's (`spec.serviceAccountName`), a PVC's (those that mount it),
/// and those a workload, a Service, a PDB or a NetworkPolicy selects (`k8s::pod_selection`).
/// None for a page without pods, or a selector that picks none.
pub(super) fn pods_of(kind: &k8s::Kind, ns: Option<&str>, obj: &Value) -> Option<PodsOf> {
    let name = obj.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default();
    let all = |namespace: Option<String>| PodsOf { namespace, labels: None, fields: None, claim: None };
    if kind.is("", "Node") {
        return Some(PodsOf { fields: Some(format!("spec.nodeName={name}")), ..all(None) });
    }
    if kind.is("", "Namespace") {
        return Some(all(Some(name.to_string())));
    }
    let ns = Some(ns?.to_string());
    if kind.is("", "ServiceAccount") {
        return Some(PodsOf { fields: Some(format!("spec.serviceAccountName={name}")), ..all(ns) });
    }
    if kind.is("", "PersistentVolumeClaim") {
        return Some(PodsOf { claim: Some(name.to_string()), ..all(ns) });
    }
    let selection = k8s::pod_selection(kind, obj).list_selector();
    let labels = if kind.is("policy", "PodDisruptionBudget") || kind.is("networking.k8s.io", "NetworkPolicy") {
        // An empty selector is every pod of the namespace.
        selection?
    } else if kind.ar.group == "apps" || kind.is("batch", "Job") || kind.is("", "Service") {
        Some(selection.flatten()?)
    } else {
        return None;
    };
    Some(PodsOf { labels, ..all(ns) })
}

impl PodsOf {
    /// The watch that follows them: whole pods without their managed fields, the first list
    /// from the API server's cache, changes sent at most every second, and a list every
    /// `metrics_refresh` where the watch is refused.
    pub(super) fn follow(&self) -> k8s::Follow {
        let keep = self.claim.clone().map(|claim| -> k8s::Keep { Arc::new(move |pod: &Value| mounts(pod, &claim)) });
        k8s::Follow {
            ar: k8s::ar_of::<k8s_openapi::api::core::v1::Pod>(),
            namespace: self.namespace.clone(),
            labels: self.labels.clone(),
            fields: self.fields.clone(),
            keep,
            managed: false,
            cached: true,
            every: PODS_EVERY,
            poll: crate::settings::secs(crate::settings::get().metrics_refresh),
        }
    }
}

/// Whether a pod mounts this claim.
fn mounts(pod: &Value, claim: &str) -> bool {
    pod.pointer("/spec/volumes").and_then(Value::as_array).is_some_and(|volumes| {
        volumes.iter().any(|v| v.pointer("/persistentVolumeClaim/claimName").and_then(Value::as_str) == Some(claim))
    })
}

/// Where to read the usage of the pods a page lists: their namespace, narrowed by the label
/// selector they were listed with, if any. None without pods, or when they are in several
/// namespaces (a node's page, whose kubelet gives their usage).
fn usage_scope(kind: &k8s::Kind, obj: &Value, pods: &[Value]) -> Option<(String, Option<String>)> {
    // A namespace's page counts its pods but has no table of them.
    if kind.is("", "Namespace") {
        return None;
    }
    fn ns_of(p: &Value) -> Option<&str> {
        p.pointer("/metadata/namespace").and_then(|v| v.as_str())
    }
    let ns = ns_of(pods.first()?)?;
    if pods.iter().any(|p| ns_of(p) != Some(ns)) {
        return None;
    }
    // Only kinds whose selector picks pods: a PVC's `spec.selector` picks volumes. Without
    // one, the whole namespace is read: more than needed, never less.
    let selects_pods = kind.is("", "Service")
        || kind.is("policy", "PodDisruptionBudget")
        || kind.is("networking.k8s.io", "NetworkPolicy")
        || kind.ar.group == "apps"
        || kind.is("batch", "Job");
    let selector = selects_pods.then(|| k8s::pod_selection(kind, obj).list_selector().flatten()).flatten();
    Some((ns.to_string(), selector))
}

async fn fetch_objects(
    client: Client,
    context: &str,
    kind: &k8s::Kind,
    ns: Option<String>,
    obj: &Value,
    clusters: Vec<(String, Client)>,
    pods: &[Value],
) -> Related {
    use k8s_openapi::api::{apps::v1 as apps, batch::v1 as batch, core::v1 as core};
    let mut related = Related::default();
    if kind.is("argoproj.io", "Application") {
        let connected = clusters.iter().map(|(n, _)| n.clone()).collect();
        let dest = k8s::argo::destination_of(obj.clone(), context.to_string(), connected).await;
        // Its resources live on the destination: read there, on a connected context.
        let drift = k8s::argo::diagnosis::drift_for(obj, dest.1.as_deref(), &clusters).await;
        related.argo_destination = Some(dest);
        related.argo_drift = Some(drift);
        return related;
    }
    if kind.is("", "Node") {
        let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default();
        // TopoLVM's volumes are listed for the whole cluster: only on a node it manages.
        let topolvm = k8s::topolvm::has_local_storage(obj);
        let volumes = async {
            if topolvm { Some(k8s::topolvm::logical_volumes(client.clone(), name.to_string()).await) } else { None }
        };
        let (slices, volumes) = futures::join!(k8s::resource_slices(client.clone(), name.to_string()), volumes);
        related.extra = slices;
        related.local_volumes = match volumes {
            Some(Ok(v)) => Some(v),
            Some(Err(e)) => {
                related.errors.insert("local_volumes", k8s::lists::load_error(&e));
                None
            }
            None => None,
        };
        return related;
    }
    if kind.is("", "Namespace") {
        let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let quotas = k8s::list_values(client, k8s::ar_of::<core::ResourceQuota>(), Some(name), None).await;
        related.extra = related.noted("extra", quotas);
        return related;
    }
    if kind.ar.group == "rbac.authorization.k8s.io" {
        // Cluster-wide roles may be bound in any namespace.
        let scope = if kind.namespaced { ns.clone() } else { None };
        related.rbac = Some(k8s::rbac_snapshot(client, scope).await);
        return related;
    }
    if kind.is("autoscaling.k8s.io", "VerticalPodAutoscaler") {
        let (Some(av), Some(tk), Some(tn), Some(ns)) = (
            obj.pointer("/spec/targetRef/apiVersion").and_then(|v| v.as_str()),
            obj.pointer("/spec/targetRef/kind").and_then(|v| v.as_str()),
            obj.pointer("/spec/targetRef/name").and_then(|v| v.as_str()),
            ns.as_deref(),
        ) else {
            return related;
        };
        let prefix = if av.contains('/') { "apis" } else { "api" };
        related.extra = k8s::get_json(client, format!("/{prefix}/{av}/namespaces/{ns}/{}s/{tn}", tk.to_lowercase()))
            .await
            .into_iter()
            .collect();
        return related;
    }
    let Some(ns) = ns else { return related };
    if kind.is("", "ServiceAccount") {
        related.rbac = Some(k8s::rbac_snapshot(client, None).await);
        return related;
    }
    if kind.is("batch", "CronJob") {
        let uid = obj.pointer("/metadata/uid").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let jobs = k8s::list_values(client, k8s::ar_of::<batch::Job>(), Some(ns), None).await;
        related.extra = related
            .noted("extra", jobs)
            .into_iter()
            .filter(|j| {
                j.pointer("/metadata/ownerReferences")
                    .and_then(|o| o.as_array())
                    .is_some_and(|o| o.iter().any(|r| r["uid"].as_str() == Some(uid.as_str())))
            })
            .collect();
        return related;
    }
    if kind.is("", "Pod") {
        // Its usage and its kubelet's stats are read next to these (`fetch_related`).
        // ConfigMaps and Secrets the environment reads, to show resolved values.
        let mut refs: std::collections::BTreeSet<(&str, String)> = Default::default();
        for c in k8s::spec_containers(&obj["spec"]).iter().map(|c| c.spec) {
            for e in c["env"].as_array().into_iter().flatten() {
                if let Some(n) = e.pointer("/valueFrom/configMapKeyRef/name").and_then(|v| v.as_str()) {
                    refs.insert(("ConfigMap", n.to_string()));
                }
                if let Some(n) = e.pointer("/valueFrom/secretKeyRef/name").and_then(|v| v.as_str()) {
                    refs.insert(("Secret", n.to_string()));
                }
            }
            for e in c["envFrom"].as_array().into_iter().flatten() {
                if let Some(n) = e.pointer("/configMapRef/name").and_then(|v| v.as_str()) {
                    refs.insert(("ConfigMap", n.to_string()));
                }
                if let Some(n) = e.pointer("/secretRef/name").and_then(|v| v.as_str()) {
                    refs.insert(("Secret", n.to_string()));
                }
            }
        }
        for (kind, name) in refs {
            let ar = if kind == "Secret" { k8s::ar_of::<core::Secret>() } else { k8s::ar_of::<core::ConfigMap>() };
            if let Some(v) = k8s::list_values_fields(
                client.clone(),
                ar,
                Some(ns.clone()),
                None,
                Some(format!("metadata.name={name}")),
            )
            .await
            .ok()
            .and_then(|l| l.into_iter().next())
            {
                related.env_sources.insert(format!("{kind}/{name}"), v);
            }
        }
        // NetworkPolicies whose podSelector selects this pod.
        let labels = obj.pointer("/metadata/labels").and_then(|l| l.as_object()).cloned().unwrap_or_default();
        related.netpols = k8s::list_values(
            client.clone(),
            k8s::ar_of::<k8s_openapi::api::networking::v1::NetworkPolicy>(),
            Some(ns.clone()),
            None,
        )
        .await
        .ok()
        .map(|all| {
            all.into_iter()
                .filter(|p| p.pointer("/spec/podSelector").is_some_and(|s| k8s::selector_matches(s, &labels)))
                .collect()
        });
        // Events and the last crash output, for the diagnosis.
        let uid = obj.pointer("/metadata/uid").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let objref = ObjectRef { kind: kind.clone(), name: name.clone(), namespace: Some(ns.clone()), uid };
        let events = k8s::events_for(client.clone(), objref).await;
        related.events =
            related.noted("events", events).into_iter().map(|e| (e.kind, e.reason, e.message, e.count)).collect();
        // A container that failed: its previous run if it restarted, else its current one.
        if let Some((container, previous)) = crate::diagnose::crashed_container(obj)
            && let Ok(text) =
                k8s::logs_text(client.clone(), ns.clone(), name, Some(container.clone()), 30, None, previous).await
        {
            related.crash_logs = Some((container, text));
        }
        let services = k8s::list_values(client, k8s::ar_of::<core::Service>(), Some(ns), None).await;
        related.services = related.noted("services", services);
    } else if kind.ar.group == "apps" || kind.is("batch", "Job") {
        // The crash output of the pod the diagnosis explains, as a pod's page shows it: one
        // read, and only when that pod has a crashed container.
        let crashed = crate::diagnose::worst_pod(pods).and_then(|p| {
            let (container, previous) = crate::diagnose::crashed_container(p)?;
            Some((p.pointer("/metadata/name")?.as_str()?.to_string(), container, previous))
        });
        let crash = {
            let (client, ns) = (client.clone(), ns.clone());
            async move {
                let (pod, container, previous) = crashed?;
                let text =
                    k8s::logs_text(client, ns, pod.clone(), Some(container.clone()), 30, None, previous).await.ok()?;
                Some((pod, (container, text)))
            }
        };
        let links = async {
            // Only a workload that selects pods has Services and revisions to show.
            if matches!(k8s::pod_selection(kind, obj), k8s::Selection::Labels(_)) {
                let services =
                    k8s::list_values(client.clone(), k8s::ar_of::<core::Service>(), Some(ns.clone()), None).await;
                related.services = related.noted("services", services);
                if kind.is("apps", "StatefulSet") || kind.is("apps", "DaemonSet") || kind.is("apps", "Deployment") {
                    let revisions = k8s::rollouts::revisions(client, kind, ns, obj).await;
                    related.revisions = related.noted("revisions", revisions);
                }
            }
        };
        let (crash, ()) = futures::join!(crash, links);
        if let Some((pod, logs)) = crash {
            related.crash_logs = Some(logs);
            related.crash_pod = Some(pod);
        }
    } else if kind.is("", "Secret") || kind.is("", "ConfigMap") {
        // Who mounts or reads this object, by scanning workloads in the namespace.
        let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let is_secret = kind.is("", "Secret");
        if is_secret {
            related.external_secret = crate::external_secrets::source_of(client.clone(), &ns, obj).await;
        }
        let sources = [
            ("deployments", k8s::ar_of::<apps::Deployment>(), "/spec/template/spec"),
            ("statefulsets", k8s::ar_of::<apps::StatefulSet>(), "/spec/template/spec"),
            ("daemonsets", k8s::ar_of::<apps::DaemonSet>(), "/spec/template/spec"),
            ("cronjobs", k8s::ar_of::<batch::CronJob>(), "/spec/jobTemplate/spec/template/spec"),
            ("pods", k8s::ar_of::<core::Pod>(), "/spec"),
        ];
        for (label, ar, spec_path) in sources {
            let items = k8s::list_values(client.clone(), ar, Some(ns.clone()), None).await;
            // One kind refused must not read as "nobody uses it": say which could not be checked.
            let items = items.unwrap_or_else(|e| {
                let note = format!("{label} {}", k8s::lists::load_error(&e));
                related.errors.entry("referenced_by").and_modify(|n| *n = format!("{n}, {note}")).or_insert(note);
                Vec::new()
            });
            let names: Vec<String> = items
                .iter()
                .filter(|item| item.pointer(spec_path).is_some_and(|spec| references(spec, &name, is_secret)))
                .filter_map(|item| item.pointer("/metadata/name").and_then(|v| v.as_str()).map(String::from))
                .collect();
            if !names.is_empty() {
                related.referenced_by.insert(label.to_string(), names);
            }
        }
    }
    related
}

/// Does a pod spec use this Secret/ConfigMap (volumes, env, envFrom, pull secrets, optional
/// ones too)? The same references the Map and the Problems page read (`spec_refs`).
pub(super) fn references(spec: &Value, name: &str, secret: bool) -> bool {
    use crate::problems::references::{Of, spec_refs};
    let of = if secret { Of::Secret } else { Of::ConfigMap };
    spec_refs(None, spec, true).iter().any(|t| t.of == of && t.name == name)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{PodsOf, pods_of, usage_scope};
    use crate::k8s;

    fn kind(group: &str, kind: &str) -> k8s::Kind {
        k8s::Kind::fake(group, kind)
    }

    fn pod(ns: &str, name: &str) -> serde_json::Value {
        json!({ "metadata": { "namespace": ns, "name": name } })
    }

    /// Every page that lists pods reads their usage, not only workloads' (#747).
    #[test]
    fn pages_listing_pods_read_their_usage() {
        let svc = json!({ "spec": { "selector": { "app": "shop" } } });
        let pods = [pod("shop", "a"), pod("shop", "b")];
        assert_eq!(usage_scope(&kind("", "Service"), &svc, &pods), Some(("shop".into(), Some("app=shop".into()))));
        let deploy = json!({ "spec": { "selector": { "matchLabels": { "app": "shop" } } } });
        assert_eq!(
            usage_scope(&kind("apps", "Deployment"), &deploy, &pods),
            Some(("shop".into(), Some("app=shop".into())))
        );
        // A ServiceAccount has no selector: its namespace is read.
        assert_eq!(usage_scope(&kind("", "ServiceAccount"), &json!({}), &pods), Some(("shop".into(), None)));
    }

    /// A PVC's selector picks volumes, not pods: it must not narrow the read.
    #[test]
    fn a_selector_that_does_not_pick_pods_is_not_used() {
        let pvc = json!({ "spec": { "selector": { "matchLabels": { "tier": "fast" } } } });
        assert_eq!(
            usage_scope(&kind("", "PersistentVolumeClaim"), &pvc, &[pod("shop", "a")]),
            Some(("shop".into(), None))
        );
    }

    #[test]
    fn no_read_without_pods_or_across_namespaces() {
        let svc = json!({ "spec": { "selector": { "app": "shop" } } });
        assert_eq!(usage_scope(&kind("", "Service"), &svc, &[]), None);
        // A node's pods come from every namespace: its kubelet gives their usage.
        let pods = [pod("shop", "a"), pod("kube-system", "b")];
        assert_eq!(usage_scope(&kind("", "Node"), &json!({}), &pods), None);
        // A namespace's page shows no pods table.
        assert_eq!(usage_scope(&kind("", "Namespace"), &json!({}), &[pod("shop", "a")]), None);
    }

    fn of(namespace: Option<&str>, labels: Option<&str>, fields: Option<&str>, claim: Option<&str>) -> Option<PodsOf> {
        let owned = |s: Option<&str>| s.map(String::from);
        Some(PodsOf { namespace: owned(namespace), labels: owned(labels), fields: owned(fields), claim: owned(claim) })
    }

    /// Each page follows the pods it always listed: a node's by field, a namespace's all of
    /// them, a ServiceAccount's by field, a PVC's those that mount it, and those a workload, a
    /// Service, a PDB or a NetworkPolicy selects.
    #[test]
    fn each_page_follows_the_pods_it_lists() {
        let named = |name: &str| json!({ "metadata": { "name": name } });
        let node = pods_of(&kind("", "Node"), None, &named("node-1"));
        assert_eq!(node, of(None, None, Some("spec.nodeName=node-1"), None));
        assert_eq!(pods_of(&kind("", "Namespace"), None, &named("shop")), of(Some("shop"), None, None, None));
        let account = pods_of(&kind("", "ServiceAccount"), Some("shop"), &named("web"));
        assert_eq!(account, of(Some("shop"), None, Some("spec.serviceAccountName=web"), None));
        let claim = pods_of(&kind("", "PersistentVolumeClaim"), Some("shop"), &named("data"));
        assert_eq!(claim, of(Some("shop"), None, None, Some("data")));
        let deploy = json!({ "spec": { "selector": { "matchLabels": { "app": "web" } } } });
        assert_eq!(
            pods_of(&kind("apps", "Deployment"), Some("shop"), &deploy),
            of(Some("shop"), Some("app=web"), None, None)
        );
        let svc = json!({ "spec": { "selector": { "app": "web" } } });
        assert_eq!(pods_of(&kind("", "Service"), Some("shop"), &svc), of(Some("shop"), Some("app=web"), None, None));
        // An empty selector: a PDB's or a NetworkPolicy's is every pod, a Service's none.
        let empty = json!({ "spec": { "selector": {}, "podSelector": {} } });
        let pdb = kind("policy", "PodDisruptionBudget");
        assert_eq!(pods_of(&pdb, Some("shop"), &empty), of(Some("shop"), None, None, None));
        let netpol = kind("networking.k8s.io", "NetworkPolicy");
        assert_eq!(pods_of(&netpol, Some("shop"), &empty), of(Some("shop"), None, None, None));
        assert_eq!(pods_of(&kind("", "Service"), Some("shop"), &empty), None);
        assert_eq!(pods_of(&pdb, Some("shop"), &json!({ "spec": {} })), None);
        // Pages without a pods table.
        assert_eq!(pods_of(&kind("", "ConfigMap"), Some("shop"), &named("settings")), None);
        assert_eq!(pods_of(&kind("batch", "CronJob"), Some("shop"), &named("backup")), None);
        assert_eq!(pods_of(&kind("", "Pod"), Some("shop"), &named("web-1")), None);
    }

    /// A Service whose selector was edited follows other pods: its watch starts again.
    #[test]
    fn a_new_selector_is_a_new_follow() {
        let svc = |app: &str| json!({ "spec": { "selector": { "app": app } } });
        let service = kind("", "Service");
        assert_ne!(pods_of(&service, Some("shop"), &svc("web")), pods_of(&service, Some("shop"), &svc("api")));
        assert_eq!(pods_of(&service, Some("shop"), &svc("web")), pods_of(&service, Some("shop"), &svc("web")));
    }

    /// A PVC's page keeps the pods of its namespace that mount it, whole but without their
    /// managed fields, the first list from the watch cache.
    #[test]
    fn a_claim_s_pods_are_those_that_mount_it() {
        let claim = json!({ "metadata": { "name": "data" } });
        let follow = pods_of(&kind("", "PersistentVolumeClaim"), Some("shop"), &claim).unwrap().follow();
        assert!(!follow.managed && follow.cached);
        let keep = follow.keep.expect("a filter on the claim");
        let mounting = |claim: &str| json!({ "spec": { "volumes": [{ "name": "v", "persistentVolumeClaim": { "claimName": claim } }] } });
        assert!(keep(&mounting("data")));
        assert!(!keep(&mounting("other")));
        assert!(!keep(&json!({ "spec": {} })));
    }
}
