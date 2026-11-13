//! `workload_logs`: the logs of all of a workload's pods, merged in time order, like
//! `kubectl logs deployment/NAME --all-pods --prefix --timestamps`.

use http::request::Parts;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;

use super::{KubeMcp, MAX_LOG_BYTES, access_for, get_named};
use crate::k8s;
use crate::mcp::{Mode, Verb};

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WorkloadLogsParams {
    pub context: String,
    #[schemars(description = "deployments, statefulsets, daemonsets, jobs, replicasets or services")]
    pub kind: String,
    pub namespace: String,
    pub name: String,
    #[schemars(
        description = "container name; defaults to each pod's default container (its `kubectl.kubernetes.io/default-container` annotation, else its first), as kubectl does"
    )]
    pub container: Option<String>,
    #[schemars(description = "most recent lines per pod (default 100, capped by policy)")]
    pub tail_lines: Option<u32>,
    #[schemars(description = "only lines from the last N seconds")]
    pub since_seconds: Option<i64>,
    #[schemars(description = "only keep lines containing this text (case-insensitive)")]
    pub grep: Option<String>,
}

/// Pods read at most (the newest ones).
const MAX_PODS: usize = 20;

#[tool_router(router = logs_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "Logs of every pod of a workload or Service, merged in time order and prefixed with the pod, like `kubectl logs deployment/NAME --all-pods --prefix`. Pods your rules don't allow logs for are left out (and counted).",
        annotations(read_only_hint = true)
    )]
    async fn workload_logs(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<WorkloadLogsParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name, "container": p.container,
            "tail_lines": p.tail_lines, "since_seconds": p.since_seconds, "grep": p.grep});
        let mut call = self.begin(&parts, "workload_logs", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        let access = access_for(&kind, &p.context, Verb::Get, Some(p.namespace.clone()), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        let (_, pod_kind) = self.kind(&call, &p.context, "pods").await?;
        let value = match get_named(&client, &kind, Some(&p.namespace), &p.name, false).await {
            Ok(v) => v,
            Err(e) => return call.finish(Err(e)),
        };
        let Some(Some(selector)) = k8s::pod_selection(&kind, &value).list_selector() else {
            return call.finish(Err(anyhow::anyhow!("{} {} selects no pods", kind.ar.kind, p.name)));
        };
        let pods = match k8s::list_values(
            client.clone(),
            pod_kind.ar.clone(),
            Some(p.namespace.clone()),
            Some(selector),
        )
        .await
        {
            Ok(pods) => pods,
            Err(e) => return call.finish(Err(e)),
        };
        let mut names: Vec<(String, String)> = pods
            .iter()
            .filter_map(|pod| {
                let name = pod.pointer("/metadata/name")?.as_str()?.to_string();
                let created = pod.pointer("/metadata/creationTimestamp").and_then(|v| v.as_str()).unwrap_or_default();
                Some((created.to_string(), name))
            })
            .collect();
        names.sort_by(|a, b| b.cmp(a));
        let total = names.len();
        // Each pod's logs are decided on their own; an Ask is not asked once per pod.
        let (allowed, refused): (Vec<String>, Vec<String>) = names.into_iter().map(|(_, n)| n).partition(|n| {
            let a = access_for(&pod_kind, &p.context, Verb::Logs, Some(p.namespace.clone()), Some(n.clone()));
            call.policy().decide(&a).mode == Mode::Allow
        });
        let tail = i64::from(p.tail_lines.unwrap_or(100).min(call.policy().max_log_lines));
        let mut texts = Vec::new();
        // With no container named, each pod's default one, as kubectl picks it: the API server
        // refuses a pod that has several. The containers left aside are named.
        let (mut read, mut others) = (Vec::new(), Vec::new());
        for pod in allowed.iter().take(MAX_PODS) {
            let container = if let Some(c) = &p.container {
                Some(c.clone())
            } else {
                let value =
                    pods.iter().find(|v| v.pointer("/metadata/name").and_then(|n| n.as_str()) == Some(pod.as_str()));
                let c = value.map_or_default(log_containers);
                add_new(&mut read, c.read.iter().cloned());
                add_new(&mut others, c.others);
                c.read
            };
            let lp = kube::api::LogParams {
                container,
                tail_lines: Some(tail),
                since_seconds: p.since_seconds,
                timestamps: true,
                limit_bytes: Some(MAX_LOG_BYTES / MAX_PODS as i64),
                ..Default::default()
            };
            let (client, ns, name) = (client.clone(), p.namespace.clone(), pod.clone());
            let text = k8s::run(async move {
                let api: kube::Api<k8s_openapi::api::core::v1::Pod> = kube::Api::namespaced(client, &ns);
                Ok(api.logs(&name, &lp).await?)
            })
            .await;
            texts.push((
                pod.clone(),
                text.unwrap_or_else(|e| format!("[could not read logs: {}]", k8s::short_error(&e))),
            ));
        }
        let mut out = merge_logs(&texts, p.grep.as_deref());
        let mut notes: Vec<String> = containers_note(&read, &others).into_iter().collect();
        if allowed.len() > MAX_PODS {
            notes.push(format!("{} of {} pods read (the newest)", MAX_PODS, allowed.len()));
        }
        if !refused.is_empty() {
            notes.push(format!("{} of {total} pods left out: your rules don't allow their logs", refused.len()));
        }
        if !notes.is_empty() {
            out = format!("[{}]\n{out}", notes.join("; "));
        }
        call.finish(Ok(out))
    }
}

/// The containers of a pod whose logs are read when none is named: its default one, as
/// kubectl picks it (`k8s::default_container`), and the others it has, init containers
/// (native sidecars) included, to name in a note.
#[derive(Debug, Default, PartialEq)]
pub(super) struct LogContainers {
    pub read: Option<String>,
    pub others: Vec<String>,
}

pub(super) fn log_containers(pod: &serde_json::Value) -> LogContainers {
    // Every container, default first: sidecars, init and debug ones included.
    let mut others = k8s::pod_containers(pod);
    let read = (!others.is_empty()).then(|| others.remove(0));
    LogContainers { read, others }
}

/// What an agent is told when logs were read from the default container of pods that have
/// others: which one was read, and that `container` reads another.
pub(super) fn containers_note(read: &[String], others: &[String]) -> Option<String> {
    let others: Vec<&str> = others.iter().filter(|o| !read.contains(*o)).map(String::as_str).collect();
    if read.is_empty() || others.is_empty() {
        return None;
    }
    Some(format!(
        "logs of container {} (the default), not of {}: pass `container` to read one",
        read.join(", "),
        others.join(", ")
    ))
}

/// `items` added to `to`, each once, in order.
fn add_new(to: &mut Vec<String>, items: impl IntoIterator<Item = String>) {
    for item in items {
        if !to.contains(&item) {
            to.push(item);
        }
    }
}

/// Lines of several pods' logs (each line starting with its RFC 3339 timestamp, as
/// `timestamps=true` gives them) in time order, each prefixed with its pod, and kept only
/// when they contain `grep` (case-insensitive). A line without a timestamp stays after the
/// line before it.
fn merge_logs(texts: &[(String, String)], grep: Option<&str>) -> String {
    let grep = grep.map(str::to_lowercase);
    let mut lines: Vec<(String, usize, usize, String)> = Vec::new();
    for (pi, (pod, text)) in texts.iter().enumerate() {
        let mut last = String::new();
        for (li, line) in text.lines().enumerate() {
            let (time, rest) = match line.split_once(' ') {
                Some((t, rest)) if t.parse::<jiff::Timestamp>().is_ok() => (t.to_string(), rest),
                _ => (last.clone(), line),
            };
            last.clone_from(&time);
            if grep.as_ref().is_some_and(|g| !rest.to_lowercase().contains(g)) {
                continue;
            }
            lines.push((time, pi, li, format!("[{pod}] {rest}")));
        }
    }
    lines.sort_by(|a, b| (&a.0, a.1, a.2).cmp(&(&b.0, b.1, b.2)));
    lines.into_iter().map(|(_, _, _, l)| l).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{LogContainers, containers_note, log_containers, merge_logs};

    /// A pod with `app` and `helper`, and a native sidecar `proxy` (listed first by the API
    /// server when it refuses a call with no container).
    fn multi(annotation: Option<&str>) -> serde_json::Value {
        let mut pod = json!({
            "metadata": {"name": "multi-1"},
            "spec": {
                "initContainers": [{"name": "proxy", "restartPolicy": "Always"}],
                "containers": [{"name": "app"}, {"name": "helper"}],
            },
        });
        if let Some(a) = annotation {
            pod["metadata"]["annotations"] = json!({"kubectl.kubernetes.io/default-container": a});
        }
        pod
    }

    #[test]
    fn with_no_container_named_the_default_one_is_read() {
        let s = |v: &[&str]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
        // The first container, not the sidecar the API server lists first.
        assert_eq!(
            log_containers(&multi(None)),
            LogContainers { read: Some("app".into()), others: s(&["helper", "proxy"]) }
        );
        // The annotation's container when the pod has it.
        assert_eq!(
            log_containers(&multi(Some("helper"))),
            LogContainers { read: Some("helper".into()), others: s(&["app", "proxy"]) }
        );
        assert_eq!(log_containers(&multi(Some("gone"))).read.as_deref(), Some("app"));
        // A pod with one container: no others, no note.
        let single = json!({"spec": {"containers": [{"name": "web"}]}});
        let c = log_containers(&single);
        assert_eq!(c, LogContainers { read: Some("web".into()), others: vec![] });
        assert_eq!(containers_note(&s(&["web"]), &c.others), None);
    }

    #[test]
    fn the_note_names_what_was_read_and_what_was_not() {
        let s = |v: &[&str]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
        let note = containers_note(&s(&["app"]), &s(&["helper", "proxy"])).unwrap();
        assert!(note.contains("container app (the default)") && note.contains("not of helper, proxy:"), "{note}");
        // Pods of a workload that default to different containers: one read in some pod is not listed as unread.
        let note = containers_note(&s(&["app", "helper"]), &s(&["helper", "app", "proxy"])).unwrap();
        assert!(note.contains("not of proxy:"), "{note}");
    }

    #[test]
    fn pods_lines_interleave_by_time() {
        let texts = [
            ("web-a".to_string(), "2026-10-26T10:00:01Z one\n2026-10-26T10:00:03Z three\n".to_string()),
            ("web-b".to_string(), "2026-10-26T10:00:02Z two\ncontinued\n".to_string()),
        ];
        assert_eq!(merge_logs(&texts, None), "[web-a] one\n[web-b] two\n[web-b] continued\n[web-a] three");
        assert_eq!(merge_logs(&texts, Some("THREE")), "[web-a] three");
    }
}
