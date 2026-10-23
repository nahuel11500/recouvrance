//! What a detail page fetches next to the object: pods, revisions, references, usage history.

#[allow(unused_imports)]
use super::*;

pub(super) fn container_names(value: &serde_json::Value) -> Vec<String> {
    value
        .pointer("/spec/containers")
        .and_then(|c| c.as_array())
        .map(|cs| cs.iter().filter_map(|c| c["name"].as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// Everything the overview links to or measures for one object.
/// CPU (millicores) and memory (bytes) of a pod, workload (its pods) or node from Prometheus.
pub(super) async fn history_for(
    client: Client,
    context: &str,
    kind: &k8s::Kind,
    ns: Option<&str>,
    obj: &serde_json::Value,
    pods: &[serde_json::Value],
    minutes: i64,
) -> Option<(crate::prom::Series, crate::prom::Series)> {
    let target = crate::prom::target(context, client.clone()).await.ok()??;
    let name = obj.pointer("/metadata/name").and_then(|v| v.as_str())?;
    let (cpu, mem) = if kind.is("", "Node") {
        crate::prom::node_usage(client, target, name, minutes).await.ok()?
    } else {
        let names: Vec<String> = if kind.is("", "Pod") {
            vec![name.to_string()]
        } else {
            pods.iter()
                .take(crate::prom::MAX_CHART_PODS)
                .filter_map(|p| p.pointer("/metadata/name").and_then(|v| v.as_str()).map(String::from))
                .collect()
        };
        if names.is_empty() {
            return None;
        }
        crate::prom::pod_usage(client, target, ns?, &names, minutes).await.ok()?
    };
    // Cores to millicores, like the rest of the UI.
    Some((cpu.into_iter().map(|(t, v)| (t, v * 1000.0)).collect(), mem))
}

/// A pod's CPU and memory history per container (empty for other kinds or without Prometheus).
pub(super) async fn container_history_for(
    client: Client,
    context: &str,
    kind: &k8s::Kind,
    ns: Option<&str>,
    obj: &serde_json::Value,
    minutes: i64,
) -> std::collections::HashMap<String, (crate::prom::Series, crate::prom::Series)> {
    if !kind.is("", "Pod") {
        return Default::default();
    }
    let (Some(ns), Some(name)) = (ns, obj.pointer("/metadata/name").and_then(|v| v.as_str())) else {
        return Default::default();
    };
    let Ok(Some(target)) = crate::prom::target(context, client.clone()).await else { return Default::default() };
    crate::prom::container_usage(client, target, ns, name, minutes).await.unwrap_or_default()
}

/// GPU history for the same object as `history_for` (None without GPU metrics).
pub(super) async fn gpu_history_for(
    client: Client,
    context: &str,
    kind: &k8s::Kind,
    ns: Option<&str>,
    obj: &serde_json::Value,
    pods: &[serde_json::Value],
    minutes: i64,
) -> Option<crate::prom::GpuUsage> {
    let target = crate::prom::target(context, client.clone()).await.ok()??;
    let name = obj.pointer("/metadata/name").and_then(|v| v.as_str())?;
    if kind.is("", "Node") {
        return crate::prom::gpu_usage(client, target, crate::prom::GpuScope::Node(name), minutes).await;
    }
    let names: Vec<String> = if kind.is("", "Pod") {
        vec![name.to_string()]
    } else {
        pods.iter()
            .take(crate::prom::MAX_CHART_PODS)
            .filter_map(|p| p.pointer("/metadata/name").and_then(|v| v.as_str()).map(String::from))
            .collect()
    };
    if names.is_empty() {
        return None;
    }
    crate::prom::gpu_usage(client, target, crate::prom::GpuScope::Pods { ns: ns?, pods: &names }, minutes).await
}

pub(super) async fn fetch_related(
    client: Client,
    kind: &k8s::Kind,
    ns: Option<String>,
    obj: &serde_json::Value,
) -> Related {
    use k8s_openapi::api::{apps::v1 as apps, batch::v1 as batch, core::v1 as core};
    let mut related = Related::default();
    if kind.is("", "Node") {
        let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default();
        let pods = k8s::list_values_fields(
            client.clone(),
            k8s::ar_of::<core::Pod>(),
            None,
            None,
            Some(format!("spec.nodeName={name}")),
        )
        .await;
        related.pods = related.noted("pods", pods);
        related.node_stats = k8s::node_stats(client.clone(), name.to_string()).await;
        related.extra = k8s::resource_slices(client, name.to_string()).await;
        return related;
    }
    if kind.is("", "Namespace") {
        let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let quotas =
            k8s::list_values(client.clone(), k8s::ar_of::<core::ResourceQuota>(), Some(name.clone()), None).await;
        related.extra = related.noted("extra", quotas);
        let pods = k8s::list_values(client, k8s::ar_of::<core::Pod>(), Some(name), None).await;
        related.pods = related.noted("pods", pods);
        return related;
    }
    if kind.ar.group == "rbac.authorization.k8s.io" {
        // Cluster-wide roles may be bound in any namespace.
        let scope = if kind.namespaced { ns.clone() } else { None };
        related.rbac = Some(k8s::rbac_snapshot(client, scope).await);
        return related;
    }
    if kind.is("autoscaling.k8s.io", "VerticalPodAutoscaler") {
        let (Some(av), Some(tk), Some(tn), Some(ns)) = (
            obj.pointer("/spec/targetRef/apiVersion").and_then(|v| v.as_str()),
            obj.pointer("/spec/targetRef/kind").and_then(|v| v.as_str()),
            obj.pointer("/spec/targetRef/name").and_then(|v| v.as_str()),
            ns.as_deref(),
        ) else {
            return related;
        };
        let prefix = if av.contains('/') { "apis" } else { "api" };
        related.extra = k8s::get_json(client, format!("/{prefix}/{av}/namespaces/{ns}/{}s/{tn}", tk.to_lowercase()))
            .await
            .into_iter()
            .collect();
        return related;
    }
    let Some(ns) = ns else { return related };
    if kind.is("", "ServiceAccount") {
        let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let pods = k8s::list_values_fields(
            client.clone(),
            k8s::ar_of::<core::Pod>(),
            Some(ns.clone()),
            None,
            Some(format!("spec.serviceAccountName={name}")),
        )
        .await;
        related.pods = related.noted("pods", pods);
        related.rbac = Some(k8s::rbac_snapshot(client, None).await);
        return related;
    }
    if kind.is("policy", "PodDisruptionBudget") {
        let sel = obj.pointer("/spec/selector").and_then(k8s::selector_string);
        let pods = k8s::list_values(client, k8s::ar_of::<core::Pod>(), Some(ns), sel).await;
        related.pods = related.noted("pods", pods);
        return related;
    }
    if kind.is("", "PersistentVolumeClaim") {
        let claim = obj.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let pods = k8s::list_values(client, k8s::ar_of::<core::Pod>(), Some(ns), None).await;
        related.pods = related
            .noted("pods", pods)
            .into_iter()
            .filter(|p| {
                p.pointer("/spec/volumes").and_then(|v| v.as_array()).is_some_and(|vs| {
                    vs.iter().any(|v| {
                        v.pointer("/persistentVolumeClaim/claimName").and_then(|c| c.as_str()) == Some(claim.as_str())
                    })
                })
            })
            .collect();
        return related;
    }
    if kind.is("batch", "CronJob") {
        let uid = obj.pointer("/metadata/uid").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let jobs = k8s::list_values(client, k8s::ar_of::<batch::Job>(), Some(ns), None).await;
        related.extra = related
            .noted("extra", jobs)
            .into_iter()
            .filter(|j| {
                j.pointer("/metadata/ownerReferences")
                    .and_then(|o| o.as_array())
                    .is_some_and(|o| o.iter().any(|r| r["uid"].as_str() == Some(uid.as_str())))
            })
            .collect();
        return related;
    }
    if kind.is("networking.k8s.io", "NetworkPolicy") {
        // The pods this policy applies to (an empty podSelector means all pods).
        let sel = obj.pointer("/spec/podSelector").and_then(k8s::selector_string);
        let pods = k8s::list_values(client, k8s::ar_of::<core::Pod>(), Some(ns), sel).await;
        related.pods = related.noted("pods", pods);
        return related;
    }
    if kind.is("", "Pod") {
        let selector = None;
        // Metrics are optional (no metrics-server): no usage is not an error to show.
        if crate::settings::get().metrics_enabled {
            related.usage = k8s::container_usage(client.clone(), ns.clone(), selector).await.unwrap_or_default();
        }
        // ConfigMaps and Secrets the environment reads, to show resolved values.
        let mut refs: std::collections::BTreeSet<(&str, String)> = Default::default();
        for c in obj.pointer("/spec/containers").and_then(|c| c.as_array()).into_iter().flatten() {
            for e in c["env"].as_array().into_iter().flatten() {
                if let Some(n) = e.pointer("/valueFrom/configMapKeyRef/name").and_then(|v| v.as_str()) {
                    refs.insert(("ConfigMap", n.to_string()));
                }
                if let Some(n) = e.pointer("/valueFrom/secretKeyRef/name").and_then(|v| v.as_str()) {
                    refs.insert(("Secret", n.to_string()));
                }
            }
            for e in c["envFrom"].as_array().into_iter().flatten() {
                if let Some(n) = e.pointer("/configMapRef/name").and_then(|v| v.as_str()) {
                    refs.insert(("ConfigMap", n.to_string()));
                }
                if let Some(n) = e.pointer("/secretRef/name").and_then(|v| v.as_str()) {
                    refs.insert(("Secret", n.to_string()));
                }
            }
        }
        for (kind, name) in refs {
            let ar = if kind == "Secret" { k8s::ar_of::<core::Secret>() } else { k8s::ar_of::<core::ConfigMap>() };
            if let Some(v) = k8s::list_values_fields(
                client.clone(),
                ar,
                Some(ns.clone()),
                None,
                Some(format!("metadata.name={name}")),
            )
            .await
            .ok()
            .and_then(|l| l.into_iter().next())
            {
                related.env_sources.insert(format!("{kind}/{name}"), v);
            }
        }
        // NetworkPolicies whose podSelector selects this pod.
        let labels = obj.pointer("/metadata/labels").and_then(|l| l.as_object()).cloned().unwrap_or_default();
        related.netpols = k8s::list_values(
            client.clone(),
            k8s::ar_of::<k8s_openapi::api::networking::v1::NetworkPolicy>(),
            Some(ns.clone()),
            None,
        )
        .await
        .ok()
        .map(|all| {
            all.into_iter()
                .filter(|p| p.pointer("/spec/podSelector").is_some_and(|s| k8s::selector_matches(s, &labels)))
                .collect()
        });
        // Events and the last crash output, for the diagnosis.
        let uid = obj.pointer("/metadata/uid").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let objref = ObjectRef { kind: kind.clone(), name: name.clone(), namespace: Some(ns.clone()), uid };
        let events = k8s::events_for(client.clone(), objref).await;
        related.events =
            related.noted("events", events).into_iter().map(|e| (e.kind, e.reason, e.message, e.count)).collect();
        // A container that failed: its previous run if it restarted, else its current one.
        let failed = |c: &&serde_json::Value| {
            let code = |p: &str| c.pointer(p).and_then(|v| v.as_i64()).unwrap_or(0);
            code("/lastState/terminated/exitCode") != 0 || code("/state/terminated/exitCode") != 0
        };
        let crashing = obj
            .pointer("/status/containerStatuses")
            .and_then(|c| c.as_array())
            .and_then(|cs| cs.iter().find(failed))
            .and_then(|c| Some((c["name"].as_str()?.to_string(), c.pointer("/state/terminated").is_none())));
        if let Some((container, previous)) = crashing
            && let Ok(text) =
                k8s::logs_text(client.clone(), ns.clone(), name, Some(container.clone()), 30, None, previous).await
        {
            related.crash_logs = Some((container, text));
        }
        if let Some(node) = obj.pointer("/spec/nodeName").and_then(|v| v.as_str()) {
            related.node_stats = k8s::node_stats(client.clone(), node.to_string()).await;
        }
        let services = k8s::list_values(client, k8s::ar_of::<core::Service>(), Some(ns), None).await;
        related.services = related.noted("services", services);
    } else if kind.ar.group == "apps" || kind.is("batch", "Job") {
        let selector = obj.pointer("/spec/selector").and_then(k8s::selector_string);
        if let Some(sel) = selector {
            let pods =
                k8s::list_values(client.clone(), k8s::ar_of::<core::Pod>(), Some(ns.clone()), Some(sel.clone())).await;
            related.pods = related.noted("pods", pods);
            let services =
                k8s::list_values(client.clone(), k8s::ar_of::<core::Service>(), Some(ns.clone()), None).await;
            related.services = related.noted("services", services);
            if crate::settings::get().metrics_enabled {
                related.usage =
                    k8s::container_usage(client.clone(), ns.clone(), Some(sel.clone())).await.unwrap_or_default();
            }
            if kind.is("apps", "StatefulSet") || kind.is("apps", "DaemonSet") {
                let uid = obj.pointer("/metadata/uid").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                related.revisions = k8s::controller_revisions(client.clone(), ns.clone(), uid).await;
            }
            if kind.is("apps", "Deployment") {
                let uid = obj.pointer("/metadata/uid").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let sets = k8s::list_values(client, k8s::ar_of::<apps::ReplicaSet>(), Some(ns), Some(sel)).await;
                related.revisions = related
                    .noted("revisions", sets)
                    .into_iter()
                    .filter(|rs| {
                        rs.pointer("/metadata/ownerReferences")
                            .and_then(|o| o.as_array())
                            .is_some_and(|o| o.iter().any(|r| r["uid"].as_str() == Some(uid.as_str())))
                    })
                    .collect();
            }
        }
    } else if kind.is("", "Service") {
        let selector: Option<String> =
            obj.pointer("/spec/selector").and_then(|s| s.as_object()).filter(|m| !m.is_empty()).map(|m| {
                m.iter().map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default())).collect::<Vec<_>>().join(",")
            });
        if let Some(sel) = selector {
            let pods = k8s::list_values(client, k8s::ar_of::<core::Pod>(), Some(ns), Some(sel)).await;
            related.pods = related.noted("pods", pods);
        }
    } else if kind.is("", "Secret") || kind.is("", "ConfigMap") {
        // Who mounts or reads this object, by scanning workloads in the namespace.
        let name = obj.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        let is_secret = kind.is("", "Secret");
        let sources = [
            ("deployments", k8s::ar_of::<apps::Deployment>(), "/spec/template/spec"),
            ("statefulsets", k8s::ar_of::<apps::StatefulSet>(), "/spec/template/spec"),
            ("daemonsets", k8s::ar_of::<apps::DaemonSet>(), "/spec/template/spec"),
            ("cronjobs", k8s::ar_of::<batch::CronJob>(), "/spec/jobTemplate/spec/template/spec"),
            ("pods", k8s::ar_of::<core::Pod>(), "/spec"),
        ];
        for (label, ar, spec_path) in sources {
            let items = k8s::list_values(client.clone(), ar, Some(ns.clone()), None).await;
            // One kind refused must not read as "nobody uses it": say which could not be checked.
            let items = items.unwrap_or_else(|e| {
                let note = format!("{label} {}", k8s::lists::load_error(&e));
                related.errors.entry("referenced_by").and_modify(|n| *n = format!("{n}, {note}")).or_insert(note);
                Vec::new()
            });
            let names: Vec<String> = items
                .iter()
                .filter(|item| item.pointer(spec_path).is_some_and(|spec| references(spec, &name, is_secret)))
                .filter_map(|item| item.pointer("/metadata/name").and_then(|v| v.as_str()).map(String::from))
                .collect();
            if !names.is_empty() {
                related.referenced_by.insert(label.to_string(), names);
            }
        }
    }
    related
}

/// Does a pod spec use this Secret/ConfigMap (volumes, env, envFrom, pull secrets)?
pub(super) fn references(spec: &serde_json::Value, name: &str, secret: bool) -> bool {
    let is = |v: Option<&serde_json::Value>| v.and_then(|v| v.as_str()) == Some(name);
    let volumes = spec["volumes"].as_array().into_iter().flatten();
    let in_volumes = volumes
        .clone()
        .any(|v| if secret { is(v.pointer("/secret/secretName")) } else { is(v.pointer("/configMap/name")) })
        || volumes
            .flat_map(|v| v.pointer("/projected/sources").and_then(|s| s.as_array()).into_iter().flatten())
            .any(|s| if secret { is(s.pointer("/secret/name")) } else { is(s.pointer("/configMap/name")) });
    let containers = ["containers", "initContainers"].iter().flat_map(|k| spec[*k].as_array().into_iter().flatten());
    let in_env = containers.clone().any(|c| {
        c["env"].as_array().into_iter().flatten().any(|e| {
            if secret {
                is(e.pointer("/valueFrom/secretKeyRef/name"))
            } else {
                is(e.pointer("/valueFrom/configMapKeyRef/name"))
            }
        }) || c["envFrom"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|e| if secret { is(e.pointer("/secretRef/name")) } else { is(e.pointer("/configMapRef/name")) })
    });
    let in_pull = secret && spec["imagePullSecrets"].as_array().into_iter().flatten().any(|p| is(p.get("name")));
    in_volumes || in_env || in_pull
}
