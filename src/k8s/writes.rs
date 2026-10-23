//! Writes: scale, restart, patch, apply, create, replace, delete, rollback, debug, drain, node shell, GitOps actions.

#[allow(unused_imports)]
use super::*;

pub(super) fn dyn_api(client: Client, kind: &Kind, ns: Option<&str>) -> Api<DynamicObject> {
    match (kind.namespaced, ns) {
        (true, Some(ns)) => Api::namespaced_with(client, ns, &kind.ar),
        _ => Api::all_with(client, &kind.ar),
    }
}

pub(super) fn patch_params(dry_run: bool) -> kube::api::PatchParams {
    kube::api::PatchParams { dry_run, field_manager: Some("recouvrance-agent".into()), ..Default::default() }
}

pub async fn scale(
    client: Client,
    kind: Kind,
    ns: Option<String>,
    name: String,
    replicas: i32,
    dry_run: bool,
) -> Result<String> {
    run(async move {
        let api = dyn_api(client, &kind, ns.as_deref());
        let patch = kube::api::Patch::Merge(serde_json::json!({ "spec": { "replicas": replicas } }));
        let s = api.patch_scale(&name, &patch_params(dry_run), &patch).await?;
        let now = s.spec.and_then(|sp| sp.replicas).unwrap_or(replicas);
        Ok(format!("{}{} {name} scaled to {now} replicas", if dry_run { "[dry run] " } else { "" }, kind.ar.kind))
    })
    .await
}

/// What `kubectl rollout restart` does: bump a pod template annotation.
pub async fn rollout_restart(
    client: Client,
    kind: Kind,
    ns: Option<String>,
    name: String,
    dry_run: bool,
) -> Result<String> {
    run(async move {
        let api = dyn_api(client, &kind, ns.as_deref());
        let now = jiff::Timestamp::now().to_string();
        let patch = kube::api::Patch::Merge(serde_json::json!({
            "spec": { "template": { "metadata": { "annotations": { "kubectl.kubernetes.io/restartedAt": now } } } }
        }));
        api.patch(&name, &patch_params(dry_run), &patch).await?;
        Ok(format!("{}{} {name} restarted", if dry_run { "[dry run] " } else { "" }, kind.ar.kind))
    })
    .await
}

/// A one-line summary, never the patched object: it may hold Secret values
/// the caller may not see (read it with a `get` that masks them).
pub async fn merge_patch(
    client: Client,
    kind: Kind,
    ns: Option<String>,
    name: String,
    patch: serde_json::Value,
    dry_run: bool,
) -> Result<String> {
    run(async move {
        let api = dyn_api(client, &kind, ns.as_deref());
        let obj = api.patch(&name, &patch_params(dry_run), &kube::api::Patch::Merge(patch)).await?;
        let version = obj.metadata.resource_version.map(|v| format!(" (resourceVersion {v})")).unwrap_or_default();
        Ok(format!("{}{} {name} patched{version}", if dry_run { "[dry run] " } else { "" }, kind.ar.kind))
    })
    .await
}

/// Server-side apply of one object.
pub async fn apply(
    client: Client,
    kind: Kind,
    ns: Option<String>,
    obj: serde_json::Value,
    dry_run: bool,
) -> Result<String> {
    run(async move {
        let name =
            obj.pointer("/metadata/name").and_then(|v| v.as_str()).context("object has no metadata.name")?.to_string();
        let api = dyn_api(client, &kind, ns.as_deref());
        let mut pp = kube::api::PatchParams::apply("recouvrance-agent");
        pp.dry_run = dry_run;
        // Report misspelled fields instead of silently dropping them.
        pp.field_validation = Some(kube::api::ValidationDirective::Strict);
        api.patch(&name, &pp, &kube::api::Patch::Apply(obj)).await?;
        Ok(format!("{}{} {name} applied", if dry_run { "[dry run] " } else { "" }, kind.ar.kind))
    })
    .await
}

/// Delete one object. With `uid`, only that object: one re-created under the same name since
/// the user picked it (a pod replaced by its ReplicaSet, a list that was out of date) is left
/// alone, the server refusing with a conflict.
pub async fn delete(
    client: Client,
    kind: Kind,
    ns: Option<String>,
    name: String,
    uid: Option<String>,
    dry_run: bool,
) -> Result<String> {
    run(async move {
        let api = dyn_api(client, &kind, ns.as_deref());
        let dp = kube::api::DeleteParams { dry_run, preconditions: preconditions(uid), ..Default::default() };
        api.delete(&name, &dp).await?;
        Ok(format!("{}{} {name} deleted", if dry_run { "[dry run] " } else { "" }, kind.ar.kind))
    })
    .await
}

/// "Only if it is still this object", when its real uid is known (rows of a fast first load
/// have a stand-in: namespace/name).
fn preconditions(uid: Option<String>) -> Option<kube::api::Preconditions> {
    let uid = uid.filter(|u| !u.is_empty() && !u.contains('/'))?;
    Some(kube::api::Preconditions { uid: Some(uid), resource_version: None })
}

/// Whether an error is the API server's 404 (the object does not exist).
pub fn is_not_found(e: &anyhow::Error) -> bool {
    e.chain().any(|c| matches!(c.downcast_ref::<kube::Error>(), Some(kube::Error::Api(r)) if r.code == 404))
}

/// How long controllers get to run their finalizers before "Remove finalizers" clears them.
pub const FINALIZER_GRACE_SECS: u64 = 10;

/// Delete with options: `force` skips the grace period (`--grace-period=0 --force`).
/// `strip_finalizers` clears finalizers only when the delete leaves the object stuck
/// terminating (already before, or still after `FINALIZER_GRACE_SECS`), so controllers that
/// can still clean up get the chance to.
pub async fn delete_with(
    client: Client,
    kind: Kind,
    ns: Option<String>,
    name: String,
    uid: Option<String>,
    force: bool,
    strip_finalizers: bool,
) -> Result<String> {
    run(async move {
        let api = dyn_api(client, &kind, ns.as_deref());
        let mut dp = if force {
            kube::api::DeleteParams {
                grace_period_seconds: Some(0),
                propagation_policy: Some(kube::api::PropagationPolicy::Background),
                ..Default::default()
            }
        } else {
            kube::api::DeleteParams::default()
        };
        dp.preconditions = preconditions(uid);
        let was_stuck = strip_finalizers && stuck_terminating(api.get_opt(&name).await?.as_ref());
        api.delete(&name, &dp).await?;
        let mut stripped = false;
        if strip_finalizers {
            // Give its controllers time to finish, unless it was already stuck before.
            let (step, mut waited) = (std::time::Duration::from_millis(500), std::time::Duration::ZERO);
            let mut current = api.get_opt(&name).await?;
            while !was_stuck
                && stuck_terminating(current.as_ref())
                && waited < std::time::Duration::from_secs(FINALIZER_GRACE_SECS)
            {
                tokio::time::sleep(step).await;
                waited += step;
                current = api.get_opt(&name).await?;
            }
            if stuck_terminating(current.as_ref()) {
                let patch = serde_json::json!({"metadata": {"finalizers": null}});
                match api.patch(&name, &patch_params(false), &kube::api::Patch::Merge(&patch)).await {
                    // Its finalizers completed in between.
                    Err(kube::Error::Api(e)) if e.code == 404 => {}
                    r => {
                        r?;
                        stripped = true;
                    }
                }
            }
        }
        Ok(format!(
            "{} {name} deleted{}{}",
            kind.ar.kind,
            if force { " (forced)" } else { "" },
            if stripped { ", finalizers removed" } else { "" }
        ))
    })
    .await
}

/// Deleted, but kept by finalizers.
fn stuck_terminating(o: Option<&DynamicObject>) -> bool {
    o.is_some_and(|o| {
        o.metadata.deletion_timestamp.is_some() && o.metadata.finalizers.as_ref().is_some_and(|f| !f.is_empty())
    })
}

/// Add an ephemeral debug container to a pod (like `kubectl debug -it --target`)
/// and wait until it runs. Returns the container name.
pub async fn add_debug_container(
    client: Client,
    ns: String,
    pod: String,
    image: String,
    target: Option<String>,
    dry_run: bool,
) -> Result<String> {
    run(async move {
        let api: Api<Pod> = Api::namespaced(client, &ns);
        let name = format!("debugger-{}", &uuid::Uuid::new_v4().simple().to_string()[..5]);
        let mut container = serde_json::json!({
            "name": name, "image": image, "stdin": true, "tty": true,
            "command": ["sh"], "terminationMessagePolicy": "File",
        });
        if let Some(t) = target.filter(|t| !t.is_empty()) {
            container["targetContainerName"] = t.into();
        }
        let pp = kube::api::PatchParams { dry_run, ..Default::default() };
        let patch = serde_json::json!({"spec": {"ephemeralContainers": [container.clone()]}});
        if let Err(e) = api.patch_ephemeral_containers(&pod, &pp, &kube::api::Patch::Strategic(&patch)).await {
            // Namespaces enforcing Pod Security "restricted" need what
            // `kubectl debug --profile=restricted` sets (non-root, no privileges).
            if !e.to_string().contains("PodSecurity") {
                return Err(e.into());
            }
            container["securityContext"] = serde_json::json!({
                "allowPrivilegeEscalation": false, "capabilities": {"drop": ["ALL"]},
                "runAsNonRoot": true, "runAsUser": 65534, "seccompProfile": {"type": "RuntimeDefault"},
            });
            let patch = serde_json::json!({"spec": {"ephemeralContainers": [container]}});
            api.patch_ephemeral_containers(&pod, &pp, &kube::api::Patch::Strategic(&patch)).await?;
        }
        if dry_run {
            return Ok(format!("[dry run] debug container {name} accepted"));
        }
        for _ in 0..120 {
            let p = api.get(&pod).await?;
            let st = p.status.and_then(|s| s.ephemeral_container_statuses).unwrap_or_default();
            if let Some(cs) = st.iter().find(|c| c.name == name) {
                let state = cs.state.clone().unwrap_or_default();
                if state.running.is_some() {
                    return Ok(name);
                }
                if let Some(w) = state.waiting.as_ref().filter(|w| {
                    matches!(w.reason.as_deref(), Some("ErrImagePull" | "ImagePullBackOff" | "InvalidImageName"))
                }) {
                    anyhow::bail!(
                        "{}: {}",
                        w.reason.clone().unwrap_or_default(),
                        w.message.clone().unwrap_or_default()
                    );
                }
                if let Some(t) = state.terminated {
                    anyhow::bail!("debug container exited: {}", t.reason.unwrap_or_default());
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        anyhow::bail!("debug container {name} did not start within 60s")
    })
    .await
}

/// Namespace where node shell pods run (usually allowed to be privileged).
pub const NODE_SHELL_NS: &str = "kube-system";

/// Start a privileged pod on `node` that enters the host's namespaces (a node
/// shell), wait for it to run, and return its name. It deletes itself after
/// four hours at most; callers delete it when done. If the caller stops waiting
/// (its page closed), the pod is deleted as soon as it exists.
pub async fn create_node_shell(client: Client, node: String, image: String, dry_run: bool) -> Result<String> {
    let (tx, rx) = futures::channel::oneshot::channel::<Result<String>>();
    let cleanup = client.clone();
    spawn_detached(async move {
        let result = start_node_shell(client, node, image, dry_run).await;
        if let Err(Ok(name)) = tx.send(result)
            && !dry_run
        {
            delete_node_shell(cleanup, name);
        }
    });
    rx.await.map_err(|_| anyhow::anyhow!("node shell creation stopped"))?
}

async fn start_node_shell(client: Client, node: String, image: String, dry_run: bool) -> Result<String> {
    run(async move {
        let api: Api<Pod> = Api::namespaced(client, NODE_SHELL_NS);
        let name = format!("recouvrance-node-shell-{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
        let pod: Pod = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1", "kind": "Pod",
            "metadata": {"name": name, "namespace": NODE_SHELL_NS, "labels": {"app.kubernetes.io/managed-by": "recouvrance", "recouvrance/node-shell": "true"}},
            "spec": {
                "nodeName": node,
                "hostPID": true, "hostNetwork": true, "hostIPC": true,
                "restartPolicy": "Never",
                "activeDeadlineSeconds": 14400,
                "terminationGracePeriodSeconds": 0,
                "priorityClassName": "system-node-critical",
                "tolerations": [{"operator": "Exists"}],
                "containers": [{
                    "name": "shell", "image": image,
                    "stdin": true, "tty": true,
                    "command": ["nsenter", "--target", "1", "--mount", "--uts", "--ipc", "--net", "--pid", "--", "sh", "-c", "command -v bash >/dev/null && exec bash -l || exec sh -l"],
                    "securityContext": {"privileged": true},
                }],
            },
        }))?;
        let pp = kube::api::PostParams { dry_run, ..Default::default() };
        api.create(&pp, &pod).await?;
        if dry_run {
            return Ok(format!("[dry run] node shell pod {name} accepted on {}", NODE_SHELL_NS));
        }
        // Delete the pod on every way out but a running shell: it is privileged.
        let started = wait_node_shell(&api, &name).await;
        if started.is_err() {
            remove_node_shell(&api, &name).await;
        }
        started.map(|()| name)
    })
    .await
}

/// Wait for a node shell pod to run, or say why it will not.
async fn wait_node_shell(api: &Api<Pod>, name: &str) -> Result<()> {
    for _ in 0..120 {
        let p = api.get(name).await?;
        let phase = p.status.as_ref().and_then(|s| s.phase.clone()).unwrap_or_default();
        if phase == "Running" {
            return Ok(());
        }
        if phase == "Failed" || phase == "Succeeded" {
            anyhow::bail!("node shell pod ended ({phase})");
        }
        let waiting = p
            .status
            .and_then(|s| s.container_statuses)
            .unwrap_or_default()
            .into_iter()
            .find_map(|c| c.state.and_then(|s| s.waiting));
        if let Some(w) = waiting.filter(|w| {
            matches!(w.reason.as_deref(), Some("ErrImagePull" | "ImagePullBackOff" | "CreateContainerConfigError"))
        }) {
            anyhow::bail!("{}: {}", w.reason.unwrap_or_default(), w.message.unwrap_or_default());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    anyhow::bail!("node shell pod did not start within 60s")
}

/// Delete a node shell pod in the background (when its page goes away).
pub fn delete_node_shell(client: Client, name: String) {
    spawn_detached(async move {
        remove_node_shell(&Api::namespaced(client, NODE_SHELL_NS), &name).await;
    });
}

/// Delete a node shell pod now, trying again a few times: the error that made us give up on
/// it (a 429, a 5xx, a network blip) often fails the first delete too.
async fn remove_node_shell(api: &Api<Pod>, name: &str) {
    let dp = kube::api::DeleteParams { grace_period_seconds: Some(0), ..Default::default() };
    for _ in 0..5 {
        match api.delete(name, &dp).await {
            Ok(_) => return,
            // Gone already, or refused: trying again won't help.
            Err(kube::Error::Api(e)) if e.code != 429 && e.code < 500 => return,
            Err(_) => tokio::time::sleep(std::time::Duration::from_secs(2)).await,
        }
    }
}

/// Ask Flux to reconcile now (what `flux reconcile` does).
pub async fn flux_reconcile(client: Client, kind: Kind, ns: Option<String>, name: String) -> Result<String> {
    let now = jiff::Timestamp::now().to_string();
    merge_patch(
        client,
        kind,
        ns,
        name,
        serde_json::json!({"metadata": {"annotations": {"reconcile.fluxcd.io/requestedAt": now}}}),
        false,
    )
    .await
}

/// Ask Argo CD to refresh an Application from git.
pub async fn argo_refresh(client: Client, kind: Kind, ns: Option<String>, name: String) -> Result<String> {
    merge_patch(
        client,
        kind,
        ns,
        name,
        serde_json::json!({"metadata": {"annotations": {"argocd.argoproj.io/refresh": "normal"}}}),
        false,
    )
    .await
}

/// The merge patch that starts an Argo CD sync (also shown as the kubectl equivalent).
pub fn argo_sync_patch() -> serde_json::Value {
    serde_json::json!({"operation": {"initiatedBy": {"username": "recouvrance"}, "sync": {"syncStrategy": {"hook": {}}}}})
}

/// Start an Argo CD sync of an Application to its target revision.
pub async fn argo_sync(client: Client, kind: Kind, ns: Option<String>, name: String) -> Result<String> {
    merge_patch(client, kind, ns, name, argo_sync_patch(), false).await
}

/// Replace an object with an edited version (what `kubectl edit` does).
/// The resourceVersion in `obj` guards against overwriting newer changes.
pub async fn replace(
    client: Client,
    kind: Kind,
    ns: Option<String>,
    obj: serde_json::Value,
    dry_run: bool,
) -> Result<String> {
    run(async move {
        let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).context("no metadata.name")?.to_string();
        let dynobj: DynamicObject = serde_json::from_value(obj)?;
        let api = dyn_api(client, &kind, ns.as_deref());
        let pp = kube::api::PostParams { dry_run, field_manager: Some("recouvrance".into()) };
        api.replace(&name, &pp, &dynobj).await?;
        Ok(format!("{}{} {name} updated", if dry_run { "[dry run] " } else { "" }, kind.ar.kind))
    })
    .await
}

/// What `kubectl create job --from=cronjob/X` does.
pub async fn trigger_cronjob(client: Client, ns: String, name: String) -> Result<String> {
    trigger_cronjob_opts(client, ns, name, false).await
}

/// The start of a manual Job's name; a number from the clock follows.
pub fn manual_job_prefix(cronjob: &str) -> String {
    format!("{}-manual-", cronjob.chars().take(40).collect::<String>())
}

pub async fn trigger_cronjob_opts(client: Client, ns: String, name: String, dry_run: bool) -> Result<String> {
    use k8s_openapi::api::batch::v1::{CronJob, Job};
    run(async move {
        let cronjobs: Api<CronJob> = Api::namespaced(client.clone(), &ns);
        let cj = cronjobs.get(&name).await?;
        let template = cj.spec.context("cronjob has no spec")?.job_template;
        let suffix = jiff::Timestamp::now().as_second() % 100_000;
        let job_name = format!("{}{suffix}", manual_job_prefix(&name));
        let mut annotations = template.metadata.as_ref().and_then(|m| m.annotations.clone()).unwrap_or_default();
        annotations.insert("cronjob.kubernetes.io/instantiate".into(), "manual".into());
        let job = Job {
            metadata: ObjectMeta {
                name: Some(job_name.clone()),
                namespace: Some(ns.clone()),
                labels: template.metadata.as_ref().and_then(|m| m.labels.clone()),
                annotations: Some(annotations),
                owner_references: Some(vec![k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference {
                    api_version: "batch/v1".into(),
                    kind: "CronJob".into(),
                    name: name.clone(),
                    uid: cj.metadata.uid.clone().unwrap_or_default(),
                    controller: Some(true),
                    block_owner_deletion: None,
                }]),
                ..Default::default()
            },
            spec: template.spec,
            ..Default::default()
        };
        let jobs: Api<Job> = Api::namespaced(client, &ns);
        jobs.create(&kube::api::PostParams { dry_run, field_manager: Some("recouvrance".into()) }, &job).await?;
        Ok(format!("{}Job {job_name} created from CronJob {name}", if dry_run { "[dry run] " } else { "" }))
    })
    .await
}

/// What `kubectl drain --ignore-daemonsets --delete-emptydir-data` does:
/// cordon, then evict every pod except DaemonSet and static pods. Evictions
/// respect PodDisruptionBudgets; refused ones are reported, not forced. Pods no
/// controller manages would be gone for good, so (like kubectl without
/// `--force`) they are left in place and reported.
pub async fn drain(client: Client, node: String) -> Result<String> {
    run(async move {
        let nodes: Api<k8s_openapi::api::core::v1::Node> = Api::all(client.clone());
        nodes
            .patch(
                &node,
                &patch_params(false),
                &kube::api::Patch::Merge(serde_json::json!({"spec": {"unschedulable": true}})),
            )
            .await?;
        let pods: Api<Pod> = Api::all(client.clone());
        let on_node = pods.list(&ListParams::default().fields(&format!("spec.nodeName={node}"))).await?;
        let (mut evicted, mut skipped, mut refused, mut unmanaged) = (0, 0, Vec::new(), Vec::new());
        for p in on_node.items {
            let owners = p.metadata.owner_references.clone().unwrap_or_default();
            let daemon = owners.iter().any(|o| o.kind == "DaemonSet");
            let mirror = p.metadata.annotations.as_ref().is_some_and(|a| a.contains_key("kubernetes.io/config.mirror"));
            let done = matches!(p.status.as_ref().and_then(|s| s.phase.as_deref()), Some("Succeeded" | "Failed"));
            if daemon || mirror || done {
                skipped += 1;
                continue;
            }
            let (Some(ns), Some(name)) = (p.metadata.namespace.clone(), p.metadata.name.clone()) else { continue };
            if !owners.iter().any(|o| o.controller == Some(true)) {
                unmanaged.push(format!("{ns}/{name}"));
                continue;
            }
            let api: Api<Pod> = Api::namespaced(client.clone(), &ns);
            match api.evict(&name, &Default::default()).await {
                Ok(_) => evicted += 1,
                Err(e) => refused.push(format!("{ns}/{name}: {e}")),
            }
        }
        let mut msg =
            format!("Node {node} cordoned; {evicted} pods evicted, {skipped} skipped (DaemonSet/static/finished)");
        if !refused.is_empty() {
            msg.push_str(&format!(
                "; {} refused (likely a PodDisruptionBudget, retry later): {}",
                refused.len(),
                refused.iter().take(5).cloned().collect::<Vec<_>>().join("; ")
            ));
        }
        if !unmanaged.is_empty() {
            msg.push_str(&format!(
                "; {} not evicted, no controller would recreate them (delete them yourself, or `kubectl drain --force`): {}",
                unmanaged.len(),
                unmanaged.iter().take(5).cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        Ok(msg)
    })
    .await
}

/// Roll a Deployment, StatefulSet or DaemonSet back to an older pod template, like
/// `kubectl rollout undo --to-revision`. `template` is the revision's pod template
/// (from a ReplicaSet or a ControllerRevision); it replaces the current one.
pub async fn rollback(
    client: Client,
    kind: Kind,
    ns: Option<String>,
    name: String,
    template: serde_json::Value,
    dry_run: bool,
) -> Result<String> {
    run(async move {
        let mut template = template;
        if let Some(labels) = template.pointer_mut("/metadata/labels").and_then(|l| l.as_object_mut()) {
            labels.remove("pod-template-hash");
        }
        if let Some(t) = template.as_object_mut() {
            t.insert("$patch".into(), "replace".into());
        }
        let api = dyn_api(client, &kind, ns.as_deref());
        let patch = kube::api::Patch::Strategic(serde_json::json!({ "spec": { "template": template } }));
        api.patch(&name, &patch_params(dry_run), &patch).await?;
        Ok(format!("{}{} {name} rolled back", if dry_run { "[dry run] " } else { "" }, kind.ar.kind))
    })
    .await
}

/// A StatefulSet's or DaemonSet's ControllerRevisions, shaped like ReplicaSets
/// (revision annotation, spec.template) so the Revisions tab treats them alike.
pub async fn controller_revisions(client: Client, ns: String, owner_uid: String) -> Vec<serde_json::Value> {
    let ar = ar_of::<k8s_openapi::api::apps::v1::ControllerRevision>();
    list_values(client, ar, Some(ns), None)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|r| {
            r.pointer("/metadata/ownerReferences")
                .and_then(|o| o.as_array())
                .is_some_and(|o| o.iter().any(|x| x["uid"].as_str() == Some(owner_uid.as_str())))
        })
        .map(|r| {
            serde_json::json!({
                "kind": "ControllerRevision",
                "metadata": {
                    "name": r.pointer("/metadata/name"),
                    "creationTimestamp": r.pointer("/metadata/creationTimestamp"),
                    "annotations": { "deployment.kubernetes.io/revision": r["revision"].as_i64().unwrap_or(0).to_string() },
                },
                "spec": { "template": r.pointer("/data/spec/template").cloned().unwrap_or_default() },
            })
        })
        .collect()
}

/// Create one object (fails if it exists, like `kubectl create`).
pub async fn create(
    client: Client,
    kind: Kind,
    ns: Option<String>,
    obj: serde_json::Value,
    dry_run: bool,
) -> Result<String> {
    run(async move {
        let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let dynobj: DynamicObject = serde_json::from_value(obj)?;
        let api = dyn_api(client, &kind, ns.as_deref());
        let pp = kube::api::PostParams { dry_run, field_manager: Some("recouvrance".into()) };
        api.create(&pp, &dynobj).await?;
        Ok(format!("{}{} {name} created", if dry_run { "[dry run] " } else { "" }, kind.ar.kind))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletes_are_pinned_to_the_object_picked() {
        assert_eq!(preconditions(Some("0b3c-uid".into())).and_then(|p| p.uid).as_deref(), Some("0b3c-uid"));
        // A stand-in uid (namespace/name) or none: nothing to pin to.
        assert!(preconditions(Some("shop/web-1".into())).is_none());
        assert!(preconditions(Some(String::new())).is_none());
        assert!(preconditions(None).is_none());
    }

    #[test]
    fn stuck_only_when_finalizers_hold_a_deleted_object() {
        let obj = |deleted: bool, finalizers: Option<Vec<String>>| -> DynamicObject {
            let mut meta = serde_json::json!({"name": "x", "finalizers": finalizers});
            if deleted {
                meta["deletionTimestamp"] = "2026-01-01T00:00:00Z".into();
            }
            serde_json::from_value(
                serde_json::json!({"apiVersion": "v1", "kind": "PersistentVolumeClaim", "metadata": meta}),
            )
            .unwrap()
        };
        let held = Some(vec!["kubernetes.io/pvc-protection".to_string()]);
        assert!(stuck_terminating(Some(&obj(true, held.clone()))));
        assert!(!stuck_terminating(Some(&obj(false, held))));
        assert!(!stuck_terminating(Some(&obj(true, Some(vec![])))));
        assert!(!stuck_terminating(Some(&obj(true, None))));
        assert!(!stuck_terminating(None));
    }
}
