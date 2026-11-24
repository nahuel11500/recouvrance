//! "Why is this broken?": plain-language findings for a pod or workload, from its
//! status, events and the last lines its crashed containers wrote.

use crate::json::str_or_empty as s;
use serde_json::Value;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Critical,
    Warning,
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub level: Level,
    pub title: String,
    pub detail: String,
    /// What to try next.
    pub hint: String,
}

/// An event as diagnosis needs it: (type, reason, message, count).
pub type Ev<'a> = (&'a str, &'a str, &'a str, i32);

fn statuses(pod: &Value) -> impl Iterator<Item = (&Value, bool)> {
    let get = |p: &str| pod.pointer(p).and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    get("/status/initContainerStatuses")
        .iter()
        .map(|c| (c, true))
        .chain(get("/status/containerStatuses").iter().map(|c| (c, false)))
}

/// Exit codes people meet most, in words.
fn exit_meaning(code: i64) -> &'static str {
    match code {
        0 => "exited normally (a container that should keep running stopped)",
        1 => "the application failed (generic error)",
        2 => "misuse of a shell builtin or bad arguments",
        126 => "the command could not be executed (permissions?)",
        127 => "the command was not found in the image",
        128 => "invalid exit argument",
        134 => "aborted (SIGABRT)",
        137 => "killed (SIGKILL): out of memory or a failed liveness probe",
        139 => "segmentation fault (SIGSEGV)",
        143 => "terminated (SIGTERM), usually a normal shutdown",
        _ => "",
    }
}

const NOT_SCHEDULED: &str = "Not scheduled";

/// Whether a finding is a crash of `container`, the one its crash output goes under: a
/// crash loop or a failed exit ("Cannot pull the image of container x" has no space after).
fn about_crash(f: &Finding, container: &str) -> bool {
    f.level == Level::Critical && f.title.to_lowercase().contains(&format!("container {container} "))
}

/// Findings for a pod, worst first. `crash_logs` are the last lines of the
/// previous run of a crashing container, as (container, text).
pub fn pod(pod: &Value, events: &[Ev], crash_logs: Option<&(String, String)>) -> Vec<Finding> {
    let mut out = Vec::new();
    let phase = s(pod, "/status/phase");
    if pod.pointer("/metadata/deletionTimestamp").is_some() {
        let finalizers = pod.pointer("/metadata/finalizers").and_then(Value::as_array).map_or(0, Vec::len);
        out.push(Finding {
            level: Level::Warning,
            title: "Being deleted".into(),
            detail: format!(
                "Deletion was requested{}.",
                if finalizers > 0 {
                    format!(
                        "; {} {} it",
                        crate::plural::count(finalizers, "finalizer"),
                        crate::plural::pick(finalizers, "holds", "hold")
                    )
                } else {
                    String::new()
                }
            ),
            hint: "If it stays Terminating, the node may be unreachable: Delete → force, or remove finalizers.".into(),
        });
    }
    let evicted = s(pod, "/status/reason") == "Evicted";
    if evicted {
        out.push(Finding {
            level: Level::Critical,
            title: "Evicted".into(),
            detail: s(pod, "/status/message").to_string(),
            hint: "The node ran short of a resource (often memory or disk). Check the node's pressure conditions and the pod's requests.".into(),
        });
    }
    // Held back on purpose: the scheduler leaves it alone until its gates are removed.
    let gates: Vec<&str> = pod
        .pointer("/spec/schedulingGates")
        .and_then(Value::as_array)
        .map_or_default(|g| g.iter().filter_map(|g| g["name"].as_str()).collect());
    if phase == "Pending" && s(pod, "/spec/nodeName").is_empty() && !gates.is_empty() {
        out.push(Finding {
            level: Level::Warning,
            title: "Waiting on scheduling gates".into(),
            detail: gates.join(", "),
            hint: "A controller (such as Kueue) removes the gates when the pod may run. If it waits long, check that controller.".into(),
        });
    } else if phase == "Pending" && s(pod, "/spec/nodeName").is_empty() {
        // Unscheduled.
        let cond = pod
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .and_then(|c| c.iter().find(|c| c["type"] == "PodScheduled" && c["status"] == "False"));
        let msg = events
            .iter()
            .find(|e| e.1 == "FailedScheduling")
            .map(|e| e.2.to_string())
            .or_else(|| cond.map(|c| s(c, "/message").to_string()))
            .unwrap_or_default();
        let hint = if msg.contains("Insufficient") {
            "No node has enough free capacity for its requests: lower the requests, or add nodes (check the cluster autoscaler)."
        } else if msg.contains("taint") {
            "Nodes have taints the pod does not tolerate: add a toleration, or check the node selector."
        } else if msg.contains("affinity") || msg.contains("selector") {
            "No node matches its node selector or affinity rules."
        } else if msg.contains("persistentvolumeclaim")
            || msg.contains("PersistentVolumeClaim")
            || msg.contains("volume")
        {
            "A volume cannot be bound or attached here: check the PVC and its StorageClass."
        } else {
            "The scheduler cannot place it yet."
        };
        out.push(Finding { level: Level::Critical, title: NOT_SCHEDULED.into(), detail: msg, hint: hint.into() });
    }
    for (c, init) in statuses(pod) {
        let name = s(c, "/name");
        let label = if init { format!("init container {name}") } else { format!("container {name}") };
        let restarts = c["restartCount"].as_i64().unwrap_or(0);
        let waiting = s(c, "/state/waiting/reason");
        let wmsg = s(c, "/state/waiting/message");
        let last = c.pointer("/lastState/terminated");
        let last_reason = last.map_or("", |t| s(t, "/reason"));
        let last_code = last.and_then(|t| t["exitCode"].as_i64());
        // Stopped with a failure now (a Job pod that does not restart, a failed init container).
        // An evicted pod's containers were killed by the eviction: "Evicted" says it all.
        let ended = c.pointer("/state/terminated");
        let end_code = ended.and_then(|t| t["exitCode"].as_i64()).unwrap_or(0);
        let end_reason = ended.map_or("", |t| s(t, "/reason"));
        let failed = ended.is_some()
            && (end_code != 0 || !matches!(end_reason, "" | "Completed"))
            && phase != "Succeeded"
            && !evicted;
        let memory_limit = || {
            pod.pointer(if init { "/spec/initContainers" } else { "/spec/containers" })
                .and_then(Value::as_array)
                .and_then(|cs| cs.iter().find(|x| x["name"] == name))
                .map_or_default(|x| s(x, "/resources/limits/memory").to_string())
        };
        match waiting {
            "ImagePullBackOff" | "ErrImagePull" | "InvalidImageName" => {
                let image = s(c, "/image");
                let hint = if wmsg.contains("not found") || wmsg.contains("manifest unknown") {
                    "The image or tag does not exist: check the name and tag."
                } else if wmsg.contains("unauthorized") || wmsg.contains("denied") || wmsg.contains("authentication") {
                    "The registry refused access: check imagePullSecrets and their credentials."
                } else {
                    "Check the image name, tag, registry access and imagePullSecrets."
                };
                out.push(Finding {
                    level: Level::Critical,
                    title: format!("Cannot pull the image of {label}"),
                    detail: format!("{image}: {wmsg}"),
                    hint: hint.into(),
                });
            }
            "CreateContainerConfigError" | "CreateContainerError" => {
                let hint = if wmsg.contains("secret") || wmsg.contains("configmap") {
                    "A Secret or ConfigMap (or one of its keys) it reads does not exist."
                } else {
                    "The container could not be created from its spec."
                };
                out.push(Finding {
                    level: Level::Critical,
                    title: format!("Cannot create {label}"),
                    detail: wmsg.to_string(),
                    hint: hint.into(),
                });
            }
            "CrashLoopBackOff" | "RunContainerError" => {
                let mut detail = format!("Restarted {}.", crate::plural::count(restarts, "time"));
                if let Some(code) = last_code {
                    detail.push_str(&format!(
                        " Last exit code {code}{}",
                        if last_reason.is_empty() { String::new() } else { format!(" ({last_reason})") }
                    ));
                    let m = exit_meaning(code);
                    if !m.is_empty() {
                        detail.push_str(&format!(": {m}"));
                    }
                    detail.push('.');
                }
                let hint = if last_reason == "OOMKilled" {
                    "It runs out of memory: raise its memory limit or fix the leak."
                } else if last_code == Some(127) || last_code == Some(126) {
                    "Check the command and args against what the image contains."
                } else {
                    "Read the crash output below, or its previous logs (Logs → Previous)."
                };
                out.push(Finding {
                    level: Level::Critical,
                    title: format!("{} is crash-looping", capital(&label)),
                    detail,
                    hint: hint.into(),
                });
            }
            _ => {
                if last_reason == "OOMKilled" && restarts > 0 && !failed {
                    let limit = memory_limit();
                    out.push(Finding {
                        level: Level::Warning,
                        title: format!("{} was killed for using too much memory", capital(&label)),
                        detail: format!(
                            "OOMKilled; restarted {}{}.",
                            crate::plural::count(restarts, "time"),
                            if limit.is_empty() { String::new() } else { format!(", memory limit {limit}") }
                        ),
                        hint: "Raise the memory limit, or look at the memory chart for a leak.".into(),
                    });
                }
            }
        }
        if failed {
            let mut detail = format!("Exit code {end_code}");
            if !matches!(end_reason, "" | "Error") {
                detail.push_str(&format!(" ({end_reason})"));
            }
            match exit_meaning(end_code) {
                "" => {}
                m => detail.push_str(&format!(": {m}")),
            }
            detail.push('.');
            let message = c.pointer("/state/terminated/message").and_then(Value::as_str).unwrap_or("").trim();
            if !message.is_empty() {
                detail.push_str(&format!(" {message}"));
            }
            let (title, hint) = match end_reason {
                "OOMKilled" => {
                    let limit = memory_limit();
                    if !limit.is_empty() {
                        detail.push_str(&format!(" Memory limit {limit}."));
                    }
                    ("was killed for using too much memory", "It ran out of memory: raise its memory limit.")
                }
                "ContainerCannotRun" | "StartError" => {
                    ("could not start", "Check the command and args against what the image contains.")
                }
                _ => ("exited with an error", "Read its logs."),
            };
            out.push(Finding {
                level: Level::Critical,
                title: format!("{} {title}", capital(&label)),
                detail,
                hint: hint.into(),
            });
        }
        // Running but not ready: probes.
        if !init && c.pointer("/state/running").is_some() && c["ready"] == false && phase == "Running" {
            let probe = events.iter().find(|e| e.1 == "Unhealthy").map_or_default(|e| e.2.to_string());
            out.push(Finding {
                level: Level::Warning,
                title: format!("{} is running but not ready", capital(&label)),
                detail: if probe.is_empty() { "Its readiness probe does not pass yet.".into() } else { probe },
                hint: "Check the readiness probe's path, port and timing against the application.".into(),
            });
        }
    }
    // Volumes and sandbox problems from events.
    for (reason, title, hint) in [
        (
            "FailedMount",
            "A volume cannot be mounted",
            "Check the referenced Secret/ConfigMap/PVC exists and the volume is not attached elsewhere.",
        ),
        (
            "FailedAttachVolume",
            "A volume cannot be attached",
            "The volume may still be attached to another node; check the PV and the CSI driver.",
        ),
        ("FailedCreatePodSandBox", "The pod sandbox cannot be created", "Usually a CNI (network) problem on the node."),
    ] {
        if let Some(e) = events.iter().find(|e| e.1 == reason)
            && !out.iter().any(|f| f.title == title)
        {
            out.push(Finding {
                level: Level::Warning,
                title: title.into(),
                detail: e.2.to_string(),
                hint: hint.into(),
            });
        }
    }
    if let Some((container, text)) = crash_logs
        && let Some(f) = out.iter_mut().find(|f| about_crash(f, container))
    {
        let tail: Vec<&str> = text.lines().rev().filter(|l| !l.trim().is_empty()).take(6).collect();
        if !tail.is_empty() {
            f.detail.push_str("\nLast output before the crash:\n");
            f.detail.push_str(&tail.into_iter().rev().collect::<Vec<_>>().join("\n"));
        }
    }
    out.sort_by_key(|f| f.level);
    out
}

/// Findings for a Deployment, `StatefulSet`, `DaemonSet` or Job: its own, where the failure
/// comes from (`cause`), then its worst pod's. `revisions` are its `ReplicaSets` or
/// `ControllerRevisions` (`k8s::rollouts::revisions`); `crash` is the last output of a pod's
/// crashed container, as (pod, (container, text)), shown when that pod is the worst.
pub fn workload(
    obj: &Value,
    pods: &[Value],
    revisions: &[Value],
    crash: Option<(&str, &(String, String))>,
) -> Vec<Finding> {
    let mut out = Vec::new();
    for c in obj.pointer("/status/conditions").and_then(Value::as_array).into_iter().flatten() {
        let (t, st, reason) = (s(c, "/type"), s(c, "/status"), s(c, "/reason"));
        if t == "Progressing" && reason == "ProgressDeadlineExceeded" {
            out.push(Finding {
                level: Level::Critical,
                title: "The rollout is stuck".into(),
                detail: s(c, "/message").to_string(),
                hint: "New pods do not become ready: see the pod findings below, or roll back from Revisions.".into(),
            });
        }
        if t == "ReplicaFailure" && st == "True" {
            out.push(Finding {
                level: Level::Critical,
                title: "Pods cannot be created".into(),
                detail: s(c, "/message").to_string(),
                hint: "Often a quota, a LimitRange or an admission webhook refusing them.".into(),
            });
        }
        if t == "Failed" && st == "True" {
            out.push(Finding {
                level: Level::Critical,
                title: "The job failed".into(),
                detail: format!("{reason}: {}", s(c, "/message")),
                hint: "Read the logs of its failed pods.".into(),
            });
        }
    }
    let want = obj
        .pointer("/spec/replicas")
        .and_then(Value::as_i64)
        .or_else(|| obj.pointer("/status/desiredNumberScheduled").and_then(Value::as_i64));
    let ready = obj
        .pointer("/status/readyReplicas")
        .and_then(Value::as_i64)
        .or_else(|| obj.pointer("/status/numberReady").and_then(Value::as_i64))
        .unwrap_or(0);
    if let Some(w) = want
        && ready < w
        && out.is_empty()
    {
        out.push(Finding {
            level: if ready == 0 { Level::Critical } else { Level::Warning },
            title: format!("{ready} of {w} replicas ready"),
            detail: String::new(),
            hint: "See why below.".into(),
        });
    }
    let judged = judged(pods);
    out.extend(cause(&judged, revisions));
    if let Some(i) = worst(&judged) {
        let (p, findings) = &judged[i];
        let name = s(p, "/metadata/name");
        // The crash output was read for the worst pod of an earlier turn: only its own.
        let findings = match crash {
            Some((pod_name, logs)) if pod_name == name => pod(p, &[], Some(logs)),
            _ => findings.clone(),
        };
        out.extend(findings.into_iter().map(|mut f| {
            f.title = format!("Pod {name}: {}", f.title);
            f
        }));
    }
    out
}

/// A workload's pods with their findings. Pods being deleted (old ones of a rollout) are left
/// out: they only say "Being deleted".
fn judged(pods: &[Value]) -> Vec<(&Value, Vec<Finding>)> {
    pods.iter().filter(|p| p.pointer("/metadata/deletionTimestamp").is_none()).map(|p| (p, pod(p, &[], None))).collect()
}

/// The pod with the worst problem, which explains most workload outages: the worst level,
/// then the most findings at that level.
fn worst(judged: &[(&Value, Vec<Finding>)]) -> Option<usize> {
    judged
        .iter()
        .enumerate()
        .filter(|(_, (_, f))| !f.is_empty())
        .min_by_key(|(_, (_, f))| (f[0].level, std::cmp::Reverse(f.iter().filter(|x| x.level == f[0].level).count())))
        .map(|(i, _)| i)
}

/// The pod a workload's diagnosis explains (`workload`), to read its crash output.
pub fn worst_pod(pods: &[Value]) -> Option<&Value> {
    let judged = judged(pods);
    worst(&judged).map(|i| judged[i].0)
}

/// The container whose output explains a pod's crash, and whether to read its previous run
/// (it restarted since) rather than its current one: the first whose crash the pod's findings
/// name. None when no finding would show the output, so no log read is wasted on a sidecar
/// that failed once long ago.
pub fn crashed_container(p: &Value) -> Option<(String, bool)> {
    let findings = pod(p, &[], None);
    statuses(p).find_map(|(c, _)| {
        let name = c["name"].as_str()?;
        let now = c.pointer("/state/terminated").is_some();
        let ran = now || c.pointer("/lastState/terminated").is_some();
        (ran && findings.iter().any(|f| about_crash(f, name))).then(|| (name.to_string(), !now))
    })
}

/// Whether a pod fails, rather than being slow: a crash, a failed exit, an image or create
/// error, an eviction, or a container back to not ready after a failed run (between two
/// crash-loop backoffs). A pod still starting, warming up behind its readiness probe or
/// waiting for room on a node is neither failing nor fine: every rollout and scale-up has
/// those, and a capacity shortage is not the revision's fault.
fn failing(p: &Value, findings: &[Finding]) -> bool {
    let restarted_after_error = || {
        !ready(p)
            && statuses(p).any(|(c, _)| {
                c["restartCount"].as_i64().unwrap_or(0) > 0
                    && c.pointer("/lastState/terminated/exitCode").and_then(Value::as_i64).unwrap_or(0) != 0
            })
    };
    findings.iter().any(|f| f.level == Level::Critical && f.title != NOT_SCHEDULED) || restarted_after_error()
}

/// Ready, or finished well (a Job's pod).
fn ready(pod: &Value) -> bool {
    s(pod, "/status/phase") == "Succeeded"
        || pod
            .pointer("/status/conditions")
            .and_then(Value::as_array)
            .is_some_and(|c| c.iter().any(|c| c["type"] == "Ready" && c["status"] == "True"))
}

/// The revision a pod belongs to, by its `pod-template-hash` label (a Deployment's
/// `ReplicaSet` is named `<deployment>-<hash>`) or `controller-revision-hash` (a
/// `StatefulSet`'s pods carry the `ControllerRevision`'s name, a `DaemonSet`'s only its hash).
fn revision_of<'a>(pod: &Value, revisions: &'a [Value]) -> Option<&'a Value> {
    let labels = pod.pointer("/metadata/labels")?;
    let key = labels.get("pod-template-hash").or_else(|| labels.get("controller-revision-hash"))?.as_str()?;
    if key.is_empty() {
        return None;
    }
    revisions.iter().find(|r| {
        let name = s(r, "/metadata/name");
        name == key || name.strip_suffix(key).is_some_and(|rest| rest.ends_with('-'))
    })
}

/// Where a workload's failure comes from, when its pods tell: one node (only pods there fail,
/// while pods of the same revision are ready elsewhere), the new revision (only its pods fail,
/// while the previous one's are ready), or the app (pods of old and new revisions fail, or
/// every pod fails, on several nodes). Nothing when they don't tell, such as one failing pod
/// and no other, or pods only starting (`failing`).
fn cause(judged: &[(&Value, Vec<Finding>)], revisions: &[Value]) -> Option<Finding> {
    use crate::k8s::rollouts::number;
    use crate::plural::{count, pick};
    use std::collections::BTreeSet;
    let (failing, others): (Vec<&(&Value, Vec<Finding>)>, Vec<_>) =
        judged.iter().partition(|(p, f)| self::failing(p, f));
    let failing: Vec<&Value> = failing.into_iter().map(|(p, _)| *p).collect();
    if failing.is_empty() {
        return None;
    }
    // Serving: ready, whatever warning it has (a restart for memory long ago).
    let fine: Vec<&Value> = others.iter().map(|(p, _)| *p).filter(|p| ready(p)).collect();
    fn node(p: &Value) -> &str {
        s(p, "/spec/nodeName")
    }
    // A pod's revision number; None when it is not known (a Job's pods have none).
    let rev = |p: &Value| revision_of(p, revisions).map(number).filter(|n| *n > 0);
    // An unscheduled pod is on no node: it says nothing about one.
    let nodes: BTreeSet<&str> = failing.iter().map(|p| node(p)).filter(|n| !n.is_empty()).collect();
    let failing_revs: BTreeSet<Option<i64>> = failing.iter().map(|p| rev(p)).collect();
    // The node: the same pods are ready on other nodes.
    if nodes.len() == 1
        && failing.iter().all(|p| !node(p).is_empty())
        && let Some(only) = nodes.first()
    {
        let elsewhere = fine.iter().filter(|p| node(p) != *only && failing_revs.contains(&rev(p))).count();
        if elsewhere > 0 {
            return Some(Finding {
                level: Level::Warning,
                title: format!("Only pods on node {only} fail"),
                detail: format!(
                    "{} on {only}; {} of the same revision {} ready on other nodes.",
                    count(failing.len(), "failing pod"),
                    count(elsewhere, "pod"),
                    pick(elsewhere, "is", "are"),
                ),
                hint: "Look at the node's page: its pressure conditions, taints and events. Cordon and drain it if it is broken.".into(),
            });
        }
    }
    let newest = revisions.iter().map(number).max().filter(|n| *n > 0);
    if let Some(newest) = newest
        && !failing_revs.contains(&None)
    {
        // The new revision: only its pods fail, while older ones still serve.
        let old: BTreeSet<i64> = fine.iter().filter_map(|p| rev(p)).filter(|n| *n < newest).collect();
        let old_ready = fine.iter().filter(|p| rev(p).is_some_and(|n| n < newest)).count();
        if failing_revs.len() == 1
            && failing_revs.contains(&Some(newest))
            && let Some(&previous) = old.last()
        {
            let of_old = if old.len() == 1 { format!("revision {previous}") } else { "earlier revisions".into() };
            let mut detail = format!(
                "{} {} of revision {newest}; {} of {of_old} {} ready.",
                count(failing.len(), "failing pod"),
                pick(failing.len(), "is", "are all"),
                count(old_ready, "pod"),
                pick(old_ready, "is", "are"),
            );
            let images =
                |n: i64| revisions.iter().find(|r| number(r) == n).map_or_default(crate::k8s::rollouts::images);
            let before = images(previous);
            let changed: Vec<String> = images(newest).into_iter().filter(|i| !before.contains(i)).collect();
            if !changed.is_empty() {
                detail.push_str(&format!(" Revision {newest} runs {}.", changed.join(", ")));
            }
            return Some(Finding {
                level: Level::Warning,
                title: format!("The new revision ({newest}) fails"),
                detail,
                hint: format!(
                    "Roll back to revision {previous} from Revisions, or fix what revision {newest} changed."
                ),
            });
        }
        // The app: pods of older revisions fail too, on several nodes.
        if failing_revs.len() > 1 && nodes.len() > 1 {
            let numbers: Vec<String> = failing_revs.iter().flatten().map(i64::to_string).collect();
            return Some(Finding {
                level: Level::Warning,
                title: "Old and new revisions fail".into(),
                detail: format!(
                    "{} of revisions {}, on {}.",
                    count(failing.len(), "failing pod"),
                    numbers.join(", "),
                    count(nodes.len(), "node"),
                ),
                hint: "Not the new revision nor one node: look at the app's configuration and what it depends on (a database, a Secret, another service).".into(),
            });
        }
    }
    // Every pod fails, on several nodes: not a node, and no ready pod to set a revision against.
    // A pod still starting may yet be fine: then it is not every pod.
    if others.is_empty() && nodes.len() > 1 {
        return Some(Finding {
            level: Level::Warning,
            title: format!("Every pod fails, on {}", count(nodes.len(), "node")),
            detail: String::new(),
            hint: "Not one node: the app, its configuration or something it depends on. If it began with the last rollout, roll back from Revisions.".into(),
        });
    }
    None
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_default(|f| f.to_uppercase().collect::<String>() + c.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn crash_loop_with_logs() {
        let p = json!({"status": {"phase": "Running", "containerStatuses": [{
            "name": "web", "restartCount": 7, "ready": false,
            "state": {"waiting": {"reason": "CrashLoopBackOff"}},
            "lastState": {"terminated": {"exitCode": 127, "reason": "Error"}}
        }]}});
        let logs = ("web".to_string(), "starting\nsh: foo: not found\n".to_string());
        let f = pod(&p, &[], Some(&logs));
        assert_eq!(f[0].title, "Container web is crash-looping");
        assert!(f[0].detail.contains("exit code 127"));
        assert!(f[0].detail.contains("not found in the image"));
        assert!(f[0].detail.contains("sh: foo: not found"));
        assert!(f[0].hint.contains("command"));
    }

    #[test]
    fn unscheduled_and_pull() {
        let p = json!({"status": {"phase": "Pending"}, "spec": {}});
        let ev = [("Warning", "FailedScheduling", "0/3 nodes are available: 3 Insufficient memory.", 4)];
        let f = pod(&p, &ev, None);
        assert_eq!(f[0].title, "Not scheduled");
        assert!(f[0].hint.contains("enough free capacity"));
        // Held by a scheduling gate (Kueue): waiting on purpose, not a scheduling failure.
        let p = json!({"status": {"phase": "Pending"}, "spec": {"schedulingGates": [{"name": "kueue.x-k8s.io/admission"}]}});
        let f = pod(&p, &[], None);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].level, Level::Warning);
        assert_eq!(f[0].title, "Waiting on scheduling gates");
        assert_eq!(f[0].detail, "kueue.x-k8s.io/admission");
        let p = json!({"spec": {"nodeName": "n1"}, "status": {"phase": "Pending", "containerStatuses": [{
            "name": "app", "image": "nginx:nope", "restartCount": 0,
            "state": {"waiting": {"reason": "ImagePullBackOff", "message": "manifest unknown"}}
        }]}});
        let f = pod(&p, &[], None);
        assert!(f[0].title.contains("Cannot pull"));
        assert!(f[0].hint.contains("does not exist"));
    }

    #[test]
    fn healthy_is_empty() {
        let p = json!({"spec": {"nodeName": "n"}, "status": {"phase": "Running", "containerStatuses": [{
            "name": "a", "ready": true, "restartCount": 0, "state": {"running": {}}
        }]}});
        assert!(pod(&p, &[], None).is_empty());
        let d = json!({"spec": {"replicas": 2}, "status": {"readyReplicas": 2}});
        assert!(workload(&d, &[p], &[], None).is_empty());
    }

    #[test]
    fn workload_picks_worst_pod() {
        let unready = json!({"metadata": {"name": "slow"}, "spec": {"nodeName": "n"}, "status": {"phase": "Running",
            "containerStatuses": [{"name": "a", "ready": false, "restartCount": 0, "state": {"running": {}}}]}});
        let deleting = json!({"metadata": {"name": "old", "deletionTimestamp": "2026-01-01T00:00:00Z"},
            "spec": {"nodeName": "n"}, "status": {"phase": "Running", "containerStatuses": [{
                "name": "a", "restartCount": 3, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}});
        let crashing = json!({"metadata": {"name": "crash"}, "spec": {"nodeName": "n"}, "status": {"phase": "Running",
            "containerStatuses": [{"name": "a", "restartCount": 5, "ready": false,
                "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}});
        let d = json!({"spec": {"replicas": 3}, "status": {"readyReplicas": 3}});
        let f = workload(&d, &[unready.clone(), deleting.clone(), crashing], &[], None);
        assert!(f[0].title.starts_with("Pod crash: "), "{f:?}");
        assert!(f.iter().all(|x| !x.title.starts_with("Pod old") && !x.title.starts_with("Pod slow")));
        // Only the pod being deleted has a problem: nothing from it.
        let f = workload(&d, &[deleting, unready], &[], None);
        assert!(f[0].title.starts_with("Pod slow: "), "{f:?}");
    }

    #[test]
    fn failed_containers_that_do_not_restart() {
        // A Job pod OOM-killed once, never restarted.
        let p = json!({"spec": {"nodeName": "n", "containers": [
            {"name": "job", "resources": {"limits": {"memory": "64Mi"}}}]},
            "status": {"phase": "Failed", "containerStatuses": [{
            "name": "job", "restartCount": 0, "ready": false,
            "state": {"terminated": {"exitCode": 137, "reason": "OOMKilled"}}}]}});
        let f = pod(&p, &[], None);
        assert_eq!(f.len(), 1, "{f:?}");
        assert_eq!(f[0].title, "Container job was killed for using too much memory");
        assert!(f[0].detail.contains("64Mi"));
        // A failed init container, and one that could not start.
        let p = json!({"spec": {"nodeName": "n"}, "status": {"phase": "Failed",
            "initContainerStatuses": [{"name": "migrate", "restartCount": 0,
                "state": {"terminated": {"exitCode": 1, "reason": "Error"}}}],
            "containerStatuses": [{"name": "app", "restartCount": 0,
                "state": {"terminated": {"exitCode": 128, "reason": "ContainerCannotRun", "message": "exec: no such file"}}}]}});
        let f = pod(&p, &[], None);
        let titles: Vec<&str> = f.iter().map(|x| x.title.as_str()).collect();
        assert_eq!(titles, ["Init container migrate exited with an error", "Container app could not start"]);
        assert!(f[1].detail.contains("no such file"));
        // Finished normally: nothing.
        let p = json!({"spec": {"nodeName": "n"}, "status": {"phase": "Succeeded", "containerStatuses": [{
            "name": "job", "restartCount": 0, "state": {"terminated": {"exitCode": 0, "reason": "Completed"}}}]}});
        assert!(pod(&p, &[], None).is_empty());
    }

    #[test]
    fn evicted_pod_is_not_a_failed_container() {
        let p = json!({"spec": {"nodeName": "n"}, "status": {"phase": "Failed", "reason": "Evicted",
            "message": "The node was low on resource: memory.",
            "containerStatuses": [{"name": "app", "restartCount": 0,
                "state": {"terminated": {"exitCode": 137, "reason": "ContainerStatusUnknown"}}}]}});
        let f = pod(&p, &[], None);
        let titles: Vec<&str> = f.iter().map(|x| x.title.as_str()).collect();
        assert_eq!(titles, ["Evicted"]);
    }

    /// A Deployment's pod of the `ReplicaSet` `web-<hash>` on `node`: crash-looping, running
    /// again between two crash-loop backoffs (`rerun`), ready, warming up behind its readiness
    /// probe, or starting (Pending; not scheduled when `node` is empty).
    fn wpod(name: &str, node: &str, hash: &str, state: &str) -> Value {
        let (phase, ready, status) = match state {
            "crash" => (
                "Running",
                "False",
                json!({"name": "app", "restartCount": 4, "ready": false,
                    "state": {"waiting": {"reason": "CrashLoopBackOff"}},
                    "lastState": {"terminated": {"exitCode": 1, "reason": "Error"}}}),
            ),
            "rerun" => (
                "Running",
                "False",
                json!({"name": "app", "restartCount": 4, "ready": false, "state": {"running": {}},
                    "lastState": {"terminated": {"exitCode": 1, "reason": "Error"}}}),
            ),
            "ready" => {
                ("Running", "True", json!({"name": "app", "restartCount": 0, "ready": true, "state": {"running": {}}}))
            }
            "warming" => (
                "Running",
                "False",
                json!({"name": "app", "restartCount": 0, "ready": false, "state": {"running": {}}}),
            ),
            _ => (
                "Pending",
                "False",
                json!({"name": "app", "restartCount": 0, "ready": false,
                    "state": {"waiting": {"reason": "ContainerCreating"}}}),
            ),
        };
        json!({"metadata": {"name": name, "labels": {"app": "web", "pod-template-hash": hash}},
            "spec": {"nodeName": node},
            "status": {"phase": phase, "conditions": [{"type": "Ready", "status": ready}],
                "containerStatuses": [status]}})
    }

    fn rs(hash: &str, n: i64, image: &str) -> Value {
        json!({"metadata": {"name": format!("web-{hash}"),
                "annotations": {"deployment.kubernetes.io/revision": n.to_string()}},
            "spec": {"template": {"spec": {"containers": [{"name": "app", "image": image}]}}}})
    }

    fn revs() -> Vec<Value> {
        vec![rs("aaa", 11, "app:1.4"), rs("bbb", 12, "app:1.5")]
    }

    fn cause_of(pods: &[Value], revisions: &[Value]) -> Option<Finding> {
        cause(&judged(pods), revisions)
    }

    #[test]
    fn the_new_revision_fails_while_the_old_one_is_ready() {
        let pods = [
            wpod("web-aaa-1", "n1", "aaa", "ready"),
            wpod("web-aaa-2", "n2", "aaa", "ready"),
            wpod("web-bbb-1", "n1", "bbb", "crash"),
            wpod("web-bbb-2", "n2", "bbb", "crash"),
        ];
        let f = cause_of(&pods, &revs()).expect("a cause");
        assert_eq!(f.title, "The new revision (12) fails");
        assert_eq!(
            f.detail,
            "2 failing pods are all of revision 12; 2 pods of revision 11 are ready. Revision 12 runs app=app:1.5."
        );
        assert!(f.hint.contains("Roll back to revision 11"), "{f:?}");
        // Shown before the worst pod's findings, after the workload's own.
        let d = json!({"spec": {"replicas": 2}, "status": {"readyReplicas": 0}});
        let all = workload(&d, &pods, &revs(), None);
        let titles: Vec<&str> = all.iter().map(|x| x.title.as_str()).collect();
        assert_eq!(
            titles[..3],
            ["0 of 2 replicas ready", "The new revision (12) fails", "Pod web-bbb-1: Container app is crash-looping"]
        );
    }

    #[test]
    fn only_the_pods_on_one_node_fail() {
        let pods = [
            wpod("web-bbb-1", "n1", "bbb", "crash"),
            wpod("web-bbb-2", "n1", "bbb", "crash"),
            wpod("web-bbb-3", "n2", "bbb", "ready"),
        ];
        let f = cause_of(&pods, &revs()).expect("a cause");
        assert_eq!(f.title, "Only pods on node n1 fail");
        assert_eq!(f.detail, "2 failing pods on n1; 1 pod of the same revision is ready on other nodes.");
        // A Job's pods have no revision: a pod that finished on another node counts.
        let mut done = wpod("job-2", "n2", "", "ready");
        done["status"] = json!({"phase": "Succeeded", "containerStatuses": [{"name": "app", "restartCount": 0,
            "state": {"terminated": {"exitCode": 0, "reason": "Completed"}}}]});
        let f = cause_of(&[wpod("job-1", "n1", "", "crash"), done], &[]).expect("a cause");
        assert_eq!(f.title, "Only pods on node n1 fail");
        // Ready pods elsewhere are only of the old revision: that is the new revision, not the node.
        let pods = [wpod("web-bbb-1", "n1", "bbb", "crash"), wpod("web-aaa-1", "n2", "aaa", "ready")];
        assert_eq!(cause_of(&pods, &revs()).expect("a cause").title, "The new revision (12) fails");
    }

    #[test]
    fn every_revision_or_every_pod_fails() {
        let pods = [wpod("web-aaa-1", "n1", "aaa", "crash"), wpod("web-bbb-1", "n2", "bbb", "crash")];
        let f = cause_of(&pods, &revs()).expect("a cause");
        assert_eq!(f.title, "Old and new revisions fail");
        assert_eq!(f.detail, "2 failing pods of revisions 11, 12, on 2 nodes.");
        let pods = [wpod("web-bbb-1", "n1", "bbb", "crash"), wpod("web-bbb-2", "n2", "bbb", "crash")];
        assert_eq!(cause_of(&pods, &revs()).expect("a cause").title, "Every pod fails, on 2 nodes");
    }

    #[test]
    fn no_cause_when_the_pods_do_not_tell() {
        // One failing pod and nothing to set it against; pods still starting say nothing.
        let pods = [wpod("web-bbb-1", "n1", "bbb", "crash"), wpod("web-bbb-2", "n2", "bbb", "starting")];
        assert!(cause_of(&pods, &revs()).is_none());
        // Healthy: nothing.
        let pods = [wpod("web-bbb-1", "n1", "bbb", "ready"), wpod("web-bbb-2", "n2", "bbb", "ready")];
        assert!(cause_of(&pods, &revs()).is_none());
    }

    #[test]
    fn pods_starting_or_waiting_for_room_are_not_failing() {
        // A rolling update: the new revision's pod warms up while the old one's serve.
        let pods = [
            wpod("web-aaa-1", "n1", "aaa", "ready"),
            wpod("web-aaa-2", "n2", "aaa", "ready"),
            wpod("web-bbb-1", "n1", "bbb", "warming"),
        ];
        assert!(cause_of(&pods, &revs()).is_none());
        // A scale-up: a new pod warms up on n1 while the others are ready on n2.
        let pods = [wpod("web-bbb-1", "n2", "bbb", "ready"), wpod("web-bbb-2", "n1", "bbb", "warming")];
        assert!(cause_of(&pods, &revs()).is_none());
        // A new Deployment whose pods all warm up.
        let pods = [
            wpod("web-bbb-1", "n1", "bbb", "warming"),
            wpod("web-bbb-2", "n2", "bbb", "warming"),
            wpod("web-bbb-3", "n3", "bbb", "warming"),
        ];
        assert!(cause_of(&pods, &revs()).is_none());
        // A surge with no room for the new pod: a capacity shortage, not the new revision.
        let pods = [wpod("web-aaa-1", "n1", "aaa", "ready"), wpod("web-bbb-1", "", "bbb", "starting")];
        assert!(cause_of(&pods, &revs()).is_none());
        // Two crash, a third is still starting: not every pod.
        let pods = [
            wpod("web-bbb-1", "n1", "bbb", "crash"),
            wpod("web-bbb-2", "n2", "bbb", "crash"),
            wpod("web-bbb-3", "n3", "bbb", "warming"),
        ];
        assert!(cause_of(&pods, &revs()).is_none());
    }

    #[test]
    fn a_crash_loop_between_two_backoffs_still_fails() {
        let pods = [
            wpod("web-aaa-1", "n1", "aaa", "ready"),
            wpod("web-bbb-1", "n1", "bbb", "rerun"),
            wpod("web-bbb-2", "n2", "bbb", "crash"),
        ];
        assert_eq!(cause_of(&pods, &revs()).expect("a cause").title, "The new revision (12) fails");
    }

    #[test]
    fn the_crash_output_read_is_the_container_that_crashes() {
        // A sidecar that failed once long ago, then the app crash-looping: the app's output.
        let p = json!({"metadata": {"name": "web-1"}, "status": {"phase": "Running", "containerStatuses": [
            {"name": "proxy", "restartCount": 1, "ready": true, "state": {"running": {}},
                "lastState": {"terminated": {"exitCode": 2, "reason": "Error"}}},
            {"name": "app", "restartCount": 4, "ready": false,
                "state": {"waiting": {"reason": "CrashLoopBackOff"}},
                "lastState": {"terminated": {"exitCode": 1, "reason": "Error"}}}]}});
        assert_eq!(crashed_container(&p), Some(("app".to_string(), true)));
        // Only the sidecar's old failure, nothing shown for it: no read.
        let p = json!({"metadata": {"name": "web-1"}, "status": {"phase": "Running", "containerStatuses": [
            {"name": "proxy", "restartCount": 1, "ready": true, "state": {"running": {}},
                "lastState": {"terminated": {"exitCode": 2, "reason": "Error"}}}]}});
        assert_eq!(crashed_container(&p), None);
        // A Job's container that exited with an error: its current run.
        let p = json!({"metadata": {"name": "job-1"}, "status": {"phase": "Failed", "containerStatuses": [
            {"name": "task", "restartCount": 0, "ready": false,
                "state": {"terminated": {"exitCode": 1, "reason": "Error"}}}]}});
        assert_eq!(crashed_container(&p), Some(("task".to_string(), false)));
        // A warming pod is the worst of a rollout with nothing else wrong: no read either.
        let pods = [wpod("web-aaa-1", "n1", "aaa", "ready"), wpod("web-bbb-1", "n1", "bbb", "warming")];
        assert_eq!(worst_pod(&pods).and_then(crashed_container), None);
    }

    #[test]
    fn pods_find_their_revision_whatever_the_workload() {
        // A StatefulSet's pods carry the ControllerRevision's name, a DaemonSet's its hash.
        let cr = |name: &str| json!({"metadata": {"name": name}});
        let revisions = [cr("db-5d8f"), cr("agent-6c7b")];
        let labelled = |l: Value| json!({"metadata": {"labels": l}});
        let of = |l: Value| revision_of(&labelled(l), &revisions).map(|r| s(r, "/metadata/name").to_string());
        assert_eq!(of(json!({"controller-revision-hash": "db-5d8f"})).as_deref(), Some("db-5d8f"));
        assert_eq!(of(json!({"controller-revision-hash": "6c7b"})).as_deref(), Some("agent-6c7b"));
        // A hash that only ends a name, without the dash, is another revision.
        assert_eq!(of(json!({"controller-revision-hash": "c7b"})), None);
        assert_eq!(of(json!({"controller-revision-hash": ""})), None);
    }

    #[test]
    fn the_crash_output_is_the_worst_pod_s() {
        let pods = [wpod("web-bbb-1", "n1", "bbb", "crash"), wpod("web-bbb-2", "n2", "bbb", "starting")];
        assert_eq!(worst_pod(&pods).map(|p| s(p, "/metadata/name")), Some("web-bbb-1"));
        assert_eq!(crashed_container(&pods[0]), Some(("app".to_string(), true)));
        assert_eq!(crashed_container(&pods[1]), None);
        let d = json!({"spec": {"replicas": 2}, "status": {"readyReplicas": 0}});
        let logs = ("app".to_string(), "boot\npanic: no DATABASE_URL\n".to_string());
        let f = workload(&d, &pods, &revs(), Some(("web-bbb-1", &logs)));
        assert!(
            f.iter().any(|x| x.title.starts_with("Pod web-bbb-1") && x.detail.contains("panic: no DATABASE_URL")),
            "{f:?}"
        );
        // Read for a pod that is no longer the worst: not shown under another one.
        let f = workload(&d, &pods, &revs(), Some(("web-bbb-9", &logs)));
        assert!(f.iter().all(|x| !x.detail.contains("DATABASE_URL")), "{f:?}");
    }

    use proptest::prelude::*;

    proptest! {
        /// Whatever the pods, a cause is only named with its evidence: a node when every failing
        /// pod runs there and one of the same revision is ready elsewhere, the new revision when
        /// only its pods fail and an older one's is ready, old and new when both fail, and every
        /// pod when every pod fails. Pods only warming up, starting or waiting for room never
        /// make a cause.
        #[test]
        fn a_cause_is_named_only_with_its_evidence(
            spec in prop::collection::vec((0..4usize, 0..2usize, 0..5usize), 0..8)
        ) {
            let nodes = ["n1", "n2", "n3", ""];
            let hashes = ["aaa", "bbb"];
            let states = ["crash", "ready", "starting", "warming", "rerun"];
            let pods: Vec<Value> = spec
                .iter()
                .enumerate()
                .map(|(i, (n, h, st))| wpod(&format!("web-{i}"), nodes[*n], hashes[*h], states[*st]))
                .collect();
            // Only a crash fails: a pod warming up, starting or not scheduled does not.
            let failing: Vec<&(usize, usize, usize)> = spec.iter().filter(|p| p.2 == 0 || p.2 == 4).collect();
            let ready: Vec<&(usize, usize, usize)> = spec.iter().filter(|p| p.2 == 1).collect();
            let Some(f) = cause_of(&pods, &revs()) else { return Ok(()) };
            prop_assert!(!failing.is_empty());
            if let Some(node) = f.title.strip_prefix("Only pods on node ").and_then(|t| t.strip_suffix(" fail")) {
                prop_assert!(failing.iter().all(|p| nodes[p.0] == node));
                prop_assert!(ready.iter().any(|p| nodes[p.0] != node && failing.iter().any(|q| q.1 == p.1)));
            } else if f.title == "The new revision (12) fails" {
                prop_assert!(failing.iter().all(|p| p.1 == 1));
                prop_assert!(ready.iter().any(|p| p.1 == 0));
            } else if f.title == "Old and new revisions fail" {
                prop_assert!(failing.iter().any(|p| p.1 == 0) && failing.iter().any(|p| p.1 == 1));
            } else {
                prop_assert!(f.title.starts_with("Every pod fails"), "{f:?}");
                prop_assert_eq!(failing.len(), spec.len());
            }
        }
    }
}
