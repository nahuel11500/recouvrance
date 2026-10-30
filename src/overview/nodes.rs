//! Node page: system info, allocation, devices.

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use super::{Ctx, arr, card, clipped_list, field, heading, mono, row, s, tone_color, usage_bar};
use crate::k8s;
use crate::ui::selectable::selectable;

pub(super) fn node(obj: &Value, cx: &App) -> Div {
    let info = |k: &str| s(obj, &format!("/status/nodeInfo/{k}")).unwrap_or("-").to_string();
    let cap = |k: &str| s(obj, &format!("/status/capacity/{k}")).unwrap_or("-").to_string();
    let alloc = |k: &str| s(obj, &format!("/status/allocatable/{k}")).unwrap_or("-").to_string();
    let text = |t: &Value, k: &str| t[k].as_str().unwrap_or_default().to_string();
    let spec_taints: Vec<(String, String, String)> =
        arr(obj, "/spec/taints").iter().map(|t| (text(t, "key"), text(t, "value"), text(t, "effect"))).collect();
    // As kubectl prints them, and "none" rather than nothing: an empty field reads as not loaded.
    let mut taints: Vec<String> = spec_taints
        .iter()
        .map(|(k, v, e)| if v.is_empty() { format!("{k}:{e}") } else { format!("{k}={v}:{e}") })
        .collect();
    if taints.is_empty() {
        taints.push("none".into());
    }
    // kubectl's Status, with the taints that keep pods off it because something is wrong.
    let status = crate::taints::node_status(
        arr(obj, "/status/conditions").iter().find(|c| c["type"] == "Ready").and_then(|c| c["status"].as_str()),
        obj.pointer("/spec/unschedulable").and_then(Value::as_bool).unwrap_or(false),
        &crate::taints::trouble_labels(spec_taints.iter().map(|(k, _, e)| (k.as_str(), e.as_str()))),
    );
    let addresses: Vec<String> = arr(obj, "/status/addresses")
        .iter()
        .map(|a| format!("{}: {}", a["type"].as_str().unwrap_or("?"), a["address"].as_str().unwrap_or("?")))
        .collect();
    v_flex()
        .child(
            row()
                .pt_4()
                .child(field(
                    "Status",
                    div().text_color(tone_color(crate::status::node_tone(&status), cx)).child(status),
                    cx,
                ))
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
                                c["type"].as_str().is_some_and(|ty| {
                                    crate::status::node_condition_bad(ty, c["status"].as_str().unwrap_or_default())
                                })
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
    let cards = names.into_iter().filter(|n| alloc.contains_key(n)).map(|name| {
        let a = alloc.get(&name).and_then(Value::as_str).map_or(0.0, |q| parse(&name, q));
        let c = capacity.get(&name).and_then(Value::as_str).map_or(a, |q| parse(&name, q));
        let (req, lim) = if name == "pods" { (running, 0.0) } else { ctx.related.allocated(&name) };
        let share = if a > 0.0 { req / a } else { 0.0 };
        let color = match share {
            s if s >= 0.95 => theme.danger,
            s if s >= 0.8 => theme.warning,
            _ => theme.blue,
        };
        let limits = (name != "pods").then(|| {
            let over = lim > a && a > 0.0;
            div().when(over, |d| d.text_color(theme.warning)).child(selectable(format!(
                "limits {}{}{}",
                fmt(&name, lim),
                pct(lim, a),
                if over { " · overcommitted" } else { "" }
            )))
        });
        card(cx)
            .w(px(300.))
            .p_3()
            .gap_2()
            .child(
                h_flex()
                    .justify_between()
                    .items_baseline()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(selectable(name.clone())))
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(color)
                            .child(selectable(format!("{:.0}%", share * 100.0))),
                    ),
            )
            .child(allocation_bar(req, lim, a, color, cx))
            .child(
                v_flex()
                    .text_xs()
                    .text_color(muted)
                    .child(div().text_color(theme.foreground).child(selectable(format!(
                        "{} of {} {}",
                        fmt(&name, req),
                        fmt(&name, a),
                        if name == "pods" { "running" } else { "requested" }
                    ))))
                    .children(limits)
                    // Capacity is worth a line only when the node reserves part of it.
                    .when(fmt(&name, c) != fmt(&name, a), |d| {
                        d.child(selectable(format!("capacity {}", fmt(&name, c))))
                    }),
            )
    });
    let mut out =
        v_flex().child(heading("Allocation")).child(h_flex().flex_wrap().items_stretch().gap_3().children(cards));
    // Filesystems, from the kubelet (needs nodes/proxy).
    if let Some(st) = &ctx.related.node_stats {
        let fs = |path: &str, label: &str| {
            let used = st.pointer(&format!("{path}/usedBytes")).and_then(Value::as_f64)?;
            let cap = st.pointer(&format!("{path}/capacityBytes")).and_then(Value::as_f64)?;
            Some(
                v_flex()
                    .w(px(260.))
                    .gap_0p5()
                    .child(div().text_xs().text_color(muted).child(selectable(label.to_string())))
                    .child(div().text_sm().child(selectable(format!(
                        "{} of {}{}",
                        k8s::format_bytes(used),
                        k8s::format_bytes(cap),
                        pct(used, cap)
                    ))))
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

/// Requests as a solid fill and limits as a lighter one behind it, against what the
/// node can allocate. Limits past it fill the bar and end in a warning edge.
fn allocation_bar(req: f64, lim: f64, alloc: f64, color: Hsla, cx: &App) -> Div {
    let theme = cx.theme();
    let at = |v: f64| if alloc > 0.0 { (v / alloc).clamp(0.0, 1.0) as f32 } else { 0.0 };
    let fill = |w: f32, bg: Hsla| div().absolute().left_0().top_0().h_full().rounded_full().bg(bg).w(relative(w));
    div()
        .relative()
        .h(px(10.))
        .w_full()
        .rounded_full()
        .bg(theme.secondary)
        .child(fill(at(lim), color.opacity(0.25)))
        .child(fill(at(req), color))
        .when(lim > alloc && alloc > 0.0, |d| {
            d.child(div().absolute().right_0().top_0().h_full().w(px(3.)).rounded_r_full().bg(theme.warning))
        })
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
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .child(selectable(s(sl, "/spec/driver").unwrap_or("?").to_string())),
                    )
                    .child(div().text_xs().text_color(muted).child(selectable(format!(
                        "pool {} · {} devices",
                        s(sl, "/spec/pool/name").unwrap_or("?"),
                        devices.len()
                    )))),
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
                    .child(div().text_color(muted).child(selectable(text.join("  "))))
            }))
    })))
}
