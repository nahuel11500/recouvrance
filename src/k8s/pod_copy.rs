//! A copy of a pod whose container isn't running, to read its files: like
//! `kubectl debug <pod> --copy-to=<name> --same-node`, the same spec and volumes (so mounted
//! ConfigMaps and Secrets are there) on the same node (its images are there already, pulled
//! with the node's credentials), with every container sleeping instead of starting its app.
//!
//! A distroless image has no `sleep`: an init container copies a static busybox into an
//! `emptyDir` mounted in every container at [`TOOLS`], and installs its applets in
//! [`TOOLS_BIN`]. The containers sleep with it, and the file browser's scripts run with it.
//!
//! A copy is deleted (pinned by uid) when its page goes away and when the app quits. It stops
//! by itself after [`COPY_SECS`], and one left behind (the app killed) is deleted the next time
//! the app connects to its cluster.

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow, bail};
use k8s_openapi::api::core::v1::Pod;
use kube::Client;
use kube::api::{Api, DynamicObject, ListParams, PostParams};
use serde_json::{Value, json};

use super::{Kind, ar_of, spawn_detached};

/// The start of every copy's name. Deleting a pod named so needs no write mode (`gate`): a
/// copy must go even after write mode is turned off.
pub const NAME_PREFIX: &str = "recouvrance-copy-";
/// The label that marks a copy (with `app.kubernetes.io/managed-by: recouvrance`).
pub const LABEL: &str = "recouvrance/pod-copy";
/// The annotation naming the pod copied.
const COPY_OF: &str = "recouvrance/copy-of";
/// How long a copy lives at most: its containers sleep that long, and the kubelet stops the
/// pod then (`activeDeadlineSeconds`).
pub const COPY_SECS: u64 = 3600;
/// Where the busybox is, in every container of a copy, and its applets.
pub const TOOLS: &str = "/.recouvrance-copy";
pub const TOOLS_BIN: &str = "/.recouvrance-copy/bin";
/// The `emptyDir` holding them, and the init container that fills it.
const TOOLS_VOLUME: &str = "recouvrance-copy-tools";
/// The busybox copied when the cluster's settings name no node shell image: its `-musl`
/// build, linked statically, so it runs in an image without libraries (`busybox:1.37`, the
/// glibc build, needs the image's).
pub const DEFAULT_IMAGE: &str = "busybox:1.37-musl";
/// The user the init container runs as when no container says one (`nobody`).
const NOBODY: i64 = 65534;

/// A copy created: deleted by its uid only, so a pod re-created under its name is left alone.
#[derive(Clone)]
pub struct PodCopy {
    client: Client,
    pub ns: String,
    pub name: String,
    pub uid: String,
    /// When its containers stop.
    pub ends: jiff::Timestamp,
}

/// A new copy's name: the prefix, a random part, then the pod's name, within the 63
/// characters of a hostname.
pub fn new_name(pod: &str) -> String {
    copy_name(pod, &uuid::Uuid::new_v4().simple().to_string()[..6])
}

fn copy_name(pod: &str, random: &str) -> String {
    let name: String = format!("{NAME_PREFIX}{random}-{pod}").chars().take(63).collect();
    name.trim_end_matches(['-', '.']).to_string()
}

/// Why `container` of `pod` isn't running (its state's reason), or none when it runs or the
/// pod has no status yet.
pub fn not_running(pod: &Value, container: &str) -> Option<String> {
    let status = pod.get("status")?;
    let statuses = status.get("containerStatuses").and_then(Value::as_array);
    let Some(state) = statuses.into_iter().flatten().find(|c| c["name"] == container).map(|c| &c["state"]) else {
        return status.get("phase").map(|_| "not started".to_string());
    };
    if state.get("running").is_some() {
        return None;
    }
    let reason = |s: &str, default: &str| {
        let r = state.pointer(&format!("/{s}/reason")).and_then(Value::as_str).filter(|r| !r.is_empty());
        r.unwrap_or(default).to_string()
    };
    Some(match (state.get("waiting"), state.get("terminated")) {
        (Some(_), _) => reason("waiting", "waiting"),
        (None, Some(_)) => reason("terminated", "terminated"),
        (None, None) => "not started".to_string(),
    })
}

/// The copy of `pod` (as read) named `name`, with `image` giving the busybox. Its spec is the
/// pod's, with:
/// - no labels, annotations or owner references: no Service sends it traffic, no controller
///   adopts it;
/// - `nodeName` kept: the scheduler is skipped, and it runs where the original was;
/// - no init containers (they run code, migrations or waits) and no ephemeral containers;
///   fields that admission sets again from the rest (`priority`, `overhead`...) left out;
/// - every container's command replaced by the busybox's `sleep`, its probes, lifecycle hooks
///   and ports removed: no app starts, whichever container is browsed;
/// - `restartPolicy: Never` and a deadline of [`COPY_SECS`].
pub fn copy_spec(pod: &Value, name: &str, image: &str) -> Result<Value> {
    let original = pod.pointer("/metadata/name").and_then(Value::as_str).context("the pod has no name")?;
    let ns = pod.pointer("/metadata/namespace").and_then(Value::as_str).context("the pod has no namespace")?;
    let mut spec = pod.get("spec").and_then(Value::as_object).cloned().context("the pod has no spec")?;
    let user = run_as_user(&spec);
    // Refused on a new pod, or set again by admission from the rest (and refused when they differ).
    for field in
        ["initContainers", "ephemeralContainers", "priority", "preemptionPolicy", "overhead", "schedulingGates"]
    {
        spec.remove(field);
    }
    spec.insert("restartPolicy".into(), "Never".into());
    spec.insert("activeDeadlineSeconds".into(), COPY_SECS.into());
    spec.insert("terminationGracePeriodSeconds".into(), 0.into());
    let sleep = json!([format!("{TOOLS}/busybox"), "sleep", COPY_SECS.to_string()]);
    let containers = spec.get_mut("containers").and_then(Value::as_array_mut).context("the pod has no containers")?;
    for c in containers.iter_mut().filter_map(Value::as_object_mut) {
        for field in ["args", "livenessProbe", "readinessProbe", "startupProbe", "lifecycle", "ports"] {
            c.remove(field);
        }
        c.insert("command".into(), sleep.clone());
        let mounts = c.entry("volumeMounts").or_insert_with(|| json!([]));
        if let Some(mounts) = mounts.as_array_mut() {
            mounts.push(json!({"name": TOOLS_VOLUME, "mountPath": TOOLS, "readOnly": true}));
        }
    }
    let volumes = spec.entry("volumes").or_insert_with(|| json!([]));
    if let Some(volumes) = volumes.as_array_mut() {
        volumes.push(json!({"name": TOOLS_VOLUME, "emptyDir": {}}));
    }
    // Each applet a link to `../busybox`: relative, so it holds wherever the folder is mounted.
    let install = format!(
        "mkdir -p {TOOLS_BIN} && cp /bin/busybox {TOOLS}/busybox && \
         for a in $(/bin/busybox --list); do ln -sf ../busybox {TOOLS_BIN}/$a || exit 1; done"
    );
    // What Pod Security "restricted" asks, as the containers' user: it can read what it copied.
    let security = json!({
        "runAsUser": user, "runAsNonRoot": user != 0, "allowPrivilegeEscalation": false,
        "capabilities": {"drop": ["ALL"]}, "seccompProfile": {"type": "RuntimeDefault"},
    });
    spec.insert(
        "initContainers".into(),
        json!([{
            "name": TOOLS_VOLUME, "image": image, "imagePullPolicy": "IfNotPresent",
            "command": ["/bin/busybox", "sh", "-c", install],
            "volumeMounts": [{"name": TOOLS_VOLUME, "mountPath": TOOLS}],
            "securityContext": security,
        }]),
    );
    Ok(json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "namespace": ns,
            "labels": {"app.kubernetes.io/managed-by": "recouvrance", LABEL: "true"},
            "annotations": {COPY_OF: original},
        },
        "spec": spec,
    }))
}

/// The user the first container runs as (its own setting, else the pod's), else `nobody`.
fn run_as_user(spec: &serde_json::Map<String, Value>) -> i64 {
    let first = spec.get("containers").and_then(|c| c.pointer("/0/securityContext/runAsUser"));
    let pod = spec.get("securityContext").and_then(|s| s.get("runAsUser"));
    first.or(pod).and_then(Value::as_i64).unwrap_or(NOBODY)
}

fn pod_kind() -> Kind {
    Kind { ar: ar_of::<Pod>(), namespaced: true, section: "", label: "Pod".into() }
}

/// The copies created by this run and not deleted yet, by uid: quitting deletes them.
static LIVE: LazyLock<Mutex<HashMap<String, PodCopy>>> = LazyLock::new(Default::default);

fn live() -> std::sync::MutexGuard<'static, HashMap<String, PodCopy>> {
    LIVE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Create the copy `name` of `pod` (as read), with `image` giving the busybox, and wait until
/// its containers run. A copy that fails to start is deleted. If the caller stops waiting (its
/// page closed), the copy is deleted as soon as it exists.
pub async fn create(client: Client, pod: Value, name: String, image: String) -> Result<PodCopy> {
    let spec = copy_spec(&pod, &name, &image)?;
    let (tx, rx) = futures::channel::oneshot::channel::<Result<PodCopy>>();
    spawn_detached(async move {
        let result = start(client, spec).await;
        if let Err(Ok(copy)) = tx.send(result) {
            remove(copy).await;
        }
    });
    rx.await.map_err(|_| anyhow!("the copy's creation stopped"))?
}

async fn start(client: Client, spec: Value) -> Result<PodCopy> {
    let ns = spec.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default().to_string();
    let name = spec.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default().to_string();
    // Untyped, so fields newer than this build's Kubernetes types are copied too.
    let obj: DynamicObject = serde_json::from_value(spec)?;
    let api: Api<DynamicObject> = Api::namespaced_with(client.clone(), &ns, &ar_of::<Pod>());
    let created = match api.create(&PostParams::default(), &obj).await {
        Ok(created) => created,
        Err(e) => {
            // It may be stored all the same (an answer lost): the name is ours alone.
            if super::writes::may_have_stored(&e) {
                remove_named(&client, &ns, &name, None).await;
            }
            return Err(e.into());
        }
    };
    let uid = created.metadata.uid.clone().unwrap_or_default();
    let ends = jiff::Timestamp::now() + jiff::SignedDuration::from_secs(COPY_SECS as i64);
    let copy = PodCopy { client: client.clone(), ns: ns.clone(), name: name.clone(), uid: uid.clone(), ends };
    live().insert(uid, copy.clone());
    match wait_running(&Api::namespaced(client, &ns), &name).await {
        Ok(()) => Ok(copy),
        Err(e) => {
            remove(copy).await;
            Err(e)
        }
    }
}

/// Wait until every container of the copy runs, or say why it won't: a container that
/// ended, an image that can't be pulled, a pod the kubelet refused (no room on the node).
async fn wait_running(api: &Api<Pod>, name: &str) -> Result<()> {
    // Running on two reads in a row: a busybox that can't run in the image (linked against
    // libraries it doesn't have) shows running for a moment, then ends.
    let mut running = false;
    // Two minutes: the busybox image may need pulling.
    for _ in 0..240 {
        let pod = api.get(name).await?;
        let status = pod.status.unwrap_or_default();
        if let Some(phase @ ("Failed" | "Succeeded")) = status.phase.as_deref() {
            let why = [status.reason.as_deref(), status.message.as_deref()].into_iter().flatten().collect::<Vec<_>>();
            bail!("the copy stopped ({phase}): {}", why.join(": "));
        }
        let unschedulable =
            status.conditions.iter().flatten().find(|c| {
                c.type_ == "PodScheduled" && c.status == "False" && c.reason.as_deref() == Some("Unschedulable")
            });
        if let Some(c) = unschedulable {
            bail!("the copy can't be scheduled: {}", c.message.clone().unwrap_or_default());
        }
        let containers = status.container_statuses.unwrap_or_default();
        let all_running =
            !containers.is_empty() && containers.iter().all(|c| c.state.as_ref().is_some_and(|s| s.running.is_some()));
        if all_running && running {
            return Ok(());
        }
        running = all_running;
        let all = status.init_container_statuses.iter().flatten().chain(&containers);
        for c in all {
            let Some(state) = &c.state else { continue };
            if let Some(t) = state.terminated.as_ref().filter(|t| t.exit_code != 0 || c.name != TOOLS_VOLUME) {
                let why = t.message.clone().or_else(|| t.reason.clone()).unwrap_or_default();
                bail!("container {} of the copy ended (exit code {}): {why}", c.name, t.exit_code);
            }
            let stuck = ["ErrImagePull", "ImagePullBackOff", "InvalidImageName", "CreateContainerConfigError"];
            if let Some(w) = state.waiting.as_ref().filter(|w| stuck.contains(&w.reason.as_deref().unwrap_or_default()))
            {
                bail!(
                    "container {} of the copy: {}: {}",
                    c.name,
                    w.reason.clone().unwrap_or_default(),
                    w.message.clone().unwrap_or_default()
                );
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("the copy did not start within 2 minutes")
}

/// Delete a copy in the background (its page went away, or the user asked).
pub fn delete(copy: PodCopy) {
    spawn_detached(remove(copy));
}

/// Delete a copy now, by its uid, trying again a few times: the error that made us give up
/// on it (a 429, a 5xx, a network blip) often fails the first delete too.
async fn remove(copy: PodCopy) {
    remove_named(&copy.client, &copy.ns, &copy.name, Some(copy.uid.clone())).await;
    live().remove(&copy.uid);
}

async fn remove_named(client: &Client, ns: &str, name: &str, uid: Option<String>) {
    for _ in 0..5 {
        let deleted = super::delete(client.clone(), pod_kind(), Some(ns.into()), name.into(), uid.clone(), false).await;
        match deleted {
            // Gone already, re-created under its name (the uid differs), or refused: trying
            // again won't help.
            Err(e) if transient(&e) => tokio::time::sleep(Duration::from_secs(2)).await,
            Err(e) if !super::is_not_found(&e) => {
                eprintln!("could not delete the pod copy {ns}/{name}: {}", super::short_error(&e));
                return;
            }
            _ => return,
        }
    }
}

/// An error worth trying again after: too many requests, the server failing, or no answer.
fn transient(e: &anyhow::Error) -> bool {
    e.chain().any(|c| match c.downcast_ref::<kube::Error>() {
        Some(kube::Error::Api(s)) => s.code == 429 || s.code >= 500,
        Some(_) => true,
        None => false,
    })
}

/// Delete every copy of this run still there, waiting `timeout` at most: the app is quitting,
/// and deletes left in the background would end with it.
pub fn remove_all_now(timeout: Duration) {
    let copies: Vec<PodCopy> = live().values().cloned().collect();
    if copies.is_empty() {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    spawn_detached(async move {
        futures::future::join_all(copies.into_iter().map(remove)).await;
        let _ = tx.send(());
    });
    let _ = rx.recv_timeout(timeout);
}

/// Delete the copies on cluster `context` older than [`COPY_SECS`]: left behind when the app
/// was killed, they have stopped by now. Once a run for each context.
pub fn clean_up(context: &str, client: Client, namespaces: Vec<String>) {
    static DONE: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
    if !DONE.lock().unwrap_or_else(PoisonError::into_inner).insert(context.to_string()) {
        return;
    }
    let context = context.to_string();
    spawn_detached(async move {
        if let Err(e) = sweep(client, namespaces, jiff::SignedDuration::from_secs(COPY_SECS as i64)).await {
            eprintln!("{context}: pod copies left behind were not read: {}", super::short_error(&e));
        }
    });
}

/// Delete the copies older than `max_age`, read from every namespace, else (not allowed) from
/// `namespaces`, the ones the cluster's settings name. Returns how many were deleted.
pub(crate) async fn sweep(client: Client, namespaces: Vec<String>, max_age: jiff::SignedDuration) -> Result<usize> {
    let lp = ListParams::default().labels(&format!("{LABEL}=true")).match_any();
    let pods = match Api::<Pod>::all(client.clone()).list(&lp).await {
        Ok(list) => list.items,
        Err(e) if namespaces.is_empty() => return Err(e.into()),
        Err(_) => {
            let mut pods = Vec::new();
            for ns in &namespaces {
                // One namespace refused doesn't keep the others from being cleaned.
                match Api::<Pod>::namespaced(client.clone(), ns).list(&lp).await {
                    Ok(list) => pods.extend(list.items),
                    Err(e) => eprintln!("pod copies left in {ns} were not read: {e}"),
                }
            }
            pods
        }
    };
    let now = jiff::Timestamp::now();
    let mut deleted = 0;
    for pod in pods {
        let meta = pod.metadata;
        let (Some(ns), Some(name), Some(uid)) = (meta.namespace, meta.name, meta.uid) else { continue };
        let created = meta.creation_timestamp.map(|t| t.0);
        if name.starts_with(NAME_PREFIX) && created.is_some_and(|t| older(t, now, max_age)) {
            remove_named(&client, &ns, &name, Some(uid.clone())).await;
            live().remove(&uid);
            deleted += 1;
        }
    }
    Ok(deleted)
}

/// Whether something created at `created` is older than `max_age` at `now`.
fn older(created: jiff::Timestamp, now: jiff::Timestamp, max_age: jiff::SignedDuration) -> bool {
    now.duration_since(created) > max_age
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod() -> Value {
        json!({
            "metadata": {
                "name": "web-7d9f-abcde", "namespace": "shop", "uid": "u-1",
                "labels": {"app": "web", "pod-template-hash": "7d9f"},
                "annotations": {"prometheus.io/scrape": "true"},
                "ownerReferences": [{"kind": "ReplicaSet", "name": "web-7d9f", "uid": "rs-1", "controller": true}],
            },
            "spec": {
                "nodeName": "node-3",
                "priority": 1000, "priorityClassName": "high",
                "serviceAccountName": "web",
                "securityContext": {"runAsNonRoot": true, "fsGroup": 2000},
                "initContainers": [{"name": "migrate", "image": "web:1", "command": ["/migrate"]}],
                "ephemeralContainers": [{"name": "debugger-x", "image": "busybox"}],
                "containers": [
                    {
                        "name": "app", "image": "gcr.io/distroless/static:nonroot", "command": ["/app"], "args": ["--serve"],
                        "securityContext": {"runAsUser": 65532},
                        "livenessProbe": {"httpGet": {"port": 8080}}, "readinessProbe": {"httpGet": {"port": 8080}},
                        "startupProbe": {"exec": {"command": ["/ready"]}}, "lifecycle": {"preStop": {"exec": {"command": ["/stop"]}}},
                        "ports": [{"containerPort": 8080, "hostPort": 8080}],
                        "volumeMounts": [{"name": "config", "mountPath": "/etc/web"}],
                    },
                    {"name": "proxy", "image": "envoy:1", "args": ["-c", "/etc/envoy.yaml"]},
                ],
                "volumes": [{"name": "config", "configMap": {"name": "web-config"}}],
                "restartPolicy": "Always",
            },
            "status": {"phase": "Running"},
        })
    }

    #[test]
    fn a_copy_is_the_pod_asleep_on_its_node_and_selected_by_nothing() {
        let copy = copy_spec(&pod(), "recouvrance-copy-abc123-web", "busybox:1.37").unwrap();
        let meta = &copy["metadata"];
        assert_eq!(meta["name"], "recouvrance-copy-abc123-web");
        assert_eq!(meta["namespace"], "shop");
        // Only its own labels: no Service selects it, no ReplicaSet adopts it.
        assert_eq!(meta["labels"], json!({"app.kubernetes.io/managed-by": "recouvrance", LABEL: "true"}));
        assert_eq!(meta["annotations"], json!({COPY_OF: "web-7d9f-abcde"}));
        assert!(meta.get("ownerReferences").is_none() && meta.get("uid").is_none());
        assert!(copy.get("status").is_none());

        let spec = &copy["spec"];
        assert_eq!(spec["nodeName"], "node-3");
        assert_eq!(spec["restartPolicy"], "Never");
        assert_eq!(spec["activeDeadlineSeconds"], COPY_SECS);
        assert_eq!(spec["serviceAccountName"], "web");
        assert_eq!(spec["priorityClassName"], "high");
        assert!(spec.get("priority").is_none() && spec.get("ephemeralContainers").is_none());
        // The pod's volumes are all there, and the busybox's.
        let volumes: Vec<&str> =
            spec["volumes"].as_array().unwrap().iter().map(|v| v["name"].as_str().unwrap()).collect();
        assert_eq!(volumes, ["config", TOOLS_VOLUME]);
        assert_eq!(spec["volumes"][1]["emptyDir"], json!({}));

        // The pod's init containers don't run; the busybox's does, as the app's user.
        let inits = spec["initContainers"].as_array().unwrap();
        assert_eq!(inits.len(), 1);
        let init = &inits[0];
        assert_eq!(init["image"], "busybox:1.37");
        assert_eq!(init["command"][0], "/bin/busybox");
        assert!(init["command"][3].as_str().unwrap().contains(&format!("ln -sf ../busybox {TOOLS_BIN}/$a")));
        assert_eq!(init["volumeMounts"], json!([{"name": TOOLS_VOLUME, "mountPath": TOOLS}]));
        assert_eq!(init["securityContext"]["runAsUser"], 65532);
        assert_eq!(init["securityContext"]["runAsNonRoot"], true);
        assert_eq!(init["securityContext"]["allowPrivilegeEscalation"], false);

        // Every container sleeps with the busybox: no probes, hooks, ports or arguments left.
        let containers = spec["containers"].as_array().unwrap();
        assert_eq!(containers.len(), 2);
        for c in containers {
            assert_eq!(c["command"], json!([format!("{TOOLS}/busybox"), "sleep", COPY_SECS.to_string()]));
            for gone in ["args", "livenessProbe", "readinessProbe", "startupProbe", "lifecycle", "ports"] {
                assert!(c.get(gone).is_none(), "{} keeps {gone}", c["name"]);
            }
            let mounts = c["volumeMounts"].as_array().unwrap();
            assert_eq!(mounts.last().unwrap(), &json!({"name": TOOLS_VOLUME, "mountPath": TOOLS, "readOnly": true}));
        }
        assert_eq!(containers[0]["image"], "gcr.io/distroless/static:nonroot");
        assert_eq!(containers[0]["volumeMounts"][0]["mountPath"], "/etc/web");
        assert_eq!(containers[0]["securityContext"]["runAsUser"], 65532);
    }

    #[test]
    fn the_busybox_runs_as_the_pod_s_user_or_nobody() {
        let mut p = pod();
        p["spec"]["containers"][0]["securityContext"] = json!({});
        p["spec"]["securityContext"]["runAsUser"] = json!(1001);
        let copy = copy_spec(&p, "c", "busybox").unwrap();
        assert_eq!(copy["spec"]["initContainers"][0]["securityContext"]["runAsUser"], 1001);
        p["spec"]["securityContext"] = json!({});
        let copy = copy_spec(&p, "c", "busybox").unwrap();
        assert_eq!(copy["spec"]["initContainers"][0]["securityContext"]["runAsUser"], NOBODY);
        // A root container: the busybox as root too, without asking for non-root.
        p["spec"]["containers"][0]["securityContext"] = json!({"runAsUser": 0});
        let copy = copy_spec(&p, "c", "busybox").unwrap();
        assert_eq!(copy["spec"]["initContainers"][0]["securityContext"]["runAsNonRoot"], false);
    }

    #[test]
    fn a_pod_not_scheduled_yet_is_copied_without_a_node() {
        let mut p = pod();
        p["spec"].as_object_mut().unwrap().remove("nodeName");
        let copy = copy_spec(&p, "c", "busybox").unwrap();
        assert!(copy["spec"].get("nodeName").is_none());
        // Without volumes, the busybox's is the only one.
        p["spec"].as_object_mut().unwrap().remove("volumes");
        p["spec"]["containers"][0].as_object_mut().unwrap().remove("volumeMounts");
        let copy = copy_spec(&p, "c", "busybox").unwrap();
        assert_eq!(copy["spec"]["volumes"].as_array().unwrap().len(), 1);
        assert_eq!(copy["spec"]["containers"][0]["volumeMounts"].as_array().unwrap().len(), 1);
        assert!(copy_spec(&json!({"metadata": {"name": "x", "namespace": "y"}}), "c", "busybox").is_err());
    }

    #[test]
    fn a_copy_s_name_is_a_hostname_and_starts_with_the_prefix() {
        assert_eq!(copy_name("web-1", "abc123"), "recouvrance-copy-abc123-web-1");
        let long = copy_name(&"a-".repeat(100), "abc123");
        assert!(long.len() <= 63 && long.starts_with(NAME_PREFIX) && !long.ends_with('-'), "{long}");
        let name = new_name("web-1");
        assert!(name.starts_with(NAME_PREFIX) && name.ends_with("-web-1") && name != new_name("web-1"));
    }

    #[test]
    fn a_container_not_running_says_why() {
        let pod = |state: Value| json!({"status": {"phase": "Running", "containerStatuses": [{"name": "app", "state": state}]}});
        assert_eq!(not_running(&pod(json!({"running": {}})), "app"), None);
        let crash = pod(json!({"waiting": {"reason": "CrashLoopBackOff"}}));
        assert_eq!(not_running(&crash, "app").as_deref(), Some("CrashLoopBackOff"));
        let done = pod(json!({"terminated": {"reason": "Completed", "exitCode": 0}}));
        assert_eq!(not_running(&done, "app").as_deref(), Some("Completed"));
        assert_eq!(not_running(&pod(json!({"waiting": {}})), "app").as_deref(), Some("waiting"));
        // No status for it yet (pending), or no status at all (just created).
        assert_eq!(not_running(&crash, "other").as_deref(), Some("not started"));
        assert_eq!(not_running(&json!({"spec": {}}), "app"), None);
    }

    #[test]
    fn copies_are_cleaned_up_once_they_have_stopped() {
        let now = jiff::Timestamp::now();
        let ago = |secs: i64| now - jiff::SignedDuration::from_secs(secs);
        let deadline = jiff::SignedDuration::from_secs(COPY_SECS as i64);
        assert!(!older(ago(60), now, deadline));
        assert!(!older(ago(COPY_SECS as i64), now, deadline));
        assert!(older(ago(COPY_SECS as i64 + 1), now, deadline));
    }
}
