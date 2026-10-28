//! Configuration and storage: Secrets (certificates), ConfigMaps, PVCs, namespaces, CRDs.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use super::{Ctx, age_of, arr, clipped_list, field, heading, mono, row, s, state_color};
use crate::k8s;

/// Certificates are public, so they are decoded even while values are masked.
pub(super) fn certificates(data: &serde_json::Map<String, Value>, base64: bool, cx: &App) -> Option<Div> {
    use base64::Engine as _;
    let mut certs = Vec::new();
    for (k, v) in data {
        let Some(text) = v.as_str() else { continue };
        let bytes = if base64 {
            base64::engine::general_purpose::STANDARD.decode(text).unwrap_or_default()
        } else {
            text.as_bytes().to_vec()
        };
        if !bytes.windows(27).any(|w| w == b"-----BEGIN CERTIFICATE-----") {
            continue;
        }
        certs.extend(crate::certs::parse_pem(&bytes).into_iter().map(|c| (k.clone(), c)));
    }
    if certs.is_empty() {
        return None;
    }
    let theme = cx.theme();
    let now = jiff::Timestamp::now();
    Some(v_flex().child(heading(&format!("Certificates ({})", certs.len()))).children(certs.into_iter().map(
        |(key, c)| {
            let date = |t: Option<jiff::Timestamp>| {
                t.map_or_else(|| "-".into(), |t| t.strftime("%Y-%m-%d %H:%M UTC").to_string())
            };
            let (state, color) = match c.not_after.map(|t| days_left(t, now)) {
                _ if c.not_yet_valid(now) => (format!("not valid until {}", date(c.not_before)), theme.danger),
                Some(d) if d < 0 => (expired(c.not_after, now), theme.danger),
                Some(d) if d < 14 => (format!("expires in {}", days(d)), theme.danger),
                Some(d) if d < 30 => (format!("expires in {}", days(d)), theme.warning),
                Some(d) => (format!("valid, {} left", days(d)), theme.success),
                None => ("unknown validity".into(), theme.muted_foreground),
            };
            v_flex()
                .p_3()
                .mb_2()
                .gap_1()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .child(
                    h_flex()
                        .gap_2()
                        .child(Icon::new(Lucide::ShieldCheck).text_color(color))
                        .child(div().font_weight(FontWeight::MEDIUM).child(c.subject.clone()))
                        .when(c.is_ca, |d| d.child(div().text_xs().px_1().rounded_sm().bg(theme.secondary).child("CA")))
                        .child(div().flex_1())
                        .child(div().text_sm().text_color(color).child(state))
                        .child(div().text_xs().text_color(theme.muted_foreground).child(key)),
                )
                .child(
                    row()
                        .child(field("Issuer", c.issuer.clone(), cx))
                        .child(field("Not before", date(c.not_before), cx))
                        .child(field("Not after", date(c.not_after), cx))
                        .when(!c.sans.is_empty(), |r| r.child(field("Names", clipped_list(c.sans.clone(), 8, cx), cx))),
                )
        },
    )))
}

/// Whole days until `t`, rounded down: negative as soon as it has passed.
fn days_left(t: jiff::Timestamp, now: jiff::Timestamp) -> i64 {
    (t.as_second() - now.as_second()).div_euclid(86400)
}

fn days(n: i64) -> String {
    if n == 1 { "1 day".into() } else { format!("{n} days") }
}

/// "expired 2 days ago", from the whole days since it passed.
fn expired(not_after: Option<jiff::Timestamp>, now: jiff::Timestamp) -> String {
    match not_after.map(|t| crate::certs::days_ago(t, now)) {
        Some(n) if n > 0 => format!("expired {} ago", days(n)),
        _ => "expired less than a day ago".into(),
    }
}

/// A Secret's type and certificates; its keys are items of the page's list (`secret_entry`).
pub(super) fn secret(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let data = obj.get("data").and_then(Value::as_object).cloned().unwrap_or_default();
    let muted = cx.theme().muted_foreground;
    v_flex()
        .child(row().pt_4().child(field("Type", s(obj, "/type").unwrap_or("Opaque").to_string(), cx)))
        .children(external_secret_source(ctx, cx))
        .children(certificates(&data, true, cx))
        .child(heading(&format!("Data ({} keys)", data.len())))
        .child(div().pb_1().text_xs().text_color(muted).child("Click a value to show or hide it."))
}

/// "From ExternalSecret": the ExternalSecret that writes this Secret, its store, how often it
/// syncs and whether the last sync worked. Nothing for a Secret no ExternalSecret owns.
fn external_secret_source(ctx: &Ctx, cx: &App) -> Option<Div> {
    let es = ctx.related.external_secret.as_ref()?;
    let name = s(es, "/metadata/name")?;
    let link = ctx.link(name.to_string(), &crate::external_secrets::kubectl_name("ExternalSecret"), name, cx);
    let mut fields = sync_fields(ctx, es, cx).child(field("ExternalSecret", link, cx));
    if let Some((status, reason, message)) = crate::external_secrets::ready(es) {
        use crate::status::Tone;
        let theme = cx.theme();
        let color = match crate::status::condition_tone("Ready", status) {
            Some(Tone::Good) => theme.success,
            Some(Tone::Bad) => theme.danger,
            Some(Tone::Busy) => theme.warning,
            Some(Tone::Done) | None => theme.muted_foreground,
        };
        let why = crate::external_secrets::shorten(if message.trim().is_empty() { reason } else { message });
        let ready = v_flex()
            .child(div().text_color(color).child(status.to_string()))
            .when(!why.is_empty(), |d| d.child(div().text_xs().text_color(theme.muted_foreground).child(why)));
        fields = fields.child(field("Ready", ready, cx).max_w(px(480.)));
    }
    Some(v_flex().child(heading("From ExternalSecret")).child(fields))
}

/// An ExternalSecret's page: the Secret it writes and where its values come from.
pub(super) fn external_secret_page(ctx: &Ctx, cx: &App) -> Div {
    let es = ctx.obj;
    let mut fields = sync_fields(ctx, es, cx);
    if let Some(target) = crate::external_secrets::target_of(es) {
        fields = fields.child(field("Target Secret", ctx.link(target.to_string(), "secrets", target, cx), cx));
    }
    v_flex().child(fields)
}

/// An ExternalSecret's store (a link), refresh interval and last sync, in a row.
fn sync_fields(ctx: &Ctx, es: &Value, cx: &App) -> Div {
    let mut fields = row().pt_4();
    if let Some((kind, name)) = crate::external_secrets::store_of(es) {
        // A ClusterSecretStore has no namespace.
        let ns = if kind == "ClusterSecretStore" { None } else { s(es, "/metadata/namespace").map(String::from) };
        let link = ctx.link_in(format!("{kind}: {name}"), &crate::external_secrets::kubectl_name(kind), name, ns, cx);
        fields = fields.child(field("Store", link, cx));
    }
    // The operator's default when unset; 0 syncs once only.
    let interval = s(es, "/spec/refreshInterval").unwrap_or("1h");
    let last = age_of(es, "/status/refreshTime").map_or_else(|| "never".to_string(), |a| format!("{a} ago"));
    fields.child(field("Refresh interval", interval.to_string(), cx)).child(field("Last refresh", last, cx))
}

pub(super) fn secret_entry(ctx: &Ctx, i: usize, cx: &App) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let Some((k, v)) = ctx.obj.get("data").and_then(Value::as_object).and_then(|d| d.iter().nth(i)) else {
        return div().into_any_element();
    };
    use base64::Engine as _;
    let decoded = v
        .as_str()
        .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
        .map(String::from_utf8_lossy_owned)
        .unwrap_or_default();
    let shown = ctx.reveal || ctx.revealed_keys.contains(k);
    let value = if shown { decoded.clone() } else { "••••••••••••".to_string() };
    let (panel, key) = (ctx.panel.clone(), k.clone());
    h_flex()
        .py_1()
        .px_2()
        .mb_1()
        .gap_2()
        .items_start()
        .rounded_md()
        .bg(cx.theme().secondary.opacity(0.5))
        .child(
            v_flex()
                .id(("secret-key", i))
                .flex_1()
                .min_w_0()
                .cursor_pointer()
                .on_click(move |_, _, cx| {
                    let _ = panel.update(cx, |p, cx| p.toggle_secret_key(&key, cx));
                })
                .child(div().text_xs().text_color(muted).child(k.clone()))
                .child(mono(value, cx)),
        )
        .child(
            Button::new(("copy-secret", i))
                .xsmall()
                .ghost()
                .icon(Lucide::Copy)
                .tooltip("Copy the value")
                .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(decoded.clone()))),
        )
        .into_any_element()
}

/// A ConfigMap's certificates; its keys are items of the page's list (`config_entry`).
pub(super) fn config_map(obj: &Value, cx: &App) -> Div {
    let data = obj.get("data").and_then(Value::as_object).cloned().unwrap_or_default();
    v_flex().children(certificates(&data, false, cx)).child(heading(&format!("Data ({} keys)", data.len())))
}

pub(super) fn config_entry(obj: &Value, i: usize, cx: &App) -> AnyElement {
    let Some((k, v)) = obj.get("data").and_then(Value::as_object).and_then(|d| d.iter().nth(i)) else {
        return div().into_any_element();
    };
    let muted = cx.theme().muted_foreground;
    let text = v.as_str().unwrap_or_default();
    let lines: Vec<&str> = text.lines().take(40).collect();
    let more = text.lines().count().saturating_sub(40);
    v_flex()
        .py_1()
        .px_2()
        .mb_1()
        .rounded_md()
        .bg(cx.theme().secondary.opacity(0.5))
        .child(div().text_xs().text_color(muted).child(k.clone()))
        .children(lines.into_iter().map(|l| mono(l.to_string(), cx)))
        .when(more > 0, |d| d.child(div().text_xs().text_color(muted).child(format!("… {more} more lines (see YAML)"))))
        .into_any_element()
}

/// `string`, `[]object`, `map[string]string`… from an OpenAPI schema node.
pub(super) fn schema_type(v: &Value) -> String {
    match v["type"].as_str() {
        Some("array") => format!("[]{}", schema_type(&v["items"])),
        Some("object") if v.get("additionalProperties").is_some_and(Value::is_object) => {
            format!("map[string]{}", schema_type(&v["additionalProperties"]))
        }
        Some(t) => t.to_string(),
        None if v.get("x-kubernetes-int-or-string").is_some() => "int-or-string".into(),
        None if v.get("x-kubernetes-preserve-unknown-fields").is_some() => "any".into(),
        None => "-".into(),
    }
}

/// A field of a CRD's spec: depth, name, type, required, first line of its description.
pub type SchemaRow = (usize, String, String, bool, String);

/// The spec fields of a CRD's stored version, worked out once when it is loaded.
pub fn crd_rows(obj: &Value) -> Vec<SchemaRow> {
    let versions = arr(obj, "/spec/versions");
    let Some(storage) = versions.iter().find(|v| v["storage"] == true).or(versions.first()) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    schema_rows(&storage["schema"]["openAPIV3Schema"]["properties"]["spec"], 0, &mut rows);
    rows
}

/// The fields of a schema node (objects, or arrays of objects), two levels deep.
fn schema_rows(node: &Value, depth: usize, out: &mut Vec<SchemaRow>) {
    let node = if node["type"] == "array" { &node["items"] } else { node };
    let required: Vec<&str> =
        node["required"].as_array().map(|r| r.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    let Some(props) = node["properties"].as_object() else { return };
    for (name, p) in props {
        let desc = p["description"].as_str().unwrap_or_default().lines().next().unwrap_or_default().to_string();
        out.push((depth, name.clone(), schema_type(p), required.contains(&name.as_str()), desc));
        if depth < 1 {
            schema_rows(p, depth + 1, out);
        }
    }
}

pub(super) fn crd(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let theme = cx.theme();
    let names = &obj["spec"]["names"];
    let group = s(obj, "/spec/group").unwrap_or_default().to_string();
    let plural = names["plural"].as_str().unwrap_or_default().to_string();
    let short: Vec<String> = names["shortNames"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let versions = arr(obj, "/spec/versions");
    let storage = versions.iter().find(|v| v["storage"] == true).or(versions.first());
    let storage = storage.and_then(|v| v["name"].as_str()).unwrap_or_default();
    let instances = ctx.link(format!("Open the {plural} list"), &format!("{plural}.{group}"), "", cx);
    v_flex()
        .child(
            row()
                .pt_4()
                .child(field("Kind", names["kind"].as_str().unwrap_or_default().to_string(), cx))
                .child(field("Group", group, cx))
                .child(field("Scope", s(obj, "/spec/scope").unwrap_or_default().to_string(), cx))
                .child(field(
                    "kubectl",
                    format!(
                        "{plural}{}",
                        if short.is_empty() { String::new() } else { format!(" · {}", short.join(", ")) }
                    ),
                    cx,
                ))
                .child(field("Instances", instances, cx)),
        )
        .child(heading("Versions"))
        .children(versions.iter().map(|v| {
            h_flex()
                .gap_3()
                .text_sm()
                .child(mono(v["name"].as_str().unwrap_or_default().to_string(), cx))
                .when(v["served"] == true, |d| d.child(div().text_color(theme.success).child("served")))
                .when(v["storage"] == true, |d| d.child(div().text_color(theme.blue).child("storage")))
                .when(v["deprecated"] == true, |d| {
                    d.child(
                        div()
                            .text_color(theme.warning)
                            .child(v["deprecationWarning"].as_str().unwrap_or("deprecated").to_string()),
                    )
                })
        }))
        .child(heading(&format!("spec fields ({storage})")))
}

/// One spec field of a CRD, an item of the page's list.
pub(super) fn schema_row((depth, name, ty, required, desc): &SchemaRow, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let depth = *depth;
    h_flex()
        .gap_3()
        .py_0p5()
        .text_sm()
        .border_b_1()
        .border_color(theme.border.opacity(0.5))
        .child(
            div()
                .w(px(260.))
                .flex_shrink_0()
                .pl(px(depth as f32 * 18.))
                .font_family(theme.mono_font_family.clone())
                .text_color(if depth == 0 { theme.foreground } else { theme.muted_foreground })
                .child(name.clone()),
        )
        .child(
            div()
                .w(px(170.))
                .flex_shrink_0()
                .font_family(theme.mono_font_family.clone())
                .text_xs()
                .text_color(theme.blue)
                .child(ty.clone()),
        )
        .child(
            div()
                .w(px(70.))
                .flex_shrink_0()
                .text_xs()
                .text_color(theme.warning)
                .when(*required, |d| d.child("required")),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_xs()
                .text_color(theme.muted_foreground)
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(desc.clone()),
        )
        .into_any_element()
}

pub(super) fn pvc(ctx: &Ctx, cx: &App) -> Div {
    let obj = ctx.obj;
    let volume = s(obj, "/spec/volumeName").unwrap_or_default().to_string();
    let class = s(obj, "/spec/storageClassName").unwrap_or_default().to_string();
    let modes: Vec<&str> = arr(obj, "/spec/accessModes").iter().filter_map(Value::as_str).collect();
    let phase = s(obj, "/status/phase").unwrap_or("-").to_string();
    v_flex().child(
        row()
            .pt_4()
            .child(field("Status", div().text_color(state_color(&phase, cx)).child(phase.clone()), cx))
            .child(field("Capacity", s(obj, "/status/capacity/storage").unwrap_or("-").to_string(), cx))
            .child(field("Requested", s(obj, "/spec/resources/requests/storage").unwrap_or("-").to_string(), cx))
            .child(field("Access modes", modes.join(", "), cx))
            .child(field(
                "Volume",
                if volume.is_empty() {
                    div().child("-").into_any_element()
                } else {
                    ctx.link(volume.clone(), "persistentvolumes", &volume, cx).into_any_element()
                },
                cx,
            ))
            .child(field(
                "Storage class",
                if class.is_empty() {
                    div().child("-").into_any_element()
                } else {
                    ctx.link(class.clone(), "storageclasses", &class, cx).into_any_element()
                },
                cx,
            )),
    )
}

pub(super) fn namespace(ctx: &Ctx, cx: &App) -> Div {
    let pods = &ctx.related.pods;
    let running = pods.iter().filter(|p| s(p, "/status/phase") == Some("Running")).count();
    let theme = cx.theme();
    let mut quotas = v_flex().gap_2();
    for q in &ctx.related.extra {
        let name = s(q, "/metadata/name").unwrap_or_default().to_string();
        let hard = q.pointer("/status/hard").and_then(Value::as_object).cloned().unwrap_or_default();
        let used = q.pointer("/status/used").and_then(Value::as_object).cloned().unwrap_or_default();
        let rows = hard.iter().map(|(res, h)| {
            let h = h.as_str().unwrap_or_default();
            let u = used.get(res).and_then(Value::as_str).unwrap_or("0");
            let parse = |q: &str| if res.contains("cpu") { k8s::parse_cpu(q) } else { k8s::parse_bytes(q) };
            let pct = if parse(h) > 0.0 { (parse(u) / parse(h)).clamp(0.0, 1.0) } else { 0.0 };
            let color = if pct > 0.9 {
                theme.danger
            } else if pct > 0.75 {
                theme.warning
            } else {
                theme.success
            };
            h_flex()
                .gap_3()
                .text_sm()
                .child(div().w(px(260.)).child(res.clone()))
                .child(div().w(px(160.)).child(format!("{u} / {h}")))
                .child(
                    div()
                        .w(px(240.))
                        .h(px(6.))
                        .rounded_full()
                        .bg(theme.secondary)
                        .child(div().h_full().rounded_full().bg(color).w(relative(pct as f32))),
                )
                .child(div().text_xs().text_color(theme.muted_foreground).child(format!("{:.0}%", pct * 100.0)))
        });
        quotas = quotas.child(
            v_flex()
                .gap_1()
                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(ctx.link_in(
                    name.clone(),
                    "resourcequotas",
                    &name,
                    s(ctx.obj, "/metadata/name").map(String::from),
                    cx,
                )))
                .children(rows),
        );
    }
    v_flex()
        .child(
            row()
                .pt_4()
                .child(field("Status", s(ctx.obj, "/status/phase").unwrap_or("-").to_string(), cx))
                .child(field("Pods", format!("{running} running of {}", pods.len()), cx)),
        )
        .child(heading("Resource quotas"))
        .child(if ctx.related.extra.is_empty() {
            div().text_sm().text_color(theme.muted_foreground).child("No quota in this namespace.").into_any_element()
        } else {
            quotas.into_any_element()
        })
}

#[cfg(test)]
mod tests {
    #[test]
    fn expired_certificates_count_as_past() {
        let now: jiff::Timestamp = "2026-01-10T12:00:00Z".parse().unwrap();
        let at = |t: &str| super::days_left(t.parse().unwrap(), now);
        // Days left, rounded down: negative as soon as it has passed...
        assert_eq!(at("2026-01-10T09:00:00Z"), -1);
        assert_eq!(at("2026-01-08T13:00:00Z"), -2);
        // ...but how long ago is told in the days that went by.
        let ago = |t: &str| super::expired(Some(t.parse().unwrap()), now);
        assert_eq!(ago("2026-01-10T09:00:00Z"), "expired less than a day ago");
        assert_eq!(ago("2026-01-09T11:00:00Z"), "expired 1 day ago");
        assert_eq!(ago("2026-01-08T13:00:00Z"), "expired 1 day ago");
        assert_eq!(ago("2026-01-07T12:00:00Z"), "expired 3 days ago");
        assert_eq!(at("2026-01-10T15:00:00Z"), 0);
        assert_eq!(at("2026-01-12T12:00:00Z"), 2);
        assert_eq!(super::days(1), "1 day");
        assert_eq!(super::days(0), "0 days");
    }
}
