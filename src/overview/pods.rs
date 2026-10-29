//! Pod and container pages.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use super::{
    ChartSpec, Ctx, age_of, arr, capitalize, card, field, heading, mono, n, quantity_cpu, quantity_mem, row, s, scroll,
    state_color, usage_bar, usage_card, usage_chart,
};
use crate::k8s;
use crate::k8s::ContainerUsage;

pub(super) fn pod(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let phase = pod_status(obj);
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

    let root = v_flex()
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

    let root = root
        .children(container_ports(ctx, c, cx))
        .children(container_env(ctx, c, cx))
        .children(container_mounts(ctx, c, cx));
    scroll(root, "container")
}

/// The container's ports, with the Services that select its pod: a heading and the cards,
/// or nothing when it declares none.
fn container_ports(ctx: &Ctx, c: &Value, cx: &App) -> Vec<AnyElement> {
    let Some(ports) = c["ports"].as_array().filter(|p| !p.is_empty()) else { return Vec::new() };
    let pod_labels = ctx.obj.pointer("/metadata/labels").and_then(Value::as_object);
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
    let cards = ports.iter().map(|p| {
        let port = p["containerPort"].as_u64().and_then(|n| u16::try_from(n).ok()).unwrap_or(0);
        let protocol = p["protocol"].as_str().unwrap_or("TCP");
        let name = p["name"].as_str().map(String::from);
        // Port-forward is TCP only.
        let forward = (port > 0 && protocol == "TCP").then(|| forward_buttons(ctx, port, name.as_deref()));
        card(cx)
            .w(px(260.))
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(name.unwrap_or_else(|| "-".into())))
            .child(h_flex().justify_between().child(div().child(format!("{port}/{protocol}"))).children(forward))
            .when(!services.is_empty(), |d| {
                d.child(div().pt_1().text_xs().text_color(cx.theme().muted_foreground).child("Services"))
                    .children(services.iter().map(|svc| ctx.link(svc.clone(), "services", svc, cx)))
            })
    });
    vec![heading("Ports").into_any_element(), h_flex().flex_wrap().gap_3().children(cards).into_any_element()]
}

/// "Forward" and "Forward with options" for a container port.
fn forward_buttons(ctx: &Ctx, port: u16, name: Option<&str>) -> Div {
    let forward = |options: bool| {
        let (panel, name) = (ctx.panel.clone(), name.map(String::from));
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            let _ = panel.update(cx, |p, cx| p.forward_port(port, name.as_deref(), options, window, cx));
        }
    };
    h_flex()
        .gap_1()
        .child(
            Button::new(("pf", port as usize))
                .xsmall()
                .outline()
                .icon(Lucide::ArrowRightLeft)
                .label("Forward")
                .tooltip(format!("Port-forward {port} to this machine"))
                .on_click(forward(false)),
        )
        .child(
            Button::new(("pf-opt", port as usize))
                .xsmall()
                .ghost()
                .icon(Lucide::Settings2)
                .tooltip("Forward with options: local port, address, https, open in browser")
                .on_click(forward(true)),
        )
}

/// The ConfigMaps and Secrets a pod's environment reads, by `Kind/name`.
struct EnvSources<'a> {
    sources: &'a std::collections::HashMap<String, Value>,
    reveal: bool,
}

impl EnvSources<'_> {
    fn data(&self, kind: &str, name: &str, key: &str) -> Option<&str> {
        let key = key.replace('~', "~0").replace('/', "~1");
        self.sources.get(&format!("{kind}/{name}"))?.pointer(&format!("/data/{key}"))?.as_str()
    }

    /// A Secret's value, decoded, or dots unless secrets are revealed.
    fn secret(&self, name: &str, key: &str) -> Option<String> {
        use base64::Engine as _;
        let value = base64::engine::general_purpose::STANDARD.decode(self.data("Secret", name, key)?).ok()?;
        Some(if self.reveal { String::from_utf8_lossy_owned(value) } else { "••••••".into() })
    }

    fn config_map(&self, name: &str, key: &str) -> Option<String> {
        self.data("ConfigMap", name, key).map(String::from)
    }
}

/// The container's environment variables, with values resolved from ConfigMaps, Secrets
/// and the pod: a heading and the list (long ones folded), or nothing.
fn container_env(ctx: &Ctx, c: &Value, cx: &App) -> Vec<AnyElement> {
    let env = c["env"].as_array().map(Vec::as_slice).unwrap_or_default();
    let env_from = c["envFrom"].as_array().map(Vec::as_slice).unwrap_or_default();
    if env.is_empty() && env_from.is_empty() {
        return Vec::new();
    }
    let muted = cx.theme().muted_foreground;
    let sources = EnvSources { sources: &ctx.related.env_sources, reveal: ctx.reveal };
    let mut rows: Vec<AnyElement> = Vec::new();
    for from in env_from {
        let prefix = from["prefix"].as_str().unwrap_or_default();
        for (kind, key_ptr, plural) in
            [("ConfigMap", "/configMapRef/name", "configmaps"), ("Secret", "/secretRef/name", "secrets")]
        {
            let Some(name) = from.pointer(key_ptr).and_then(Value::as_str) else { continue };
            let data = ctx.related.env_sources.get(&format!("{kind}/{name}")).and_then(|v| v["data"].as_object());
            let link = |label: String| {
                h_flex().gap_1().child(label).child(ctx.link(name.to_string(), plural, name, cx)).into_any_element()
            };
            if data.is_none_or(serde_json::Map::is_empty) {
                rows.push(env_row(
                    "(all keys)".into(),
                    div().text_color(muted).child("not readable").into_any_element(),
                    link(format!("from {kind}")),
                    cx,
                ));
            }
            for k in data.into_iter().flat_map(serde_json::Map::keys) {
                let v = if kind == "Secret" { sources.secret(name, k) } else { sources.config_map(name, k) };
                rows.push(env_row(
                    format!("{prefix}{k}"),
                    div().child(v.unwrap_or_default()).into_any_element(),
                    link(kind.to_string()),
                    cx,
                ));
            }
        }
    }
    for e in env {
        let key = e["name"].as_str().unwrap_or_default().to_string();
        let (value, source) = env_value(ctx, &sources, c, e, cx);
        rows.push(env_row(key, value, source, cx));
    }
    const SHOWN: usize = 12;
    let total = rows.len();
    let hidden = if ctx.env_expanded { 0 } else { total.saturating_sub(SHOWN) };
    let panel = ctx.panel.clone();
    let list = v_flex().children(rows.into_iter().take(total - hidden)).when(total > SHOWN, |d| {
        d.child(
            div()
                .id("env-toggle")
                .pt_1()
                .text_sm()
                .text_color(cx.theme().blue)
                .cursor_pointer()
                .child(if hidden > 0 { format!("Show {hidden} more") } else { "Show less".to_string() })
                .on_click(move |_, _, cx| {
                    let _ = panel.update(cx, crate::details::DetailPanel::toggle_env);
                }),
        )
    });
    vec![heading(&format!("Environment variables ({total})")).into_any_element(), list.into_any_element()]
}

/// One `env` entry's value and where it comes from.
fn env_value(ctx: &Ctx, sources: &EnvSources, c: &Value, e: &Value, cx: &App) -> (AnyElement, AnyElement) {
    let text = |t: String| div().child(t).into_any_element();
    fn key_ref(r: &Value) -> (&str, &str) {
        (r["name"].as_str().unwrap_or_default(), r["key"].as_str().unwrap_or("?"))
    }
    if let Some(v) = e["value"].as_str() {
        (text(v.to_string()), div().into_any_element())
    } else if let Some(r) = e.pointer("/valueFrom/secretKeyRef") {
        let (name, k) = key_ref(r);
        let v = sources.secret(name, k).unwrap_or_else(|| "(not readable)".into());
        let source =
            h_flex().gap_1().child(format!("Secret key {k} of")).child(ctx.link(name.to_string(), "secrets", name, cx));
        (text(v), source.into_any_element())
    } else if let Some(r) = e.pointer("/valueFrom/configMapKeyRef") {
        let (name, k) = key_ref(r);
        let v = sources.config_map(name, k).unwrap_or_else(|| "(missing)".into());
        let source =
            h_flex().gap_1().child(format!("key {k} of")).child(ctx.link(name.to_string(), "configmaps", name, cx));
        (text(v), source.into_any_element())
    } else if let Some(p) = e.pointer("/valueFrom/fieldRef/fieldPath").and_then(Value::as_str) {
        (text(field_ref(ctx.obj, p).unwrap_or("-").to_string()), text(format!("field {p}")))
    } else if let Some(r) = e.pointer("/valueFrom/resourceFieldRef/resource").and_then(Value::as_str) {
        let limit =
            c.pointer(&format!("/resources/{}", r.replacen('.', "/", 1))).and_then(Value::as_str).unwrap_or("-");
        (text(limit.to_string()), text(format!("resource {r}")))
    } else {
        (text("-".into()), div().into_any_element())
    }
}

/// A downward-API field (`fieldRef.fieldPath`) read from the pod itself: the common
/// metadata, spec and status fields, and single labels or annotations.
fn field_ref<'a>(pod: &'a Value, path: &str) -> Option<&'a str> {
    let item = |prefix: &str, map: &str| {
        let key = path.strip_circumfix(prefix, "']")?;
        pod.pointer(map)?.get(key)?.as_str()
    };
    match path {
        "metadata.name" => s(pod, "/metadata/name"),
        "metadata.namespace" => s(pod, "/metadata/namespace"),
        "metadata.uid" => s(pod, "/metadata/uid"),
        "spec.nodeName" => s(pod, "/spec/nodeName"),
        "spec.serviceAccountName" => s(pod, "/spec/serviceAccountName"),
        "status.podIP" => s(pod, "/status/podIP"),
        "status.hostIP" => s(pod, "/status/hostIP"),
        _ => item("metadata.labels['", "/metadata/labels")
            .or_else(|| item("metadata.annotations['", "/metadata/annotations")),
    }
}

/// An environment variable's line: name, value, source.
fn env_row(key: String, value: AnyElement, source: AnyElement, cx: &App) -> AnyElement {
    let theme = cx.theme();
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
        .child(div().flex_shrink_0().text_xs().text_color(theme.muted_foreground).child(source))
        .into_any_element()
}

/// The container's volume mounts with their source, or nothing.
fn container_mounts(ctx: &Ctx, c: &Value, cx: &App) -> Vec<AnyElement> {
    let Some(mounts) = c["volumeMounts"].as_array().filter(|m| !m.is_empty()) else { return Vec::new() };
    let volumes = arr(ctx.obj, "/spec/volumes");
    let cards = mounts.iter().map(|m| {
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
                let n = v.pointer("/persistentVolumeClaim/claimName").and_then(Value::as_str).unwrap_or_default();
                ctx.link(format!("PVC: {n}"), "persistentvolumeclaims", n, cx).into_any_element()
            }
            Some(v) => {
                let t = v.as_object().and_then(|o| o.keys().find(|k| *k != "name").cloned()).unwrap_or_default();
                div().text_xs().text_color(cx.theme().muted_foreground).child(format!("({t})")).into_any_element()
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
    });
    vec![heading("Volume mounts").into_any_element(), h_flex().flex_wrap().gap_3().children(cards).into_any_element()]
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

/// One line of a pods table, worked out once when the pods are loaded: a node can hold a
/// thousand pods, and their rows are drawn again on every frame while the page scrolls.
pub struct PodRow {
    /// Where the pod came in the list as loaded, to go back to that order.
    pub order: usize,
    pub name: String,
    pub namespace: Option<String>,
    pub ready: usize,
    pub total: usize,
    pub restarts: i64,
    /// kubectl's Status column (CrashLoopBackOff, Completed…), not the phase.
    pub phase: String,
    /// Usage, requests and limits: CPU in millicores, memory in bytes.
    pub cpu: [Option<f64>; 3],
    pub mem: [Option<f64>; 3],
    pub node: String,
    pub created: Option<jiff::Timestamp>,
}

pub fn pod_rows(pods: &[Value], usage: &ContainerUsage) -> Vec<PodRow> {
    pods.iter()
        .enumerate()
        .map(|(order, p)| {
            let name = s(p, "/metadata/name").unwrap_or_default().to_string();
            let statuses = arr(p, "/status/containerStatuses");
            let containers = arr(p, "/spec/containers");
            let usage = usage.get(&name);
            let sum = |f: fn(&(f64, f64)) -> f64| usage.map(|u| u.values().map(f).sum::<f64>());
            let spec = |r: &str| {
                let e = k8s::podspec::of_value(p, r, k8s::podspec::Count::Running);
                (e.request(), e.limit())
            };
            let (cpu, mem) = (spec("cpu"), spec("memory"));
            PodRow {
                order,
                namespace: s(p, "/metadata/namespace").map(String::from),
                ready: statuses.iter().filter(|c| c["ready"].as_bool() == Some(true)).count(),
                total: containers.len(),
                restarts: statuses.iter().filter_map(|c| c["restartCount"].as_i64()).sum(),
                phase: pod_status(p),
                cpu: [sum(|u| u.0), cpu.0, cpu.1],
                mem: [sum(|u| u.1), mem.0, mem.1],
                node: s(p, "/spec/nodeName").unwrap_or("-").to_string(),
                created: s(p, "/metadata/creationTimestamp").and_then(|t| t.parse().ok()),
                name,
            }
        })
        .collect()
}

/// Usage from the node's kubelet (its stats summary) for the rows metrics-server gave
/// none: a node's page lists pods from every namespace, and reads them there. Summed over
/// containers, like metrics-server; matched by namespace and name.
pub fn fill_usage_from_kubelet(rows: &mut [PodRow], stats: &Value) {
    let mut usage = std::collections::HashMap::new();
    for pod in arr(stats, "/pods") {
        let (Some(ns), Some(name)) = (s(pod, "/podRef/namespace"), s(pod, "/podRef/name")) else { continue };
        let containers = arr(pod, "/containers");
        let sum = |ptr: &str| containers.iter().filter_map(|c| c.pointer(ptr)?.as_f64()).sum::<f64>();
        usage.insert((ns, name), (sum("/cpu/usageNanoCores") / 1e6, sum("/memory/workingSetBytes")));
    }
    for row in rows.iter_mut().filter(|r| r.cpu[0].is_none() && r.mem[0].is_none()) {
        if let Some(&(cpu, mem)) = usage.get(&(row.namespace.as_deref().unwrap_or_default(), row.name.as_str())) {
            row.cpu[0] = Some(cpu);
            row.mem[0] = Some(mem);
        }
    }
}

/// A column the pods table sorts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PodSort {
    Namespace,
    Name,
    Ready,
    Status,
    Cpu,
    Memory,
    Restarts,
    Node,
    Age,
}

/// The pods table's sort: a column, and whether it is descending.
pub type PodOrder = Option<(PodSort, bool)>;

/// A header click: ascending, then descending, then the order the pods were loaded in.
pub fn next_pod_order(current: PodOrder, col: PodSort) -> PodOrder {
    match current {
        Some((c, false)) if c == col => Some((col, true)),
        Some((c, true)) if c == col => None,
        _ => Some((col, false)),
    }
}

/// Sort the rows. CPU and memory sort on usage, pods without a reading last either way;
/// ascending age is the newest first. Ties keep the load order.
pub fn sort_pod_rows(rows: &mut [PodRow], order: PodOrder) {
    let Some((col, desc)) = order else {
        rows.sort_unstable_by_key(|r| r.order);
        return;
    };
    let usage = |v: Option<f64>| v.map(|v| if desc { -v } else { v });
    rows.sort_by(|a, b| {
        let by = match col {
            PodSort::Namespace => a.namespace.cmp(&b.namespace).then_with(|| a.name.cmp(&b.name)),
            PodSort::Name => a.name.cmp(&b.name),
            PodSort::Ready => (a.ready, a.total).cmp(&(b.ready, b.total)),
            PodSort::Status => a.phase.cmp(&b.phase),
            PodSort::Restarts => a.restarts.cmp(&b.restarts),
            PodSort::Node => a.node.cmp(&b.node),
            PodSort::Age => b.created.cmp(&a.created),
            PodSort::Cpu | PodSort::Memory => {
                let pick = |r: &PodRow| usage(if col == PodSort::Cpu { r.cpu[0] } else { r.mem[0] });
                // Missing readings last, ascending or descending.
                return match (pick(a), pick(b)) {
                    (Some(x), Some(y)) => x.total_cmp(&y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                }
                .then(a.order.cmp(&b.order));
            }
        };
        if desc { by.reverse() } else { by }.then(a.order.cmp(&b.order))
    });
}

/// The pods table's columns: header, width, sort. Namespace only when the pods come from
/// several namespaces (a node's page).
fn pod_columns(ctx: &Ctx) -> Vec<(&'static str, f32, PodSort)> {
    let mut cols = vec![("Name", 320., PodSort::Name)];
    if ctx.ns().is_none() {
        cols.insert(0, ("Namespace", 180., PodSort::Namespace));
    }
    cols.extend([
        ("Ready", 50., PodSort::Ready),
        ("Status", 110., PodSort::Status),
        ("CPU", 150., PodSort::Cpu),
        ("Memory", 150., PodSort::Memory),
        ("Restarts", 60., PodSort::Restarts),
        ("Node", 220., PodSort::Node),
        ("Age", 60., PodSort::Age),
    ]);
    cols
}

/// The header of a pods table; its rows are items of the page's list (`pod_row`). A click
/// on a column sorts on it.
pub(super) fn pods_table(ctx: &Ctx, cx: &App) -> AnyElement {
    if ctx.related.pod_rows.is_empty() {
        return div().text_sm().text_color(cx.theme().muted_foreground).child("No pods.").into_any_element();
    }
    let muted = cx.theme().muted_foreground;
    h_flex()
        .gap_3()
        .py_1()
        .text_xs()
        .text_color(muted)
        .border_b_1()
        .border_color(cx.theme().border)
        .children(pod_columns(ctx).into_iter().map(|(title, width, col)| {
            let sorted = ctx.pod_order.filter(|(c, _)| *c == col).map(|(_, desc)| desc);
            let panel = ctx.panel.clone();
            h_flex()
                .id(title)
                .w(px(width))
                .gap_1()
                .cursor_pointer()
                .hover(|s| s.text_color(cx.theme().foreground))
                .child(title)
                .when_some(sorted, |d, desc| {
                    let icon = if desc { Lucide::SortDescending } else { Lucide::SortAscending };
                    d.child(Icon::new(icon).size_3())
                })
                .on_click(move |_, _, cx| {
                    let _ = panel.update(cx, |p, cx| p.sort_pods(col, cx));
                })
        }))
        .into_any_element()
}

/// A pod's status as kubectl's Status column says it (its `printPod`): the reason of the
/// first container waiting or terminated over the phase, Init:… while init containers run,
/// Terminating once deleted. The pods list shows this column, colored by `status::tone`.
pub(super) fn pod_status(p: &Value) -> String {
    let mut reason = s(p, "/status/reason").or(s(p, "/status/phase")).unwrap_or("Unknown").to_string();
    let inits = arr(p, "/status/initContainerStatuses");
    let init_total = arr(p, "/spec/initContainers").len().max(inits.len());
    let mut initializing = false;
    for (i, c) in inits.iter().enumerate() {
        let code = c.pointer("/state/terminated/exitCode").and_then(Value::as_i64);
        if code == Some(0) {
            continue;
        }
        // A sidecar (restartPolicy Always) that is running counts as done.
        if c["started"].as_bool() == Some(true) && c.pointer("/state/running").is_some() {
            continue;
        }
        reason = if let Some(t) = c.pointer("/state/terminated") {
            match t["reason"].as_str() {
                Some(r) if !r.is_empty() => format!("Init:{r}"),
                _ => match t["signal"].as_i64().filter(|&s| s != 0) {
                    Some(sig) => format!("Init:Signal:{sig}"),
                    None => format!("Init:ExitCode:{}", code.unwrap_or(0)),
                },
            }
        } else if let Some(r) = s(c, "/state/waiting/reason").filter(|r| !r.is_empty() && *r != "PodInitializing") {
            format!("Init:{r}")
        } else {
            format!("Init:{i}/{init_total}")
        };
        initializing = true;
        break;
    }
    if !initializing {
        let mut has_running = false;
        for c in arr(p, "/status/containerStatuses").iter().rev() {
            if let Some(r) = s(c, "/state/waiting/reason").filter(|r| !r.is_empty()) {
                reason = r.to_string();
            } else if let Some(t) = c.pointer("/state/terminated") {
                reason = match t["reason"].as_str() {
                    Some(r) if !r.is_empty() => r.to_string(),
                    _ => match t["signal"].as_i64().filter(|&s| s != 0) {
                        Some(sig) => format!("Signal:{sig}"),
                        None => format!("ExitCode:{}", t["exitCode"].as_i64().unwrap_or(0)),
                    },
                };
            } else if c.pointer("/state/running").is_some() && c["ready"].as_bool() == Some(true) {
                has_running = true;
            }
        }
        if reason == "Completed" && has_running {
            let ready = arr(p, "/status/conditions")
                .iter()
                .any(|c| c["type"].as_str() == Some("Ready") && c["status"].as_str() == Some("True"));
            reason = if ready { "Running" } else { "NotReady" }.into();
        }
    }
    if s(p, "/metadata/deletionTimestamp").is_some() {
        reason = if s(p, "/status/reason") == Some("NodeLost") { "Unknown" } else { "Terminating" }.into();
    }
    reason
}

pub(super) fn pod_row(ctx: &Ctx, p: &PodRow, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let usage = |[value, request, limit]: [Option<f64>; 3], format: fn(f64) -> String, color: Hsla| {
        h_flex()
            .w(px(150.))
            .gap_2()
            .child(div().w(px(48.)).child(value.map(format).unwrap_or_default()))
            .child(div().flex_1().child(usage_bar(value, request, limit, color, cx)))
    };
    // Pods are opened in their own namespace (a node lists pods from every namespace).
    h_flex()
        .gap_3()
        .py_1()
        .text_sm()
        .border_b_1()
        .border_color(theme.border)
        .when(ctx.ns().is_none(), |d| {
            d.child(
                div()
                    .w(px(180.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .text_color(theme.muted_foreground)
                    .child(p.namespace.clone().unwrap_or_default()),
            )
        })
        .child(div().w(px(320.)).child(ctx.link_in(p.name.clone(), "pods", &p.name, p.namespace.clone(), cx)))
        .child(
            div()
                .w(px(50.))
                .text_color(if p.ready == p.total { theme.success } else { theme.warning })
                .child(format!("{}/{}", p.ready, p.total)),
        )
        .child(div().w(px(110.)).text_color(state_color(&p.phase, cx)).child(p.phase.clone()))
        .child(usage(p.cpu, k8s::format_cpu, theme.blue))
        .child(usage(p.mem, k8s::format_bytes, theme.success))
        .child(div().w(px(60.)).when(p.restarts > 0, |d| d.text_color(theme.warning)).child(p.restarts.to_string()))
        .child(div().w(px(220.)).child(ctx.link(p.node.clone(), "nodes", &p.node, cx)))
        .child(div().w(px(60.)).text_color(theme.muted_foreground).child(p.created.map(k8s::age).unwrap_or_default()))
        .into_any_element()
}

/// A pod's ephemeral storage use (from the kubelet), against its limit if any.
pub(super) fn pod_ephemeral(ctx: &Ctx) -> Option<String> {
    let st = ctx.related.node_stats.as_ref()?;
    let (name, ns) = (s(ctx.obj, "/metadata/name")?, s(ctx.obj, "/metadata/namespace")?);
    let pod = arr(st, "/pods")
        .iter()
        .find(|p| s(p, "/podRef/name") == Some(name) && s(p, "/podRef/namespace") == Some(ns))?;
    let used = pod.pointer("/ephemeral-storage/usedBytes").and_then(Value::as_f64)?;
    let limit =
        k8s::podspec::of_value(ctx.obj, "ephemeral-storage", k8s::podspec::Count::Running).limit().unwrap_or(0.0);
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

#[cfg(test)]
mod tests {
    #[test]
    fn pod_status_as_kubectl() {
        use super::pod_status;
        use serde_json::json;
        let crash = json!({"status": {"phase": "Running", "containerStatuses": [
            {"ready": false, "state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}});
        assert_eq!(pod_status(&crash), "CrashLoopBackOff");
        let error = json!({"status": {"phase": "Running", "containerStatuses": [
            {"state": {"terminated": {"reason": "Error", "exitCode": 1}}}]}});
        assert_eq!(pod_status(&error), "Error");
        let done = json!({"status": {"phase": "Succeeded", "containerStatuses": [
            {"state": {"terminated": {"reason": "Completed", "exitCode": 0}}}]}});
        assert_eq!(pod_status(&done), "Completed");
        let running = json!({"status": {"phase": "Running", "containerStatuses": [
            {"ready": true, "state": {"running": {}}}]}});
        assert_eq!(pod_status(&running), "Running");
        let init = json!({"spec": {"initContainers": [{}, {}]}, "status": {"phase": "Pending",
            "initContainerStatuses": [{"state": {"terminated": {"exitCode": 0}}}, {"state": {"running": {}}}]}});
        assert_eq!(pod_status(&init), "Init:1/2");
        let init_crash = json!({"status": {"phase": "Pending", "initContainerStatuses": [
            {"state": {"waiting": {"reason": "CrashLoopBackOff"}}}]}});
        assert_eq!(pod_status(&init_crash), "Init:CrashLoopBackOff");
        let evicted = json!({"status": {"phase": "Failed", "reason": "Evicted"}});
        assert_eq!(pod_status(&evicted), "Evicted");
        let deleting =
            json!({"metadata": {"deletionTimestamp": "2026-01-01T00:00:00Z"}, "status": {"phase": "Running"}});
        assert_eq!(pod_status(&deleting), "Terminating");
        // The pods list colors these by `status::tone`: a crash loop must not read as healthy.
        assert_eq!(crate::status::tone("CrashLoopBackOff"), Some(crate::status::Tone::Bad));
    }

    use serde_json::json;

    use super::{PodRow, PodSort, field_ref, fill_usage_from_kubelet, next_pod_order, sort_pod_rows};

    #[test]
    fn downward_api_fields_come_from_the_pod() {
        let pod = json!({
            "metadata": {"name": "web-1", "labels": {"app": "web"}, "annotations": {"team": "core"}},
            "spec": {"nodeName": "node-a"},
        });
        assert_eq!(field_ref(&pod, "metadata.name"), Some("web-1"));
        assert_eq!(field_ref(&pod, "spec.nodeName"), Some("node-a"));
        assert_eq!(field_ref(&pod, "metadata.labels['app']"), Some("web"));
        assert_eq!(field_ref(&pod, "metadata.annotations['team']"), Some("core"));
        assert_eq!(field_ref(&pod, "metadata.labels['missing']"), None);
        assert_eq!(field_ref(&pod, "status.podIP"), None);
        assert_eq!(field_ref(&pod, "metadata.labels"), None);
    }

    fn row(order: usize, ns: &str, name: &str, cpu: Option<f64>) -> PodRow {
        PodRow {
            order,
            name: name.into(),
            namespace: Some(ns.into()),
            ready: 1,
            total: 1,
            restarts: 0,
            phase: "Running".into(),
            cpu: [cpu, None, None],
            mem: [None; 3],
            node: "n1".into(),
            created: None,
        }
    }

    fn names(rows: &[PodRow]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn sorts_by_usage_with_missing_readings_last() {
        let mut rows = vec![row(0, "b", "small", Some(5.)), row(1, "a", "none", None), row(2, "a", "big", Some(900.))];
        sort_pod_rows(&mut rows, Some((PodSort::Cpu, true)));
        assert_eq!(names(&rows), ["big", "small", "none"]);
        sort_pod_rows(&mut rows, Some((PodSort::Cpu, false)));
        assert_eq!(names(&rows), ["small", "big", "none"]);
        sort_pod_rows(&mut rows, Some((PodSort::Namespace, false)));
        assert_eq!(names(&rows), ["big", "none", "small"]);
        sort_pod_rows(&mut rows, None);
        assert_eq!(names(&rows), ["small", "none", "big"], "back to the load order");
    }

    #[test]
    fn a_header_click_cycles_up_down_off() {
        let up = next_pod_order(None, PodSort::Cpu);
        assert_eq!(up, Some((PodSort::Cpu, false)));
        let down = next_pod_order(up, PodSort::Cpu);
        assert_eq!(down, Some((PodSort::Cpu, true)));
        assert_eq!(next_pod_order(down, PodSort::Cpu), None);
        assert_eq!(next_pod_order(down, PodSort::Name), Some((PodSort::Name, false)));
    }

    #[test]
    fn a_nodes_pods_get_their_usage_from_the_kubelet() {
        let stats = json!({"pods": [
            {"podRef": {"namespace": "shop", "name": "cache-0"}, "containers": [
                {"cpu": {"usageNanoCores": 2_000_000.0}, "memory": {"workingSetBytes": 4096.0}},
                {"cpu": {"usageNanoCores": 1_000_000.0}, "memory": {"workingSetBytes": 1024.0}},
            ]},
            {"podRef": {"namespace": "other", "name": "web"}, "containers": [
                {"cpu": {"usageNanoCores": 9_000_000.0}, "memory": {"workingSetBytes": 1.0}},
            ]},
        ]});
        let mut rows = vec![row(0, "shop", "cache-0", None), row(1, "shop", "web", None)];
        fill_usage_from_kubelet(&mut rows, &stats);
        assert_eq!((rows[0].cpu[0], rows[0].mem[0]), (Some(3.), Some(5120.)));
        assert_eq!(rows[1].cpu[0], None, "same name in another namespace is another pod");
    }
}
