//! Networking: Services, Ingresses, Gateway API, NetworkPolicies.

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use super::{Ctx, arr, clipped_list, field, heading, mono, mono_url, pairs, row, s};

pub(super) fn service(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let ports: Vec<String> = arr(obj, "/spec/ports")
        .iter()
        .map(|p| {
            format!(
                "{}{} → {} / {}",
                p["name"].as_str().map(|n| format!("{n}: ")).unwrap_or_default(),
                p["port"].as_i64().unwrap_or(0),
                p["targetPort"]
                    .as_i64()
                    .map(|n| n.to_string())
                    .or_else(|| p["targetPort"].as_str().map(String::from))
                    .unwrap_or_default(),
                p["protocol"].as_str().unwrap_or("TCP")
            )
        })
        .collect();
    let selector = pairs(obj, "/spec/selector");
    v_flex().child(
        row()
            .pt_4()
            .child(field("Type", s(obj, "/spec/type").unwrap_or("-").to_string(), cx))
            .child(field("Cluster IP", s(obj, "/spec/clusterIP").unwrap_or("-").to_string(), cx))
            .child(field("Ports", clipped_list(ports, 10, cx), cx))
            .child(field("Selector", clipped_list(selector, 10, cx), cx)),
    )
}

pub(super) fn ingress(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let muted = cx.theme().muted_foreground;
    let class = s(obj, "/spec/ingressClassName").unwrap_or("-").to_string();
    let addresses: Vec<String> = arr(obj, "/status/loadBalancer/ingress")
        .iter()
        .filter_map(|a| a["ip"].as_str().or(a["hostname"].as_str()).map(String::from))
        .collect();
    let tls: Vec<(Vec<&str>, String)> = arr(obj, "/spec/tls")
        .iter()
        .map(|t| {
            let hosts: Vec<&str> =
                t["hosts"].as_array().map(|h| h.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
            (hosts, t["secretName"].as_str().unwrap_or_default().to_string())
        })
        .collect();
    let https = |host: &str| tls.iter().any(|(hosts, _)| crate::forwards::tls_covers(hosts, host));
    let mut rules = v_flex().gap_1();
    for (i, r) in arr(obj, "/spec/rules").iter().enumerate() {
        let host = r["host"].as_str().unwrap_or("*").to_string();
        for (j, p) in arr(r, "/http/paths").iter().enumerate() {
            let path = p["path"].as_str().unwrap_or("/");
            let url = crate::forwards::host_url(&host, path, https(&host));
            let svc = p.pointer("/backend/service/name").and_then(Value::as_str).unwrap_or_default().to_string();
            let port = p
                .pointer("/backend/service/port/number")
                .and_then(Value::as_i64)
                .map(|n| n.to_string())
                .or_else(|| p.pointer("/backend/service/port/name").and_then(Value::as_str).map(String::from))
                .unwrap_or_default();
            rules = rules.child(
                h_flex()
                    .gap_2()
                    .text_sm()
                    .child(mono_url(("ingress-rule", i * 1000 + j), format!("{host}{path}"), url, cx))
                    .child(div().text_color(muted).child("→"))
                    .child(ctx.link(svc.clone(), "services", &svc, cx))
                    .child(div().text_color(muted).child(format!(":{port}"))),
            );
        }
    }
    v_flex()
        .child(row().pt_4().child(field("Class", class, cx)).child(field(
            "Address",
            if addresses.is_empty() { "-".into() } else { addresses.join(", ") },
            cx,
        )))
        .child(heading("Rules"))
        .child(rules)
        .when(!tls.is_empty(), |d| {
            d.child(heading("TLS")).children(tls.iter().enumerate().map(|(i, (hosts, secret))| {
                h_flex()
                    .gap_2()
                    .text_sm()
                    .children(hosts.iter().enumerate().map(|(j, h)| {
                        mono_url(
                            ("ingress-tls", i * 1000 + j),
                            h.to_string(),
                            crate::forwards::host_url(h, "/", true),
                            cx,
                        )
                    }))
                    .child(div().text_color(muted).child("secret"))
                    .child(ctx.link(secret.clone(), "secrets", secret, cx))
            }))
        })
}

/// A Gateway API object reference (`parentRefs`, `backendRefs`, `certificateRefs`) as a link.
pub(super) fn gw_ref(ctx: &Ctx, r: &Value, default_group: &str, default_kind: &str, cx: &App) -> AnyElement {
    let group = r["group"].as_str().unwrap_or(default_group);
    let kind = r["kind"].as_str().unwrap_or(default_kind);
    let name = r["name"].as_str().unwrap_or_default();
    let ns = r["namespace"].as_str().map(String::from).or_else(|| ctx.ns());
    let query = if group.is_empty() { kind.to_lowercase() } else { format!("{}.{group}", kind.to_lowercase()) };
    let same_ns = ns == ctx.ns();
    let text = if same_ns { name.to_string() } else { format!("{}/{name}", ns.clone().unwrap_or_default()) };
    let link = ctx.link_in(text, &query, name, ns, cx);
    if kind == default_kind && group == default_group {
        link.into_any_element()
    } else {
        h_flex()
            .gap_1()
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(kind.to_string()))
            .child(link)
            .into_any_element()
    }
}

/// Whether a route's status `parentRef` is this `spec.parentRefs` entry: the same gateway
/// (group, kind, namespace and name, with their defaults) and the same listener (section, port).
fn same_parent(spec: &Value, status: &Value, route_ns: Option<&str>) -> bool {
    let str_or = |r: &Value, key: &str, default: Option<&str>| r[key].as_str().or(default).map(String::from);
    let group = |r: &Value| str_or(r, "group", Some("gateway.networking.k8s.io"));
    let kind = |r: &Value| str_or(r, "kind", Some("Gateway"));
    let ns = |r: &Value| str_or(r, "namespace", route_ns);
    spec["name"] == status["name"]
        && group(spec) == group(status)
        && kind(spec) == kind(status)
        && ns(spec) == ns(status)
        && spec["sectionName"] == status["sectionName"]
        && spec["port"] == status["port"]
}

pub(super) fn condition_ok(conds: &[Value], ty: &str) -> Option<bool> {
    conds.iter().find(|c| c["type"] == ty).map(|c| c["status"] == "True")
}

pub(super) fn status_dot(ok: Option<bool>, label: &str, cx: &App) -> Div {
    let theme = cx.theme();
    let color = match ok {
        Some(true) => theme.success,
        Some(false) => theme.danger,
        None => theme.muted_foreground,
    };
    h_flex()
        .gap_1()
        .text_sm()
        .text_color(color)
        .child(div().size(px(7.)).rounded_full().bg(color))
        .child(label.to_string())
}

pub(super) fn gateway(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let addresses: Vec<String> =
        arr(obj, "/status/addresses").iter().filter_map(|a| a["value"].as_str().map(String::from)).collect();
    let listener_status = arr(obj, "/status/listeners");
    let header = |t: &str, w: f32| div().w(px(w)).flex_shrink_0().text_xs().text_color(muted).child(t.to_string());
    let listeners = arr(obj, "/spec/listeners").iter().map(|l| {
        let name = l["name"].as_str().unwrap_or_default().to_string();
        let st = listener_status.iter().find(|s| s["name"] == name.as_str());
        let conds = st.map(|s| arr(s, "/conditions")).unwrap_or_default();
        let attached = st.and_then(|s| s["attachedRoutes"].as_i64()).unwrap_or(0);
        let programmed = condition_ok(conds, "Programmed");
        let certs: Vec<AnyElement> =
            arr(l, "/tls/certificateRefs").iter().map(|r| gw_ref(ctx, r, "", "Secret", cx)).collect();
        let from = l.pointer("/allowedRoutes/namespaces/from").and_then(Value::as_str).unwrap_or("Same").to_string();
        h_flex()
            .gap_2()
            .py_1()
            .border_b_1()
            .border_color(theme.border)
            .text_sm()
            .child(div().w(px(140.)).flex_shrink_0().font_weight(FontWeight::MEDIUM).child(name))
            .child(div().w(px(120.)).flex_shrink_0().child(format!(
                "{} :{}",
                l["protocol"].as_str().unwrap_or("-"),
                l["port"].as_i64().unwrap_or(0)
            )))
            .child(
                div()
                    .w(px(220.))
                    .flex_shrink_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(l["hostname"].as_str().unwrap_or("*").to_string()),
            )
            .child(div().w(px(90.)).flex_shrink_0().child(attached.to_string()))
            .child(div().w(px(110.)).flex_shrink_0().text_color(muted).child(from))
            .child(div().w(px(110.)).flex_shrink_0().child(status_dot(
                programmed,
                if programmed == Some(false) { "Not ready" } else { "Programmed" },
                cx,
            )))
            .child(h_flex().gap_2().flex_wrap().children(certs))
    });
    v_flex()
        .child(
            row()
                .pt_4()
                .child(field(
                    "Class",
                    ctx.link(
                        s(obj, "/spec/gatewayClassName").unwrap_or("-").to_string(),
                        "gatewayclasses",
                        s(obj, "/spec/gatewayClassName").unwrap_or_default(),
                        cx,
                    ),
                    cx,
                ))
                .child(field("Addresses", if addresses.is_empty() { "-".into() } else { addresses.join(", ") }, cx))
                .child(field(
                    "Status",
                    {
                        let ok = condition_ok(arr(obj, "/status/conditions"), "Programmed");
                        status_dot(
                            ok,
                            match ok {
                                Some(true) => "Programmed",
                                Some(false) => "Not programmed",
                                None => "Unknown",
                            },
                            cx,
                        )
                    },
                    cx,
                )),
        )
        .child(heading("Listeners"))
        .child(
            h_flex()
                .gap_2()
                .child(header("Name", 140.))
                .child(header("Protocol", 120.))
                .child(header("Hostname", 220.))
                .child(header("Routes", 90.))
                .child(header("Routes from", 110.))
                .child(header("Status", 110.))
                .child(header("Certificates", 160.)),
        )
        .children(listeners)
}

/// HTTPRoute, GRPCRoute, TLSRoute, TCPRoute…: where it attaches and where its rules send traffic.
pub(super) fn route(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let theme = cx.theme();
    let muted = theme.muted_foreground;
    let hostnames: Vec<String> =
        arr(obj, "/spec/hostnames").iter().filter_map(|h| h.as_str().map(String::from)).collect();
    let parents_status = arr(obj, "/status/parents");
    let parents = arr(obj, "/spec/parentRefs").iter().map(|p| {
        let st = parents_status.iter().find(|s| same_parent(p, &s["parentRef"], ctx.ns().as_deref()));
        let conds = st.map(|s| arr(s, "/conditions")).unwrap_or_default();
        let accepted = condition_ok(conds, "Accepted");
        let resolved = condition_ok(conds, "ResolvedRefs");
        let problem = conds
            .iter()
            .find(|c| c["status"] == "False")
            .and_then(|c| c["message"].as_str().filter(|m| !m.is_empty()).or(c["reason"].as_str()))
            .map(String::from);
        h_flex()
            .gap_3()
            .py_1()
            .child(gw_ref(ctx, p, "gateway.networking.k8s.io", "Gateway", cx))
            .when_some(p["sectionName"].as_str(), |d, s| {
                d.child(div().text_sm().text_color(muted).child(format!("listener {s}")))
            })
            .child(status_dot(accepted, "Accepted", cx))
            .child(status_dot(resolved, "Refs resolved", cx))
            .when_some(problem, |d, m| d.child(div().text_sm().text_color(theme.danger).child(m)))
            .when_some(st.and_then(|s| s["controllerName"].as_str()), |d, c| {
                d.child(div().text_xs().text_color(muted).child(c.to_string()))
            })
    });
    let rules = arr(obj, "/spec/rules").iter().enumerate().map(|(i, r)| {
        let matches: Vec<String> = arr(r, "/matches")
            .iter()
            .map(|m| {
                let mut parts = Vec::new();
                if let Some(method) = m["method"].as_str() {
                    parts.push(method.to_string());
                }
                if let Some(p) = m.get("path") {
                    let op = match p["type"].as_str().unwrap_or("PathPrefix") {
                        "Exact" => "=",
                        "RegularExpression" => "~",
                        _ => "",
                    };
                    parts.push(format!(
                        "{op}{}{}",
                        p["value"].as_str().unwrap_or("/"),
                        if op.is_empty() { "*" } else { "" }
                    ));
                }
                if let Some(svc) = m.pointer("/method/service") {
                    parts.push(format!(
                        "{}/{}",
                        svc.as_str().unwrap_or("*"),
                        m.pointer("/method/method").and_then(Value::as_str).unwrap_or("*")
                    ));
                }
                for h in arr(m, "/headers") {
                    parts.push(format!(
                        "{}: {}",
                        h["name"].as_str().unwrap_or_default(),
                        h["value"].as_str().unwrap_or_default()
                    ));
                }
                for q in arr(m, "/queryParams") {
                    parts.push(format!(
                        "?{}={}",
                        q["name"].as_str().unwrap_or_default(),
                        q["value"].as_str().unwrap_or_default()
                    ));
                }
                parts.join(" ")
            })
            .collect();
        let backends: Vec<AnyElement> = arr(r, "/backendRefs")
            .iter()
            .map(|b| {
                h_flex()
                    .gap_1()
                    .child(gw_ref(ctx, b, "", "Service", cx))
                    .when_some(b["port"].as_i64(), |d, p| d.child(div().text_color(muted).child(format!(":{p}"))))
                    .when_some(b["weight"].as_i64(), |d, w| {
                        d.child(div().text_xs().text_color(muted).child(format!("weight {w}")))
                    })
                    .into_any_element()
            })
            .collect();
        let filters: Vec<String> =
            arr(r, "/filters").iter().filter_map(|f| f["type"].as_str().map(String::from)).collect();
        h_flex()
            .gap_3()
            .py_2()
            .items_start()
            .border_b_1()
            .border_color(theme.border)
            .child(div().w(px(24.)).flex_shrink_0().text_xs().text_color(muted).child(format!("#{}", i + 1)))
            .child(
                v_flex().w(px(320.)).flex_shrink_0().children(
                    if matches.is_empty() { vec!["everything".to_string()] } else { matches }
                        .into_iter()
                        .map(|m| mono(m, cx)),
                ),
            )
            .child(div().text_color(muted).child("→"))
            .child(v_flex().gap_1().children(backends).when(!filters.is_empty(), |d| {
                d.child(div().text_xs().text_color(muted).child(format!("filters: {}", filters.join(", "))))
            }))
    });
    v_flex()
        .child(row().pt_4().child(field(
            "Hostnames",
            if hostnames.is_empty() { "any (from the listener)".into() } else { hostnames.join(", ") },
            cx,
        )))
        .child(heading("Attached to"))
        .children(parents)
        .child(heading("Rules"))
        .children(rules)
}

/// A label selector's terms: `matchLabels` as "key: value", then `matchExpressions`
/// as kubectl writes them ("tier in (db, cache)", "!canary"). Empty selects everything.
fn selector_terms(v: &Value, path: &str) -> Vec<String> {
    let mut out = pairs(v, &format!("{path}/matchLabels"));
    for e in arr(v, &format!("{path}/matchExpressions")) {
        let key = e["key"].as_str().unwrap_or_default();
        let values: Vec<&str> = arr(e, "/values").iter().filter_map(Value::as_str).collect();
        out.push(match e["operator"].as_str().unwrap_or_default() {
            "In" => format!("{key} in ({})", values.join(", ")),
            "NotIn" => format!("{key} notin ({})", values.join(", ")),
            "Exists" => key.to_string(),
            "DoesNotExist" => format!("!{key}"),
            op => format!("{key} {op} ({})", values.join(", ")),
        });
    }
    out
}

pub(super) fn netpol(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let muted = cx.theme().muted_foreground;
    let selector = selector_terms(obj, "/spec/podSelector");
    let types: Vec<&str> = arr(obj, "/spec/policyTypes").iter().filter_map(Value::as_str).collect();
    let peer = |p: &Value| -> String {
        if let Some(c) = p.pointer("/ipBlock/cidr").and_then(Value::as_str) {
            let except: Vec<&str> = p
                .pointer("/ipBlock/except")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                .unwrap_or_default();
            return if except.is_empty() {
                format!("CIDR {c}")
            } else {
                format!("CIDR {c} except {}", except.join(", "))
            };
        }
        let ns = selector_terms(p, "/namespaceSelector");
        let pods = selector_terms(p, "/podSelector");
        match (p.get("namespaceSelector").is_some(), p.get("podSelector").is_some()) {
            (true, true) => format!(
                "pods [{}] in namespaces [{}]",
                if pods.is_empty() { "all".into() } else { pods.join(", ") },
                if ns.is_empty() { "all".into() } else { ns.join(", ") }
            ),
            (true, false) => format!("namespaces [{}]", if ns.is_empty() { "all".into() } else { ns.join(", ") }),
            (false, true) => {
                format!("pods [{}] in this namespace", if pods.is_empty() { "all".into() } else { pods.join(", ") })
            }
            _ => "anything".into(),
        }
    };
    let ports = |r: &Value| -> String {
        let p: Vec<String> = r["ports"].as_array().into_iter().flatten().map(netpol_port).collect();
        if p.is_empty() { "all ports".into() } else { p.join(", ") }
    };
    let section = |title: &str, key: &str, peers_key: &str, empty: &str| {
        let rules = arr(obj, &format!("/spec/{key}"));
        let applies = types.contains(&title);
        v_flex()
            .child(heading(title))
            .when(!applies, |d| d.child(div().text_sm().text_color(muted).child("Not restricted by this policy.")))
            .when(applies && rules.is_empty(), |d| d.child(div().text_sm().child(empty.to_string())))
            .children(rules.iter().map(|r| {
                let peers: Vec<String> =
                    r[peers_key].as_array().map_or_else(|| vec!["anything".into()], |a| a.iter().map(peer).collect());
                v_flex()
                    .py_1()
                    .text_sm()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .children(peers.into_iter().map(|p| div().child(p)))
                    .child(div().text_xs().text_color(muted).child(ports(r)))
            }))
    };
    v_flex()
        .child(
            row()
                .pt_4()
                .child(field(
                    "Applies to pods",
                    if selector.is_empty() {
                        mono("every pod in the namespace", cx)
                    } else {
                        clipped_list(selector, 8, cx)
                    },
                    cx,
                ))
                .child(field("Policy types", types.join(", "), cx)),
        )
        .child(section("Ingress", "ingress", "from", "All incoming traffic is denied."))
        .child(section("Egress", "egress", "to", "All outgoing traffic is denied."))
}

/// One NetworkPolicy port: `80/TCP`, `http/TCP`, a range `8000-9000/TCP`, or `*/UDP` for any port.
fn netpol_port(p: &Value) -> String {
    let port = p["port"]
        .as_i64()
        .map(|n| n.to_string())
        .or_else(|| p["port"].as_str().map(String::from))
        .unwrap_or("*".into());
    let end = p["endPort"].as_i64().map(|n| format!("-{n}")).unwrap_or_default();
    format!("{port}{end}/{}", p["protocol"].as_str().unwrap_or("TCP"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn selector_terms_include_expressions() {
        let p = json!({"podSelector": {"matchLabels": {"app": "web"}, "matchExpressions": [
            {"key": "tier", "operator": "In", "values": ["db", "cache"]},
            {"key": "canary", "operator": "DoesNotExist"}]}});
        assert_eq!(super::selector_terms(&p, "/podSelector"), ["app: web", "tier in (db, cache)", "!canary"]);
        let only = json!({"podSelector": {"matchExpressions": [{"key": "tier", "operator": "Exists"}]}});
        assert_eq!(super::selector_terms(&only, "/podSelector"), ["tier"]);
        assert!(super::selector_terms(&json!({"podSelector": {}}), "/podSelector").is_empty());
    }

    #[test]
    fn netpol_port_shows_ranges() {
        assert_eq!(super::netpol_port(&json!({"port": 8000, "endPort": 9000})), "8000-9000/TCP");
        assert_eq!(super::netpol_port(&json!({"port": 53, "protocol": "UDP"})), "53/UDP");
        assert_eq!(super::netpol_port(&json!({"port": "http"})), "http/TCP");
        assert_eq!(super::netpol_port(&json!({"protocol": "SCTP"})), "*/SCTP");
    }

    #[test]
    fn route_parent_status_matches_the_listener() {
        let http = json!({"name": "gw", "sectionName": "http"});
        let https = json!({"name": "gw", "sectionName": "https"});
        let status = json!({"group": "gateway.networking.k8s.io", "kind": "Gateway", "namespace": "web",
            "name": "gw", "sectionName": "https"});
        assert!(super::same_parent(&https, &status, Some("web")));
        assert!(!super::same_parent(&http, &status, Some("web")));
        let other_ns = json!({"name": "gw", "namespace": "infra", "sectionName": "https"});
        assert!(!super::same_parent(&other_ns, &status, Some("web")));
        let port = json!({"name": "gw", "port": 443});
        assert!(super::same_parent(&port, &json!({"name": "gw", "port": 443}), Some("web")));
        assert!(!super::same_parent(&port, &json!({"name": "gw", "port": 80}), Some("web")));
    }
}
