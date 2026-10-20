//! "Why is this broken?": plain-language findings for a pod or workload, from its
//! status, events and the last lines its crashed containers wrote.

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

fn s<'a>(v: &'a Value, p: &str) -> &'a str {
    v.pointer(p).and_then(Value::as_str).unwrap_or("")
}

fn statuses(pod: &Value) -> impl Iterator<Item = (&Value, bool)> {
    let get = |p: &str| pod.pointer(p).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
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
                if finalizers > 0 { format!("; {finalizers} finalizer(s) hold it") } else { String::new() }
            ),
            hint: "If it stays Terminating, the node may be unreachable: Delete → force, or remove finalizers.".into(),
        });
    }
    if s(pod, "/status/reason") == "Evicted" {
        out.push(Finding {
            level: Level::Critical,
            title: "Evicted".into(),
            detail: s(pod, "/status/message").to_string(),
            hint: "The node ran short of a resource (often memory or disk). Check the node's pressure conditions and the pod's requests.".into(),
        });
    }
    // Unscheduled.
    if phase == "Pending" && s(pod, "/spec/nodeName").is_empty() {
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
        out.push(Finding { level: Level::Critical, title: "Not scheduled".into(), detail: msg, hint: hint.into() });
    }
    for (c, init) in statuses(pod) {
        let name = s(c, "/name");
        let label = if init { format!("init container {name}") } else { format!("container {name}") };
        let restarts = c["restartCount"].as_i64().unwrap_or(0);
        let waiting = s(c, "/state/waiting/reason");
        let wmsg = s(c, "/state/waiting/message");
        let last = c.pointer("/lastState/terminated");
        let last_reason = last.map(|t| s(t, "/reason")).unwrap_or("");
        let last_code = last.and_then(|t| t["exitCode"].as_i64());
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
                let mut detail = format!("Restarted {restarts} times.");
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
                if last_reason == "OOMKilled" && restarts > 0 {
                    let limit = pod
                        .pointer("/spec/containers")
                        .and_then(Value::as_array)
                        .and_then(|cs| cs.iter().find(|x| x["name"] == name))
                        .map(|x| s(x, "/resources/limits/memory").to_string())
                        .unwrap_or_default();
                    out.push(Finding {
                        level: Level::Warning,
                        title: format!("{} was killed for using too much memory", capital(&label)),
                        detail: format!(
                            "OOMKilled; restarted {restarts} times{}.",
                            if limit.is_empty() { String::new() } else { format!(", memory limit {limit}") }
                        ),
                        hint: "Raise the memory limit, or look at the memory chart for a leak.".into(),
                    });
                }
            }
        }
        if s(c, "/state/terminated/reason") == "Error" && !init && phase != "Succeeded" {
            let code = c.pointer("/state/terminated/exitCode").and_then(Value::as_i64).unwrap_or(0);
            out.push(Finding {
                level: Level::Critical,
                title: format!("{} exited with an error", capital(&label)),
                detail: format!(
                    "Exit code {code}{}",
                    match exit_meaning(code) {
                        "" => String::new(),
                        m => format!(": {m}"),
                    }
                ),
                hint: "Read its logs.".into(),
            });
        }
        // Running but not ready: probes.
        if !init && c.pointer("/state/running").is_some() && c["ready"] == false && phase == "Running" {
            let probe = events.iter().find(|e| e.1 == "Unhealthy").map(|e| e.2.to_string()).unwrap_or_default();
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
        && let Some(f) = out
            .iter_mut()
            .find(|f| f.title.to_lowercase().contains(&format!("container {container} ")) && f.level == Level::Critical)
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

/// Findings for a Deployment, StatefulSet, DaemonSet or Job, plus its worst pod's.
pub fn workload(obj: &Value, pods: &[Value]) -> Vec<Finding> {
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
    // The pod with the worst problem explains most workload outages. Pods being
    // deleted (old ones of a rollout) are left out: they only say "Being deleted".
    if let Some((name, findings)) = pods
        .iter()
        .filter(|p| p.pointer("/metadata/deletionTimestamp").is_none())
        .map(|p| (s(p, "/metadata/name"), pod(p, &[], None)))
        .filter(|(_, f)| !f.is_empty())
        .min_by_key(|(_, f)| (f[0].level, std::cmp::Reverse(f.iter().filter(|x| x.level == f[0].level).count())))
    {
        out.extend(findings.into_iter().map(|mut f| {
            f.title = format!("Pod {name}: {}", f.title);
            f
        }));
    }
    out
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
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
        assert!(workload(&d, &[p]).is_empty());
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
        let f = workload(&d, &[unready.clone(), deleting.clone(), crashing]);
        assert!(f[0].title.starts_with("Pod crash: "), "{f:?}");
        assert!(f.iter().all(|x| !x.title.starts_with("Pod old") && !x.title.starts_with("Pod slow")));
        // Only the pod being deleted has a problem: nothing from it.
        let f = workload(&d, &[deleting, unready]);
        assert!(f[0].title.starts_with("Pod slow: "), "{f:?}");
    }
}
