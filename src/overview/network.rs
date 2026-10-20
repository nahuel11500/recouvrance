//! Networking: Services, Ingresses, Gateway API, NetworkPolicies.

#[allow(unused_imports)]
use super::*;

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
    let mut rules = v_flex().gap_1();
    for r in arr(obj, "/spec/rules") {
        let host = r["host"].as_str().unwrap_or("*").to_string();
        for p in arr(r, "/http/paths") {
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
                    .child(mono(format!("{host}{}", p["path"].as_str().unwrap_or("/")), cx))
                    .child(div().text_color(muted).child("→"))
                    .child(ctx.link(svc.clone(), "services", &svc, cx))
                    .child(div().text_color(muted).child(format!(":{port}"))),
            );
        }
    }
    let tls: Vec<(String, String)> = arr(obj, "/spec/tls")
        .iter()
        .map(|t| {
            let hosts: Vec<&str> =
                t["hosts"].as_array().map(|h| h.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
            (hosts.join(", "), t["secretName"].as_str().unwrap_or_default().to_string())
        })
        .collect();
    v_flex()
        .child(row().pt_4().child(field("Class", class, cx)).child(field(
            "Address",
            if addresses.is_empty() { "-".into() } else { addresses.join(", ") },
            cx,
        )))
        .child(heading("Rules"))
        .child(rules)
        .when(!tls.is_empty(), |d| {
            d.child(heading("TLS")).children(tls.into_iter().map(|(hosts, secret)| {
                h_flex()
                    .gap_2()
                    .text_sm()
                    .child(mono(hosts, cx))
                    .child(div().text_color(muted).child("secret"))
                    .child(ctx.link(secret.clone(), "secrets", &secret, cx))
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
        let st = parents_status.iter().find(|s| s.pointer("/parentRef/name") == p.get("name"));
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

pub(super) fn netpol(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let muted = cx.theme().muted_foreground;
    let selector = pairs(obj, "/spec/podSelector/matchLabels");
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
        let ns = pairs(p, "/namespaceSelector/matchLabels");
        let pods = pairs(p, "/podSelector/matchLabels");
        match (p.get("namespaceSelector").is_some(), p.get("podSelector").is_some()) {
            (true, true) => format!(
                "pods [{}] in namespaces [{}]",
                pods.join(", "),
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
        let p: Vec<String> = r["ports"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| {
                format!(
                    "{}/{}",
                    p["port"]
                        .as_i64()
                        .map(|n| n.to_string())
                        .or_else(|| p["port"].as_str().map(String::from))
                        .unwrap_or("*".into()),
                    p["protocol"].as_str().unwrap_or("TCP")
                )
            })
            .collect();
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
                let peers: Vec<String> = r[peers_key]
                    .as_array()
                    .map(|a| a.iter().map(peer).collect())
                    .unwrap_or_else(|| vec!["anything".into()]);
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
