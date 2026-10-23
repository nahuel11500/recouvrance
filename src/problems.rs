//! "What is broken?": problems found from kubectl's own tables (cheap even on
//! very large clusters), for the Problems page and the `find_problems` MCP tool.

use anyhow::Result;
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
    defs.iter().position(|d| d.name == name).and_then(|i| row.cells.get(i)).map(String::as_str).unwrap_or("")
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

/// Judge one table row of a kind; None when it looks healthy.
pub fn judge(kind: &Kind, defs: &[ColumnDef], row: &Row) -> Option<(Severity, String)> {
    let c = |n: &str| cell(defs, row, n);
    match (kind.ar.group.as_str(), kind.ar.kind.as_str()) {
        ("", "Pod") => {
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
                s if s.strip_prefix("Init:").is_some_and(|r| ratio(r).is_none()) => {
                    Some((Severity::Critical, s.into()))
                }
                s if s.starts_with("Init:") => {
                    older_than(row, 300).then(|| (Severity::Warning, format!("{s} for more than 5 minutes")))
                }
                "Pending" | "SchedulingGated" | "ContainerCreating" | "PodInitializing" if older_than(row, 300) => {
                    Some((Severity::Warning, format!("{status} for more than 5 minutes")))
                }
                // From the deletion (its grace period's end), not the creation.
                "Terminating" if since(row.deleted_at.or(row.created), 600) => {
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

/// Kinds checked, in display order.
pub const CHECKED: [(&str, &str); 7] = [
    ("", "Node"),
    ("apps", "Deployment"),
    ("apps", "StatefulSet"),
    ("apps", "DaemonSet"),
    ("", "Pod"),
    ("batch", "Job"),
    ("", "PersistentVolumeClaim"),
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

/// Whether a row listed without its object has been terminating for more than 10 minutes,
/// from its object's deletion time (one small request; true when it can't be read).
async fn stuck_terminating(client: &Client, kind: &Kind, row: &Row) -> bool {
    let api: kube::Api<kube::api::DynamicObject> = match &row.namespace {
        Some(ns) => kube::Api::namespaced_with(client.clone(), ns, &kind.ar),
        None => kube::Api::all_with(client.clone(), &kind.ar),
    };
    let name = row.name.clone();
    let deleted = k8s::run(async move { Ok(api.get_metadata(&name).await?.metadata.deletion_timestamp) }).await;
    match deleted {
        Ok(Some(t)) => since(jiff::Timestamp::from_second(t.0.as_second()).ok(), 600),
        Ok(None) => false,
        Err(_) => true,
    }
}

/// Problems in a cluster (or one namespace), worst first. Fails when any kind could
/// not be read; `scan_partial` keeps what it could read.
pub async fn scan(client: Client, kinds: &[Kind], namespace: Option<String>) -> Result<Vec<Problem>> {
    let scan = scan_partial(client, kinds, namespace).await;
    match scan.skipped.into_iter().next() {
        Some((_, e)) => Err(e),
        None => Ok(scan.problems),
    }
}

/// Problems in a cluster (or one namespace), worst first, from every kind that
/// could be listed: one kind refused (403) does not hide the others.
pub async fn scan_partial(client: Client, kinds: &[Kind], namespace: Option<String>) -> Scan {
    let jobs = CHECKED.iter().filter_map(|(g, k)| kinds.iter().find(|x| x.is(g, k)).cloned()).map(|kind| {
        let (client, ns) = (client.clone(), namespace.clone());
        async move {
            // Big lists come from their cells alone: a small part of the bytes.
            let (defs, rows) = match k8s::list_light(client.clone(), kind.clone(), ns, true).await {
                Ok(t) => t,
                Err(e) => return Err((kind.label.clone(), e)),
            };
            let mut found = Vec::new();
            for r in &rows {
                let Some((severity, reason)) = judge(&kind, &defs, r) else { continue };
                // Without its object, a terminating row has no deletion time: ask for it,
                // so a pod in its normal grace period is not called stuck.
                if r.is_partial() && reason == STUCK_TERMINATING && !stuck_terminating(&client, &kind, r).await {
                    continue;
                }
                found.push(Problem {
                    severity,
                    kind: kind.kubectl_name(),
                    label: kind.ar.kind.clone(),
                    namespace: r.namespace.clone(),
                    name: r.name.clone(),
                    reason,
                    age: r.created,
                });
            }
            Ok(found)
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

#[cfg(test)]
mod tests {
    use super::*;

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
            cells: cells.iter().map(|c| c.to_string()).collect(),
            labels: "".into(),
            deleting: false,
            deleted_at: None,
        }
    }

    fn defs(names: &[&str]) -> Vec<ColumnDef> {
        names.iter().map(|n| ColumnDef { name: n.to_string(), priority: 0 }).collect()
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
}
