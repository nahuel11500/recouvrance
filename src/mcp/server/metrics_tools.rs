//! MCP tool to read the custom and external metrics APIs (`custom.metrics.k8s.io`,
//! `external.metrics.k8s.io`), what an HPA or KEDA reads from prometheus-adapter, like
//! `kubectl get --raw /apis/custom.metrics.k8s.io/v1beta1/...`.
//!
//! These APIs are not made of objects, so the other tools can't reach them. The path is built
//! here from checked parts (no raw path from the agent), and each read is decided by the rules
//! as a resource of the API's group: `pods.custom.metrics.k8s.io` for a pod metric,
//! `namespaces.custom.metrics.k8s.io` for a namespace's, `<metric>.external.metrics.k8s.io` for
//! an external one, so `*.custom.metrics.k8s.io` covers the whole API.

use anyhow::{Result, bail};
use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{KubeMcp, format_table};
use crate::k8s;
use crate::mcp::{Access, Mode, Verb};

pub const CUSTOM: &str = "custom.metrics.k8s.io";
pub const EXTERNAL: &str = "external.metrics.k8s.io";

#[derive(Deserialize, serde::Serialize, schemars::JsonSchema, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum MetricsApi {
    /// custom.metrics.k8s.io: metrics about objects (pods, namespaces, ...).
    Custom,
    /// external.metrics.k8s.io: metrics about nothing in the cluster (queues, ...).
    External,
}

#[derive(Deserialize, schemars::JsonSchema, Clone, Debug, Default)]
pub struct MetricsParams {
    pub context: String,
    #[schemars(description = "custom (custom.metrics.k8s.io) or external (external.metrics.k8s.io)")]
    pub api: Option<MetricsApi>,
    #[schemars(description = "the metric; omit to list the metrics the API serves")]
    pub metric: Option<String>,
    #[schemars(description = "required for external metrics and for metrics of namespaced objects")]
    pub namespace: Option<String>,
    #[schemars(
        description = "custom only: the plural of the objects the metric describes (pods, deployments.apps, nodes); omit for the namespace's own metric"
    )]
    pub object_kind: Option<String>,
    #[schemars(description = "custom only: the object, or * (the default) for every one the selector picks")]
    pub object_name: Option<String>,
    #[schemars(description = "label selector, e.g. app=web or queue_name=jobs")]
    pub label_selector: Option<String>,
}

/// One read of a metrics API: the path to GET and how the rules see it.
#[derive(Debug)]
pub struct MetricsRead {
    pub path: String,
    /// What the rules decide on: a resource of the API's group. For a read of every object
    /// (`*`) its verb is list with no name, and the rows are then filtered by name.
    pub access: Access,
}

/// A path segment from the agent: a name, a plural or a metric. Anything that could leave
/// the segment (`/`, `?`, `%`, `..`) is refused.
fn segment<'a>(what: &str, s: &'a str) -> Result<&'a str> {
    let ok = !s.is_empty()
        && s.len() <= 253
        && s != "."
        && s != ".."
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':'));
    if !ok {
        bail!("{what} {s:?} is not a valid name (letters, digits, - _ . : only)");
    }
    Ok(s)
}

/// The read of `p.metric`, or None to list the metrics (`p.metric` unset).
pub fn metrics_read(p: &MetricsParams) -> Result<Option<MetricsRead>> {
    let api = p.api.unwrap_or(MetricsApi::Custom);
    let Some(metric) = &p.metric else { return Ok(None) };
    let metric = segment("metric", metric)?;
    let ns = p.namespace.as_deref().map(|n| segment("namespace", n)).transpose()?;
    let query = p
        .label_selector
        .as_deref()
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!("?{}", crate::prom::form_urlencoded(&[("labelSelector", s.trim())])))
        .unwrap_or_default();
    let access = |resource: String, verb: Verb, namespace: Option<&str>, name: Option<&str>| Access {
        context: p.context.clone(),
        verb,
        resource,
        resource_aliases: Vec::new(),
        namespace: namespace.map(String::from),
        name: name.map(String::from),
        cluster_scoped: namespace.is_none(),
    };
    let read = match api {
        MetricsApi::External => {
            if p.object_kind.is_some() || p.object_name.is_some() {
                bail!("external metrics describe no object: leave out object_kind and object_name");
            }
            let Some(ns) = ns else { bail!("external metrics need a namespace") };
            MetricsRead {
                path: format!("/apis/{EXTERNAL}/v1beta1/namespaces/{ns}/{metric}{query}"),
                access: access(format!("{metric}.{EXTERNAL}"), Verb::Get, Some(ns), Some(metric)),
            }
        }
        MetricsApi::Custom => match p.object_kind.as_deref().filter(|k| *k != "namespaces" && *k != "namespace") {
            None => {
                if p.object_name.is_some() {
                    bail!("object_name needs object_kind");
                }
                let Some(ns) = ns else { bail!("a namespace's metric needs the namespace") };
                MetricsRead {
                    path: format!("/apis/{CUSTOM}/v1beta1/namespaces/{ns}/metrics/{metric}{query}"),
                    access: access(format!("namespaces.{CUSTOM}"), Verb::Get, Some(ns), Some(ns)),
                }
            }
            Some(kind) => {
                let kind = segment("object_kind", kind)?;
                let name = match p.object_name.as_deref().unwrap_or("*") {
                    "*" => None,
                    n => Some(segment("object_name", n)?),
                };
                let prefix = ns.map(|ns| format!("/namespaces/{ns}")).unwrap_or_default();
                let verb = if name.is_some() { Verb::Get } else { Verb::List };
                MetricsRead {
                    path: format!("/apis/{CUSTOM}/v1beta1{prefix}/{kind}/{}/{metric}{query}", name.unwrap_or("*")),
                    access: access(format!("{kind}.{CUSTOM}"), verb, ns, name),
                }
            }
        },
    };
    Ok(Some(read))
}

/// How the rules see the object a returned value describes, to filter a read of every object.
fn item_access(read: &Access, item: &Value) -> Access {
    let o = &item["describedObject"];
    let mut a = read.clone();
    a.verb = Verb::List;
    if let Some(name) = o["name"].as_str() {
        a.name = Some(name.to_string());
    }
    if let Some(ns) = o["namespace"].as_str() {
        a.namespace = Some(ns.to_string());
    }
    a
}

/// A metrics API's discovery entry as the rules see it: `pods/queue_length` reads as
/// `pods.custom.metrics.k8s.io`, `queue_length` (external) as `queue_length.external.metrics.k8s.io`.
fn entry_access(api: MetricsApi, context: &str, entry: &str) -> Access {
    let resource = match api {
        MetricsApi::Custom => format!("{}.{CUSTOM}", entry.split_once('/').map_or(entry, |(r, _)| r)),
        MetricsApi::External => format!("{entry}.{EXTERNAL}"),
    };
    Access {
        context: context.to_string(),
        verb: Verb::List,
        resource,
        resource_aliases: Vec::new(),
        namespace: None,
        name: None,
        cluster_scoped: false,
    }
}

async fn get(client: kube::Client, path: String) -> Result<Value> {
    k8s::run(async move { Ok(client.request::<Value>(http::Request::get(path).body(Vec::new())?).await?) }).await
}

/// A 404 on the API itself means nothing serves it on this cluster.
fn explain(e: anyhow::Error, group: &str) -> anyhow::Error {
    let text = format!("{e:#}");
    if text.contains("404")
        && (text.contains("page not found") || text.contains("the server could not find the requested resource"))
    {
        anyhow::anyhow!(
            "{text}\n{group} may not be served on this cluster (no prometheus-adapter, KEDA or other adapter registered for it), or the metric is unknown: call without metric to list what it serves"
        )
    } else {
        e
    }
}

#[tool_router(router = metrics_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "Read the custom or external metrics API (custom.metrics.k8s.io, external.metrics.k8s.io), what an HPA or KEDA sees from prometheus-adapter, like `kubectl get --raw /apis/custom.metrics.k8s.io/v1beta1/...`. Without metric: the metrics the API serves. With metric: its current values, for a namespace (custom, no object_kind), for objects of a kind (custom, object_kind such as pods, object_name or * with a label_selector), or for an external metric in a namespace (with label_selector). Rules see it as a resource of the API's group: pods.custom.metrics.k8s.io, namespaces.custom.metrics.k8s.io, <metric>.external.metrics.k8s.io (verb get, or list for *). For CPU and memory use top_pods and top_nodes.",
        annotations(read_only_hint = true)
    )]
    async fn get_metrics(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<MetricsParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "api": p.api, "metric": p.metric, "namespace": p.namespace,
            "object_kind": p.object_kind, "object_name": p.object_name, "label_selector": p.label_selector});
        let mut call = self.begin(&parts, "get_metrics", args)?;
        let api = p.api.unwrap_or(MetricsApi::Custom);
        let group = if api == MetricsApi::Custom { CUSTOM } else { EXTERNAL };
        let read = match metrics_read(&p) {
            Ok(r) => r,
            Err(e) => return call.finish(Err(e)),
        };
        let (client, _) = match self.hub.client(&p.context).await {
            Ok(c) => c,
            Err(e) => return call.finish(Err(e)),
        };
        let Some(read) = read else {
            // The metrics served, as far as some rule could let the agent read them.
            let list = match get(client, format!("/apis/{group}/v1beta1")).await {
                Ok(v) => v,
                Err(e) => return call.finish(Err(explain(e, group))),
            };
            let rows: Vec<Vec<String>> = list["resources"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|r| r["name"].as_str())
                .filter(|name| {
                    call.policy().decide_any_namespace(&entry_access(api, &p.context, name)).mode != Mode::Deny
                })
                .map(|name| match name.split_once('/') {
                    Some((kind, metric)) => vec![metric.to_string(), kind.to_string()],
                    None => vec![name.to_string(), String::new()],
                })
                .collect();
            let max = call.policy().max_rows as usize;
            let more = rows.len().saturating_sub(max);
            let mut out = format_table(&["METRIC", "OBJECTS"], &rows[..rows.len().min(max)]);
            if more > 0 {
                out.push_str(&format!("\n… {more} more"));
            }
            return call.finish(Ok(out));
        };
        let every = read.access.name.is_none() && read.access.verb == Verb::List;
        if every {
            self.authorize_list(&mut call, &read.access, false).await?;
        } else {
            self.authorize(&mut call, &read.access).await?;
        }
        let mut value = match get(client, read.path).await {
            Ok(v) => v,
            Err(e) => return call.finish(Err(explain(e, group))),
        };
        let max = call.policy().max_rows as usize;
        let mut note = String::new();
        if let Some(items) = value["items"].as_array_mut() {
            if every {
                let before = items.len();
                items.retain(|i| call.may_see(&item_access(&read.access, i)));
                let hidden = before - items.len();
                if hidden > 0 {
                    note.push_str(&format!("# {hidden} value(s) of objects the rules don't let you read left out\n"));
                }
            }
            if items.len() > max {
                note.push_str(&format!("# first {max} of {} values\n", items.len()));
                items.truncate(max);
            }
        }
        call.finish(k8s::to_yaml(&value, false, false).map(|y| format!("{note}{y}")))
    }
}

#[cfg(test)]
mod tests {
    use super::{MetricsApi, MetricsParams, metrics_read};
    use crate::mcp::Verb;
    use proptest::prelude::*;

    fn params(api: MetricsApi) -> MetricsParams {
        MetricsParams { context: "c".into(), api: Some(api), ..Default::default() }
    }

    #[test]
    fn paths_are_the_apis_own() {
        let mut p = params(MetricsApi::Custom);
        assert!(metrics_read(&p).unwrap().is_none(), "no metric lists them");
        p.metric = Some("cnpg_pgmq_queue_length".into());
        p.namespace = Some("team-a".into());
        p.label_selector = Some("cnpg_cluster=db,queue_name=e2e-a".into());
        let r = metrics_read(&p).unwrap().unwrap();
        assert_eq!(
            r.path,
            "/apis/custom.metrics.k8s.io/v1beta1/namespaces/team-a/metrics/cnpg_pgmq_queue_length?labelSelector=cnpg_cluster%3Ddb%2Cqueue_name%3De2e-a"
        );
        assert_eq!(r.access.resource, "namespaces.custom.metrics.k8s.io");
        assert_eq!((r.access.namespace.as_deref(), r.access.name.as_deref()), (Some("team-a"), Some("team-a")));

        p.object_kind = Some("pods".into());
        p.label_selector = None;
        let r = metrics_read(&p).unwrap().unwrap();
        assert_eq!(r.path, "/apis/custom.metrics.k8s.io/v1beta1/namespaces/team-a/pods/*/cnpg_pgmq_queue_length");
        assert_eq!((r.access.verb, r.access.name.as_deref()), (Verb::List, None));
        assert_eq!(r.access.resource, "pods.custom.metrics.k8s.io");

        p.object_name = Some("web-0".into());
        let r = metrics_read(&p).unwrap().unwrap();
        assert!(r.path.ends_with("/pods/web-0/cnpg_pgmq_queue_length"));
        assert_eq!((r.access.verb, r.access.name.as_deref()), (Verb::Get, Some("web-0")));

        // A cluster-scoped kind: no namespace in the path, and only rules for every
        // namespace cover it.
        p.namespace = None;
        p.object_kind = Some("nodes".into());
        p.object_name = Some("n1".into());
        let r = metrics_read(&p).unwrap().unwrap();
        assert_eq!(r.path, "/apis/custom.metrics.k8s.io/v1beta1/nodes/n1/cnpg_pgmq_queue_length");
        assert!(r.access.cluster_scoped && r.access.namespace.is_none());

        let mut e = params(MetricsApi::External);
        e.metric = Some("queue_length".into());
        assert!(metrics_read(&e).is_err(), "an external metric needs a namespace");
        e.namespace = Some("team-a".into());
        let r = metrics_read(&e).unwrap().unwrap();
        assert_eq!(r.path, "/apis/external.metrics.k8s.io/v1beta1/namespaces/team-a/queue_length");
        assert_eq!(r.access.resource, "queue_length.external.metrics.k8s.io");
        e.object_kind = Some("pods".into());
        assert!(metrics_read(&e).is_err(), "external metrics describe no object");
    }

    proptest! {
        /// Whatever the agent passes, the path stays in the metrics API asked for: each part it
        /// gave is one whole segment, never `.` or `..`, and nothing but the label selector
        /// lands in the query.
        #[test]
        fn paths_stay_in_the_api(
            custom in any::<bool>(),
            metric in "[a-z/.%?#:_ -]{0,12}",
            namespace in proptest::option::of("[a-z/.%?_-]{0,8}"),
            kind in proptest::option::of("[a-z/.%?*-]{0,8}"),
            name in proptest::option::of("[a-z/.%?*-]{0,8}"),
            selector in proptest::option::of("[a-z=,/?&#%. -]{0,12}"),
        ) {
            let p = MetricsParams {
                context: "c".into(),
                api: Some(if custom { MetricsApi::Custom } else { MetricsApi::External }),
                metric: Some(metric),
                namespace,
                object_kind: kind,
                object_name: name,
                label_selector: selector,
            };
            if let Ok(Some(r)) = metrics_read(&p) {
                let group = if custom { "custom.metrics.k8s.io" } else { "external.metrics.k8s.io" };
                let prefix = format!("/apis/{group}/v1beta1/");
                prop_assert!(r.path.starts_with(&prefix), "{}", r.path);
                let (path, query) = r.path.split_once('?').unwrap_or((&r.path, ""));
                prop_assert!(query.is_empty() || query.starts_with("labelSelector=") && !query.contains('&'));
                prop_assert!(!path.contains('%') && !path.contains('#'));
                for seg in path[prefix.len()..].split('/') {
                    prop_assert!(!seg.is_empty() && seg != "." && seg != "..", "{}", r.path);
                }
                let suffix = format!(".{group}");
                prop_assert!(r.access.resource.ends_with(&suffix));
                prop_assert_eq!(r.access.namespace.as_deref(), p.namespace.as_deref());
            }
        }
    }
}
