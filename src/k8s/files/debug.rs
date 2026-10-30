//! Reading a container without a shell or the tools to list files (distroless, scratch)
//! through an ephemeral debug container, like `kubectl debug --target`: the debug container
//! shares the container's processes, and sees its filesystem at `/proc/<pid>/root` of its
//! main process. Live, mounts included, as exec in the container would.
//!
//! Entering `/proc/<pid>/root` takes the same user and group as the process, or
//! `CAP_SYS_PTRACE`. The debug container gets both where Pod Security allows it
//! (`kubectl debug`'s `general` profile), else runs as the container's user only.

use anyhow::{Result, anyhow};
use k8s_openapi::api::core::v1::Pod;
use kube::{Api, Client};
use serde_json::{Value, json};

use super::FileSource;

/// The start of the names of the debug containers the Files tab adds: browsing again reuses
/// one that still runs.
pub const PREFIX: &str = "recouvrance-files-";

/// How long a debug container stays up: it sleeps, then ends. It can't be removed from the
/// pod, but it doesn't run on.
pub const SLEEP_SECS: u64 = 4 * 3600;

/// The user a debug container runs as where it may not be root and the container's user
/// isn't known: `nobody`, as the Debug action's under `restricted`.
const NOBODY: i64 = 65534;

/// The image debug containers run: the cluster's node shell image (Settings → Contexts).
pub fn image(context: &str) -> String {
    crate::settings::cluster(context).node_shell_image.unwrap_or_else(|| "busybox:1.37".into())
}

/// Whether `e`, from reading a container by exec, says it has no shell, or lacks a tool the
/// scripts run (`stat`, `xargs`, `head`…): a debug container brings them. A path that isn't
/// there, or a refusal, is not that.
pub fn needed(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}").to_ascii_lowercase();
    text.contains("no shell in this image")
        || text.contains(": not found")
        || text.contains("command not found")
        || (text.contains("xargs: ") && text.contains("no such file or directory"))
}

/// Whether `e`, from reading through debug container `source`, says that container is gone
/// (it slept its time, or the container it shares processes with restarted, which ends it):
/// another one is needed. An error about a file doesn't name the debug container.
pub fn ended(source: &FileSource, e: &anyhow::Error) -> bool {
    source.origin() == super::Origin::Debug && source.container().is_some_and(|c| format!("{e:#}").contains(c))
}

/// The debug container to add to read container `target`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub target: String,
    pub name: String,
    pub image: String,
    /// The user and group the debug container runs as: the container's, when known.
    pub user: Option<i64>,
    pub group: Option<i64>,
}

impl Plan {
    /// For container `target` of `pod` as read.
    pub fn new(pod: &Pod, target: &str, image: String) -> Self {
        let (user, group) = identity(pod, target);
        // A pod that says non-root refuses to start a container that would run as root.
        let non_root = pod.spec.as_ref().and_then(|s| s.security_context.as_ref()?.run_as_non_root) == Some(true);
        let user = user.or(non_root.then_some(NOBODY));
        // A group not said: the user's number, as most images have it (nonroot is 65532:65532).
        let group = group.or(user);
        let name = format!("{PREFIX}{}", &uuid::Uuid::new_v4().simple().to_string()[..5]);
        Self { target: target.to_string(), name, image, user, group }
    }

    /// The ephemeral container: it shares `target`'s processes and sleeps.
    fn container(&self) -> Value {
        json!({
            "name": self.name, "image": self.image, "targetContainerName": self.target,
            "command": ["sleep", SLEEP_SECS.to_string()], "terminationMessagePolicy": "File",
            "securityContext": self.security(None),
        })
    }

    /// Its `securityContext`: `kubectl debug --profile=general` (`SYS_PTRACE`) by default,
    /// none added under Pod Security `baseline`, `--profile=restricted` under `restricted`.
    /// Run as the container's user in all three.
    pub fn security(&self, level: Option<&str>) -> Value {
        let (mut sc, user, group) = match level {
            None => (json!({"capabilities": {"add": ["SYS_PTRACE"]}}), self.user, self.group),
            Some("baseline") => (json!({}), self.user, self.group),
            Some(_) => {
                let sc = json!({
                    "allowPrivilegeEscalation": false, "capabilities": {"drop": ["ALL"]},
                    "runAsNonRoot": true, "seccompProfile": {"type": "RuntimeDefault"},
                });
                let user = self.user.filter(|u| *u != 0).unwrap_or(NOBODY);
                (sc, Some(user), Some(self.group.filter(|g| *g != 0).unwrap_or(user)))
            }
        };
        if let Some(user) = user {
            sc["runAsUser"] = user.into();
        }
        if let Some(group) = group {
            sc["runAsGroup"] = group.into();
        }
        sc
    }
}

/// The user and group `target`'s process runs as: as the kubelet reports it (Kubernetes
/// 1.33 and later), else as the container's or the pod's `securityContext` says. None where
/// only the image says.
fn identity(pod: &Pod, target: &str) -> (Option<i64>, Option<i64>) {
    let status = pod.status.as_ref().and_then(|s| s.container_statuses.as_ref()?.iter().find(|c| c.name == target));
    if let Some(linux) = status.and_then(|c| c.user.as_ref()?.linux.as_ref()) {
        return (Some(linux.uid), Some(linux.gid));
    }
    let spec = pod.spec.as_ref();
    let own = spec.and_then(|s| s.containers.iter().find(|c| c.name == target)?.security_context.as_ref());
    let pods = spec.and_then(|s| s.security_context.as_ref());
    let user = own.and_then(|c| c.run_as_user).or_else(|| pods.and_then(|p| p.run_as_user));
    let group = own.and_then(|c| c.run_as_group).or_else(|| pods.and_then(|p| p.run_as_group));
    (user, group)
}

/// The runtime's ID of `target`'s running container (`containerd://<id>` gives `<id>`).
fn container_id(pod: &Pod, target: &str) -> Option<String> {
    let status = pod.status.as_ref()?.container_statuses.as_ref()?.iter().find(|c| c.name == target)?;
    status.state.as_ref()?.running.as_ref()?;
    let id = status.container_id.as_deref()?;
    let id = id.split_once("://").map_or(id, |(_, id)| id);
    (!id.is_empty()).then(|| id.to_string())
}

/// The debug containers this browser added to read `target` that still run, newest first.
fn running(pod: &Pod, target: &str) -> Vec<String> {
    let statuses = pod.status.as_ref().and_then(|s| s.ephemeral_container_statuses.as_ref());
    let runs = |name: &str| {
        statuses.into_iter().flatten().any(|c| c.name == name && c.state.as_ref().is_some_and(|s| s.running.is_some()))
    };
    let spec = pod.spec.as_ref().and_then(|s| s.ephemeral_containers.as_ref());
    let ours =
        spec.into_iter().flatten().rev().filter(|c| {
            c.name.starts_with(PREFIX) && c.target_container_name.as_deref() == Some(target) && runs(&c.name)
        });
    ours.map(|c| c.name.clone()).collect()
}

/// What reading `target` through a debug container finds.
pub enum Look {
    /// One this browser added before still runs: read through it.
    Reuse(FileSource),
    /// None: the one to add.
    Add(Plan),
}

/// Look for a debug container this browser added to read `target` (the pod's default
/// container when empty) that still runs and still sees its process; else plan one, of
/// `image`.
pub async fn look(client: Client, ns: String, pod: String, target: String, image: String) -> Result<Look> {
    crate::k8s::run(async move {
        let read = Api::<Pod>::namespaced(client.clone(), &ns).get(&pod).await?;
        let target = if target.is_empty() {
            crate::k8s::typed_pod_containers(&read).into_iter().next().unwrap_or_default()
        } else {
            target
        };
        for debug in running(&read, &target) {
            // One that no longer sees the container (restarted since) is passed over.
            if let Ok(source) = through(&client, &ns, &pod, &read, &debug, &target).await {
                return Ok(Look::Reuse(source));
            }
        }
        Ok(Look::Add(Plan::new(&read, &target, image)))
    })
    .await
}

/// Add the debug container `plan` says, wait until it runs, and read through it.
pub async fn add(client: Client, ns: String, pod: String, plan: Plan) -> Result<FileSource> {
    let secure = plan.clone();
    let debug = crate::k8s::add_ephemeral(
        client.clone(),
        ns.clone(),
        pod.clone(),
        plan.container(),
        move |level| secure.security(Some(level)),
        false,
    )
    .await?;
    crate::k8s::run(async move {
        let read = Api::<Pod>::namespaced(client.clone(), &ns).get(&pod).await?;
        through(&client, &ns, &pod, &read, &debug, &plan.target).await
    })
    .await
}

/// Read `target` through debug container `debug`: find the target's main process among those
/// it sees, and check it may enter its root.
async fn through(client: &Client, ns: &str, pod: &str, read: &Pod, debug: &str, target: &str) -> Result<FileSource> {
    let id = container_id(read, target).ok_or_else(|| anyhow!("container {target} isn't running"))?;
    let shared = read.spec.as_ref().and_then(|s| s.share_process_namespace) == Some(true);
    let exec = FileSource::exec(client.clone(), ns.to_string(), pod.to_string(), Some(debug.to_string()));
    let args = vec![id, if shared { "0" } else { "1" }.to_string()];
    let out = exec.script(FIND_PID.to_string(), args, 4096, 30).await?;
    let pid = String::from_utf8_lossy(&out).trim().parse::<u32>();
    let pid = pid.map_err(|_| anyhow!("the debug container did not say which process is {target}'s"))?;
    Ok(FileSource::debug(client.clone(), ns.to_string(), pod.to_string(), debug.to_string(), pid))
}

/// Prints the lowest PID whose cgroup names container ID `$1`: the container's main process.
/// When it shares no processes with the pod's other containers (`$2` is 1), its first process
/// is PID 1. Then checks it may enter its root, and says why not.
const FIND_PID: &str = r#"best=
for d in /proc/[0-9]*; do
n=${d#/proc/}
case $(cat "$d/cgroup" 2>/dev/null) in
*"$1"*) if [ -z "$best" ] || [ "$n" -lt "$best" ]; then best=$n; fi ;;
esac
done
[ -n "$best" ] || [ "$2" != 1 ] || best=1
[ -n "$best" ] || { echo "the debug container sees no process of the container" >&2; exit 1; }
if ! cd "/proc/$best/root" 2>/dev/null; then
ids() { sed -n "s/^$1:[[:space:]]*\([0-9]*\).*/\1/p" "/proc/$best/status"; }
echo "the debug container (user $(id -u), group $(id -g)) may not read the files of the container's process (user $(ids Uid), group $(ids Gid)): it needs the same user and group, or CAP_SYS_PTRACE, which Pod Security may refuse" >&2
exit 1
fi
echo "$best"
"#;

#[cfg(test)]
mod tests {
    use super::{NOBODY, PREFIX, Plan, container_id, running};
    use k8s_openapi::api::core::v1::Pod;
    use serde_json::json;

    fn pod(value: serde_json::Value) -> Pod {
        serde_json::from_value(value).unwrap()
    }

    fn plan(value: serde_json::Value) -> Plan {
        Plan::new(&pod(value), "app", "busybox:1.37".into())
    }

    /// The user comes from what the kubelet reports, else the container's securityContext,
    /// else the pod's; a group not said is the user's number.
    #[test]
    fn the_debug_container_runs_as_the_container_s_user() {
        let spec = |pod_sc: serde_json::Value, own: serde_json::Value| json!({"spec": {"securityContext": pod_sc, "containers": [{"name": "app", "securityContext": own}]}});
        let p = plan(spec(json!({"runAsUser": 1000, "runAsGroup": 2000}), json!({"runAsUser": 65532})));
        assert_eq!((p.user, p.group), (Some(65532), Some(2000)));
        let p = plan(spec(json!({}), json!({"runAsUser": 65532})));
        assert_eq!((p.user, p.group), (Some(65532), Some(65532)));
        let mut reported = spec(json!({"runAsUser": 1000}), json!({}));
        reported["status"] = json!({"containerStatuses": [{
            "name": "app", "image": "x", "imageID": "", "ready": true, "restartCount": 0,
            "user": {"linux": {"uid": 101, "gid": 102}},
        }]});
        let p = plan(reported);
        assert_eq!((p.user, p.group), (Some(101), Some(102)));
        // Only the image says: its user, unless the pod requires non-root.
        assert_eq!(plan(spec(json!({}), json!({}))).user, None);
        assert_eq!(plan(spec(json!({"runAsNonRoot": true}), json!({}))).user, Some(NOBODY));
        assert!(p.name.starts_with(PREFIX));
    }

    /// `general` adds SYS_PTRACE, `baseline` nothing, `restricted` what it requires, never
    /// as root; all as the container's user.
    #[test]
    fn profiles_follow_pod_security() {
        let p = plan(json!({"spec": {"containers": [{"name": "app", "securityContext": {"runAsUser": 1000}}]}}));
        let general = p.security(None);
        assert_eq!(general["capabilities"]["add"], json!(["SYS_PTRACE"]));
        assert_eq!((general["runAsUser"].as_i64(), general["runAsGroup"].as_i64()), (Some(1000), Some(1000)));
        assert_eq!(p.security(Some("baseline")), json!({"runAsUser": 1000, "runAsGroup": 1000}));
        let restricted = p.security(Some("restricted"));
        assert_eq!(restricted["capabilities"]["drop"], json!(["ALL"]));
        assert_eq!(restricted["allowPrivilegeEscalation"], json!(false));
        assert_eq!(restricted["runAsNonRoot"], json!(true));
        assert_eq!(restricted["seccompProfile"]["type"], "RuntimeDefault");
        assert_eq!(restricted["runAsUser"].as_i64(), Some(1000));
        // The image's user (root, for busybox) unless restricted.
        let unknown = plan(json!({"spec": {"containers": [{"name": "app"}]}}));
        assert!(unknown.security(None).get("runAsUser").is_none());
        assert_eq!(unknown.security(Some("restricted"))["runAsUser"].as_i64(), Some(NOBODY));
        let root = plan(json!({"spec": {"containers": [{"name": "app", "securityContext": {"runAsUser": 0}}]}}));
        assert_eq!(root.security(Some("restricted"))["runAsUser"].as_i64(), Some(NOBODY));
        let container = p.container();
        assert_eq!(container["targetContainerName"], "app");
        assert_eq!(container["command"][0], "sleep");
        assert!(container.get("stdin").is_none());
    }

    #[test]
    fn only_a_missing_shell_or_tool_calls_for_a_debug_container() {
        let needed = |text: &str| super::needed(&anyhow::anyhow!("{text}"));
        assert!(needed("no shell in this image (…): exec: \"sh\": executable file not found in $PATH"));
        assert!(needed("sh: stat: not found"));
        assert!(needed("sh: 1: stat: not found"));
        assert!(needed("sh: line 3: readlink: command not found"));
        assert!(needed("xargs: stat: No such file or directory"));
        assert!(!needed("sh: cd: line 1: can't cd to /nope: No such file or directory"));
        assert!(!needed("404 Not Found (the pod is gone)"));
        assert!(!needed("your access does not allow it: create on pods/exec"));
    }

    #[test]
    fn debug_containers_are_reused_only_while_they_run_for_the_same_container() {
        let status = |name: &str, running: bool| {
            let state = if running { json!({"running": {}}) } else { json!({"terminated": {"exitCode": 0}}) };
            json!({"name": name, "image": "x", "imageID": "", "ready": false, "restartCount": 0, "state": state})
        };
        let read = pod(json!({
            "spec": {"containers": [{"name": "app"}], "ephemeralContainers": [
                {"name": "recouvrance-files-old", "targetContainerName": "app"},
                {"name": "recouvrance-files-done", "targetContainerName": "app"},
                {"name": "recouvrance-files-other", "targetContainerName": "sidecar"},
                {"name": "debugger-abcde", "targetContainerName": "app"},
                {"name": "recouvrance-files-new", "targetContainerName": "app"},
            ]},
            "status": {
                "containerStatuses": [{
                    "name": "app", "image": "x", "imageID": "", "ready": true, "restartCount": 0,
                    "containerID": "containerd://abc123", "state": {"running": {}},
                }],
                "ephemeralContainerStatuses": [
                    status("recouvrance-files-old", true), status("recouvrance-files-done", false),
                    status("recouvrance-files-other", true), status("debugger-abcde", true),
                    status("recouvrance-files-new", true),
                ],
            },
        }));
        assert_eq!(running(&read, "app"), ["recouvrance-files-new", "recouvrance-files-old"]);
        assert_eq!(container_id(&read, "app").as_deref(), Some("abc123"));
        assert_eq!(container_id(&read, "sidecar"), None);
    }
}
