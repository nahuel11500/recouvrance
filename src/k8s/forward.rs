//! Port-forwards that follow replacement pods.

use std::sync::Arc;

use anyhow::{Context as _, Result};
use futures::channel::mpsc;
use k8s_openapi::api::apps::v1::ReplicaSet;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kube::Client;
use kube::api::{Api, ListParams};

use super::{TaskGuard, run, spawn_guarded};

pub enum ForwardMsg {
    Listening(u16),
    Connection,
    Error(String),
    /// The pod was replaced; now forwarding to this one.
    Repointed(String),
}

/// Forward 127.0.0.1:`local` to `remote` on the pod, one tunnel per TCP connection.
///
/// Each connection opens its own port-forward: the WebSocket protocol carries one data
/// stream per port, so a tunnel cannot be shared between connections.
pub fn port_forward(
    client: Client,
    ns: String,
    pod: String,
    service: Option<String>,
    address: String,
    local: u16,
    remote: u16,
) -> (TaskGuard, mpsc::UnboundedReceiver<ForwardMsg>) {
    let (tx, rx) = mpsc::unbounded();
    let guard = spawn_guarded(async move {
        let listener = match tokio::net::TcpListener::bind((address.as_str(), local)).await {
            Ok(l) => l,
            Err(e) => {
                let _ = tx.unbounded_send(ForwardMsg::Error(format!("cannot listen on {address}:{local}: {e}")));
                return;
            }
        };
        let port = listener.local_addr().map_or(local, |a| a.port());
        let _ = tx.unbounded_send(ForwardMsg::Listening(port));
        let api: Api<Pod> = Api::namespaced(client.clone(), &ns);
        // The pod in use; replaced when it goes away (rollout, crash, eviction).
        let current = Arc::new(tokio::sync::Mutex::new(pod.clone()));
        // The workload whose pods may replace it, read while the pod is still there.
        let owner = match &service {
            Some(_) => None,
            None => forward_owner(&client, &ns, &pod).await,
        };
        // Open tunnels belong to this forward: dropping the set (the forward is
        // stopped and this task aborted) aborts them all.
        let mut connections = tokio::task::JoinSet::new();
        let mut backoff = std::time::Duration::from_millis(50);
        loop {
            let accepted = tokio::select! {
                a = listener.accept() => a,
                // Reap finished tunnels so the set does not grow forever.
                Some(_) = connections.join_next(), if !connections.is_empty() => continue,
            };
            let mut socket = if let Ok((socket, _)) = accepted {
                backoff = std::time::Duration::from_millis(50);
                socket
            } else {
                // Out of file descriptors and the like: wait instead of spinning.
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(2));
                continue;
            };
            let (api, tx, current, client, ns, service, owner) =
                (api.clone(), tx.clone(), current.clone(), client.clone(), ns.clone(), service.clone(), owner.clone());
            let _ = tx.unbounded_send(ForwardMsg::Connection);
            connections.spawn(async move {
                let result = async {
                    let target = current.lock().await.clone();
                    let mut pf = match api.portforward(&target, &[remote]).await {
                        Ok(pf) => pf,
                        Err(first) => {
                            // Find the pod that replaced it, then retry once.
                            let Ok(next) = resolve_forward_pod(client, ns, service, target.clone(), owner).await else {
                                return Err(first.into());
                            };
                            if next == target {
                                return Err(first.into());
                            }
                            *current.lock().await = next.clone();
                            let _ = tx.unbounded_send(ForwardMsg::Repointed(next.clone()));
                            api.portforward(&next, &[remote]).await?
                        }
                    };
                    let mut upstream = pf.take_stream(remote).context("no stream")?;
                    // The pod reports why it cannot connect (nothing listens on the port…) here.
                    let error = futures::future::OptionFuture::from(pf.take_error(remote));
                    tokio::select! {
                        copied = tokio::io::copy_bidirectional(&mut socket, &mut upstream) => {
                            copied?;
                        }
                        Some(Some(msg)) = error => anyhow::bail!("{}", pod_error(&msg, remote)),
                    }
                    anyhow::Ok(())
                }
                .await;
                if let Err(e) = result {
                    let _ = tx.unbounded_send(ForwardMsg::Error(format!("{e:#}")));
                }
            });
        }
    });
    (guard, rx)
}

/// The running pod to forward to now: behind the Service, the same pod, or a newer pod of
/// the same workload. `owner` is that workload's uid (see `forward_owner`), worked out here
/// when None. A pod no workload owns is never followed: a look-alike name proves nothing.
pub async fn resolve_forward_pod(
    client: Client,
    ns: String,
    service: Option<String>,
    pod: String,
    owner: Option<String>,
) -> Result<String> {
    use k8s_openapi::api::core::v1::Service;
    run(async move {
        let pods: Api<Pod> = Api::namespaced(client.clone(), &ns);
        if let Some(svc) = service {
            let svc = Api::<Service>::namespaced(client, &ns).get(&svc).await?;
            let selector: Vec<String> = svc
                .spec
                .and_then(|s| s.selector)
                .unwrap_or_default()
                .into_iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect();
            anyhow::ensure!(!selector.is_empty(), "the Service has no selector");
            let list = pods.list(&ListParams::default().labels(&selector.join(","))).await?;
            return list
                .items
                .iter()
                .find(|p| running(p))
                .and_then(|p| p.metadata.name.clone())
                .context("no running pod behind the Service");
        }
        if let Ok(p) = pods.get(&pod).await
            && running(&p)
        {
            return Ok(pod);
        }
        let owner = match owner {
            Some(o) => o,
            None => forward_owner(&client, &ns, &pod)
                .await
                .with_context(|| format!("{pod} is gone and belongs to no workload to follow"))?,
        };
        let list = pods.list(&ListParams::default()).await?;
        let replicasets = Api::<ReplicaSet>::namespaced(client, &ns).list(&ListParams::default()).await?;
        replacement(&list.items, &replicasets.items, &owner)
            .with_context(|| format!("{pod} is gone and no pod replaced it"))
    })
    .await
}

/// The pod's port-forward error, short when it is the usual one.
fn pod_error(msg: &str, port: u16) -> String {
    if msg.contains("connection refused") {
        format!("nothing listens on port {port} in the pod")
    } else {
        msg.trim().to_string()
    }
}

fn running(p: &Pod) -> bool {
    p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running") && p.metadata.deletion_timestamp.is_none()
}

/// The uid of the workload that runs `pod`, to recognise the pods that replace it: from the
/// pod itself while it exists, else the Deployment its name points at (after a restart the
/// old pod is long gone; that Deployment is a real object, looked up by name, not a pod).
async fn forward_owner(client: &Client, ns: &str, pod: &str) -> Option<String> {
    use k8s_openapi::api::apps::v1::Deployment;
    if let Ok(p) = Api::<Pod>::namespaced(client.clone(), ns).get(pod).await {
        let replicasets = Api::<ReplicaSet>::namespaced(client.clone(), ns).list(&ListParams::default()).await.ok()?;
        return workload_uid(&p, &replicasets.items);
    }
    let base = crate::forwards::pod_base(pod);
    if base == pod {
        return None;
    }
    Api::<Deployment>::namespaced(client.clone(), ns).get(base).await.ok()?.metadata.uid
}

/// The controller among an object's owners.
fn controller(meta: &ObjectMeta) -> Option<&OwnerReference> {
    meta.owner_references.as_ref()?.iter().find(|o| o.controller == Some(true))
}

/// The workload a pod belongs to: the Deployment behind its ReplicaSet, else its controller
/// (StatefulSet, DaemonSet, a bare ReplicaSet…). None for a pod nothing controls.
fn workload_uid(pod: &Pod, replicasets: &[ReplicaSet]) -> Option<String> {
    let owner = controller(&pod.metadata)?;
    if owner.kind == "ReplicaSet" {
        let rs = replicasets.iter().find(|rs| rs.metadata.uid.as_deref() == Some(owner.uid.as_str()))?;
        return Some(controller(&rs.metadata).map_or(&owner.uid, |d| &d.uid).clone());
    }
    Some(owner.uid.clone())
}

/// A running pod of the workload `owner`.
fn replacement(pods: &[Pod], replicasets: &[ReplicaSet], owner: &str) -> Option<String> {
    pods.iter()
        .filter(|p| running(p))
        .find(|p| workload_uid(p, replicasets).as_deref() == Some(owner))
        .and_then(|p| p.metadata.name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(name: &str, uid: &str, owner: Option<(&str, &str)>) -> ObjectMeta {
        ObjectMeta {
            name: Some(name.into()),
            uid: Some(uid.into()),
            owner_references: owner.map(|(kind, uid)| {
                vec![OwnerReference {
                    kind: kind.into(),
                    uid: uid.into(),
                    controller: Some(true),
                    ..Default::default()
                }]
            }),
            ..Default::default()
        }
    }

    fn pod(name: &str, owner: Option<(&str, &str)>) -> Pod {
        Pod {
            metadata: owned(name, &format!("uid-{name}"), owner),
            status: Some(k8s_openapi::api::core::v1::PodStatus { phase: Some("Running".into()), ..Default::default() }),
            ..Default::default()
        }
    }

    #[test]
    fn follows_the_owner_chain_not_the_name() {
        let rs = |name: &str, uid: &str, deploy: &str| ReplicaSet {
            metadata: owned(name, uid, Some(("Deployment", deploy))),
            ..Default::default()
        };
        let replicasets = [rs("web-old", "rs-old", "dep-web"), rs("web-new", "rs-new", "dep-web")];
        let old = pod("web-5c6d7e8f9-aaaaa", Some(("ReplicaSet", "rs-old")));
        let owner = workload_uid(&old, &replicasets).unwrap();
        assert_eq!(owner, "dep-web");
        let pods = [
            // Named like a replacement, but nothing owns it.
            pod("web-6d7e8f9a0-zzzzz", None),
            // Owned by an unrelated ReplicaSet.
            pod("web-6d7e8f9a0-yyyyy", Some(("ReplicaSet", "rs-other"))),
            pod("web-6d7e8f9a0-bbbbb", Some(("ReplicaSet", "rs-new"))),
        ];
        assert_eq!(replacement(&pods, &replicasets, &owner).as_deref(), Some("web-6d7e8f9a0-bbbbb"));
        assert_eq!(replacement(&pods[..2], &replicasets, &owner), None);
    }

    #[test]
    fn pod_errors_are_short() {
        let refused = "error forwarding port 8080 to pod 1a2b, uid : failed to execute portforward in network \
                       namespace \"/var/run/netns/cni-1\": failed to connect to localhost:8080 inside namespace \
                       \"1a2b\", IPv4: dial tcp4 127.0.0.1:8080: connect: connection refused";
        assert_eq!(pod_error(refused, 8080), "nothing listens on port 8080 in the pod");
        assert_eq!(pod_error("pod not found\n", 80), "pod not found");
    }

    #[test]
    fn bare_pods_are_not_followed() {
        assert_eq!(workload_uid(&pod("debug", None), &[]), None);
        let sts = pod("db-0", Some(("StatefulSet", "sts-db")));
        assert_eq!(workload_uid(&sts, &[]).as_deref(), Some("sts-db"));
    }
}
