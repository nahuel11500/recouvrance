//! Workloads: status, scheduling, relations map, revisions, CronJobs, HPA, PDB, VPA.

#[allow(unused_imports)]
use super::*;

pub(super) fn workload(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let desired = n(obj, "/spec/replicas").or_else(|| n(obj, "/status/desiredNumberScheduled")).unwrap_or(0);
    let ready = n(obj, "/status/readyReplicas").or_else(|| n(obj, "/status/numberReady")).unwrap_or(0);
    let updated = n(obj, "/status/updatedReplicas").or_else(|| n(obj, "/status/updatedNumberScheduled")).unwrap_or(0);
    let available = n(obj, "/status/availableReplicas").or_else(|| n(obj, "/status/numberAvailable")).unwrap_or(0);
    let strategy = s(obj, "/spec/strategy/type").or_else(|| s(obj, "/spec/updateStrategy/type"));
    let surge = obj.pointer("/spec/strategy/rollingUpdate").map(|r| {
        let v = |k: &str| {
            r[k].as_str().map(String::from).or_else(|| r[k].as_i64().map(|n| n.to_string())).unwrap_or("-".into())
        };
        format!("surge: {}, unavailable: {}", v("maxSurge"), v("maxUnavailable"))
    });
    let true_conditions: Vec<String> = arr(obj, "/status/conditions")
        .iter()
        .filter(|c| c["status"].as_str() == Some("True"))
        .filter_map(|c| c["type"].as_str().map(String::from))
        .collect();

    let template_spec =
        if ctx.kind.is("batch", "CronJob") { "/spec/jobTemplate/spec/template/spec" } else { "/spec/template/spec" };
    let mut root = v_flex().child(
        row()
            .pt_4()
            .when_some(strategy, |d, st| {
                d.child(field(
                    "Strategy",
                    v_flex().child(st.to_string()).when_some(surge.clone(), |d, s| d.child(div().text_xs().child(s))),
                    cx,
                ))
            })
            .when_some(s(obj, "/spec/schedule"), |d, sch| d.child(field("Schedule", mono(sch.to_string(), cx), cx)))
            .when(!true_conditions.is_empty(), |d| d.child(field("Conditions", true_conditions.join("  "), cx))),
    );
    root = root.child(scheduling(obj, template_spec, cx));

    // Template containers with average usage across the workload's pods.
    let containers = arr(obj, &format!("{template_spec}/containers"));
    if !containers.is_empty() {
        let pods = &ctx.related.pods;
        root = root.child(heading("Containers")).child(h_flex().flex_wrap().gap_3().children(containers.iter().map(
            |c| {
                let name = c["name"].as_str().unwrap_or_default();
                let samples: Vec<(f64, f64)> = pods
                    .iter()
                    .filter_map(|p| s(p, "/metadata/name"))
                    .filter_map(|p| ctx.related.usage.get(p).and_then(|u| u.get(name)).copied())
                    .collect();
                let avg = (!samples.is_empty()).then(|| {
                    let k = samples.len() as f64;
                    (samples.iter().map(|s| s.0).sum::<f64>() / k, samples.iter().map(|s| s.1).sum::<f64>() / k)
                });
                v_flex()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Icon::new(Lucide::Box).size_4())
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(name.to_string())),
                    )
                    .child(
                        div()
                            .w(px(500.))
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(c["image"].as_str().unwrap_or_default().to_string()),
                    )
                    .child(container_usage_row(c, avg.as_ref(), cx))
            },
        )));
    }

    // The workload's pods.
    if !ctx.kind.is("batch", "CronJob") {
        root = root
            .child(heading_for("Pods", ctx.related, "pods", cx))
            .child(
                h_flex()
                    .gap_4()
                    .text_sm()
                    .pb_2()
                    .child(format!("{desired} desired"))
                    .child(format!("{updated} updated"))
                    .child(format!("{ready} ready"))
                    .child(format!("{available} available")),
            )
            .child(pods_table(ctx, cx));
    }
    root
}

pub(super) fn scheduling(obj: &Value, spec: &str, cx: &App) -> AnyElement {
    let affinity = obj.pointer(&format!("{spec}/affinity"));
    let tolerations = arr(obj, &format!("{spec}/tolerations"));
    let selector = pairs(obj, &format!("{spec}/nodeSelector"));
    if affinity.is_none() && tolerations.is_empty() && selector.is_empty() {
        return div().into_any_element();
    }
    let expr = |e: &Value| {
        let vals: Vec<&str> =
            e["values"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
        format!("{} {} [{}]", e["key"].as_str().unwrap_or("?"), e["operator"].as_str().unwrap_or("?"), vals.join(", "))
    };
    let mut col = v_flex().w(px(520.)).gap_2();
    if !selector.is_empty() {
        col = col.child(field("Node selector", clipped_list(selector, 10, cx), cx));
    }
    if let Some(a) = affinity {
        for term in arr(a, "/nodeAffinity/preferredDuringSchedulingIgnoredDuringExecution") {
            let exprs: Vec<String> = arr(term, "/preference/matchExpressions").iter().map(expr).collect();
            col = col.child(field(
                "Node affinity (preferred)",
                card(cx)
                    .child(div().text_xs().child(format!("Weight: {}", term["weight"])))
                    .children(exprs.into_iter().map(|e| div().text_xs().child(e))),
                cx,
            ));
        }
        for term in arr(a, "/nodeAffinity/requiredDuringSchedulingIgnoredDuringExecution/nodeSelectorTerms") {
            let exprs: Vec<String> = arr(term, "/matchExpressions").iter().map(expr).collect();
            col = col.child(field(
                "Node affinity (required)",
                card(cx).children(exprs.into_iter().map(|e| div().text_xs().child(e))),
                cx,
            ));
        }
        for (kind, label) in [("podAntiAffinity", "Pod anti-affinity"), ("podAffinity", "Pod affinity")] {
            for (mode, path) in [
                ("preferred", "preferredDuringSchedulingIgnoredDuringExecution"),
                ("required", "requiredDuringSchedulingIgnoredDuringExecution"),
            ] {
                for term in arr(a, &format!("/{kind}/{path}")) {
                    let t = term.get("podAffinityTerm").unwrap_or(term);
                    let labels = pairs(t, "/labelSelector/matchLabels");
                    col = col.child(field(
                        &format!("{label} ({mode})"),
                        card(cx)
                            .when_some(term["weight"].as_i64(), |d, w| {
                                d.child(div().text_xs().child(format!("Weight: {w}")))
                            })
                            .child(
                                div()
                                    .text_xs()
                                    .child(format!("topologyKey: {}", t["topologyKey"].as_str().unwrap_or("-"))),
                            )
                            .child(clipped_list(labels, 6, cx)),
                        cx,
                    ));
                }
            }
        }
    }
    if !tolerations.is_empty() {
        let items: Vec<String> = tolerations
            .iter()
            .map(|t| {
                let key = t["key"].as_str().unwrap_or("*");
                let value = t["value"].as_str().map(|v| format!(" = {v}")).unwrap_or_default();
                let effect = t["effect"].as_str().map(|e| format!(" ({e})")).unwrap_or_default();
                format!("{key}{value}{effect}")
            })
            .collect();
        col = col.child(field("Tolerations", clipped_list(items, 8, cx), cx));
    }
    v_flex().child(heading("Scheduling")).child(col).into_any_element()
}

/// Map tab: what this workload is made of and what it touches, as a tree.
pub fn relations(ctx: &Ctx, cx: &App) -> AnyElement {
    let obj = ctx.obj;
    let theme = cx.theme().clone();
    let is_pod = ctx.kind.is("", "Pod");
    let pods: Vec<&Value> = if is_pod { vec![obj] } else { ctx.related.pods.iter().collect() };
    let spec = if is_pod {
        "/spec"
    } else if ctx.kind.is("batch", "CronJob") {
        "/spec/jobTemplate/spec/template/spec"
    } else {
        "/spec/template/spec"
    };

    let node = |depth: usize, icon: Lucide, kind_label: &str, content: AnyElement, note: Option<(String, Hsla)>| {
        h_flex()
            .pl(px(depth as f32 * 22.))
            .py(px(3.))
            .gap_2()
            .text_sm()
            .child(div().w(px(14.)).text_color(theme.border).child(if depth == 0 { "" } else { "└" }))
            .child(Icon::new(icon).size_4().text_color(theme.muted_foreground))
            .child(div().w(px(110.)).text_xs().text_color(theme.muted_foreground).child(kind_label.to_string()))
            .child(content)
            .when_some(note, |d, (n, c)| d.child(div().text_xs().text_color(c).child(n)))
            .into_any_element()
    };
    let mut rows: Vec<AnyElement> = Vec::new();
    let own_name = s(obj, "/metadata/name").unwrap_or_default().to_string();
    rows.push(node(
        0,
        Lucide::Boxes,
        &ctx.kind.ar.kind,
        div().font_weight(FontWeight::SEMIBOLD).child(own_name).into_any_element(),
        None,
    ));

    // Services that select these pods.
    let pod_labels = pods.first().and_then(|p| p.pointer("/metadata/labels")).and_then(Value::as_object);
    for svc in &ctx.related.services {
        let Some(sel) = svc.pointer("/spec/selector").and_then(Value::as_object) else { continue };
        if sel.is_empty() || !sel.iter().all(|(k, v)| pod_labels.and_then(|l| l.get(k)) == Some(v)) {
            continue;
        }
        let name = s(svc, "/metadata/name").unwrap_or_default().to_string();
        let ports: Vec<String> =
            arr(svc, "/spec/ports").iter().filter_map(|p| p["port"].as_i64().map(|n| n.to_string())).collect();
        rows.push(node(
            1,
            Lucide::Network,
            "Service",
            ctx.link(name.clone(), "services", &name, cx).into_any_element(),
            Some((format!(":{}", ports.join(",")), theme.muted_foreground)),
        ));
    }

    // Config the pods read.
    let mut seen = std::collections::BTreeSet::new();
    let mut config = |kind: &str, plural: &str, name: Option<&str>, icon: Lucide, rows: &mut Vec<AnyElement>| {
        if let Some(name) = name.filter(|n| !n.is_empty())
            && seen.insert((kind.to_string(), name.to_string()))
        {
            rows.push(node(1, icon, kind, ctx.link(name.to_string(), plural, name, cx).into_any_element(), None));
        }
    };
    for v in arr(obj, &format!("{spec}/volumes")) {
        config(
            "ConfigMap",
            "configmaps",
            v.pointer("/configMap/name").and_then(Value::as_str),
            Lucide::FileText,
            &mut rows,
        );
        config(
            "Secret",
            "secrets",
            v.pointer("/secret/secretName").and_then(Value::as_str),
            Lucide::KeyRound,
            &mut rows,
        );
        config(
            "PVC",
            "persistentvolumeclaims",
            v.pointer("/persistentVolumeClaim/claimName").and_then(Value::as_str),
            Lucide::Database,
            &mut rows,
        );
    }
    for c in arr(obj, &format!("{spec}/containers")).iter().chain(arr(obj, &format!("{spec}/initContainers"))) {
        for e in c["envFrom"].as_array().into_iter().flatten() {
            config(
                "ConfigMap",
                "configmaps",
                e.pointer("/configMapRef/name").and_then(Value::as_str),
                Lucide::FileText,
                &mut rows,
            );
            config(
                "Secret",
                "secrets",
                e.pointer("/secretRef/name").and_then(Value::as_str),
                Lucide::KeyRound,
                &mut rows,
            );
        }
        for e in c["env"].as_array().into_iter().flatten() {
            config(
                "ConfigMap",
                "configmaps",
                e.pointer("/valueFrom/configMapKeyRef/name").and_then(Value::as_str),
                Lucide::FileText,
                &mut rows,
            );
            config(
                "Secret",
                "secrets",
                e.pointer("/valueFrom/secretKeyRef/name").and_then(Value::as_str),
                Lucide::KeyRound,
                &mut rows,
            );
        }
    }
    // StatefulSet volume claim templates.
    for t in arr(obj, "/spec/volumeClaimTemplates") {
        let tmpl = s(t, "/metadata/name").unwrap_or_default();
        for p in &pods {
            let claim = format!("{tmpl}-{}", s(p, "/metadata/name").unwrap_or_default());
            config("PVC", "persistentvolumeclaims", Some(&claim), Lucide::Database, &mut rows);
        }
    }

    // ReplicaSets (Deployments), then pods grouped under them, each with its node.
    let current_rs: Vec<&Value> =
        ctx.related.revisions.iter().filter(|r| n(r, "/status/replicas").unwrap_or(0) > 0).collect();
    let pod_line = |p: &Value, depth: usize| -> Vec<AnyElement> {
        let name = s(p, "/metadata/name").unwrap_or_default().to_string();
        let phase = s(p, "/status/phase").unwrap_or("?").to_string();
        let restarts: i64 = arr(p, "/status/containerStatuses").iter().filter_map(|c| c["restartCount"].as_i64()).sum();
        let note = if restarts > 0 { format!("{phase} · {restarts} restarts") } else { phase.clone() };
        let mut out = vec![node(
            depth,
            Lucide::Box,
            "Pod",
            ctx.link(name.clone(), "pods", &name, cx).into_any_element(),
            Some((note, state_color(&phase, cx))),
        )];
        if let Some(node_name) = s(p, "/spec/nodeName") {
            out.push(node(
                depth + 1,
                Lucide::Server,
                "Node",
                ctx.link(node_name.to_string(), "nodes", node_name, cx).into_any_element(),
                None,
            ));
        }
        out
    };
    if is_pod {
        rows.extend(pod_line(obj, 1).into_iter().skip(1));
    } else if !current_rs.is_empty() {
        for rs in current_rs {
            let rs_name = s(rs, "/metadata/name").unwrap_or_default().to_string();
            let rs_uid = s(rs, "/metadata/uid").unwrap_or_default();
            rows.push(node(
                1,
                Lucide::Layers,
                "ReplicaSet",
                ctx.link(rs_name.clone(), "replicasets", &rs_name, cx).into_any_element(),
                None,
            ));
            for p in pods
                .iter()
                .filter(|p| arr(p, "/metadata/ownerReferences").iter().any(|o| o["uid"].as_str() == Some(rs_uid)))
            {
                rows.extend(pod_line(p, 2));
            }
        }
    } else {
        for p in &pods {
            rows.extend(pod_line(p, 1));
        }
    }
    let notes = load_notes(ctx.related, &["pods", "services", "revisions"], cx);
    scroll(v_flex().pt_2().children(notes).children(rows), "relations")
}

/// Pod template of a ReplicaSet as YAML, without the per-revision hash label.
/// Lines added and removed going from one pod template to another.
pub fn template_changes(from: &Value, to: &Value) -> (usize, usize) {
    let wrap = |t: &Value| serde_json::json!({ "spec": { "template": t } });
    let (a, b) = (template_yaml(&wrap(from)), template_yaml(&wrap(to)));
    let d = similar::TextDiff::from_lines(&a, &b);
    let (mut add, mut del) = (0, 0);
    for c in d.iter_all_changes() {
        match c.tag() {
            similar::ChangeTag::Insert => add += 1,
            similar::ChangeTag::Delete => del += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    (add, del)
}

pub(super) fn template_yaml(rs: &Value) -> String {
    let mut t = rs.pointer("/spec/template").cloned().unwrap_or(Value::Null);
    if let Some(labels) = t.pointer_mut("/metadata/labels").and_then(Value::as_object_mut) {
        labels.remove("pod-template-hash");
    }
    crate::yaml::to_string(&t).unwrap_or_default()
}

/// Revisions tab: the Deployment's ReplicaSets, newest first, with a diff of
/// any older revision against the current one.
pub fn revisions(ctx: &Ctx, cx: &App) -> AnyElement {
    let mut rs: Vec<&Value> = ctx.related.revisions.iter().collect();
    let rev = |r: &Value| {
        s(r, "/metadata/annotations/deployment.kubernetes.io~1revision")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
    };
    rs.sort_by_key(|r| std::cmp::Reverse(rev(r)));
    let theme = cx.theme().clone();
    let muted = theme.muted_foreground;
    let header = h_flex()
        .gap_3()
        .py_1()
        .text_xs()
        .text_color(muted)
        .border_b_1()
        .border_color(theme.border)
        .child(div().w(px(70.)).child("Revision"))
        .child(div().w(px(380.)).child("Name"))
        .child(div().w(px(70.)).child("Replicas"))
        .child(div().w(px(340.)).child("Images"))
        .child(div().w(px(70.)).child("Age"))
        .child(div().w(px(110.)))
        .child(div().w(px(110.)));
    let rs_kind = if ctx.kind.is("apps", "Deployment") { "replicasets" } else { "controllerrevisions" };
    let newest = rs.first().copied();
    let rows = rs.iter().enumerate().map(|(i, r)| {
        let name = s(r, "/metadata/name").unwrap_or_default().to_string();
        let images: Vec<&str> =
            arr(r, "/spec/template/spec/containers").iter().filter_map(|c| c["image"].as_str()).collect();
        let selected = ctx.compare == Some(name.as_str());
        let panel = ctx.panel.clone();
        let pick = name.clone();
        h_flex()
            .gap_3()
            .py_1()
            .text_sm()
            .border_b_1()
            .border_color(theme.border)
            .when(selected, |d| d.bg(theme.list_active))
            .child(div().w(px(70.)).child(format!("{}{}", rev(r), if i == 0 { " (current)" } else { "" })))
            .child(div().w(px(380.)).child(ctx.link(name.clone(), rs_kind, &name, cx)))
            .child(
                div().w(px(70.)).child(n(r, "/status/replicas").map(|n| n.to_string()).unwrap_or_else(|| "-".into())),
            )
            .child(
                div()
                    .w(px(340.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_xs()
                    .child(images.join(", ")),
            )
            .child(
                div().w(px(70.)).text_color(muted).child(age_of(r, "/metadata/creationTimestamp").unwrap_or_default()),
            )
            .when(i > 0, |d| {
                d.child(
                    div()
                        .id(SharedString::from(format!("cmp-{name}")))
                        .w(px(110.))
                        .text_xs()
                        .text_color(theme.blue)
                        .cursor_pointer()
                        .on_click(move |_, _, cx| {
                            let pick = pick.clone();
                            let _ = panel.update(cx, |p, cx| {
                                p.compare_rev = Some(pick);
                                cx.notify();
                            });
                        })
                        .child(if selected { "Comparing" } else { "Compare →" }),
                )
                .child({
                    let (panel, pick) = (ctx.panel.clone(), name.clone());
                    div()
                        .id(SharedString::from(format!("undo-{name}")))
                        .w(px(110.))
                        .text_xs()
                        .text_color(theme.warning)
                        .cursor_pointer()
                        .on_click(move |_, window, cx| {
                            let pick = pick.clone();
                            let _ = panel.update(cx, |p, cx| p.confirm_rollback(pick, window, cx));
                        })
                        .child("Roll back…")
                })
            })
    });

    let diff: AnyElement =
        match (newest, ctx.compare.and_then(|c| rs.iter().find(|r| s(r, "/metadata/name") == Some(c)))) {
            (Some(new), Some(old)) => {
                let (a, b) = (template_yaml(old), template_yaml(new));
                let d = similar::TextDiff::from_lines(&a, &b);
                let mono = theme.mono_font_family.clone();
                let mut lines: Vec<AnyElement> = Vec::new();
                for group in d.grouped_ops(3) {
                    lines.push(div().text_xs().text_color(muted).child("⋯").into_any_element());
                    for op in group {
                        for change in d.iter_changes(&op) {
                            let (sign, bg, fg) = match change.tag() {
                                similar::ChangeTag::Delete => ("-", Some(theme.danger.opacity(0.12)), theme.danger),
                                similar::ChangeTag::Insert => ("+", Some(theme.success.opacity(0.12)), theme.success),
                                similar::ChangeTag::Equal => (" ", None, theme.muted_foreground),
                            };
                            lines.push(
                                h_flex()
                                    .px_2()
                                    .when_some(bg, |d, bg| d.bg(bg))
                                    .text_color(fg)
                                    .child(div().w(px(16.)).child(sign))
                                    .child(div().whitespace_nowrap().child(change.value().trim_end().to_string()))
                                    .into_any_element(),
                            );
                        }
                    }
                }
                v_flex()
                    .gap_1()
                    .child(heading(&format!("Revision {} → {} (current)", rev(old), rev(new))))
                    .child(if lines.is_empty() {
                        div().text_sm().text_color(muted).child("The pod templates are identical.").into_any_element()
                    } else {
                        v_flex()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .py_1()
                            .font_family(mono)
                            .text_xs()
                            .children(lines)
                            .into_any_element()
                    })
                    .into_any_element()
            }
            _ => div()
                .pt_3()
                .text_sm()
                .text_color(muted)
                .child("Pick an older revision to see what changed in the pod template.")
                .into_any_element(),
        };
    let notes = load_notes(ctx.related, &["revisions"], cx);
    scroll(v_flex().children(notes).child(header).children(rows).child(diff), "revisions")
}

/// What identifies an HPA metric, in its spec and in its status alike:
/// (type, metric or resource name, container or described object).
fn hpa_metric_id(m: &Value) -> (String, String, String) {
    let kind = m["type"].as_str().unwrap_or_default();
    if kind.is_empty() {
        return Default::default();
    }
    let v = &m[metric_key(kind).as_str()];
    let name = v.pointer("/name").or(v.pointer("/metric/name")).and_then(Value::as_str).unwrap_or_default();
    let other =
        v.pointer("/container").or(v.pointer("/describedObject/name")).and_then(Value::as_str).unwrap_or_default();
    (kind.to_string(), name.to_string(), other.to_string())
}

/// The field holding a metric of this type: "Resource" -> "resource", "" stays "".
fn metric_key(kind: &str) -> String {
    let mut chars = kind.chars();
    chars.next().map(|first| first.to_lowercase().chain(chars).collect()).unwrap_or_default()
}

pub(super) fn hpa(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let theme = cx.theme();
    let target_kind = s(obj, "/spec/scaleTargetRef/kind").unwrap_or_default().to_string();
    let target = s(obj, "/spec/scaleTargetRef/name").unwrap_or_default().to_string();
    let (min, max) = (n(obj, "/spec/minReplicas").unwrap_or(1), n(obj, "/spec/maxReplicas").unwrap_or(0));
    let (current, desired) =
        (n(obj, "/status/currentReplicas").unwrap_or(0), n(obj, "/status/desiredReplicas").unwrap_or(0));
    let at_max = current >= max && max > 0;
    // Metric targets vs current values, matched by type and name (the status may
    // list fewer metrics, or in another order, than the spec).
    let current_metrics = arr(obj, "/status/currentMetrics");
    let metric_rows = arr(obj, "/spec/metrics").iter().map(|m| {
        let kind = m["type"].as_str().filter(|k| !k.is_empty()).unwrap_or("?");
        let key = metric_key(kind);
        let spec = &m[key.as_str()];
        let name =
            spec.pointer("/name").or(spec.pointer("/metric/name")).and_then(Value::as_str).unwrap_or("?").to_string();
        let target = spec
            .pointer("/target/averageUtilization")
            .and_then(Value::as_i64)
            .map(|v| format!("{v}%"))
            .or_else(|| spec.pointer("/target/averageValue").and_then(Value::as_str).map(String::from))
            .or_else(|| spec.pointer("/target/value").and_then(Value::as_str).map(String::from))
            .unwrap_or_else(|| "-".into());
        let id = hpa_metric_id(m);
        let cur = current_metrics.iter().find(|c| hpa_metric_id(c) == id).map(|c| &c[key.as_str()]);
        let current = cur
            .and_then(|c| {
                c.pointer("/current/averageUtilization")
                    .and_then(Value::as_i64)
                    .map(|v| format!("{v}%"))
                    .or_else(|| c.pointer("/current/averageValue").and_then(Value::as_str).map(String::from))
                    .or_else(|| c.pointer("/current/value").and_then(Value::as_str).map(String::from))
            })
            .unwrap_or_else(|| "unknown".into());
        h_flex()
            .gap_3()
            .py_0p5()
            .text_sm()
            .border_b_1()
            .border_color(theme.border)
            .child(div().w(px(120.)).text_color(theme.muted_foreground).child(kind.to_string()))
            .child(div().w(px(260.)).child(name))
            .child(div().w(px(120.)).child(current.clone()))
            .child(div().w(px(120.)).text_color(theme.muted_foreground).child(format!("target {target}")))
    });
    v_flex()
        .child(
            row()
                .pt_4()
                .child(field(
                    "Target",
                    ctx.link(format!("{target_kind}/{target}"), &target_kind.to_lowercase(), &target, cx),
                    cx,
                ))
                .child(field(
                    "Replicas",
                    div().when(at_max, |d| d.text_color(theme.warning)).child(format!("{current} (wants {desired})")),
                    cx,
                ))
                .child(field("Range", format!("{min} – {max}"), cx))
                .when(at_max, |d| {
                    d.child(field(
                        "Note",
                        div().text_color(theme.warning).child("At maximum: it cannot scale further"),
                        cx,
                    ))
                }),
        )
        .child(heading("Metrics"))
        .children(metric_rows)
}

pub(super) fn cron_jobs(ctx: &Ctx, cx: &App) -> Div {
    let theme = cx.theme();
    let mut jobs: Vec<&Value> = ctx.related.extra.iter().collect();
    jobs.sort_by_key(|j| std::cmp::Reverse(s(j, "/metadata/creationTimestamp").unwrap_or_default().to_string()));
    let last = s(ctx.obj, "/status/lastScheduleTime")
        .and_then(|t| t.parse::<jiff::Timestamp>().ok())
        .map(|t| format!("{} ago", k8s::age(t)));
    v_flex()
        .child(row().pt_4().child(field("Last scheduled", last.unwrap_or_else(|| "-".into()), cx)).child(field(
            "Suspended",
            ctx.obj.pointer("/spec/suspend").and_then(Value::as_bool).unwrap_or(false).to_string(),
            cx,
        )))
        .child(heading_for(&format!("Jobs ({})", jobs.len()), ctx.related, "extra", cx))
        .children(jobs.into_iter().take(20).map(|j| {
            let name = s(j, "/metadata/name").unwrap_or_default().to_string();
            let status = job_status(j);
            let color = match status {
                "Failed" => theme.danger,
                "Running" => theme.success,
                "Retrying" | "Suspended" => theme.warning,
                _ => theme.muted_foreground,
            };
            h_flex()
                .gap_3()
                .py_0p5()
                .text_sm()
                .border_b_1()
                .border_color(theme.border)
                .child(div().w(px(380.)).child(ctx.link(name.clone(), "jobs", &name, cx)))
                .child(div().w(px(90.)).text_color(color).child(status))
                .child(div().w(px(110.)).child(format!(
                    "{}/{}",
                    n(j, "/status/succeeded").unwrap_or(0),
                    n(j, "/spec/completions").unwrap_or(1)
                )))
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(age_of(j, "/metadata/creationTimestamp").unwrap_or_default()),
                )
        }))
}

/// A Job's state from its conditions; from its pod counts while it has none (still running).
fn job_status(j: &Value) -> &'static str {
    let is = |t: &str| arr(j, "/status/conditions").iter().any(|c| c["type"] == t && c["status"] == "True");
    let count = |p: &str| n(j, p).unwrap_or(0);
    if is("Failed") || is("FailureTarget") {
        "Failed"
    } else if is("Complete") || is("SuccessCriteriaMet") {
        "Complete"
    } else if is("Suspended") {
        "Suspended"
    } else if count("/status/active") > 0 {
        "Running"
    } else if count("/status/failed") > 0 {
        "Retrying"
    } else {
        "Pending"
    }
}

/// PodDisruptionBudget: how many may be disrupted now, and the pods it covers.
pub(super) fn pdb_page(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let theme = cx.theme();
    let allowed = n(obj, "/status/disruptionsAllowed").unwrap_or(0);
    let blocks = pdb_blocks(obj);
    let budget = obj
        .pointer("/spec/minAvailable")
        .map(|v| format!("min available {}", v.as_str().map(String::from).unwrap_or_else(|| v.to_string())))
        .or_else(|| {
            obj.pointer("/spec/maxUnavailable")
                .map(|v| format!("max unavailable {}", v.as_str().map(String::from).unwrap_or_else(|| v.to_string())))
        })
        .unwrap_or_else(|| "-".into());
    v_flex()
        .child(
            row()
                .pt_4()
                .child(field("Budget", budget, cx))
                .child(field(
                    "Disruptions allowed now",
                    div()
                        .text_color(if blocks {
                            theme.warning
                        } else if allowed == 0 {
                            theme.muted_foreground
                        } else {
                            theme.success
                        })
                        .child(allowed.to_string()),
                    cx,
                ))
                .child(field(
                    "Healthy",
                    format!(
                        "{} of {} (desired {})",
                        n(obj, "/status/currentHealthy").unwrap_or(0),
                        n(obj, "/status/expectedPods").unwrap_or(0),
                        n(obj, "/status/desiredHealthy").unwrap_or(0)
                    ),
                    cx,
                ))
                .child(field(
                    "Unhealthy pod eviction",
                    s(obj, "/spec/unhealthyPodEvictionPolicy").unwrap_or("IfHealthyBudget").to_string(),
                    cx,
                )),
        )
        .when(blocks, |d| {
            d.child(
                div()
                    .pt_2()
                    .text_sm()
                    .text_color(theme.warning)
                    .child("No pod can be evicted now: node drains and cluster upgrades will wait on this budget."),
            )
        })
        .child(heading_for(&format!("Pods it covers ({})", ctx.related.pods.len()), ctx.related, "pods", cx))
        .child(pods_table(ctx, cx))
}

/// Whether a PodDisruptionBudget stops evictions now: none allowed, with pods to protect.
pub(crate) fn pdb_blocks(obj: &Value) -> bool {
    n(obj, "/status/disruptionsAllowed").unwrap_or(0) == 0 && n(obj, "/status/expectedPods").unwrap_or(0) > 0
}

/// VerticalPodAutoscaler: recommendations next to the target's current requests.
pub(super) fn vpa_page(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let theme = cx.theme();
    let tkind = s(obj, "/spec/targetRef/kind").unwrap_or("?").to_string();
    let tname = s(obj, "/spec/targetRef/name").unwrap_or("?").to_string();
    let target = ctx.related.extra.first();
    let current = |container: &str, res: &str| {
        target
            .and_then(|t| t.pointer("/spec/template/spec/containers").and_then(Value::as_array))
            .and_then(|cs| cs.iter().find(|c| c["name"] == container))
            .and_then(|c| c.pointer(&format!("/resources/requests/{res}")).and_then(Value::as_str))
            .unwrap_or("-")
            .to_string()
    };
    let recs = arr(obj, "/status/recommendation/containerRecommendations");
    let header = h_flex()
        .gap_3()
        .py_1()
        .text_xs()
        .text_color(theme.muted_foreground)
        .border_b_1()
        .border_color(theme.border)
        .child(div().w(px(160.)).child("Container"))
        .child(div().w(px(90.)).child("Resource"))
        .child(div().w(px(110.)).child("Requested now"))
        .child(div().w(px(110.)).child("Lower bound"))
        .child(div().w(px(110.)).child("Target"))
        .child(div().w(px(110.)).child("Upper bound"))
        .child(div().w(px(110.)).child("Uncapped"));
    let mut rows = Vec::new();
    for r in recs {
        let c = r["containerName"].as_str().unwrap_or("?").to_string();
        for res in ["cpu", "memory"] {
            let get = |k: &str| r.pointer(&format!("/{k}/{res}")).and_then(Value::as_str).unwrap_or("-").to_string();
            let now = current(&c, res);
            let target_v = get("target");
            let far = now != "-" && target_v != "-" && {
                let (a, b) = if res == "cpu" {
                    (k8s::parse_cpu(&now), k8s::parse_cpu(&target_v))
                } else {
                    (k8s::parse_bytes(&now), k8s::parse_bytes(&target_v))
                };
                a > 0.0 && (b / a > 2.0 || a / b > 2.0)
            };
            rows.push(
                h_flex()
                    .gap_3()
                    .py_1()
                    .text_sm()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(div().w(px(160.)).child(c.clone()))
                    .child(div().w(px(90.)).child(res))
                    .child(div().w(px(110.)).when(far, |d| d.text_color(theme.warning)).child(now))
                    .child(div().w(px(110.)).text_color(theme.muted_foreground).child(get("lowerBound")))
                    .child(div().w(px(110.)).font_weight(FontWeight::MEDIUM).child(target_v))
                    .child(div().w(px(110.)).text_color(theme.muted_foreground).child(get("upperBound")))
                    .child(div().w(px(110.)).text_color(theme.muted_foreground).child(get("uncappedTarget"))),
            );
        }
    }
    let plural = format!("{}s", tkind.to_lowercase());
    v_flex()
        .child(
            row()
                .pt_4()
                .child(field(
                    "Target",
                    h_flex().gap_1().child(tkind.clone()).child(ctx.link(tname.clone(), &plural, &tname, cx)),
                    cx,
                ))
                .child(field("Update mode", s(obj, "/spec/updatePolicy/updateMode").unwrap_or("Auto").to_string(), cx))
                .child(field(
                    "Controlled",
                    s(obj, "/spec/resourcePolicy/containerPolicies/0/controlledValues")
                        .unwrap_or("RequestsAndLimits")
                        .to_string(),
                    cx,
                )),
        )
        .child(heading("Recommendations"))
        .child(if rows.is_empty() {
            div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No recommendation yet (the recommender needs some history).")
                .into_any_element()
        } else {
            v_flex()
                .child(header)
                .children(rows)
                .child(
                    div()
                        .pt_1()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("Requests more than 2× away from the target are highlighted."),
                )
                .into_any_element()
        })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn hpa_metrics_match_by_type_and_name() {
        let spec_mem =
            json!({"type": "Resource", "resource": {"name": "memory", "target": {"averageUtilization": 80}}});
        let spec_ext =
            json!({"type": "External", "external": {"metric": {"name": "queue"}, "target": {"value": "10"}}});
        let cur_cpu = json!({"type": "Resource", "resource": {"name": "cpu", "current": {"averageUtilization": 50}}});
        let cur_mem =
            json!({"type": "Resource", "resource": {"name": "memory", "current": {"averageUtilization": 70}}});
        let cur_ext = json!({"type": "External", "external": {"metric": {"name": "queue"}, "current": {"value": "3"}}});
        let id = super::hpa_metric_id;
        assert_eq!(id(&spec_mem), id(&cur_mem));
        assert_ne!(id(&spec_mem), id(&cur_cpu));
        assert_eq!(id(&spec_ext), id(&cur_ext));
        assert_eq!(id(&spec_ext).1, "queue");
        // A metric without a type (or an empty one) must not panic.
        assert_eq!(id(&json!({"type": ""})), Default::default());
        assert_eq!(id(&json!({})), Default::default());
    }

    #[test]
    fn pdb_without_pods_blocks_nothing() {
        let pdb =
            |allowed: i64, expected: i64| json!({"status": {"disruptionsAllowed": allowed, "expectedPods": expected}});
        assert!(super::pdb_blocks(&pdb(0, 3)));
        assert!(!super::pdb_blocks(&pdb(0, 0)));
        assert!(!super::pdb_blocks(&pdb(1, 3)));
        assert!(!super::pdb_blocks(&json!({})));
    }

    #[test]
    fn metric_keys() {
        assert_eq!(super::metric_key("Resource"), "resource");
        assert_eq!(super::metric_key("ContainerResource"), "containerResource");
        assert_eq!(super::metric_key(""), "");
        assert_eq!(super::metric_key("?"), "?");
        assert_eq!(super::metric_key("Éx"), "éx");
    }

    #[test]
    fn job_status_reads_conditions_first() {
        let st = super::job_status;
        let cond = |t: &str| json!({"type": t, "status": "True"});
        // Retrying after one failure.
        assert_eq!(st(&json!({"status": {"failed": 1, "active": 1}})), "Running");
        assert_eq!(st(&json!({"status": {"failed": 1}})), "Retrying");
        // Some completions, then the backoff limit.
        assert_eq!(st(&json!({"status": {"succeeded": 2, "failed": 6, "conditions": [cond("Failed")]}})), "Failed");
        assert_eq!(st(&json!({"status": {"succeeded": 1, "conditions": [cond("Complete")]}})), "Complete");
        assert_eq!(st(&json!({"status": {"conditions": [cond("Suspended")]}})), "Suspended");
        assert_eq!(st(&json!({"status": {"conditions": [{"type": "Suspended", "status": "False"}]}})), "Pending");
        assert_eq!(st(&json!({})), "Pending");
    }
}
