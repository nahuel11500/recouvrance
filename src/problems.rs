//! "What is broken?": problems found from kubectl's own tables (cheap even on
//! very large clusters), for the Problems page and the `find_problems` MCP tool.

use kube::Client;

use crate::k8s::{self, ColumnDef, Kind, Row};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Severity {
    Critical,
    Warning,
    Info,
}

#[derive(Clone, Debug)]
pub struct Problem {
    pub severity: Severity,
    /// kubectl-style kind, e.g. `pods` or `deployments.apps`.
    pub kind: String,
    pub label: String,
    pub namespace: Option<String>,
    pub name: String,
    pub reason: String,
    pub age: Option<jiff::Timestamp>,
}

fn cell<'a>(defs: &[ColumnDef], row: &'a Row, name: &str) -> &'a str {
    defs.iter().position(|d| d.name == name).and_then(|i| row.cells.get(i)).map_or("", String::as_str)
}

/// "2/3" -> (2, 3)
fn ratio(s: &str) -> Option<(i64, i64)> {
    let (a, b) = s.split_once('/')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn older_than(row: &Row, secs: i64) -> bool {
    since(row.created, secs)
}

/// More than `secs` seconds since `t` (true when unknown).
fn since(t: Option<jiff::Timestamp>, secs: i64) -> bool {
    t.is_none_or(|t| jiff::Timestamp::now().as_second() - t.as_second() > secs)
}

/// [`judge`] for a pod's row: the pods list colors a pod's containers that aren't ready by it.
pub fn judge_pod(defs: &[ColumnDef], row: &Row) -> Option<(Severity, String)> {
    let c = |n: &str| cell(defs, row, n);
    let status = c("Status");
    let restarts: i64 = c("Restarts").split(' ').next().and_then(|n| n.parse().ok()).unwrap_or(0);
    // Pods of CronJob runs (`name-<minutes>-xxxxx`) failing is one failed run, not an outage.
    let job_pod = {
        let parts: Vec<&str> = row.name.rsplitn(3, '-').collect();
        parts.len() == 3 && parts[1].len() >= 8 && parts[1].bytes().all(|b| b.is_ascii_digit())
    };
    match status {
        "Completed" | "Succeeded" => None,
        "Error" | "OOMKilled" if job_pod => Some((Severity::Warning, format!("CronJob run failed ({status})"))),
        // Exited with an error and not restarting: a failed run, not an outage.
        "Error" => Some((Severity::Warning, "exited with an error".into())),
        "CrashLoopBackOff"
        | "OOMKilled"
        | "ImagePullBackOff"
        | "ErrImagePull"
        | "InvalidImageName"
        | "CreateContainerConfigError"
        | "CreateContainerError"
        | "RunContainerError"
        | "Evicted"
        | "ContainerStatusUnknown" => Some((Severity::Critical, status.into())),
        // Init:N/M is init containers still running: like Pending. Init:<reason>
        // (CrashLoopBackOff, Error, ImagePullBackOff…) is one failing.
        s if s.strip_prefix("Init:").is_some_and(|r| ratio(r).is_none()) => Some((Severity::Critical, s.into())),
        s if s.starts_with("Init:") => {
            older_than(row, 300).then(|| (Severity::Warning, format!("{s} for more than 5 minutes")))
        }
        "Pending" | "SchedulingGated" | "ContainerCreating" | "PodInitializing" if older_than(row, 300) => {
            Some((Severity::Warning, format!("{status} for more than 5 minutes")))
        }
        // From the deletion (its grace period's end), not the creation.
        "Terminating" if since(row.deleted_at.or(row.created), STUCK_AFTER) => {
            Some((Severity::Warning, STUCK_TERMINATING.into()))
        }
        "Running" => match ratio(c("Ready")) {
            Some((r, t)) if r < t && older_than(row, 300) => {
                Some((Severity::Warning, format!("running but {r}/{t} containers ready")))
            }
            _ if restarts >= 5 && c("Restarts").contains("ago") && !c("Restarts").contains('d') => {
                let last = c("Restarts").split('(').nth(1).unwrap_or("").trim_end_matches(')').to_string();
                // Restarted within the hour: still flapping. kubectl writes up to
                // 3 hours in minutes ("150m ago").
                let ago = k8s::age_seconds(last.trim_end_matches(" ago"));
                let recent = ago.is_none_or(|s| s <= 3600);
                Some((
                    if recent { Severity::Warning } else { Severity::Info },
                    format!("{restarts} restarts, last {last}"),
                ))
            }
            _ => None,
        },
        "Pending" | "SchedulingGated" | "ContainerCreating" | "PodInitializing" | "Terminating" => None,
        other => Some((Severity::Warning, other.into())),
    }
}

/// Judge one table row of a kind; None when it looks healthy.
pub fn judge(kind: &Kind, defs: &[ColumnDef], row: &Row) -> Option<(Severity, String)> {
    let c = |n: &str| cell(defs, row, n);
    match (kind.ar.group.as_str(), kind.ar.kind.as_str()) {
        ("", "Pod") => judge_pod(defs, row),
        ("apps", "Deployment" | "StatefulSet") => match ratio(c("Ready")) {
            Some((0, t)) if t > 0 => Some((Severity::Critical, format!("0/{t} ready"))),
            Some((r, t)) if r < t && older_than(row, 300) => Some((Severity::Warning, format!("{r}/{t} ready"))),
            _ => None,
        },
        ("apps", "DaemonSet") => {
            let (desired, ready) = (c("Desired").parse::<i64>().unwrap_or(0), c("Ready").parse::<i64>().unwrap_or(0));
            (ready < desired && older_than(row, 300)).then(|| {
                (if ready == 0 { Severity::Critical } else { Severity::Warning }, format!("{ready}/{desired} ready"))
            })
        }
        ("", "Node") => {
            let status = c("Status");
            if status.split(',').any(|s| s == "NotReady" || s == "Unknown") {
                Some((Severity::Critical, status.into()))
            } else if status.contains("SchedulingDisabled") {
                Some((Severity::Info, "cordoned".into()))
            } else {
                None
            }
        }
        ("", "PersistentVolumeClaim") => {
            (c("Status") == "Pending" && older_than(row, 300)).then(|| (Severity::Warning, "Pending".into()))
        }
        ("batch", "Job") => {
            let status = c("Status");
            (status == "Failed" && !older_than(row, 86400)).then(|| (Severity::Warning, "failed".into()))
        }
        _ => None,
    }
}

/// Kinds checked, in display order. Namespaces, PersistentVolumes and Argo CD Applications
/// only for deletions stuck; a kind the cluster doesn't serve is left out. The
/// external-secrets.io ones are judged from their objects (`external_secrets::problems`).
pub const CHECKED: [(&str, &str); 13] = [
    ("", "Node"),
    ("", "Namespace"),
    ("apps", "Deployment"),
    ("apps", "StatefulSet"),
    ("apps", "DaemonSet"),
    ("", "Pod"),
    ("batch", "Job"),
    ("", "PersistentVolumeClaim"),
    ("", "PersistentVolume"),
    ("argoproj.io", "Application"),
    (crate::external_secrets::GROUP, "ClusterSecretStore"),
    (crate::external_secrets::GROUP, "SecretStore"),
    (crate::external_secrets::GROUP, "ExternalSecret"),
];

/// A scan that went as far as it could: the problems found, and the kinds it could
/// not read (refused or failed), so a count can say it is partial.
#[derive(Default)]
pub struct Scan {
    pub problems: Vec<Problem>,
    /// (kind, error) for each kind that could not be listed.
    pub skipped: Vec<(String, anyhow::Error)>,
}

impl Scan {
    /// "Pods (forbidden), Jobs (failed: timeout)", or None when nothing was skipped.
    pub fn skipped_text(&self) -> Option<String> {
        let parts: Vec<String> = self
            .skipped
            .iter()
            .map(|(kind, e)| match k8s::api_status(e) {
                Some(403) => format!("{kind} (forbidden)"),
                _ => format!("{kind} (failed: {})", k8s::short_error(e)),
            })
            .collect();
        (!parts.is_empty()).then(|| parts.join(", "))
    }
}

const STUCK_TERMINATING: &str = "stuck terminating";

/// Seconds a deletion may take before it is called stuck, from the deletionTimestamp (for
/// a pod, the end of its grace period).
const STUCK_AFTER: i64 = 600;

/// Whether a row is being deleted: from its object, or for a row listed without it, from
/// kubectl's Status column (Pods and PVCs say Terminating).
fn deleting(defs: &[ColumnDef], row: &Row) -> bool {
    row.deleting || row.is_partial() && cell(defs, row, "Status") == "Terminating"
}

/// "stuck deleting for 2d, held by a, b: why", or None while the deletion is more recent
/// than `STUCK_AFTER` (an unknown time counts as stuck). `why`: what a Namespace's
/// conditions say holds it.
fn stuck_reason(
    kind: &Kind,
    deleted: Option<jiff::Timestamp>,
    finalizers: &[String],
    why: &[String],
) -> Option<String> {
    if !since(deleted, STUCK_AFTER) {
        return None;
    }
    let mut s = format!("stuck {}", if kind.is("", "Pod") { "terminating" } else { "deleting" });
    if let Some(t) = deleted {
        s.push_str(&format!(" for {}", k8s::age(t)));
    }
    if !finalizers.is_empty() {
        s.push_str(&format!(", held by {}", finalizers.join(", ")));
    }
    if !why.is_empty() {
        s.push_str(&format!(": {}", why.join("; ")));
    }
    Some(s)
}

/// What a Namespace's deletion waits on, from its conditions (`NamespaceContentRemaining`,
/// `NamespaceFinalizersRemaining`, `NamespaceDeletion*Failure`): the resources left in it.
fn namespace_why(status: &serde_json::Value) -> Vec<String> {
    let conditions = status.get("conditions").and_then(|c| c.as_array()).map(Vec::as_slice).unwrap_or_default();
    conditions
        .iter()
        .filter_map(|c| {
            let ty = c.get("type")?.as_str()?;
            let bad = ty.starts_with("Namespace") && crate::status::true_is_bad(ty) && c.get("status")? == "True";
            bad.then(|| c.get("message").and_then(|m| m.as_str()).unwrap_or(ty).to_string())
        })
        .collect()
}

/// An object being deleted, as read on its own.
struct Held {
    created: Option<jiff::Timestamp>,
    deleted: Option<jiff::Timestamp>,
    finalizers: Vec<String>,
    why: Vec<String>,
}

/// The deletion of one object (lists don't carry finalizers): its metadata alone, or a
/// Namespace whole for its conditions. One small request; None when it is not being
/// deleted or is gone.
async fn held(client: &Client, kind: &Kind, namespace: Option<&str>, name: &str) -> anyhow::Result<Option<Held>> {
    let api: kube::Api<kube::api::DynamicObject> = match namespace {
        Some(ns) => kube::Api::namespaced_with(client.clone(), ns, &kind.ar),
        None => kube::Api::all_with(client.clone(), &kind.ar),
    };
    let (name, whole) = (name.to_string(), kind.is("", "Namespace"));
    let read = k8s::run(async move {
        Ok(if whole {
            api.get_opt(&name).await?.map(|o| (o.metadata, o.data.get("status").map(namespace_why)))
        } else {
            api.get_metadata_opt(&name).await?.map(|o| (o.metadata, None))
        })
    })
    .await?;
    let time =
        |t: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Time| jiff::Timestamp::from_second(t.0.as_second()).ok();
    Ok(read.and_then(|(meta, why)| {
        let deleted = meta.deletion_timestamp.as_ref()?;
        Some(Held {
            created: meta.creation_timestamp.as_ref().and_then(time),
            deleted: time(deleted),
            finalizers: meta.finalizers.unwrap_or_default(),
            why: why.unwrap_or_default(),
        })
    }))
}

/// Requests at once to read the objects found deleting.
const HELD_PARALLEL: usize = 16;

/// Problems of one kind, from its table; `namespace`: the one scanned, if any.
async fn scan_kind(client: Client, kind: Kind, namespace: Option<String>) -> anyhow::Result<Vec<Problem>> {
    use futures::StreamExt as _;
    let problem = |namespace: Option<String>, name: String, severity, reason, age| Problem {
        severity,
        kind: kind.kubectl_name(),
        label: kind.ar.kind.clone(),
        namespace,
        name,
        reason,
        age,
    };
    // One namespace: its own object, not every namespace.
    if let Some(ns) = namespace.as_deref().filter(|_| kind.is("", "Namespace")) {
        let held = held(&client, &kind, None, ns).await?;
        return Ok(held
            .and_then(|h| {
                let reason = stuck_reason(&kind, h.deleted, &h.finalizers, &h.why)?;
                Some(problem(None, ns.to_string(), Severity::Warning, reason, h.created))
            })
            .into_iter()
            .collect());
    }
    // Big lists come from their cells alone: a small part of the bytes.
    let (defs, rows) = k8s::list_light(client.clone(), kind.clone(), namespace, true).await?;
    let mut found = Vec::new();
    let mut stuck = Vec::new();
    for r in &rows {
        if deleting(&defs, r) {
            // Within its grace period, or deleted a moment ago: nothing to read.
            if r.is_partial() || since(r.deleted_at, STUCK_AFTER) {
                stuck.push(r.clone());
            }
            continue;
        }
        if let Some((severity, reason)) = judge(&kind, &defs, r) {
            found.push(problem(r.namespace.clone(), r.name.clone(), severity, reason, r.created));
        }
    }
    // Lists don't carry finalizers, and a row listed without its object has no deletion
    // time: read them for the few rows deleting, so a pod in its grace period isn't stuck.
    let read: Vec<_> = futures::stream::iter(stuck)
        .map(|r| {
            let (client, kind) = (client.clone(), kind.clone());
            async move {
                let h = held(&client, &kind, r.namespace.as_deref(), &r.name).await;
                (r, h)
            }
        })
        .buffer_unordered(HELD_PARALLEL)
        .collect()
        .await;
    for (r, h) in read {
        let reason = match h {
            Ok(Some(h)) => stuck_reason(&kind, h.deleted, &h.finalizers, &h.why),
            // Deleted in between.
            Ok(None) => None,
            // Unreadable: what the row says.
            Err(_) => stuck_reason(&kind, r.deleted_at, &[], &[]),
        };
        if let Some(reason) = reason {
            found.push(problem(r.namespace.clone(), r.name.clone(), Severity::Warning, reason, r.created));
        }
    }
    Ok(found)
}

/// Problems in a cluster (or one namespace), worst first, from every kind that
/// could be listed: one kind refused (403) does not hide the others.
pub async fn scan_partial(client: Client, kinds: &[Kind], namespace: Option<String>) -> Scan {
    let jobs = CHECKED.iter().filter_map(|(g, k)| kinds.iter().find(|x| x.is(g, k)).cloned()).map(|kind| {
        let (client, ns, label) = (client.clone(), namespace.clone(), kind.label.clone());
        async move {
            if kind.ar.group == crate::external_secrets::GROUP {
                return crate::external_secrets::problems(client, &kind, ns).await.map_err(|e| (label, e));
            }
            scan_kind(client, kind, ns).await.map_err(|e| (label, e))
        }
    });
    let mut out = Scan::default();
    for r in futures::future::join_all(jobs).await {
        match r {
            Ok(found) => out.problems.extend(found),
            Err(skipped) => out.skipped.push(skipped),
        }
    }
    out.problems
        .sort_by(|a, b| a.severity.cmp(&b.severity).then(a.namespace.cmp(&b.namespace)).then(a.name.cmp(&b.name)));
    out
}

/// Problems in the namespaces a user may read, for a cluster that may not list
/// cluster-wide; the whole cluster when `namespaces` is empty. Cluster-scoped kinds
/// (Nodes) are read once, namespaced ones in each namespace, and a Namespace in itself.
pub async fn scan_namespaces(client: Client, kinds: &[Kind], namespaces: &[String]) -> Scan {
    if namespaces.is_empty() {
        return scan_partial(client, kinds, None).await;
    }
    let (cluster, namespaced): (Vec<Kind>, Vec<Kind>) =
        kinds.iter().cloned().partition(|k| !k.namespaced && !k.is("", "Namespace"));
    let (whole, per_ns) = futures::join!(
        scan_partial(client.clone(), &cluster, None),
        futures::future::join_all(namespaces.iter().map(|ns| {
            let (client, namespaced) = (client.clone(), &namespaced);
            async move { (ns, scan_partial(client, namespaced, Some(ns.clone())).await) }
        })),
    );
    let mut out = whole;
    for (ns, scan) in per_ns {
        out.problems.extend(scan.problems);
        out.skipped.extend(scan.skipped.into_iter().map(|(kind, e)| (format!("{kind} in {ns}"), e)));
    }
    out.problems
        .sort_by(|a, b| a.severity.cmp(&b.severity).then(a.namespace.cmp(&b.namespace)).then(a.name.cmp(&b.name)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn kind(group: &str, k: &str) -> Kind {
        Kind {
            ar: kube::discovery::ApiResource {
                group: group.into(),
                version: "v1".into(),
                api_version: "v1".into(),
                kind: k.into(),
                plural: format!("{}s", k.to_lowercase()),
            },
            namespaced: true,
            section: "",
            label: k.into(),
        }
    }

    fn row(cells: &[&str]) -> Row {
        Row {
            uid: "u".into(),
            name: "x".into(),
            namespace: None,
            created: None,
            cells: cells.iter().map(ToString::to_string).collect(),
            labels: "".into(),
            deleting: false,
            deleted_at: None,
        }
    }

    fn defs(names: &[&str]) -> Vec<ColumnDef> {
        names.iter().map(|n| ColumnDef { name: n.to_string(), priority: 0 }).collect()
    }

    #[test]
    fn a_running_pod_not_ready_for_5_minutes_is_a_problem() {
        let d = defs(&["Name", "Ready", "Status", "Restarts"]);
        let mut pod = row(&["x", "0/1", "Running", "0"]);
        pod.created = Some(jiff::Timestamp::now() - jiff::SignedDuration::from_mins(10));
        assert_eq!(judge_pod(&d, &pod).map(|j| j.0), Some(Severity::Warning));
        // Just started: its readiness probe may not have passed yet.
        pod.created = Some(jiff::Timestamp::now() - jiff::SignedDuration::from_mins(1));
        assert_eq!(judge_pod(&d, &pod), None);
        let ready = row(&["x", "1/1", "Running", "0"]);
        assert_eq!(judge_pod(&d, &ready), None);
    }

    #[test]
    fn restarts_flap_only_within_the_hour() {
        let pod = kind("", "Pod");
        let d = defs(&["Name", "Ready", "Status", "Restarts", "Age"]);
        let level = |restarts: &str| judge(&pod, &d, &row(&["x", "1/1", "Running", restarts, "3d"])).map(|j| j.0);
        // kubectl writes up to 3 hours in minutes.
        assert_eq!(level("8 (150m ago)"), Some(Severity::Info));
        assert_eq!(level("8 (61m ago)"), Some(Severity::Info));
        assert_eq!(level("8 (59m30s ago)"), Some(Severity::Warning));
        assert_eq!(level("8 (60m ago)"), Some(Severity::Warning));
    }

    #[test]
    fn judges_rows() {
        let pod = kind("", "Pod");
        let d = defs(&["Name", "Ready", "Status", "Restarts", "Age"]);
        assert_eq!(
            judge(&pod, &d, &row(&["x", "0/1", "CrashLoopBackOff", "7", "1h"])).map(|j| j.0),
            Some(Severity::Critical)
        );
        assert!(judge(&pod, &d, &row(&["x", "1/1", "Running", "0", "1h"])).is_none());
        assert_eq!(
            judge(&pod, &d, &row(&["x", "1/1", "Running", "99 (15m ago)", "3d"])).map(|j| j.0),
            Some(Severity::Warning)
        );
        assert_eq!(
            judge(&pod, &d, &row(&["x", "1/1", "Running", "8 (20h ago)", "3d"])).map(|j| j.0),
            Some(Severity::Info)
        );
        let mut r = row(&["x", "0/1", "Error", "0", "1h"]);
        r.name = "backup-29865060-4ps6h".into();
        assert_eq!(judge(&pod, &d, &r).map(|j| j.0), Some(Severity::Warning));
        assert!(judge(&pod, &d, &row(&["x", "0/1", "Completed", "0", "1h"])).is_none());
        assert_eq!(judge(&pod, &d, &row(&["x", "1/2", "Running", "0", "1h"])).map(|j| j.0), Some(Severity::Warning));
        let deploy = kind("apps", "Deployment");
        let d = defs(&["Name", "Ready", "Up-to-date", "Available", "Age"]);
        assert_eq!(judge(&deploy, &d, &row(&["x", "0/3", "3", "0", "1d"])).map(|j| j.0), Some(Severity::Critical));
        assert!(judge(&deploy, &d, &row(&["x", "0/0", "0", "0", "1d"])).is_none());
        let node = kind("", "Node");
        let d = defs(&["Name", "Status"]);
        assert_eq!(judge(&node, &d, &row(&["n", "Ready,SchedulingDisabled"])).map(|j| j.0), Some(Severity::Info));
        assert_eq!(judge(&node, &d, &row(&["n", "NotReady"])).map(|j| j.0), Some(Severity::Critical));
    }

    /// A cluster that may list nodes and the pods of namespace `a` only, and records
    /// the paths it was asked for.
    #[derive(Clone, Default)]
    struct Restricted(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl tower::Service<http::Request<kube::client::Body>> for Restricted {
        type Response = http::Response<http_body_util::Full<bytes::Bytes>>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<kube::client::Body>) -> Self::Future {
            let path = req.uri().path().to_string();
            self.0.lock().unwrap().push(path.clone());
            let table = |defs: &[&str], ns: Option<&str>, cells: &[&str]| {
                serde_json::json!({
                    "kind": "Table",
                    "columnDefinitions": defs.iter().map(|d| serde_json::json!({"name": d})).collect::<Vec<_>>(),
                    "rows": [{"cells": cells, "object": {"metadata": {"name": cells[0], "namespace": ns, "uid": cells[0]}}}],
                })
            };
            let (code, body) = match path.as_str() {
                "/api/v1/nodes" => (200, table(&["Name", "Status"], None, &["n1", "NotReady"])),
                "/api/v1/namespaces/a/pods" => (
                    200,
                    table(
                        &["Name", "Ready", "Status", "Restarts", "Age"],
                        Some("a"),
                        &["p1", "0/1", "CrashLoopBackOff", "7", "1h"],
                    ),
                ),
                _ => (
                    403,
                    serde_json::json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                        "message": "forbidden", "reason": "Forbidden", "code": 403}),
                ),
            };
            let response = http::Response::builder().status(code).body(body.to_string().into_bytes().into()).unwrap();
            std::future::ready(Ok(response))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn scans_accessible_namespaces() {
        let cluster = Restricted::default();
        let client = Client::new(cluster.clone(), "default");
        let mut node = kind("", "Node");
        node.namespaced = false;
        let kinds = [node, kind("", "Pod")];
        let scan = scan_namespaces(client, &kinds, &["a".into(), "b".into()]).await;
        let found: Vec<(&str, Option<&str>)> =
            scan.problems.iter().map(|p| (p.name.as_str(), p.namespace.as_deref())).collect();
        assert_eq!(found, [("n1", None), ("p1", Some("a"))]);
        assert_eq!(scan.skipped_text().unwrap(), "Pod in b (forbidden)");
        // Nodes once, pods in each namespace, nothing cluster-wide.
        let mut asked = cluster.0.lock().unwrap().clone();
        asked.sort();
        assert_eq!(asked, ["/api/v1/namespaces/a/pods", "/api/v1/namespaces/b/pods", "/api/v1/nodes"]);
    }

    #[test]
    fn skipped_kinds_are_listed() {
        assert!(Scan::default().skipped_text().is_none());
        let scan = Scan {
            problems: Vec::new(),
            skipped: vec![("Pods".into(), anyhow::anyhow!("timeout")), ("Jobs".into(), anyhow::anyhow!("reset"))],
        };
        assert_eq!(scan.skipped_text().unwrap(), "Pods (failed: timeout), Jobs (failed: reset)");
    }

    #[test]
    fn init_and_terminating() {
        let pod = kind("", "Pod");
        let d = defs(&["Name", "Ready", "Status", "Restarts", "Age"]);
        let now = jiff::Timestamp::now();
        let fresh = |status: &str| {
            let mut r = row(&["x", "0/1", status, "0", "10s"]);
            r.created = Some(now);
            r
        };
        let old = |status: &str| {
            let mut r = row(&["x", "0/1", status, "0", "1h"]);
            r.created = Some(now - jiff::SignedDuration::from_hours(1));
            r
        };
        let sev = |r: &Row| judge(&pod, &d, r).map(|j| j.0);
        // Init containers starting: fine at first, stuck later.
        assert_eq!(sev(&fresh("Init:0/2")), None);
        assert_eq!(sev(&fresh("Init:1/2")), None);
        assert_eq!(sev(&old("Init:0/1")), Some(Severity::Warning));
        assert_eq!(sev(&old("Init:1/2")), Some(Severity::Warning));
        // An init container failing is critical right away.
        assert_eq!(sev(&fresh("Init:CrashLoopBackOff")), Some(Severity::Critical));
        assert_eq!(sev(&fresh("Init:Error")), Some(Severity::Critical));
        assert_eq!(sev(&fresh("Init:ImagePullBackOff")), Some(Severity::Critical));
        // Held by scheduling gates: waiting on purpose at first, like Pending.
        assert_eq!(sev(&fresh("SchedulingGated")), None);
        assert_eq!(sev(&old("SchedulingGated")), Some(Severity::Warning));
        // Terminating: from the deletion time when known.
        let mut t = old("Terminating");
        t.deleting = true;
        t.deleted_at = Some(now - jiff::SignedDuration::from_secs(30));
        assert_eq!(sev(&t), None);
        t.deleted_at = Some(now - jiff::SignedDuration::from_mins(20));
        assert_eq!(sev(&t), Some(Severity::Warning));
    }

    fn ago(secs: i64) -> Option<jiff::Timestamp> {
        jiff::Timestamp::from_second(jiff::Timestamp::now().as_second() - secs).ok()
    }

    #[test]
    fn stuck_deletions_name_what_holds_them() {
        let app = kind("argoproj.io", "Application");
        let argo = ["resources-finalizer.argocd.argoproj.io".to_string()];
        assert_eq!(stuck_reason(&app, ago(60), &argo, &[]), None);
        assert_eq!(
            stuck_reason(&app, ago(49 * 3600), &argo, &[]).unwrap(),
            "stuck deleting for 2d, held by resources-finalizer.argocd.argoproj.io"
        );
        let two = ["a".to_string(), "b".to_string()];
        assert_eq!(stuck_reason(&app, ago(3 * 3600), &two, &[]).unwrap(), "stuck deleting for 3h, held by a, b");
        // A pod past its grace period, held by nothing (its node is gone).
        assert_eq!(stuck_reason(&kind("", "Pod"), ago(1200), &[], &[]).unwrap(), "stuck terminating for 20m");
        // Not known when: stuck, as the row said.
        assert_eq!(stuck_reason(&kind("", "Pod"), None, &[], &[]).unwrap(), "stuck terminating");
        let why = ["Some resources are remaining: pods. has 3 resource instances".to_string()];
        assert_eq!(
            stuck_reason(&kind("", "Namespace"), ago(3 * 3600), &[], &why).unwrap(),
            "stuck deleting for 3h: Some resources are remaining: pods. has 3 resource instances"
        );
    }

    #[test]
    fn a_namespace_says_what_holds_it() {
        let status = serde_json::json!({"phase": "Terminating", "conditions": [
            {"type": "NamespaceDeletionDiscoveryFailure", "status": "False", "message": "All resources successfully discovered"},
            {"type": "NamespaceContentRemaining", "status": "True", "message": "Some resources are remaining: widgets.example.com has 1 resource instances"},
            {"type": "NamespaceFinalizersRemaining", "status": "True", "message": "Some content in the namespace has finalizers remaining: example.com/cleanup in 1 resource instances"},
        ]});
        assert_eq!(
            namespace_why(&status),
            [
                "Some resources are remaining: widgets.example.com has 1 resource instances",
                "Some content in the namespace has finalizers remaining: example.com/cleanup in 1 resource instances",
            ]
        );
        assert!(namespace_why(&serde_json::json!({"phase": "Active"})).is_empty());
    }

    #[test]
    fn deleting_rows() {
        let d = defs(&["Name", "Status"]);
        let mut full = row(&["x", "Terminating"]);
        assert!(!deleting(&d, &full), "a row with its object goes by its deletion time");
        full.deleting = true;
        assert!(deleting(&d, &full));
        let mut partial = row(&["x", "Terminating"]);
        partial.uid = "ns/x".into();
        assert!(deleting(&d, &partial));
        partial.cells[1] = "Bound".into();
        assert!(!deleting(&d, &partial));
    }

    proptest! {
        /// Stuck exactly when deleted more than `STUCK_AFTER` ago, naming every finalizer.
        #[test]
        fn stuck_after_the_threshold(secs in 0..100_000_000i64, finalizers in prop::collection::vec("[a-z./-]{1,20}", 0..4)) {
            let app = kind("argoproj.io", "Application");
            let reason = stuck_reason(&app, ago(secs), &finalizers, &[]);
            // A second's leeway: the clock moves between `ago` and the check.
            if secs > STUCK_AFTER + 1 {
                let reason = reason.unwrap();
                prop_assert!(reason.starts_with("stuck deleting for "));
                for f in &finalizers {
                    prop_assert!(reason.contains(f.as_str()));
                }
            } else if secs < STUCK_AFTER {
                prop_assert_eq!(reason, None);
            }
        }
    }

    /// A cluster with an Argo CD Application stuck deleting, one deleted a moment ago, and
    /// namespace `argocd` itself terminating; records the paths it was asked for.
    #[derive(Clone, Default)]
    struct Deleting(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl tower::Service<http::Request<kube::client::Body>> for Deleting {
        type Response = http::Response<http_body_util::Full<bytes::Bytes>>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<kube::client::Body>) -> Self::Future {
            let path = req.uri().path().to_string();
            self.0.lock().unwrap().push(path.clone());
            let time = |secs: i64| ago(secs).unwrap().to_string();
            let meta = |name: &str, deleted: i64| {
                serde_json::json!({"name": name, "namespace": "argocd", "uid": name,
                    "creationTimestamp": time(9 * 86400), "deletionTimestamp": time(deleted),
                    "finalizers": ["resources-finalizer.argocd.argoproj.io"]})
            };
            let body = match path.as_str() {
                "/apis/argoproj.io/v1alpha1/namespaces/argocd/applications" => serde_json::json!({
                    "kind": "Table",
                    "columnDefinitions": [{"name": "Name"}, {"name": "Sync Status"}, {"name": "Health Status"}],
                    "rows": [
                        {"cells": ["stuck", "Synced", "Healthy"], "object": {"metadata": meta("stuck", 49 * 3600)}},
                        {"cells": ["fresh", "Synced", "Healthy"], "object": {"metadata": meta("fresh", 30)}},
                        {"cells": ["fine", "Synced", "Healthy"], "object": {"metadata": {"name": "fine", "namespace": "argocd", "uid": "fine"}}},
                    ],
                }),
                "/apis/argoproj.io/v1alpha1/namespaces/argocd/applications/stuck" => serde_json::json!({
                    "apiVersion": "meta.k8s.io/v1", "kind": "PartialObjectMetadata", "metadata": meta("stuck", 49 * 3600),
                }),
                "/api/v1/namespaces/argocd" => serde_json::json!({
                    "apiVersion": "v1", "kind": "Namespace",
                    "metadata": {"name": "argocd", "uid": "ns", "creationTimestamp": time(9 * 86400), "deletionTimestamp": time(3 * 3600)},
                    "spec": {"finalizers": ["kubernetes"]},
                    "status": {"phase": "Terminating", "conditions": [{"type": "NamespaceFinalizersRemaining", "status": "True",
                        "message": "Some content in the namespace has finalizers remaining: resources-finalizer.argocd.argoproj.io in 1 resource instances"}]},
                }),
                _ => {
                    serde_json::json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "code": 404, "reason": "NotFound"})
                }
            };
            let code = if body["kind"] == "Status" { 404 } else { 200 };
            let response = http::Response::builder().status(code).body(body.to_string().into_bytes().into()).unwrap();
            std::future::ready(Ok(response))
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn finds_stuck_deletions() {
        let cluster = Deleting::default();
        let client = Client::new(cluster.clone(), "default");
        let mut app = kind("argoproj.io", "Application");
        app.ar.version = "v1alpha1".into();
        app.ar.api_version = "argoproj.io/v1alpha1".into();
        let mut ns = kind("", "Namespace");
        ns.namespaced = false;
        let scan = scan_namespaces(client, &[ns, app], &["argocd".into()]).await;
        assert!(scan.skipped.is_empty(), "{:?}", scan.skipped_text());
        let found: Vec<(&str, &str)> = scan.problems.iter().map(|p| (p.name.as_str(), p.reason.as_str())).collect();
        assert_eq!(
            found,
            [
                (
                    "argocd",
                    "stuck deleting for 3h: Some content in the namespace has finalizers remaining: \
                     resources-finalizer.argocd.argoproj.io in 1 resource instances"
                ),
                ("stuck", "stuck deleting for 2d, held by resources-finalizer.argocd.argoproj.io"),
            ]
        );
        // The namespace scanned is read alone, not every namespace listed; only the
        // Application stuck is read again.
        let mut asked = cluster.0.lock().unwrap().clone();
        asked.sort();
        assert_eq!(
            asked,
            [
                "/api/v1/namespaces/argocd",
                "/apis/argoproj.io/v1alpha1/namespaces/argocd/applications",
                "/apis/argoproj.io/v1alpha1/namespaces/argocd/applications/stuck",
            ]
        );
    }
}
