//! Node page: system info, allocation, devices.

#[allow(unused_imports)]
use super::*;

pub(super) fn node(obj: &Value, cx: &App) -> Div {
    let info = |k: &str| s(obj, &format!("/status/nodeInfo/{k}")).unwrap_or("-").to_string();
    let cap = |k: &str| s(obj, &format!("/status/capacity/{k}")).unwrap_or("-").to_string();
    let alloc = |k: &str| s(obj, &format!("/status/allocatable/{k}")).unwrap_or("-").to_string();
    let taints: Vec<String> = arr(obj, "/spec/taints")
        .iter()
        .map(|t| {
            format!(
                "{}={} ({})",
                t["key"].as_str().unwrap_or("?"),
                t["value"].as_str().unwrap_or(""),
                t["effect"].as_str().unwrap_or("")
            )
        })
        .collect();
    let addresses: Vec<String> = arr(obj, "/status/addresses")
        .iter()
        .map(|a| format!("{}: {}", a["type"].as_str().unwrap_or("?"), a["address"].as_str().unwrap_or("?")))
        .collect();
    v_flex()
        .child(
            row()
                .pt_4()
                .child(field("Kubelet", info("kubeletVersion"), cx))
                .child(field("OS image", info("osImage"), cx))
                .child(field("Kernel", info("kernelVersion"), cx))
                .child(field("Runtime", info("containerRuntimeVersion"), cx))
                .child(field("CPU", format!("{} ({} allocatable)", cap("cpu"), alloc("cpu")), cx))
                .child(field("Memory", format!("{} ({} allocatable)", cap("memory"), alloc("memory")), cx))
                .child(field("Architecture", format!("{}/{}", info("operatingSystem"), info("architecture")), cx))
                .child(field(
                    "Instance type",
                    s(obj, "/metadata/labels/node.kubernetes.io~1instance-type").unwrap_or("-").to_string(),
                    cx,
                ))
                .child(field(
                    "Zone",
                    s(obj, "/metadata/labels/topology.kubernetes.io~1zone").unwrap_or("-").to_string(),
                    cx,
                ))
                .when_some(s(obj, "/metadata/labels/nvidia.com~1gpu.product"), |r, gpu| {
                    r.child(field("GPU", gpu.to_string(), cx))
                })
                .child(field(
                    "Pressure",
                    {
                        let p: Vec<String> = arr(obj, "/status/conditions")
                            .iter()
                            .filter(|c| {
                                c["type"].as_str().is_some_and(|t| t.ends_with("Pressure") || t == "NetworkUnavailable")
                                    && c["status"] == "True"
                            })
                            .filter_map(|c| c["type"].as_str().map(String::from))
                            .collect();
                        if p.is_empty() { "none".to_string() } else { p.join(", ") }
                    },
                    cx,
                ))
                .child(field("Images", arr(obj, "/status/images").len().to_string(), cx))
                .child(field("Max pods", cap("pods"), cx))
                .child(field(
                    "Unschedulable",
                    obj.pointer("/spec/unschedulable").and_then(Value::as_bool).unwrap_or(false).to_string(),
                    cx,
                )),
        )
        .child(row().pt_4().child(field("Addresses", clipped_list(addresses, 6, cx), cx)).child(field(
            "Taints",
            clipped_list(taints, 10, cx),
            cx,
        )))
}

/// Requests and limits of the node's pods against what it can allocate, for CPU,
/// memory, ephemeral storage, pods and extended resources (GPUs, TPUs…), plus
/// filesystem usage from the kubelet.
pub(super) fn node_allocation(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let alloc = obj.pointer("/status/allocatable").and_then(Value::as_object).cloned().unwrap_or_default();
    let capacity = obj.pointer("/status/capacity").and_then(Value::as_object).cloned().unwrap_or_default();
    let mut names: Vec<String> = vec!["cpu".into(), "memory".into(), "ephemeral-storage".into(), "pods".into()];
    names.extend(alloc.keys().filter(|k| k.contains('/') && !k.starts_with("hugepages")).cloned());
    let pods = &ctx.related.pods;
    let running = pods.iter().filter(|p| !matches!(s(p, "/status/phase"), Some("Succeeded" | "Failed"))).count() as f64;
    let fmt = |name: &str, v: f64| match name {
        "cpu" => k8s::format_cores(v),
        "memory" | "ephemeral-storage" => k8s::format_bytes(v),
        _ => format!("{}", (v * 100.0).round() / 100.0),
    };
    let parse = |name: &str, q: &str| match name {
        "cpu" => k8s::parse_cpu(q),
        "memory" | "ephemeral-storage" => k8s::parse_bytes(q),
        _ => q.parse::<f64>().unwrap_or_else(|_| k8s::parse_bytes(q)),
    };
    let pct = |v: f64, of: f64| if of > 0.0 { format!(" ({:.0}%)", v / of * 100.0) } else { String::new() };
    let header = h_flex()
        .gap_3()
        .py_1()
        .text_xs()
        .text_color(muted)
        .border_b_1()
        .border_color(theme.border)
        .child(div().w(px(200.)).child("Resource"))
        .child(div().w(px(220.)).child("Requests"))
        .child(div().w(px(180.)).child("Limits"))
        .child(div().w(px(110.)).child("Allocatable"))
        .child(div().w(px(110.)).child("Capacity"));
    let rows =
        names.into_iter().filter(|n| alloc.contains_key(n)).map(|name| {
            let a = alloc.get(&name).and_then(Value::as_str).map(|q| parse(&name, q)).unwrap_or(0.0);
            let c = capacity.get(&name).and_then(Value::as_str).unwrap_or("-").to_string();
            let (req, lim) = if name == "pods" { (running, 0.0) } else { k8s::pods_allocated(pods, &name) };
            let over = lim > a && a > 0.0;
            h_flex()
                .gap_3()
                .py_1()
                .text_sm()
                .border_b_1()
                .border_color(theme.border)
                .child(div().w(px(200.)).child(name.clone()))
                .child(
                    v_flex()
                        .w(px(220.))
                        .gap_0p5()
                        .child(format!("{}{}", fmt(&name, req), pct(req, a)))
                        .child(usage_bar(Some(req), None, Some(a), theme.blue, cx)),
                )
                .child(div().w(px(180.)).when(over, |d| d.text_color(theme.warning)).child(if name == "pods" {
                    "-".to_string()
                } else {
                    format!("{}{}", fmt(&name, lim), pct(lim, a))
                }))
                .child(div().w(px(110.)).child(fmt(&name, a)))
                .child(div().w(px(110.)).text_color(muted).child(c))
        });
    let mut out = v_flex().child(heading("Allocation")).child(header).children(rows);
    // Filesystems, from the kubelet (needs nodes/proxy).
    if let Some(st) = &ctx.related.node_stats {
        let fs = |path: &str, label: &str| {
            let used = st.pointer(&format!("{path}/usedBytes")).and_then(Value::as_f64)?;
            let cap = st.pointer(&format!("{path}/capacityBytes")).and_then(Value::as_f64)?;
            Some(
                v_flex()
                    .w(px(260.))
                    .gap_0p5()
                    .child(div().text_xs().text_color(muted).child(label.to_string()))
                    .child(div().text_sm().child(format!(
                        "{} of {}{}",
                        k8s::format_bytes(used),
                        k8s::format_bytes(cap),
                        pct(used, cap)
                    )))
                    .child(usage_bar(Some(used), None, Some(cap), theme.blue, cx)),
            )
        };
        out = out.child(
            row()
                .pt_4()
                .children(fs("/node/fs", "Root filesystem"))
                .children(fs("/node/runtime/imageFs", "Image filesystem"))
                .children(fs("/node/runtime/containerFs", "Container filesystem")),
        );
    }
    out
}

/// DRA devices the node publishes (ResourceSlices), by driver and pool.
pub(super) fn node_devices(ctx: &Ctx, cx: &App) -> Option<Div> {
    let slices = &ctx.related.extra;
    if slices.is_empty() {
        return None;
    }
    let muted = cx.theme().muted_foreground;
    Some(v_flex().child(heading("Devices (DRA)")).children(slices.iter().map(|sl| {
        let devices = arr(sl, "/spec/devices");
        card(cx)
            .mb_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div().font_weight(FontWeight::MEDIUM).child(s(sl, "/spec/driver").unwrap_or("?").to_string()),
                    )
                    .child(div().text_xs().text_color(muted).child(format!(
                        "pool {} · {} devices",
                        s(sl, "/spec/pool/name").unwrap_or("?"),
                        devices.len()
                    ))),
            )
            .children(devices.iter().take(32).map(|d| {
                // v1beta1 nests attributes under `basic`.
                let attrs = d.get("attributes").or_else(|| d.pointer("/basic/attributes")).and_then(Value::as_object);
                let text: Vec<String> = attrs
                    .into_iter()
                    .flatten()
                    .take(6)
                    .map(|(k, v)| {
                        format!(
                            "{k}={}",
                            v.as_object()
                                .and_then(|o| o.values().next())
                                .map(|x| x.to_string().trim_matches('"').to_string())
                                .unwrap_or_default()
                        )
                    })
                    .collect();
                h_flex()
                    .gap_2()
                    .text_xs()
                    .child(mono(d["name"].as_str().unwrap_or("?").to_string(), cx))
                    .child(div().text_color(muted).child(text.join("  ")))
            }))
    })))
}
