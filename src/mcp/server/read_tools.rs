//! MCP tools that read: contexts, types, resources, events, describe, top, search, logs, certificates, problems, usage history.

#[allow(unused_imports)]
use super::*;

#[tool_router(router = read_tools, vis = "pub(super)")]
impl KubeMcp {
    #[tool(
        description = "Explain what you are allowed to do: your access rules (effect, verbs, resources, namespaces, contexts, names) and limits. Read this first to avoid denied calls.",
        annotations(read_only_hint = true)
    )]
    async fn my_permissions(&self, Extension(parts): Extension<Parts>) -> Result<String, String> {
        let call = self.begin(&parts, "my_permissions", json!({}))?;
        let p = call.policy();
        let mut out = String::from(
            "Rules are checked together: Deny wins over Ask (needs the user's approval) over Allow; anything not matched is denied.\n\n",
        );
        for (i, r) in p.rules.iter().enumerate() {
            if r.expired() {
                continue;
            }
            let list = |v: &Vec<String>| if v.is_empty() { "(none)".to_string() } else { v.join(", ") };
            out.push_str(&format!(
                "#{} {:?}: verbs [{}] on resources [{}] in namespaces [{}], contexts [{}], names [{}]{}{}\n",
                i + 1,
                r.effect,
                list(&r.verbs),
                list(&r.resources),
                list(&r.namespaces),
                list(&r.contexts),
                list(&r.names),
                r.expires.map(|t| format!(", until {t}")).unwrap_or_default(),
                if r.note.is_empty() { String::new() } else { format!("  — {}", r.note) },
            ));
        }
        out.push_str(&format!(
            "\nLimits: {} rows per list, {} log lines per call, exec timeout {}s, {} calls per minute.\nWrite tools accept dry_run=true: prefer it first.",
            p.max_rows, p.max_log_lines, p.exec_timeout, p.max_calls_per_minute
        ));
        call.finish(Ok(out))
    }

    #[tool(description = "List the Kubernetes contexts (clusters) you may access.", annotations(read_only_hint = true))]
    pub(super) async fn list_contexts(&self, Extension(parts): Extension<Parts>) -> Result<String, String> {
        let call = self.begin(&parts, "list_contexts", json!({}))?;
        // The contexts a call may use: the same check as when one connects.
        let names: Vec<String> = k8s::load_contexts()
            .map(|c| c.names)
            .unwrap_or_default()
            .into_iter()
            .filter(|c| call.policy().context_allowed(c))
            .collect();
        call.finish(Ok(if names.is_empty() {
            "No contexts are allowed for this agent. Ask the user to add rules in Recouvrance.".into()
        } else {
            names.join("\n")
        }))
    }

    #[tool(
        description = "List the resource types available in a context that you may read, with their names for the `kind` parameter.",
        annotations(read_only_hint = true)
    )]
    async fn list_resource_types(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ContextParam>,
    ) -> Result<String, String> {
        let call = self.begin(&parts, "list_resource_types", json!({"context": p.context}))?;
        let kinds = match self.hub.client(&p.context).await {
            Ok((_, kinds)) => kinds,
            Err(e) => return call.finish(Err(e)),
        };
        let rows: Vec<Vec<String>> = kinds
            .iter()
            .filter(|k| list_verdict(call.policy(), k, &p.context) != Mode::Deny)
            .map(|k| vec![k.kubectl_name(), k.ar.kind.clone(), k.ar.api_version.clone(), k.namespaced.to_string()])
            .collect();
        if rows.is_empty() {
            return Err(call.deny(format!("nothing is readable in context {:?}", p.context)));
        }
        call.finish(Ok(format_table(&["NAME", "KIND", "APIVERSION", "NAMESPACED"], &rows)))
    }

    #[tool(
        description = "List resources like `kubectl get`: a table with the same columns kubectl prints.",
        annotations(read_only_hint = true)
    )]
    async fn list_resources(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ListParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace,
            "label_selector": p.label_selector, "field_selector": p.field_selector});
        let mut call = self.begin(&parts, "list_resources", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        let access = access_for(&kind, &p.context, "list", p.namespace.clone(), None);
        // A cluster-scoped kind is in no namespace, not in all of them.
        self.authorize_list(&mut call, &access, kind.namespaced && p.namespace.is_none()).await?;
        let (defs, rows) = match k8s::list_table(
            client,
            kind.clone(),
            p.namespace.clone(),
            p.label_selector,
            p.field_selector,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => return call.finish(Err(e)),
        };
        // Rows are filtered by the same rules, per namespace and name.
        let policy = call.policy();
        let visible: Vec<_> = rows
            .iter()
            .filter(|r| call.may_see(&access_for(&kind, &p.context, "list", r.namespace.clone(), Some(r.name.clone()))))
            .collect();
        let hidden = rows.len() - visible.len();
        let wide = p.wide.unwrap_or(false);
        let cols: Vec<usize> = (0..defs.len()).filter(|&i| wide || defs[i].priority == 0).collect();
        let show_ns = kind.namespaced && p.namespace.is_none();
        let mut headers: Vec<String> = Vec::new();
        if show_ns {
            headers.push("NAMESPACE".into());
        }
        headers.extend(cols.iter().map(|&i| defs[i].name.to_uppercase()));
        let max = policy.max_rows as usize;
        let table_rows: Vec<Vec<String>> = visible
            .iter()
            .take(max)
            .map(|r| {
                let mut row = Vec::new();
                if show_ns {
                    row.push(r.namespace.clone().unwrap_or_default());
                }
                row.extend(cols.iter().map(|&i| {
                    if defs[i].name == "Age" {
                        r.created.map(k8s::age).unwrap_or_default()
                    } else {
                        r.cells.get(i).cloned().unwrap_or_default()
                    }
                }));
                row
            })
            .collect();
        let header_refs: Vec<&str> = headers.iter().map(String::as_str).collect();
        let mut out = if table_rows.is_empty() {
            "No resources found.".to_string()
        } else {
            format_table(&header_refs, &table_rows)
        };
        if visible.len() > max {
            out.push_str(&format!(
                "\n… {} more not shown (limit {max}); narrow with namespace or selectors.",
                visible.len() - max
            ));
        }
        if hidden > 0 {
            out.push_str(&format!("\n({hidden} items you may not access were omitted.)"));
        }
        call.finish(Ok(out))
    }

    #[tool(
        description = "Get one resource as YAML, like `kubectl get -o yaml` (managedFields removed; Secret values masked unless you may reveal them).",
        annotations(read_only_hint = true)
    )]
    async fn get_resource(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ObjectParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "name": p.name, "namespace": p.namespace});
        let mut call = self.begin(&parts, "get_resource", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        require_ns(&call, &kind, &p.namespace)?;
        let access = access_for(&kind, &p.context, "get", p.namespace.clone(), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        let reveal = kind.is("", "Secret") && {
            let a = access_for(&kind, &p.context, "reveal", p.namespace.clone(), Some(p.name.clone()));
            call.policy().decide(&a).mode == Mode::Allow
        };
        let ns = if kind.namespaced { p.namespace } else { None };
        let obj = ObjectRef { kind: kind.clone(), name: p.name.clone(), namespace: ns.clone(), uid: String::new() };
        let value = match k8s::get_object(client, obj).await {
            Ok(v) => v,
            Err(e) => return call.finish(Err(e)),
        };
        if let Err(e) = check_identity(&kind, ns.as_deref(), &p.name, &value) {
            return call.finish(Err(e));
        }
        // Masking follows what came back, not only what was asked for.
        let secret = kind.is("", "Secret") || is_secret_object(&value);
        call.finish(k8s::to_yaml(&value, secret, reveal))
    }

    #[tool(
        description = "List Kubernetes events, newest first, optionally about one object.",
        annotations(read_only_hint = true)
    )]
    async fn get_events(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<EventsParams>,
    ) -> Result<String, String> {
        let args =
            json!({"context": p.context, "namespace": p.namespace, "name": p.name, "object_kind": p.object_kind});
        let mut call = self.begin(&parts, "get_events", args)?;
        let (client, kind) = self.kind(&call, &p.context, "events").await?;
        let access = access_for(&kind, &p.context, "list", p.namespace.clone(), None);
        self.authorize_list(&mut call, &access, p.namespace.is_none()).await?;
        let mut fields = Vec::new();
        if let Some(n) = &p.name {
            fields.push(format!("involvedObject.name={n}"));
        }
        if let Some(k) = &p.object_kind {
            fields.push(format!("involvedObject.kind={k}"));
        }
        let selector = (!fields.is_empty()).then(|| fields.join(","));
        let (defs, mut rows) = match k8s::list_table(client, kind.clone(), p.namespace.clone(), None, selector).await {
            Ok(r) => r,
            Err(e) => return call.finish(Err(e)),
        };
        rows.retain(|r| {
            call.may_see(&access_for(&kind, &p.context, "list", r.namespace.clone(), Some(r.name.clone())))
        });
        // Newest first by when they last happened (an event that keeps repeating was created
        // long ago), as the server's Last Seen column says.
        let last_seen = defs.iter().position(|d| d.name == "Last Seen");
        let ago = |r: &k8s::Row| last_seen.and_then(|i| r.cells.get(i)).and_then(|c| k8s::age_seconds(c));
        rows.sort_by_key(|r| (ago(r).unwrap_or(i64::MAX), std::cmp::Reverse(r.created)));
        let max = call.policy().max_rows as usize;
        let cols: Vec<usize> = (0..defs.len()).filter(|&i| defs[i].priority == 0).collect();
        let mut headers = vec!["NAMESPACE".to_string()];
        headers.extend(cols.iter().map(|&i| defs[i].name.to_uppercase()));
        let table_rows: Vec<Vec<String>> = rows
            .iter()
            .take(max)
            .map(|r| {
                let mut row = vec![r.namespace.clone().unwrap_or_default()];
                row.extend(cols.iter().map(|&i| r.cells.get(i).cloned().unwrap_or_default()));
                row
            })
            .collect();
        if table_rows.is_empty() {
            return call.finish(Ok("No events found.".into()));
        }
        let header_refs: Vec<&str> = headers.iter().map(String::as_str).collect();
        call.finish(Ok(format_table(&header_refs, &table_rows)))
    }

    #[tool(
        description = "Describe one object like `kubectl describe`: key fields, conditions, recent events, and for workloads/services the pods behind it with their status and restarts. Start here when investigating a problem.",
        annotations(read_only_hint = true)
    )]
    async fn describe_resource(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ObjectParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "name": p.name, "namespace": p.namespace});
        let mut call = self.begin(&parts, "describe_resource", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        require_ns(&call, &kind, &p.namespace)?;
        let access = access_for(&kind, &p.context, "get", p.namespace.clone(), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        let ns = if kind.namespaced { p.namespace.clone() } else { None };
        let obj = ObjectRef { kind: kind.clone(), name: p.name.clone(), namespace: ns.clone(), uid: String::new() };
        let value = match k8s::get_object(client.clone(), obj.clone()).await {
            Ok(v) => v,
            Err(e) => return call.finish(Err(e)),
        };
        if let Err(e) = check_identity(&kind, ns.as_deref(), &p.name, &value) {
            return call.finish(Err(e));
        }
        let mut out = format!(
            "{} {}{}\n",
            kind.ar.kind,
            p.name,
            ns.as_ref().map(|n| format!(" (namespace {n})")).unwrap_or_default()
        );
        let get = |path: &str| {
            value.pointer(path).map(|v| match v.as_str() {
                Some(s) => s.to_string(),
                None => v.to_string(),
            })
        };
        for (label, path) in [
            ("Created", "/metadata/creationTimestamp"),
            ("Phase", "/status/phase"),
            ("Node", "/spec/nodeName"),
            ("Replicas", "/spec/replicas"),
            ("Ready replicas", "/status/readyReplicas"),
            ("Type", "/spec/type"),
            ("Cluster IP", "/spec/clusterIP"),
            ("Schedule", "/spec/schedule"),
        ] {
            if let Some(v) = get(path) {
                out.push_str(&format!("{label}: {v}\n"));
            }
        }
        if let Some(conds) = value.pointer("/status/conditions").and_then(|c| c.as_array()) {
            out.push_str("Conditions:\n");
            for c in conds {
                out.push_str(&format!(
                    "  {} = {}  {} {}\n",
                    c["type"].as_str().unwrap_or("?"),
                    c["status"].as_str().unwrap_or("?"),
                    c["reason"].as_str().unwrap_or(""),
                    c["message"].as_str().unwrap_or("")
                ));
            }
        }
        for c in value.pointer("/status/containerStatuses").and_then(|c| c.as_array()).into_iter().flatten() {
            let state = c["state"].as_object().and_then(|m| m.keys().next().cloned()).unwrap_or_default();
            let reason = c.pointer(&format!("/state/{state}/reason")).and_then(|v| v.as_str()).unwrap_or("");
            let last = c
                .pointer("/lastState/terminated")
                .map(|t| format!(", last terminated: {} (exit {})", t["reason"].as_str().unwrap_or("?"), t["exitCode"]))
                .unwrap_or_default();
            out.push_str(&format!(
                "Container {}: {state} {reason}, ready={}, restarts={}{last}\n",
                c["name"].as_str().unwrap_or("?"),
                c["ready"],
                c["restartCount"]
            ));
        }
        // Pods behind workloads, services and budgets, picked as the app's pages pick them.
        let selects_pods = kind.is("", "Service")
            || kind.is("policy", "PodDisruptionBudget")
            || kind.ar.group == "apps"
            || kind.is("batch", "Job");
        let selector = selects_pods.then(|| k8s::pod_selection(&kind, &value).list_selector()).flatten();
        if let (Some(sel), Some(ns)) = (selector, ns.clone()) {
            let pods_kind = self.kind(&call, &p.context, "pods").await?.1;
            if call.policy().decide(&access_for(&pods_kind, &p.context, "list", Some(ns.clone()), None)).mode
                == Mode::Allow
                && let Ok((defs, mut rows)) =
                    k8s::list_table(client.clone(), pods_kind.clone(), Some(ns.clone()), sel, None).await
            {
                rows.retain(|r| {
                    let a = access_for(&pods_kind, &p.context, "list", Some(ns.clone()), Some(r.name.clone()));
                    call.policy().decide(&a).mode == Mode::Allow
                });
                let cols: Vec<usize> =
                    (0..defs.len()).filter(|&i| defs[i].priority == 0 || defs[i].name == "Node").collect();
                out.push_str(&format!("Pods ({}):\n", rows.len()));
                for r in rows.iter().take(30) {
                    let cells: Vec<String> =
                        cols.iter().map(|&i| r.cells.get(i).cloned().unwrap_or_default()).collect();
                    out.push_str(&format!("  {}\n", cells.join("  ")));
                }
            }
        }
        let obj = ObjectRef {
            uid: value.pointer("/metadata/uid").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            ..obj
        };
        let events_kind = self.kind(&call, &p.context, "events").await?.1;
        if call.policy().decide(&access_for(&events_kind, &p.context, "list", ns.clone(), None)).mode == Mode::Allow
            && let Ok(mut events) = k8s::events_for(client, obj).await
        {
            // The check above has no name: an Allow of some event names passes it.
            events.retain(|e| {
                let a = access_for(&events_kind, &p.context, "list", e.namespace.clone(), Some(e.name.clone()));
                call.policy().decide(&a).mode == Mode::Allow
            });
            out.push_str(&format!("Events ({}):\n", events.len()));
            for e in events.iter().take(20) {
                out.push_str(&format!("  {} ago  {}  {}  x{}  {}\n", e.age, e.kind, e.reason, e.count, e.message));
            }
        }
        call.finish(Ok(out))
    }

    #[tool(
        description = "Pods using the most CPU or memory, like `kubectl top pods --sort-by`.",
        annotations(read_only_hint = true)
    )]
    async fn top_pods(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<TopParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "sort_by": p.sort_by});
        let mut call = self.begin(&parts, "top_pods", args)?;
        let (client, kind) = self.kind(&call, &p.context, "pods").await?;
        let access = access_for(&kind, &p.context, "list", p.namespace.clone(), None);
        self.authorize_list(&mut call, &access, p.namespace.is_none()).await?;
        let usage = match &p.namespace {
            Some(ns) => k8s::container_usage(client, ns.clone(), None)
                .await
                .map(|u| u.into_iter().map(|(pod, c)| ((ns.clone(), pod), c)).collect::<Vec<_>>()),
            None => k8s::all_container_usage(client).await,
        };
        let usage = match usage {
            Ok(u) => u,
            Err(e) => return call.finish(Err(e)),
        };
        let mut rows: Vec<(String, String, f64, f64)> = usage
            .into_iter()
            .filter(|((ns, pod), _)| {
                call.may_see(&access_for(&kind, &p.context, "list", Some(ns.clone()), Some(pod.clone())))
            })
            .map(|((ns, pod), c)| (ns, pod, c.values().map(|v| v.0).sum(), c.values().map(|v| v.1).sum()))
            .collect();
        let by_mem = p.sort_by.as_deref() == Some("memory");
        rows.sort_by(|a, b| {
            let (x, y) = if by_mem { (a.3, b.3) } else { (a.2, b.2) };
            y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal)
        });
        let table: Vec<Vec<String>> = rows
            .into_iter()
            .take(p.limit.unwrap_or(20) as usize)
            .map(|(ns, pod, cpu, mem)| vec![ns, pod, k8s::format_cpu(cpu), k8s::format_bytes(mem)])
            .collect();
        call.finish(Ok(format_table(&["NAMESPACE", "POD", "CPU", "MEMORY"], &table)))
    }

    #[tool(
        description = "Find objects by name across common resource types in a context (deployments, statefulsets, services, ingresses, configmaps, pods, …).",
        annotations(read_only_hint = true)
    )]
    async fn search(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<SearchParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "query": p.query});
        let call = self.begin(&parts, "search", args)?;
        let (client, kinds) = match self.hub.client(&p.context).await {
            Ok(c) => c,
            Err(e) => return call.finish(Err(e)),
        };
        let q = p.query.to_lowercase();
        let mut rows = Vec::new();
        // The kinds that couldn't be listed, with why: the agent must not read them as empty.
        let (mut searched, mut failed) = (0, Vec::new());
        for name in [
            "deployments",
            "statefulsets",
            "daemonsets",
            "cronjobs",
            "services",
            "ingresses",
            "configmaps",
            "persistentvolumeclaims",
            "pods",
            "namespaces",
            "nodes",
        ] {
            let Some(kind) = k8s::find_kind(&kinds, name).cloned() else { continue };
            if list_verdict(call.policy(), &kind, &p.context) == Mode::Deny {
                continue;
            }
            searched += 1;
            let names = match k8s::list_names(client.clone(), kind.ar.clone(), None).await {
                Ok(names) => names,
                Err(e) => {
                    failed.push(format!("{} ({e:#})", kind.kubectl_name()));
                    continue;
                }
            };
            for (ns, obj) in names {
                if !obj.to_lowercase().contains(&q) {
                    continue;
                }
                // Search asks nobody: only what a rule allows outright.
                if !call.may_see(&access_for(&kind, &p.context, "list", ns.clone(), Some(obj.clone()))) {
                    continue;
                }
                rows.push(vec![kind.kubectl_name(), ns.unwrap_or_default(), obj]);
            }
        }
        rows.truncate(call.policy().max_rows as usize);
        if searched > 0 && failed.len() == searched {
            return call.finish(Err(anyhow::anyhow!("couldn't search: {}", failed.join("; "))));
        }
        let mut out = if rows.is_empty() {
            format!("Nothing named like {:?}.", p.query)
        } else {
            format_table(&["KIND", "NAMESPACE", "NAME"], &rows)
        };
        if !failed.is_empty() {
            out.push_str(&format!("\nNot searched, the list failed: {}", failed.join("; ")));
        }
        call.finish(Ok(out))
    }

    #[tool(
        description = "Read a pod's container logs (most recent lines, at most 1 MiB), like `kubectl logs --tail`.",
        annotations(read_only_hint = true)
    )]
    async fn get_logs(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<LogsParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "pod": p.pod, "container": p.container,
            "tail_lines": p.tail_lines, "since_seconds": p.since_seconds, "previous": p.previous, "grep": p.grep});
        let mut call = self.begin(&parts, "get_logs", args)?;
        let (client, kind) = self.kind(&call, &p.context, "pods").await?;
        let access = access_for(&kind, &p.context, "logs", Some(p.namespace.clone()), Some(p.pod.clone()));
        self.authorize(&mut call, &access).await?;
        let lp = kube::api::LogParams {
            container: p.container,
            tail_lines: Some(p.tail_lines.unwrap_or(200).min(call.policy().max_log_lines) as i64),
            since_seconds: p.since_seconds,
            previous: p.previous.unwrap_or(false),
            // A few very long lines must not pull megabytes through the agent.
            limit_bytes: Some(MAX_LOG_BYTES),
            ..Default::default()
        };
        let (ns, pod) = (p.namespace, p.pod);
        let result = k8s::run(async move {
            let api: kube::Api<k8s_openapi::api::core::v1::Pod> = kube::Api::namespaced(client, &ns);
            Ok(api.logs(&pod, &lp).await?)
        })
        .await;
        call.finish(result.map(|text| match &p.grep {
            Some(g) => {
                let g = g.to_lowercase();
                text.lines().filter(|l| l.to_lowercase().contains(&g)).collect::<Vec<_>>().join("\n")
            }
            None => text,
        }))
    }

    #[tool(
        description = "TLS certificates in kubernetes.io/tls Secrets with their names (SANs), issuer and expiry, soonest first (ones not valid yet before all). Needs `get` on secrets; private keys are never returned.",
        annotations(read_only_hint = true)
    )]
    async fn check_certificates(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<CertParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "within_days": p.within_days});
        let mut call = self.begin(&parts, "check_certificates", args)?;
        let (client, kind) = self.kind(&call, &p.context, "secrets").await?;
        let access = access_for(&kind, &p.context, "get", p.namespace.clone(), None);
        self.authorize_list(&mut call, &access, p.namespace.is_none()).await?;
        let secrets = match k8s::list_values_fields(
            client,
            kind.ar.clone(),
            p.namespace.clone(),
            None,
            Some("type=kubernetes.io/tls".into()),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => return call.finish(Err(e)),
        };
        let now = jiff::Timestamp::now();
        let mut rows: Vec<(i64, Vec<String>)> = Vec::new();
        for sec in &secrets {
            let ns = sec.pointer("/metadata/namespace").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            let name = sec.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default().to_string();
            if !call.may_see(&access_for(&kind, &p.context, "get", Some(ns.clone()), Some(name.clone()))) {
                continue;
            }
            use base64::Engine as _;
            let Some(pem) = sec
                .pointer("/data/tls.crt")
                .and_then(|v| v.as_str())
                .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
            else {
                continue;
            };
            // The leaf certificate is first in the chain.
            let Some(c) = crate::certs::parse_pem(&pem).into_iter().next() else { continue };
            // Rounded down: negative as soon as the date has passed.
            let days = c.not_after.map(|t| (t.as_second() - now.as_second()).div_euclid(86400)).unwrap_or(i64::MAX);
            // Not valid yet: clients reject it now, so it is listed first whatever the window.
            let early = c.not_yet_valid(now);
            if !early && p.within_days.is_some_and(|w| days > w) {
                continue;
            }
            let mut names = c.sans.clone();
            if names.len() > 4 {
                let more = names.len() - 4;
                names.truncate(4);
                names.push(format!("+{more}"));
            }
            rows.push((
                if early { i64::MIN } else { days },
                vec![
                    ns,
                    name,
                    names.join(","),
                    c.issuer.clone(),
                    c.not_after.map(|t| t.strftime("%Y-%m-%d").to_string()).unwrap_or_default(),
                    if early {
                        let from = c.not_before.map(|t| t.strftime("%Y-%m-%d").to_string()).unwrap_or_default();
                        format!("NOT VALID UNTIL {from}")
                    } else if days < 0 {
                        // The days that went by: 3 hours ago is 0, not the -1 days left.
                        let ago = c.not_after.map(|t| crate::certs::days_ago(t, now)).unwrap_or(0);
                        format!("EXPIRED {ago}d ago")
                    } else {
                        days.to_string()
                    },
                ],
            ));
        }
        rows.sort_by_key(|r| r.0);
        let table: Vec<Vec<String>> = rows.into_iter().map(|r| r.1).collect();
        call.finish(Ok(format_table(&["NAMESPACE", "SECRET", "NAMES", "ISSUER", "NOT AFTER", "DAYS LEFT"], &table)))
    }

    #[tool(
        description = "What is broken: crash-looping or pending pods, unavailable Deployments/StatefulSets/DaemonSets, NotReady nodes, failed Jobs, pending PVCs. Worst first. Start here when asked to troubleshoot.",
        annotations(read_only_hint = true)
    )]
    async fn find_problems(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<ProblemsParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "namespace": p.namespace, "include_minor": p.include_minor});
        let mut call = self.begin(&parts, "find_problems", args)?;
        let (client, pods) = self.kind(&call, &p.context, "pods").await?;
        let access = access_for(&pods, &p.context, "list", p.namespace.clone(), None);
        self.authorize_list(&mut call, &access, p.namespace.is_none()).await?;
        let kinds = match self.hub.client(&p.context).await {
            Ok((_, kinds)) => kinds,
            Err(e) => return call.finish(Err(e)),
        };
        // A kind that can't be listed is named, and doesn't hide what the others found.
        let scan = crate::problems::scan_partial(client, &kinds, p.namespace.clone()).await;
        let skipped = scan.skipped_text().map(|s| format!("Not checked, could not read {s}."));
        let minor = p.include_minor.unwrap_or(false);
        let table: Vec<Vec<String>> = scan
            .problems
            .into_iter()
            .filter(|x| minor || x.severity != crate::problems::Severity::Info)
            .filter(|x| {
                // Only what this agent may see.
                k8s::find_kind(&kinds, &x.kind).is_some_and(|k| {
                    let a = access_for(k, &p.context, "list", x.namespace.clone(), Some(x.name.clone()));
                    problem_visible(call.policy(), call.approved_rules(), k.is("", "Pod"), &a)
                })
            })
            .take(200)
            .map(|x| {
                vec![
                    format!("{:?}", x.severity).to_uppercase(),
                    x.label,
                    x.namespace.unwrap_or_default(),
                    x.name,
                    x.reason,
                    x.age.map(k8s::age).unwrap_or_default(),
                ]
            })
            .collect();
        let mut out = if table.is_empty() {
            "No problems found.".to_string()
        } else {
            format_table(&["SEVERITY", "KIND", "NAMESPACE", "NAME", "PROBLEM", "AGE"], &table)
        };
        if let Some(note) = skipped {
            out = format!("{out}\n{note}");
        }
        call.finish(Ok(out))
    }

    #[tool(
        description = "CPU and memory history of a pod or workload (all its pods summed) from the cluster's Prometheus: min, average, p95, max and last, next to requests and limits. Good for right-sizing.",
        annotations(read_only_hint = true)
    )]
    async fn usage_history(
        &self,
        Extension(parts): Extension<Parts>,
        Parameters(p): Parameters<HistoryParams>,
    ) -> Result<String, String> {
        let args = json!({"context": p.context, "kind": p.kind, "namespace": p.namespace, "name": p.name, "minutes": p.minutes});
        let mut call = self.begin(&parts, "usage_history", args)?;
        let (client, kind) = self.kind(&call, &p.context, &p.kind).await?;
        let access = access_for(&kind, &p.context, "get", Some(p.namespace.clone()), Some(p.name.clone()));
        self.authorize(&mut call, &access).await?;
        let obj = ObjectRef {
            kind: kind.clone(),
            name: p.name.clone(),
            namespace: Some(p.namespace.clone()),
            uid: String::new(),
        };
        let value = match k8s::get_object(client.clone(), obj).await {
            Ok(v) => v,
            Err(e) => return call.finish(Err(e)),
        };
        let ns = kind.namespaced.then_some(p.namespace.as_str());
        if let Err(e) = check_identity(&kind, ns, &p.name, &value) {
            return call.finish(Err(e));
        }
        // The pods to sum: the pod itself, or the workload's pods by its selector that the
        // agent may list.
        let (pods, hidden): (Vec<serde_json::Value>, usize) = if kind.is("", "Pod") {
            (vec![value.clone()], 0)
        } else {
            let pods_kind = self.kind(&call, &p.context, "pods").await?.1;
            let Some(Some(selector)) = k8s::pod_selection(&kind, &value).list_selector() else {
                return call.finish(Err(anyhow::anyhow!("{} has no label selector", kind.ar.kind)));
            };
            let pod_ar = k8s::ar_of::<k8s_openapi::api::core::v1::Pod>();
            let mut pods =
                match k8s::list_values(client.clone(), pod_ar, Some(p.namespace.clone()), Some(selector)).await {
                    Ok(v) => v,
                    Err(e) => return call.finish(Err(e)),
                };
            let listed = pods.len();
            pods.retain(|pod| {
                let name = pod.pointer("/metadata/name").and_then(|v| v.as_str()).unwrap_or_default();
                let a = access_for(&pods_kind, &p.context, "list", Some(p.namespace.clone()), Some(name.into()));
                call.policy().decide(&a).mode == Mode::Allow
            });
            if pods.is_empty() && listed > 0 {
                return Err(call.deny(format!("{} — you may not list its pods", describe(&access))));
            }
            let hidden = listed - pods.len();
            (pods, hidden)
        };
        // Like the charts: pods not finished, at most as many as fit in the query.
        let (pods, total) = crate::prom::chart_pods(&pods);
        // A finished workload's pods may have run one after the other: no sum of requests.
        let sum_specs = pods.len() <= 1 || !pods.iter().all(|p| crate::prom::finished(p));
        let names: Vec<String> = pods
            .iter()
            .filter_map(|p| p.pointer("/metadata/name").and_then(|v| v.as_str()).map(String::from))
            .collect();
        if names.is_empty() {
            return call.finish(Ok("No running pods.".into()));
        }
        let target = match crate::prom::target(&p.context, client.clone()).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                return call.finish(Err(anyhow::anyhow!(
                    "no Prometheus found in this cluster (set one in Recouvrance Settings → Contexts)"
                )));
            }
            Err(e) => return call.finish(Err(e)),
        };
        let minutes = p.minutes.unwrap_or(60).clamp(5, 10080);
        let (cpu, mem) = match crate::prom::pod_usage(client, target, &p.namespace, &names, minutes).await {
            Ok(r) => r,
            Err(e) => return call.finish(Err(e)),
        };
        // Requests and limits as the pods run (sidecars, overhead, pod-level resources).
        let (cpu_spec, mem_spec) = if sum_specs {
            (k8s::podspec::total(pods.iter().copied(), "cpu"), k8s::podspec::total(pods.iter().copied(), "memory"))
        } else {
            ((None, None), (None, None))
        };
        let stats = |s: &crate::prom::Series, scale: f64, fmt: fn(f64) -> String| -> Vec<String> {
            let mut v: Vec<f64> = s.iter().map(|p| p.1 * scale).collect();
            if v.is_empty() {
                return vec!["-".into(); 5];
            }
            let last = *v.last().unwrap_or(&0.0);
            v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let avg = v.iter().sum::<f64>() / v.len() as f64;
            let p95 = v[((v.len() as f64 * 0.95) as usize).min(v.len() - 1)];
            [v[0], avg, p95, v[v.len() - 1], last].into_iter().map(fmt).collect()
        };
        let opt = |v: Option<f64>, fmt: fn(f64) -> String| v.map(fmt).unwrap_or_else(|| "-".into());
        let mut cpu_row = vec!["cpu".to_string()];
        cpu_row.extend(stats(&cpu, 1000.0, k8s::format_cpu));
        cpu_row.push(opt(cpu_spec.0, k8s::format_cpu));
        cpu_row.push(opt(cpu_spec.1, k8s::format_cpu));
        let mut mem_row = vec!["memory".to_string()];
        mem_row.extend(stats(&mem, 1.0, k8s::format_bytes));
        mem_row.push(opt(mem_spec.0, k8s::format_bytes));
        mem_row.push(opt(mem_spec.1, k8s::format_bytes));
        let of = if names.len() < total { format!(" of {total}") } else { String::new() };
        let left_out = if hidden > 0 { " (pods you may not list are left out)" } else { "" };
        let header = format!("{}{of} pod(s), last {minutes} minutes, summed over pods{left_out}\n", names.len());
        call.finish(Ok(header
            + &format_table(&["", "MIN", "AVG", "P95", "MAX", "LAST", "REQUESTS", "LIMITS"], &[cpu_row, mem_row])))
    }
}
