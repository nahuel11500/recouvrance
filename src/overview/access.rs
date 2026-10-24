//! RBAC: roles, bindings, ServiceAccounts.

use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use super::{Ctx, arr, field, heading, pods_table, row, s};

/// A table of PolicyRules.
pub(super) fn rules_table(rules: &[Value], cx: &App) -> Div {
    let theme = cx.theme();
    let list = |r: &Value, k: &str| {
        let v: Vec<String> = arr(r, &format!("/{k}"))
            .iter()
            .filter_map(|x| x.as_str().map(|s| if s.is_empty() { "core".to_string() } else { s.to_string() }))
            .collect();
        if v.is_empty() { "-".to_string() } else { v.join(", ") }
    };
    let danger = |verbs: &str| {
        verbs.contains('*')
            || verbs.contains("delete")
            || verbs.contains("escalate")
            || verbs.contains("bind")
            || verbs.contains("impersonate")
    };
    v_flex()
        .child(
            h_flex()
                .gap_3()
                .py_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .border_b_1()
                .border_color(theme.border)
                .child(div().w(px(180.)).child("API groups"))
                .child(div().w(px(320.)).child("Resources"))
                .child(div().w(px(200.)).child("Names"))
                .child(div().flex_1().child("Verbs")),
        )
        .children(rules.iter().map(|r| {
            let verbs = list(r, "verbs");
            let non_resource = list(r, "nonResourceURLs");
            h_flex()
                .gap_3()
                .py_1()
                .items_start()
                .text_sm()
                .border_b_1()
                .border_color(theme.border)
                .child(div().w(px(180.)).child(list(r, "apiGroups")))
                .child(div().w(px(320.)).child(if non_resource == "-" {
                    list(r, "resources")
                } else {
                    format!("URLs: {non_resource}")
                }))
                .child(div().w(px(200.)).text_color(theme.muted_foreground).child(
                    match list(r, "resourceNames").as_str() {
                        "-" => "any".to_string(),
                        n => n.to_string(),
                    },
                ))
                .child(div().flex_1().when(danger(&verbs), |d| d.text_color(theme.warning)).child(verbs))
        }))
}

pub(super) fn subjects_table(ctx: &Ctx, subjects: &[Value], removable: bool, cx: &App) -> Div {
    let theme = cx.theme();
    v_flex()
        .child(
            h_flex()
                .gap_3()
                .py_1()
                .text_xs()
                .text_color(theme.muted_foreground)
                .border_b_1()
                .border_color(theme.border)
                .child(div().w(px(140.)).child("Kind"))
                .child(div().w(px(380.)).child("Name"))
                .child(div().w(px(200.)).child("Namespace")),
        )
        .children(subjects.iter().enumerate().map(|(i, sub)| {
            let kind = sub["kind"].as_str().unwrap_or("?").to_string();
            let name = sub["name"].as_str().unwrap_or("?").to_string();
            let ns = sub["namespace"].as_str().map(String::from);
            let panel = ctx.panel.clone();
            h_flex()
                .gap_3()
                .py_1()
                .text_sm()
                .border_b_1()
                .border_color(theme.border)
                .child(div().w(px(140.)).child(kind.clone()))
                .child(div().w(px(380.)).child(if kind == "ServiceAccount" {
                    ctx.link_in(name.clone(), "serviceaccounts", &name, ns.clone(), cx).into_any_element()
                } else {
                    div().child(name).into_any_element()
                }))
                .child(div().w(px(200.)).text_color(theme.muted_foreground).child(ns.unwrap_or_else(|| "-".into())))
                .when(removable, |d| {
                    d.child(
                        div()
                            .id(("rm-subject", i))
                            .text_xs()
                            .text_color(theme.danger)
                            .cursor_pointer()
                            .child("Remove…")
                            .on_click(move |_, window, cx| {
                                let _ = panel.update(cx, |p, cx| p.confirm_remove_subject(i, window, cx));
                            }),
                    )
                })
        }))
}

pub(super) fn binding_row(ctx: &Ctx, b: &Value, cx: &App) -> Div {
    let theme = cx.theme();
    let name = s(b, "/metadata/name").unwrap_or("?").to_string();
    let ns = s(b, "/metadata/namespace").map(String::from);
    let kind = if ns.is_some() { "rolebindings" } else { "clusterrolebindings" };
    let subjects: Vec<String> = arr(b, "/subjects")
        .iter()
        .map(|x| {
            format!(
                "{} {}{}",
                x["kind"].as_str().unwrap_or("?"),
                x["namespace"].as_str().map(|n| format!("{n}/")).unwrap_or_default(),
                x["name"].as_str().unwrap_or("?")
            )
        })
        .collect();
    h_flex()
        .gap_3()
        .py_1()
        .items_start()
        .text_sm()
        .border_b_1()
        .border_color(theme.border)
        .child(div().w(px(320.)).child(ctx.link_in(
            format!("{}{name}", ns.as_ref().map(|n| format!("{n}/")).unwrap_or_default()),
            kind,
            &name,
            ns,
            cx,
        )))
        .child(div().flex_1().text_xs().child(subjects.join(", ")))
}

/// Role or ClusterRole: its rules and who it is bound to.
pub(super) fn role_page(ctx: &Ctx, cx: &App) -> Div {
    let rules = arr(ctx.obj, "/rules").to_vec();
    let kind = ctx.kind.ar.kind.clone();
    let name = s(ctx.obj, "/metadata/name").unwrap_or_default().to_string();
    let bindings = ctx
        .related
        .rbac
        .as_ref()
        .map(|snap| crate::rbac::bindings_of(snap, &kind, &name, s(ctx.obj, "/metadata/namespace")))
        .unwrap_or_default();
    let mut d = v_flex().child(heading(&format!("Rules ({})", rules.len()))).child(rules_table(&rules, cx));
    if ctx.obj.get("aggregationRule").is_some() {
        d = d.child(
            div()
                .pt_2()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Aggregated: its rules come from ClusterRoles matching its aggregationRule."),
        );
    }
    d.child(heading(&format!("Bound by ({})", bindings.len())))
        .when(ctx.related.rbac.is_none(), |d| {
            d.child(div().text_sm().text_color(cx.theme().muted_foreground).child("Loading…"))
        })
        .children(rbac_note(ctx, cx))
        .children(bindings.iter().map(|b| binding_row(ctx, b, cx)))
}

/// Which roles and bindings could not be listed, so an empty section does not read as "none".
fn rbac_note(ctx: &Ctx, cx: &App) -> Option<Div> {
    let note = ctx.related.rbac.as_ref()?.load_note()?;
    Some(div().py_1().text_xs().text_color(cx.theme().warning).child(note))
}

/// RoleBinding or ClusterRoleBinding: the role it grants and to whom.
pub(super) fn binding_page(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let role_kind = s(obj, "/roleRef/kind").unwrap_or("?").to_string();
    let role = s(obj, "/roleRef/name").unwrap_or("?").to_string();
    let link_kind = if role_kind == "ClusterRole" { "clusterroles" } else { "roles" };
    let rules = ctx
        .related
        .rbac
        .as_ref()
        .and_then(|snap| {
            let list = if role_kind == "ClusterRole" { &snap.cluster_roles } else { &snap.roles };
            list.iter().find(|r| s(r, "/metadata/name") == Some(role.as_str())).map(|r| arr(r, "/rules").to_vec())
        })
        .unwrap_or_default();
    let subjects = arr(obj, "/subjects").to_vec();
    v_flex()
        .child(
            row()
                .pt_4()
                .child(field(
                    "Grants",
                    h_flex().gap_1().child(role_kind.clone()).child(ctx.link_in(
                        role.clone(),
                        link_kind,
                        &role,
                        if role_kind == "ClusterRole" { None } else { ctx.ns() },
                        cx,
                    )),
                    cx,
                ))
                .child(field("Applies in", ctx.ns().unwrap_or_else(|| "every namespace".into()), cx)),
        )
        .child(heading(&format!("Subjects ({})", subjects.len())))
        .child(subjects_table(ctx, &subjects, true, cx))
        .child(heading("What it allows"))
        .children(rbac_note(ctx, cx))
        .child(rules_table(&rules, cx))
}

/// ServiceAccount: bindings that apply to it, what they allow, pods running as it.
pub(super) fn service_account_page(ctx: &Ctx, cx: &App) -> Div {
    let ns = ctx.ns().unwrap_or_default();
    let name = s(ctx.obj, "/metadata/name").unwrap_or_default().to_string();
    let grants =
        ctx.related.rbac.as_ref().map(|snap| crate::rbac::service_account_grants(snap, &ns, &name)).unwrap_or_default();
    let theme = cx.theme();
    let mut d = v_flex().child(heading(&format!("Bindings that apply ({})", grants.len())));
    if ctx.related.rbac.is_none() {
        d = d.child(div().text_sm().text_color(theme.muted_foreground).child("Loading…"));
    }
    d = d.children(rbac_note(ctx, cx));
    for (binding, role, scope, rules) in grants {
        d = d.child(
            v_flex()
                .mb_3()
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .child(h_flex().gap_2().text_sm().child(div().font_weight(FontWeight::MEDIUM).child(binding)).child(
                    div().text_color(theme.muted_foreground).child(format!(
                        "→ {role} · {}",
                        scope.map_or_else(|| "cluster-wide".into(), |s| format!("in {s}"))
                    )),
                ))
                .child(rules_table(&rules, cx)),
        );
    }
    d.child(heading(&format!("Pods running as it ({})", ctx.related.pods.len()))).child(pods_table(ctx, cx))
}
