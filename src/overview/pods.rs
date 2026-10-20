//! Pod and container pages.

#[allow(unused_imports)]
use super::*;

pub(super) fn pod(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let phase = s(obj, "/status/phase").unwrap_or("Unknown").to_string();
    let statuses = arr(obj, "/status/containerStatuses");
    let containers = arr(obj, "/spec/containers");
    let ready = statuses.iter().filter(|c| c["ready"].as_bool() == Some(true)).count();
    let restarts: i64 = statuses.iter().filter_map(|c| c["restartCount"].as_i64()).sum();
    let node_name = s(obj, "/spec/nodeName").unwrap_or("-").to_string();
    let pod_name = s(obj, "/metadata/name").unwrap_or_default();

    let summary = row()
        .pt_4()
        .child(field("Status", div().text_color(state_color(&phase, cx)).child(phase.clone()), cx))
        .child(field("Ready", format!("{ready}/{}", containers.len()), cx))
        .child(field("Restarts", restarts.to_string(), cx))
        .child(field("Node", ctx.link(node_name.clone(), "nodes", &node_name, cx), cx))
        .child(field("Pod IP", s(obj, "/status/podIP").unwrap_or("-").to_string(), cx))
        .child(field("QoS", s(obj, "/status/qosClass").unwrap_or("-").to_string(), cx))
        .children(pod_ephemeral(ctx).map(|text| field("Ephemeral storage", text, cx)));

    let usage = ctx.related.usage.get(pod_name);
    let cards = containers.iter().map(|c| {
        let name = c["name"].as_str().unwrap_or_default();
        let st = statuses.iter().find(|x| x["name"].as_str() == Some(name));
        let state = container_state(st);
        let u = usage.and_then(|u| u.get(name));
        v_flex()
            .w(px(520.))
            .gap_2()
            .p_3()
            .rounded_md()
            .border_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(Lucide::Box).size_4())
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(name.to_string()))
                    .child(div().text_sm().text_color(state_color(&state, cx)).child(state.clone())),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(c["image"].as_str().unwrap_or_default().to_string()),
            )
            .child(container_usage_row(c, u, cx))
    });
    v_flex().child(summary).child(heading("Containers")).child(h_flex().flex_wrap().gap_3().children(cards))
}

pub(super) fn container_state(st: Option<&Value>) -> String {
    let Some(st) = st else { return "Waiting".into() };
    let Some((state, detail)) = st["state"].as_object().and_then(|m| m.iter().next()) else {
        return "Unknown".into();
    };
    match (state.as_str(), detail["reason"].as_str()) {
        ("running", _) => "Running".into(),
        (_, Some(r)) => r.into(),
        (other, None) => capitalize(other),
    }
}

/// CPU and memory bars for one container (spec + live usage).
pub(super) fn container_usage_row(c: &Value, usage: Option<&(f64, f64)>, cx: &App) -> Div {
    let res = |kind: &str, what: &str| c.pointer(&format!("/resources/{kind}/{what}")).and_then(Value::as_str);
    h_flex()
        .gap_3()
        .child(usage_card(
            Lucide::Cpu,
            "CPU",
            usage.map(|u| u.0),
            quantity_cpu(res("requests", "cpu")),
            quantity_cpu(res("limits", "cpu")),
            k8s::format_cpu,
            cx.theme().blue,
            cx,
        ))
        .child(usage_card(
            Lucide::MemoryStick,
            "Memory",
            usage.map(|u| u.1),
            quantity_mem(res("requests", "memory")),
            quantity_mem(res("limits", "memory")),
            k8s::format_bytes,
            cx.theme().success,
            cx,
        ))
}

/// The per-container tab of a pod.
pub fn container(ctx: &Ctx, name: &str, cx: &App) -> AnyElement {
    let obj = ctx.obj;
    let Some(c) = arr(obj, "/spec/containers").iter().find(|c| c["name"].as_str() == Some(name)) else {
        return div().child("Container not found").into_any_element();
    };
    let st = arr(obj, "/status/containerStatuses").iter().find(|x| x["name"].as_str() == Some(name));
    let state = container_state(st);
    let ready = st.and_then(|s| s["ready"].as_bool()).unwrap_or(false);
    let started = st.and_then(|s| s["started"].as_bool()).unwrap_or(false);
    let last_reason = st.and_then(|s| s.pointer("/lastState/terminated")).map(|t| {
        let r = t["reason"].as_str().unwrap_or("Terminated");
        match t["exitCode"].as_i64() {
            Some(code) => format!("{r} (ExitCode: {code})"),
            None => r.to_string(),
        }
    });
    let command: Vec<String> = ["command", "args"]
        .iter()
        .filter_map(|k| c[*k].as_array())
        .flatten()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    let pod_name = s(obj, "/metadata/name").unwrap_or_default();
    let usage = ctx.related.usage.get(pod_name).and_then(|u| u.get(name));

    let status = h_flex()
        .gap_2()
        .child(div().text_color(state_color(&state, cx)).child(state.clone()))
        .when(started, |d| d.child("Started"))
        .when(ready, |d| d.child("Ready"));

    let mut root = v_flex()
        .pt_2()
        .gap_1()
        .child(row().child(field("Name", name.to_string(), cx)).child(field("Status", status, cx)).child(field(
            "Image",
            c["image"].as_str().unwrap_or_default().to_string(),
            cx,
        )))
        .child(div().pt_3().child(field(
            "Command",
            if command.is_empty() { mono("-", cx) } else { mono(command.join(" "), cx) },
            cx,
        )))
        .child(
            row()
                .pt_3()
                .child(field("Restarts", st.and_then(|s| s["restartCount"].as_i64()).unwrap_or(0).to_string(), cx))
                .child(field(
                    "Last restart",
                    st.and_then(|s| age_of(s, "/lastState/terminated/finishedAt")).unwrap_or_else(|| "-".into()),
                    cx,
                ))
                .child(field("Restart reason", last_reason.unwrap_or_else(|| "-".into()), cx)),
        )
        .child(heading("Probes"))
        .child(
            h_flex()
                .items_start()
                .gap_3()
                .child(probe_card("Startup", &c["startupProbe"], cx))
                .child(probe_card("Liveness", &c["livenessProbe"], cx))
                .child(probe_card("Readiness", &c["readinessProbe"], cx)),
        )
        .child(heading("Utilization"))
        .child(container_usage_row(c, usage, cx))
        .children(container_history(ctx, c, name, cx));

    // Ports, with the Services that select this pod.
    let ports = c["ports"].as_array().cloned().unwrap_or_default();
    if !ports.is_empty() {
        let pod_labels = obj.pointer("/metadata/labels").and_then(Value::as_object);
        let services: Vec<String> = ctx
            .related
            .services
            .iter()
            .filter(|svc| {
                let Some(sel) = svc.pointer("/spec/selector").and_then(Value::as_object) else { return false };
                !sel.is_empty() && sel.iter().all(|(k, v)| pod_labels.and_then(|l| l.get(k)) == Some(v))
            })
            .filter_map(|svc| s(svc, "/metadata/name").map(String::from))
            .collect();
        root = root.child(heading("Ports")).child(h_flex().flex_wrap().gap_3().children(ports.iter().map(|p| {
            card(cx)
                .w(px(260.))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(p["name"].as_str().unwrap_or("-").to_string()),
                )
                .child(div().child(format!(
                    "{}/{}",
                    p["containerPort"].as_i64().unwrap_or(0),
                    p["protocol"].as_str().unwrap_or("TCP")
                )))
                .when(!services.is_empty(), |d| {
                    d.child(div().pt_1().text_xs().text_color(cx.theme().muted_foreground).child("Services"))
                        .children(services.iter().map(|svc| ctx.link(svc.clone(), "services", svc, cx)))
                })
        })));
    }

    // Environment variables, with values resolved from ConfigMaps, Secrets and the pod.
    let env = c["env"].as_array().cloned().unwrap_or_default();
    let env_from = c["envFrom"].as_array().cloned().unwrap_or_default();
    if !env.is_empty() || !env_from.is_empty() {
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let sources = &ctx.related.env_sources;
        let secret_value = |name: &str, key: &str| -> Option<String> {
            use base64::Engine as _;
            let b = sources
                .get(&format!("Secret/{name}"))?
                .pointer(&format!("/data/{}", key.replace('~', "~0").replace('/', "~1")))?
                .as_str()?;
            Some(String::from_utf8_lossy(&base64::engine::general_purpose::STANDARD.decode(b).ok()?).to_string())
        };
        let cm_value = |name: &str, key: &str| -> Option<String> {
            sources
                .get(&format!("ConfigMap/{name}"))?
                .pointer(&format!("/data/{}", key.replace('~', "~0").replace('/', "~1")))?
                .as_str()
                .map(String::from)
        };
        let masked = |v: String| if ctx.reveal { v } else { "••••••".into() };
        let row_el = |key: String, value: AnyElement, source: AnyElement| {
            h_flex()
                .gap_3()
                .items_start()
                .py_0p5()
                .child(
                    div()
                        .w(px(260.))
                        .flex_shrink_0()
                        .font_family(theme.mono_font_family.clone())
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .overflow_hidden()
                        .text_ellipsis()
                        .child(key),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .font_family(theme.mono_font_family.clone())
                        .text_sm()
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(value),
                )
                .child(div().flex_shrink_0().text_xs().text_color(muted).child(source))
                .into_any_element()
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        for from in &env_from {
            let prefix = from["prefix"].as_str().unwrap_or_default();
            for (kind, key_ptr, plural) in
                [("ConfigMap", "/configMapRef/name", "configmaps"), ("Secret", "/secretRef/name", "secrets")]
            {
                let Some(name) = from.pointer(key_ptr).and_then(Value::as_str) else { continue };
                let data = sources
                    .get(&format!("{kind}/{name}"))
                    .and_then(|v| v["data"].as_object())
                    .cloned()
                    .unwrap_or_default();
                if data.is_empty() {
                    rows.push(row_el(
                        "(all keys)".into(),
                        div().text_color(muted).child("not readable").into_any_element(),
                        h_flex()
                            .gap_1()
                            .child(format!("from {kind}"))
                            .child(ctx.link(name.to_string(), plural, name, cx))
                            .into_any_element(),
                    ));
                }
                for k in data.keys() {
                    let v = if kind == "Secret" { secret_value(name, k).map(masked) } else { cm_value(name, k) };
                    rows.push(row_el(
                        format!("{prefix}{k}"),
                        div().child(v.unwrap_or_default()).into_any_element(),
                        h_flex()
                            .gap_1()
                            .child(kind.to_string())
                            .child(ctx.link(name.to_string(), plural, name, cx))
                            .into_any_element(),
                    ));
                }
            }
        }
        for e in &env {
            let key = e["name"].as_str().unwrap_or_default().to_string();
            let (value, source): (AnyElement, AnyElement) = if let Some(v) = e["value"].as_str() {
                (div().child(v.to_string()).into_any_element(), div().into_any_element())
            } else if let Some(r) = e.pointer("/valueFrom/secretKeyRef") {
                let (name, k) = (r["name"].as_str().unwrap_or_default(), r["key"].as_str().unwrap_or("?"));
                let v = secret_value(name, k).map(masked).unwrap_or_else(|| "(not readable)".into());
                (
                    div().child(v).into_any_element(),
                    h_flex()
                        .gap_1()
                        .child(format!("Secret key {k} of"))
                        .child(ctx.link(name.to_string(), "secrets", name, cx))
                        .into_any_element(),
                )
            } else if let Some(r) = e.pointer("/valueFrom/configMapKeyRef") {
                let (name, k) = (r["name"].as_str().unwrap_or_default(), r["key"].as_str().unwrap_or("?"));
                let v = cm_value(name, k).unwrap_or_else(|| "(missing)".into());
                (
                    div().child(v).into_any_element(),
                    h_flex()
                        .gap_1()
                        .child(format!("key {k} of"))
                        .child(ctx.link(name.to_string(), "configmaps", name, cx))
                        .into_any_element(),
                )
            } else if let Some(p) = e.pointer("/valueFrom/fieldRef/fieldPath").and_then(Value::as_str) {
                // Resolve the common downward-API fields from the pod itself.
                let resolved = match p {
                    "metadata.name" => s(obj, "/metadata/name"),
                    "metadata.namespace" => s(obj, "/metadata/namespace"),
                    "metadata.uid" => s(obj, "/metadata/uid"),
                    "spec.nodeName" => s(obj, "/spec/nodeName"),
                    "spec.serviceAccountName" => s(obj, "/spec/serviceAccountName"),
                    "status.podIP" => s(obj, "/status/podIP"),
                    "status.hostIP" => s(obj, "/status/hostIP"),
                    _ => p
                        .strip_prefix("metadata.labels['")
                        .and_then(|k| k.strip_suffix("']"))
                        .and_then(|k| obj.pointer("/metadata/labels").and_then(|l| l[k].as_str()))
                        .or_else(|| {
                            p.strip_prefix("metadata.annotations['")
                                .and_then(|k| k.strip_suffix("']"))
                                .and_then(|k| obj.pointer("/metadata/annotations").and_then(|l| l[k].as_str()))
                        }),
                };
                (
                    div().child(resolved.unwrap_or("-").to_string()).into_any_element(),
                    div().child(format!("field {p}")).into_any_element(),
                )
            } else if let Some(r) = e.pointer("/valueFrom/resourceFieldRef/resource").and_then(Value::as_str) {
                let limit = c
                    .pointer(&format!("/resources/{}", r.replacen('.', "/", 1)))
                    .and_then(Value::as_str)
                    .unwrap_or("-");
                (
                    div().child(limit.to_string()).into_any_element(),
                    div().child(format!("resource {r}")).into_any_element(),
                )
            } else {
                (div().child("-").into_any_element(), div().into_any_element())
            };
            rows.push(row_el(key, value, source));
        }
        const SHOWN: usize = 12;
        let total = rows.len();
        let hidden = if ctx.env_expanded { 0 } else { total.saturating_sub(SHOWN) };
        let panel = ctx.panel.clone();
        root = root.child(heading(&format!("Environment variables ({total})"))).child(
            v_flex().children(rows.into_iter().take(total - hidden)).when(total > SHOWN, |d| {
                d.child(
                    div()
                        .id("env-toggle")
                        .pt_1()
                        .text_sm()
                        .text_color(cx.theme().blue)
                        .cursor_pointer()
                        .child(if hidden > 0 { format!("Show {hidden} more") } else { "Show less".to_string() })
                        .on_click(move |_, _, cx| {
                            let _ = panel.update(cx, |p, cx| p.toggle_env(cx));
                        }),
                )
            }),
        );
    }

    // Volume mounts with their source.
    let mounts = c["volumeMounts"].as_array().cloned().unwrap_or_default();
    if !mounts.is_empty() {
        let volumes = arr(obj, "/spec/volumes");
        root =
            root.child(heading("Volume mounts")).child(h_flex().flex_wrap().gap_3().children(mounts.iter().map(|m| {
                let vname = m["name"].as_str().unwrap_or_default();
                let vol = volumes.iter().find(|v| v["name"].as_str() == Some(vname));
                let source: AnyElement = match vol {
                    Some(v) if v.get("configMap").is_some() => {
                        let n = v.pointer("/configMap/name").and_then(Value::as_str).unwrap_or_default();
                        ctx.link(format!("ConfigMap: {n}"), "configmaps", n, cx).into_any_element()
                    }
                    Some(v) if v.get("secret").is_some() => {
                        let n = v.pointer("/secret/secretName").and_then(Value::as_str).unwrap_or_default();
                        ctx.link(format!("Secret: {n}"), "secrets", n, cx).into_any_element()
                    }
                    Some(v) if v.get("persistentVolumeClaim").is_some() => {
                        let n =
                            v.pointer("/persistentVolumeClaim/claimName").and_then(Value::as_str).unwrap_or_default();
                        ctx.link(format!("PVC: {n}"), "persistentvolumeclaims", n, cx).into_any_element()
                    }
                    Some(v) => {
                        let t =
                            v.as_object().and_then(|o| o.keys().find(|k| *k != "name").cloned()).unwrap_or_default();
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("({t})"))
                            .into_any_element()
                    }
                    None => div().into_any_element(),
                };
                let ro = m["readOnly"].as_bool().unwrap_or(false);
                card(cx)
                    .w(px(360.))
                    .child(div().text_xs().child(vname.to_string()))
                    .child(source)
                    .child(div().child(format!(
                        "{}{}",
                        m["mountPath"].as_str().unwrap_or_default(),
                        m["subPath"].as_str().map(|p| format!(" (subPath {p})")).unwrap_or_default()
                    )))
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(if ro {
                        "ReadOnly"
                    } else {
                        "ReadWrite"
                    }))
            })));
    }
    scroll(root, "container")
}

pub(super) fn probe_card(title: &str, p: &Value, cx: &App) -> Div {
    let body: AnyElement = if p.is_null() {
        div().child("-").into_any_element()
    } else {
        let target = if let Some(h) = p.get("httpGet") {
            let port = h["port"]
                .as_i64()
                .map(|n| n.to_string())
                .or_else(|| h["port"].as_str().map(String::from))
                .unwrap_or_default();
            format!("GET :{port}{}", h["path"].as_str().unwrap_or("/"))
        } else if let Some(t) = p.get("tcpSocket") {
            format!("TCP :{}", t["port"])
        } else if let Some(e) = p.get("exec") {
            let cmd: Vec<&str> =
                e["command"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
            cmd.join(" ")
        } else if p.get("grpc").is_some() {
            format!("gRPC :{}", p.pointer("/grpc/port").and_then(Value::as_i64).unwrap_or(0))
        } else {
            "?".into()
        };
        let delay = n(p, "/initialDelaySeconds").unwrap_or(0);
        let first = if delay == 0 {
            "First probe at startup".to_string()
        } else {
            format!("First probe {delay}s after startup")
        };
        let sentence = format!(
            "{first}, then every {}s with {}s timeout. Status changes after {} success or {} consecutive failures.",
            n(p, "/periodSeconds").unwrap_or(10),
            n(p, "/timeoutSeconds").unwrap_or(1),
            n(p, "/successThreshold").unwrap_or(1),
            n(p, "/failureThreshold").unwrap_or(3),
        );
        card(cx)
            .child(mono(target, cx))
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(sentence))
            .into_any_element()
    };
    v_flex()
        .w(px(340.))
        .gap_1()
        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(title.to_string()))
        .child(body)
}

pub(super) fn pods_table(ctx: &Ctx, cx: &App) -> AnyElement {
    let pods = &ctx.related.pods;
    if pods.is_empty() {
        return div().text_sm().text_color(cx.theme().muted_foreground).child("No pods.").into_any_element();
    }
    let muted = cx.theme().muted_foreground;
    let header = h_flex()
        .gap_3()
        .py_1()
        .text_xs()
        .text_color(muted)
        .border_b_1()
        .border_color(cx.theme().border)
        .child(div().w(px(320.)).child("Name"))
        .child(div().w(px(50.)).child("Ready"))
        .child(div().w(px(110.)).child("Status"))
        .child(div().w(px(150.)).child("CPU"))
        .child(div().w(px(150.)).child("Memory"))
        .child(div().w(px(60.)).child("Restarts"))
        .child(div().w(px(220.)).child("Node"))
        .child(div().w(px(60.)).child("Age"));
    let rows = pods.iter().map(|p| {
        let name = s(p, "/metadata/name").unwrap_or_default();
        let statuses = arr(p, "/status/containerStatuses");
        let total = arr(p, "/spec/containers").len();
        let ready = statuses.iter().filter(|c| c["ready"].as_bool() == Some(true)).count();
        let restarts: i64 = statuses.iter().filter_map(|c| c["restartCount"].as_i64()).sum();
        let phase = s(p, "/status/phase").unwrap_or("Unknown").to_string();
        let usage = ctx.related.usage.get(name);
        let sum = |f: fn(&(f64, f64)) -> f64| usage.map(|u| u.values().map(f).sum::<f64>());
        let spec_sum = |what: &str, parse: fn(&str) -> f64| -> Option<f64> {
            let vals: Vec<f64> = arr(p, "/spec/containers")
                .iter()
                .filter_map(|c| c.pointer(what).and_then(Value::as_str).map(parse))
                .collect();
            (!vals.is_empty()).then(|| vals.iter().sum())
        };
        let cpu = sum(|u| u.0);
        let mem = sum(|u| u.1);
        let node = s(p, "/spec/nodeName").unwrap_or("-").to_string();
        h_flex()
            .gap_3()
            .py_1()
            .text_sm()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().w(px(320.)).child({
                // Pods are opened in their own namespace (a node lists pods from every namespace).
                let pod_ns = s(p, "/metadata/namespace").map(String::from);
                let label = match (&pod_ns, ctx.ns()) {
                    (Some(ns), None) => format!("{ns} / {name}"),
                    _ => name.to_string(),
                };
                ctx.link_in(label, "pods", name, pod_ns, cx)
            }))
            .child(
                div()
                    .w(px(50.))
                    .text_color(if ready == total { cx.theme().success } else { cx.theme().warning })
                    .child(format!("{ready}/{total}")),
            )
            .child(div().w(px(110.)).text_color(state_color(&phase, cx)).child(phase))
            .child(
                h_flex()
                    .w(px(150.))
                    .gap_2()
                    .child(div().w(px(48.)).child(cpu.map(k8s::format_cpu).unwrap_or_default()))
                    .child(div().flex_1().child(usage_bar(
                        cpu,
                        spec_sum("/resources/requests/cpu", k8s::parse_cpu),
                        spec_sum("/resources/limits/cpu", k8s::parse_cpu),
                        cx.theme().blue,
                        cx,
                    ))),
            )
            .child(
                h_flex()
                    .w(px(150.))
                    .gap_2()
                    .child(div().w(px(48.)).child(mem.map(k8s::format_bytes).unwrap_or_default()))
                    .child(div().flex_1().child(usage_bar(
                        mem,
                        spec_sum("/resources/requests/memory", k8s::parse_bytes),
                        spec_sum("/resources/limits/memory", k8s::parse_bytes),
                        cx.theme().success,
                        cx,
                    ))),
            )
            .child(
                div().w(px(60.)).when(restarts > 0, |d| d.text_color(cx.theme().warning)).child(restarts.to_string()),
            )
            .child(div().w(px(220.)).child(ctx.link(node.clone(), "nodes", &node, cx)))
            .child(
                div().w(px(60.)).text_color(muted).child(age_of(p, "/metadata/creationTimestamp").unwrap_or_default()),
            )
    });
    v_flex().child(header).children(rows).into_any_element()
}

/// A pod's ephemeral storage use (from the kubelet), against its limit if any.
pub(super) fn pod_ephemeral(ctx: &Ctx) -> Option<String> {
    let st = ctx.related.node_stats.as_ref()?;
    let (name, ns) = (s(ctx.obj, "/metadata/name")?, s(ctx.obj, "/metadata/namespace")?);
    let pod = arr(st, "/pods")
        .iter()
        .find(|p| s(p, "/podRef/name") == Some(name) && s(p, "/podRef/namespace") == Some(ns))?;
    let used = pod.pointer("/ephemeral-storage/usedBytes").and_then(Value::as_f64)?;
    let (_, limit) = k8s::pods_allocated(std::slice::from_ref(ctx.obj), "ephemeral-storage");
    Some(if limit > 0.0 {
        format!("{} of {} ({:.0}%)", k8s::format_bytes(used), k8s::format_bytes(limit), used / limit * 100.0)
    } else {
        format!("{} used", k8s::format_bytes(used))
    })
}

/// NetworkPolicies that select this pod, and what they restrict.
pub(super) fn pod_netpols(ctx: &Ctx, cx: &App) -> Option<Div> {
    let pols = ctx.related.netpols.as_ref()?;
    let theme = cx.theme();
    let (ingress, egress) = pols.iter().fold((false, false), |(i, e), p| {
        let types: Vec<&str> = arr(p, "/spec/policyTypes").iter().filter_map(Value::as_str).collect();
        (i || types.contains(&"Ingress") || types.is_empty(), e || types.contains(&"Egress"))
    });
    let summary = match (pols.is_empty(), ingress, egress) {
        (true, ..) => "No NetworkPolicy selects this pod: all traffic in and out is allowed.".to_string(),
        (_, true, true) => "Incoming and outgoing traffic is limited to what these policies allow.".into(),
        (_, true, false) => "Incoming traffic is limited to what these policies allow; outgoing is open.".into(),
        (_, false, true) => "Outgoing traffic is limited to what these policies allow; incoming is open.".into(),
        _ => String::new(),
    };
    Some(
        v_flex()
            .child(heading(&format!("Network policies ({})", pols.len())))
            .child(div().text_sm().text_color(theme.muted_foreground).child(summary))
            .children(pols.iter().map(|p| {
                let name = s(p, "/metadata/name").unwrap_or_default().to_string();
                let types: Vec<&str> = arr(p, "/spec/policyTypes").iter().filter_map(Value::as_str).collect();
                h_flex().gap_2().py_0p5().text_sm().child(ctx.link(name.clone(), "networkpolicies", &name, cx)).child(
                    div().text_xs().text_color(theme.muted_foreground).child(if types.is_empty() {
                        "Ingress".to_string()
                    } else {
                        types.join(", ")
                    }),
                )
            })),
    )
}

/// This container's CPU and memory over the chosen range, against its request and limit.
fn container_history(ctx: &Ctx, c: &Value, name: &str, cx: &App) -> Option<Div> {
    let (cpu, mem) = ctx.related.container_history.get(name)?;
    if cpu.is_empty() && mem.is_empty() {
        return None;
    }
    let theme = cx.theme();
    let res = |p: &str, f: fn(&str) -> f64| c.pointer(p).and_then(Value::as_str).map(f);
    let chart = |title: &'static str, data: &crate::prom::Series, req, lim, color, fmt, id| {
        usage_chart(
            data,
            ChartSpec {
                title,
                req,
                req_word: "request",
                lim,
                lim_word: "limit",
                pin_lim: false,
                color,
                fmt,
                id,
                minutes: ctx.history_minutes,
            },
            cx,
        )
    };
    Some(
        h_flex()
            .pt_3()
            .gap_3()
            .child(chart(
                "CPU over time",
                cpu,
                res("/resources/requests/cpu", k8s::parse_cpu),
                res("/resources/limits/cpu", k8s::parse_cpu),
                theme.blue,
                k8s::format_cores as fn(f64) -> String,
                "ctr-cpu",
            ))
            .child(chart(
                "Memory over time",
                mem,
                res("/resources/requests/memory", k8s::parse_bytes),
                res("/resources/limits/memory", k8s::parse_bytes),
                theme.success,
                k8s::format_bytes as fn(f64) -> String,
                "ctr-mem",
            )),
    )
}
