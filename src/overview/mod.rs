//! Overview pages for objects: a references card,
//! kind-specific sections, usage bars against requests and limits.

use std::collections::BTreeMap;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::Button;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde_json::Value;

use crate::details::{DetailPanel, OpenObject};
use crate::k8s::{self, ContainerUsage, Kind};

mod access;
mod alerts;
mod argo;
mod config;
mod network;
mod nodes;
mod pods;
mod usage;
mod workloads;

use access::{binding_page, role_page, service_account_page};
pub(crate) use config::*;
use network::{gateway, ingress, netpol, route, service};
use nodes::{node, node_allocation, node_devices};
pub(crate) use pods::*;
pub(crate) use usage::*;
pub(crate) use workloads::*;

/// Data fetched next to the object, for links and usage.
#[derive(Default)]
pub struct Related {
    pub usage: ContainerUsage,
    pub pods: Vec<Value>,
    pub services: Vec<Value>,
    pub revisions: Vec<Value>,
    pub referenced_by: BTreeMap<String, Vec<String>>,
    /// Kind-specific extras: quotas of a namespace, jobs of a cronjob.
    pub extra: Vec<Value>,
    /// CPU (millicores) and memory (bytes) over the last hour, from Prometheus.
    pub history: Option<(crate::prom::Series, crate::prom::Series)>,
    /// GPU utilization and memory (DCGM exporter), when there is any.
    pub gpu: Option<crate::prom::GpuUsage>,
    /// A pod's CPU (millicores) and memory history per container.
    pub container_history: std::collections::HashMap<String, (crate::prom::Series, crate::prom::Series)>,
    /// ConfigMaps and Secrets a pod's environment reads, by "ConfigMap/name" / "Secret/name".
    pub env_sources: std::collections::HashMap<String, Value>,
    /// NetworkPolicies selecting this pod (None: not looked up; Some(empty): none).
    pub netpols: Option<Vec<Value>>,
    /// Kubelet stats summary of the node (a node's page, or a pod's node).
    pub node_stats: Option<Value>,
    /// A pod's events as (type, reason, message, count), for diagnosis.
    pub events: Vec<(String, String, String, i32)>,
    /// Last lines of a crash-looping container's previous run: (container, text).
    pub crash_logs: Option<(String, String)>,
    /// Roles and bindings, for RBAC and ServiceAccount pages.
    pub rbac: Option<crate::rbac::Snapshot>,
    /// Lists that could not be loaded, by field ("pods", "services"…): "forbidden" or
    /// "failed: …", so an empty section does not read as "there are none".
    pub errors: BTreeMap<&'static str, String>,
    /// Prometheus alerts about the object (a namespace's: all of them), worst first.
    pub alerts: Vec<crate::alerts::Alert>,
    /// Pods' (requests, limits) per resource, summed once when they are loaded: a node's
    /// page reads them on every frame, and a node can hold a thousand pods.
    pub allocated: std::collections::HashMap<String, (f64, f64)>,
    /// The pods table's rows, worked out once from `pods` and `usage`.
    pub pod_rows: Vec<PodRow>,
}

impl Related {
    /// The pods' (requests, limits) of a resource, from the sums made at load time.
    pub fn allocated(&self, resource: &str) -> (f64, f64) {
        match self.allocated.get(resource) {
            Some(sums) => *sums,
            None => k8s::pods_allocated(&self.pods, resource),
        }
    }

    /// Work out once what the page draws from the pods: the table's rows and, for a node,
    /// the pods' requests and limits of each resource it offers. Slow for a thousand pods,
    /// so run it off the UI thread.
    pub fn index(&mut self, obj: &Value) {
        self.pod_rows = pod_rows(&self.pods, &self.usage);
        if let Some(stats) = &self.node_stats {
            fill_usage_from_kubelet(&mut self.pod_rows, stats);
        }
        let resources = obj.pointer("/status/allocatable").and_then(Value::as_object);
        self.allocated = resources
            .into_iter()
            .flat_map(|a| a.keys())
            .map(|r| (r.clone(), k8s::pods_allocated(&self.pods, r)))
            .collect();
    }

    /// Keep a list's items, or note why it failed under `section` (and keep nothing).
    pub fn noted<T: Default>(&mut self, section: &'static str, result: Result<T>) -> T {
        result.unwrap_or_else(|e| {
            self.errors.insert(section, k8s::lists::load_error(&e));
            T::default()
        })
    }
}

/// A heading, with why its list could not be loaded ("forbidden", "failed: …") next to it.
fn heading_for(text: &str, related: &Related, section: &str, cx: &App) -> Div {
    let heading = heading(text);
    match related.errors.get(section) {
        None => heading,
        Some(e) => heading
            .flex()
            .gap_2()
            .items_baseline()
            .child(div().text_xs().font_weight(FontWeight::NORMAL).text_color(cx.theme().danger).child(e.clone())),
    }
}

/// "Pods: forbidden · Services: failed: …" for the lists of a tab that could not be loaded.
fn load_notes(related: &Related, sections: &[&str], cx: &App) -> Option<Div> {
    let notes: Vec<String> =
        sections.iter().filter_map(|s| related.errors.get(s).map(|e| format!("{}: {e}", capitalize(s)))).collect();
    (!notes.is_empty()).then(|| div().py_1().text_xs().text_color(cx.theme().danger).child(notes.join(" · ")))
}

pub struct Ctx<'a> {
    /// ReplicaSet chosen for comparison in the Revisions tab.
    pub compare: Option<&'a str>,
    pub obj: &'a Value,
    pub kind: &'a Kind,
    pub related: &'a Related,
    pub panel: WeakEntity<DetailPanel>,
    pub reveal: bool,
    /// Secret keys revealed one by one (by clicking them).
    pub revealed_keys: &'a std::collections::HashSet<String>,
    /// Range of the usage charts, in minutes.
    pub history_minutes: i64,
    /// Long environment lists are shown in full.
    pub env_expanded: bool,
    /// A CRD's spec fields, worked out once from its schema.
    pub schema_rows: &'a [SchemaRow],
    /// How the pods table is sorted.
    pub pod_order: PodOrder,
    /// The Argo CD Applications that deploy the object, or into the namespace.
    pub argo: &'a k8s::argo::Lookup,
}

impl Ctx<'_> {
    pub(crate) fn ns(&self) -> Option<String> {
        s(self.obj, "/metadata/namespace").map(String::from)
    }

    /// A blue link that opens another object.
    fn link(&self, text: impl Into<SharedString>, kind: &str, name: &str, cx: &App) -> Stateful<Div> {
        self.link_in(text, kind, name, self.ns(), cx)
    }

    /// A link to an object in another namespace.
    fn link_in(
        &self,
        text: impl Into<SharedString>,
        kind: &str,
        name: &str,
        ns: Option<String>,
        cx: &App,
    ) -> Stateful<Div> {
        let (kind, name, panel) = (kind.to_string(), name.to_string(), self.panel.clone());
        let text: SharedString = text.into();
        div()
            .id(SharedString::from(format!("link-{kind}-{name}")))
            .text_color(cx.theme().blue)
            .cursor_pointer()
            .hover(Styled::underline)
            .overflow_hidden()
            .text_ellipsis()
            .whitespace_nowrap()
            .on_click(move |_, _, cx| {
                let ev = OpenObject { kind: kind.clone(), name: name.clone(), namespace: ns.clone(), context: None };
                let _ = panel.update(cx, |_, cx| cx.emit(ev));
            })
            .child(text)
    }
}

fn s<'a>(v: &'a Value, path: &str) -> Option<&'a str> {
    v.pointer(path).and_then(Value::as_str)
}

fn n(v: &Value, path: &str) -> Option<i64> {
    v.pointer(path).and_then(Value::as_i64)
}

fn arr<'a>(v: &'a Value, path: &str) -> &'a [Value] {
    v.pointer(path).and_then(Value::as_array).map_or(&[][..], Vec::as_slice)
}

fn age_of(v: &Value, path: &str) -> Option<String> {
    s(v, path)?.parse::<jiff::Timestamp>().ok().map(k8s::age)
}

/// A muted label above a value.
fn field(label: &str, value: impl IntoElement, cx: &App) -> Div {
    v_flex()
        .min_w(px(160.))
        .gap_0p5()
        .child(div().text_xs().text_color(cx.theme().muted_foreground).child(label.to_string()))
        .child(div().text_sm().child(value))
}

fn heading(text: &str) -> Div {
    div().pt_6().pb_2().text_base().font_weight(FontWeight::SEMIBOLD).child(text.to_string())
}

fn card(cx: &App) -> Div {
    v_flex().p_2().gap_1().rounded_md().border_1().border_color(cx.theme().border).bg(cx.theme().secondary.opacity(0.4))
}

fn mono(text: impl Into<SharedString>, cx: &App) -> Div {
    div().font_family(cx.theme().mono_font_family.clone()).text_xs().child(text.into())
}

fn row() -> Div {
    h_flex().flex_wrap().items_start().gap_x_8().gap_y_4()
}

fn state_color(text: &str, cx: &App) -> Hsla {
    tone_color(crate::status::tone(text), cx)
}

fn tone_color(tone: Option<crate::status::Tone>, cx: &App) -> Hsla {
    use crate::status::Tone;
    let t = cx.theme();
    match tone {
        Some(Tone::Good) => t.success,
        Some(Tone::Busy) => t.warning,
        Some(Tone::Bad) => t.danger,
        Some(Tone::Done) | None => t.muted_foreground,
    }
}

fn pairs(v: &Value, path: &str) -> Vec<String> {
    v.pointer(path)
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| format!("{k}: {}", v.as_str().unwrap_or_default())).collect())
        .unwrap_or_default()
}

/// First few items, then "+N more".
fn clipped_list(items: Vec<String>, max: usize, cx: &App) -> Div {
    let more = items.len().saturating_sub(max);
    v_flex()
        .children(
            items
                .into_iter()
                .take(max)
                .map(|i| div().overflow_hidden().text_ellipsis().whitespace_nowrap().child(mono(i, cx))),
        )
        .when(more > 0, |d| {
            d.child(div().text_xs().text_color(cx.theme().muted_foreground).child(format!("+{more} more")))
        })
}

/// Pages with a pods table: their rows are items of the page's list.
fn lists_pods(kind: &Kind) -> bool {
    kind.is("", "Node")
        || kind.is("networking.k8s.io", "NetworkPolicy")
        || kind.is("", "PersistentVolumeClaim")
        || kind.is("", "Service")
        || kind.is("", "ServiceAccount")
        || kind.is("policy", "PodDisruptionBudget")
        || ((kind.ar.group == "apps" || kind.is("batch", "Job")) && !kind.is("batch", "CronJob"))
}

/// Pages that can hold hundreds of rows (a node's pods, a ConfigMap's keys, a CRD's fields)
/// are drawn as a list, so only the items on screen are laid out: the sections above, one
/// item per row, then the conditions. The number of rows, or None for other pages.
pub fn list_rows(ctx: &Ctx) -> Option<usize> {
    let kind = ctx.kind;
    if lists_pods(kind) {
        Some(ctx.related.pod_rows.len())
    } else if kind.is("", "ConfigMap") || kind.is("", "Secret") {
        Some(ctx.obj.get("data").and_then(Value::as_object).map_or(0, serde_json::Map::len))
    } else if kind.is("apiextensions.k8s.io", "CustomResourceDefinition") {
        Some(ctx.schema_rows.len())
    } else {
        None
    }
}

pub fn item(ctx: &Ctx, ix: usize, cx: &App) -> AnyElement {
    let rows = list_rows(ctx).unwrap_or(0);
    let body = match ix {
        0 => top(ctx, cx).into_any_element(),
        ix if ix <= rows => list_row(ctx, ix - 1, cx),
        _ => conditions(ctx.obj, cx),
    };
    div().px_2().when(ix > rows, Styled::pb_8).child(body).into_any_element()
}

fn list_row(ctx: &Ctx, i: usize, cx: &App) -> AnyElement {
    let kind = ctx.kind;
    if kind.is("", "ConfigMap") {
        config_entry(ctx.obj, i, cx)
    } else if kind.is("", "Secret") {
        secret_entry(ctx, i, cx)
    } else if kind.is("apiextensions.k8s.io", "CustomResourceDefinition") {
        ctx.schema_rows.get(i).map_or_else(|| div().into_any_element(), |r| schema_row(r, cx))
    } else {
        ctx.related.pod_rows.get(i).map_or_else(|| div().into_any_element(), |r| pod_row(ctx, r, cx))
    }
}

/// The whole page in one scroll, for where it is not a list (next to the data editor).
pub fn render(ctx: &Ctx, cx: &App) -> AnyElement {
    let rows = (0..list_rows(ctx).unwrap_or(0)).map(|i| list_row(ctx, i, cx));
    scroll(top(ctx, cx).children(rows).child(conditions(ctx.obj, cx)), "overview")
}

/// Everything above the conditions.
fn top(ctx: &Ctx, cx: &App) -> Div {
    let kind = ctx.kind;
    let mut root = v_flex().gap_1().child(references_card(ctx, cx));
    if kind.is("", "Pod") || kind.ar.group == "apps" || kind.is("batch", "Job") || kind.is("", "Node") {
        root = root.children(history(ctx, cx));
    }
    root = root.children(diagnosis(ctx, cx)).children(alerts::alerts(ctx, cx)).children(argo::section(ctx, cx));
    if kind.is("", "Pod") {
        root = root.child(pod(ctx, cx)).children(pod_netpols(ctx, cx));
    } else if kind.is("batch", "CronJob") {
        root = root.child(workload(ctx, cx)).child(cron_jobs(ctx, cx));
    } else if kind.ar.group == "apps" || kind.is("batch", "Job") {
        root = root.child(workload(ctx, cx));
    } else if kind.is("", "Node") {
        root = root
            .child(node(ctx.obj, cx))
            .child(node_allocation(ctx, cx))
            .children(node_devices(ctx, cx))
            .child(heading_for(&format!("Pods on this node ({})", ctx.related.pods.len()), ctx.related, "pods", cx))
            .child(pods_table(ctx, cx));
    } else if kind.is("autoscaling", "HorizontalPodAutoscaler") {
        root = root.child(hpa(ctx, cx));
    } else if kind.is("networking.k8s.io", "NetworkPolicy") {
        root = root
            .child(netpol(ctx, cx))
            .child(heading_for(&format!("Pods it applies to ({})", ctx.related.pods.len()), ctx.related, "pods", cx))
            .child(pods_table(ctx, cx));
    } else if kind.is("networking.k8s.io", "Ingress") {
        root = root.child(ingress(ctx, cx));
    } else if kind.is("apiextensions.k8s.io", "CustomResourceDefinition") {
        root = root.child(crd(ctx, cx));
    } else if kind.is("gateway.networking.k8s.io", "Gateway") {
        root = root.child(gateway(ctx, cx));
    } else if kind.ar.group == "gateway.networking.k8s.io" && kind.ar.kind.ends_with("Route") {
        root = root.child(route(ctx, cx));
    } else if kind.is("", "PersistentVolumeClaim") {
        root =
            root.child(pvc(ctx, cx)).child(heading_for("Used by", ctx.related, "pods", cx)).child(pods_table(ctx, cx));
    } else if kind.is("", "Namespace") {
        root = root.child(namespace(ctx, cx));
    } else if kind.is("rbac.authorization.k8s.io", "Role") || kind.is("rbac.authorization.k8s.io", "ClusterRole") {
        root = root.child(role_page(ctx, cx));
    } else if kind.is("rbac.authorization.k8s.io", "RoleBinding")
        || kind.is("rbac.authorization.k8s.io", "ClusterRoleBinding")
    {
        root = root.child(binding_page(ctx, cx));
    } else if kind.is("", "ServiceAccount") {
        root = root.child(service_account_page(ctx, cx));
    } else if kind.is("policy", "PodDisruptionBudget") {
        root = root.child(pdb_page(ctx, cx));
    } else if kind.is("autoscaling.k8s.io", "VerticalPodAutoscaler") {
        root = root.child(vpa_page(ctx, cx));
    } else if kind.is("", "Secret") {
        root = root.child(secret(ctx, cx));
    } else if kind.is("", "ConfigMap") {
        root = root.child(config_map(ctx.obj, cx));
    } else if kind.is("", "Service") {
        root = root
            .child(service(ctx, cx))
            .child(heading_for("Pods behind this Service", ctx.related, "pods", cx))
            .child(pods_table(ctx, cx));
    } else {
        // Anything else (CRDs especially): spec and status as readable fields.
        root = root.child(generic_section("Spec", ctx.obj.get("spec"), cx)).child(generic_section(
            "Status",
            ctx.obj.get("status"),
            cx,
        ));
    }
    root
}

fn scroll(content: Div, id: &'static str) -> AnyElement {
    v_flex().id(id).size_full().overflow_y_scroll().px_2().pb_8().child(content).into_any_element()
}

/// Owner, references, labels and annotations, at the top of every overview.
fn references_card(ctx: &Ctx, cx: &App) -> AnyElement {
    let obj = ctx.obj;
    let muted = cx.theme().muted_foreground;
    let mut items: Vec<AnyElement> = Vec::new();
    for owner in arr(obj, "/metadata/ownerReferences") {
        let (k, name) = (owner["kind"].as_str().unwrap_or("?"), owner["name"].as_str().unwrap_or("?"));
        items.push(field("Owner", ctx.link(format!("{k}: {name}"), k, name, cx), cx).into_any_element());
    }
    let spec = if ctx.kind.is("", "Pod") { "/spec" } else { "/spec/template/spec" };
    let mut add = |label: &str, kind: &str, name: Option<&str>| {
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            items.push(field(label, ctx.link(name.to_string(), kind, name, cx), cx).into_any_element());
        }
    };
    add("Service account", "serviceaccounts", s(obj, &format!("{spec}/serviceAccountName")));
    add("Priority class", "priorityclasses", s(obj, &format!("{spec}/priorityClassName")));
    for ps in arr(obj, &format!("{spec}/imagePullSecrets")) {
        add("Pull secret", "secrets", ps["name"].as_str());
    }
    if !ctx.related.referenced_by.is_empty() {
        let mut by = v_flex().gap_1();
        for (kind, names) in &ctx.related.referenced_by {
            let more = names.len().saturating_sub(3);
            by = by.child(
                v_flex()
                    .child(div().text_xs().child(kind.clone()))
                    .children(names.iter().take(3).map(|n| div().pl_2().child(ctx.link(n.clone(), kind, n, cx))))
                    .when(more > 0, |d| {
                        d.child(div().pl_2().text_xs().text_color(muted).child(format!("+{more} more")))
                    }),
            );
        }
        items.push(field("Referenced by", by, cx).into_any_element());
    }
    // Some workload lists could not be read: who uses this object is not fully known.
    if let Some(e) = ctx.related.errors.get("referenced_by") {
        let note = div().text_xs().text_color(cx.theme().danger).child(format!("not fully known: {e}"));
        items.push(field("Referenced by", note, cx).into_any_element());
    }
    let any_ref = !items.is_empty();
    let refs = v_flex().gap_1().min_w(px(220.)).children(items);

    let labels = pairs(obj, "/metadata/labels");
    let annotations: Vec<String> = pairs(obj, "/metadata/annotations")
        .into_iter()
        .filter(|a| !a.starts_with("kubectl.kubernetes.io/last-applied-configuration"))
        .collect();
    if !any_ref && labels.is_empty() && annotations.is_empty() {
        return div().into_any_element();
    }
    card(cx)
        .p_3()
        .child(
            h_flex()
                .items_start()
                .gap_6()
                .when(any_ref, |d| d.child(refs))
                .child(v_flex().flex_1().min_w_0().child(field("Labels", clipped_list(labels, 4, cx), cx)))
                .child(v_flex().flex_1().min_w_0().child(field("Annotations", clipped_list(annotations, 4, cx), cx))),
        )
        .into_any_element()
}

/// "Why is this broken?" for pods and workloads; nothing when healthy.
fn diagnosis(ctx: &Ctx, cx: &App) -> Option<Div> {
    let findings = if ctx.kind.is("", "Pod") {
        let ev: Vec<crate::diagnose::Ev> =
            ctx.related.events.iter().map(|(t, r, m, c)| (t.as_str(), r.as_str(), m.as_str(), *c)).collect();
        crate::diagnose::pod(ctx.obj, &ev, ctx.related.crash_logs.as_ref())
    } else if ctx.kind.ar.group == "apps" || ctx.kind.is("batch", "Job") {
        crate::diagnose::workload(ctx.obj, &ctx.related.pods)
    } else {
        return None;
    };
    if findings.is_empty() {
        return None;
    }
    let investigate = {
        let summary: Vec<String> = findings
            .iter()
            .map(|f| {
                let detail: String = f.detail.chars().take(1500).collect();
                format!("- {}\n{}", f.title, detail).trim_end().to_string()
            })
            .collect();
        let ev = crate::details::Investigate {
            kind: ctx.kind.ar.kind.clone(),
            name: s(ctx.obj, "/metadata/name").unwrap_or_default().to_string(),
            namespace: ctx.ns(),
            findings: summary.join("\n"),
        };
        let panel = ctx.panel.clone();
        Button::new("investigate")
            .small()
            .outline()
            .icon(Lucide::Sparkles)
            .label("Investigate with the assistant")
            .on_click(move |_, _, cx| {
                let _ = panel.update(cx, |_, cx| cx.emit(ev.clone()));
            })
    };
    let theme = cx.theme();
    Some(
        v_flex()
            .mt_2()
            .p_3()
            .gap_2()
            .rounded_md()
            .border_1()
            .border_color(theme.danger.opacity(0.4))
            .bg(theme.danger.opacity(0.05))
            .child(
                h_flex()
                    .gap_2()
                    .child(Icon::new(Lucide::Stethoscope).text_color(theme.danger))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child("Diagnosis"))
                    .child(div().flex_1())
                    .child(investigate),
            )
            .children(findings.into_iter().map(|f| {
                let color = if f.level == crate::diagnose::Level::Critical { theme.danger } else { theme.warning };
                v_flex()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().size_2().rounded_full().bg(color))
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(f.title)),
                    )
                    .when(!f.detail.is_empty(), |d| {
                        d.child(v_flex().pl_4().children(f.detail.lines().map(|l| {
                            if l.starts_with("Last output")
                                || f.detail.find("Last output").is_none_or(|i| f.detail[..i].contains(l))
                            {
                                div().text_sm().child(l.to_string())
                            } else {
                                mono(l.to_string(), cx)
                            }
                        })))
                    })
                    .child(div().pl_4().text_xs().text_color(theme.muted_foreground).child(f.hint))
            })),
    )
}

fn conditions(obj: &Value, cx: &App) -> AnyElement {
    let conds = arr(obj, "/status/conditions");
    if conds.is_empty() {
        return div().into_any_element();
    }
    let muted = cx.theme().muted_foreground;
    v_flex()
        .child(heading("Conditions"))
        .children(conds.iter().map(|c| {
            let status = c["status"].as_str().unwrap_or_default().to_string();
            let ty = c["type"].as_str().unwrap_or_default();
            use crate::status::Tone;
            let color = match crate::status::condition_tone(ty, &status) {
                Some(Tone::Good) => cx.theme().success,
                // A problem the object reports about itself.
                Some(Tone::Bad) if status == "True" => cx.theme().warning,
                Some(Tone::Bad) => cx.theme().danger,
                Some(Tone::Busy) => cx.theme().warning,
                Some(Tone::Done) | None => muted,
            };
            h_flex()
                .gap_3()
                .py_1()
                .text_sm()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(div().w(px(220.)).flex_shrink_0().child(c["type"].as_str().unwrap_or_default().to_string()))
                .child(div().w(px(60.)).flex_shrink_0().text_color(color).child(status))
                .child(div().flex_1().min_w_0().text_color(muted).child(
                    [c["reason"].as_str(), c["message"].as_str()].into_iter().flatten().collect::<Vec<_>>().join(": "),
                ))
                .child(div().text_color(muted).child(age_of(c, "/lastTransitionTime").unwrap_or_default()))
        }))
        .into_any_element()
}

/// Flatten a JSON object into `a.b.c: value` rows (lists summarized), for any kind.
fn flatten(prefix: &str, v: &Value, depth: usize, out: &mut Vec<(String, String)>) {
    match v {
        Value::Object(map) if depth < 4 => {
            for (k, val) in map {
                if k == "conditions" {
                    continue; // shown in its own section
                }
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten(&key, val, depth + 1, out);
            }
        }
        Value::Array(items) => {
            let scalars: Vec<String> = items
                .iter()
                .filter_map(|i| match i {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    Value::Bool(b) => Some(b.to_string()),
                    _ => None,
                })
                .collect();
            if scalars.len() == items.len() {
                out.push((prefix.to_string(), scalars.join(", ")));
            } else {
                out.push((prefix.to_string(), format!("{} items (see YAML)", items.len())));
            }
        }
        Value::Object(map) => out.push((prefix.to_string(), format!("{} fields (see YAML)", map.len()))),
        Value::String(s) => out.push((prefix.to_string(), s.clone())),
        Value::Null => {}
        other => out.push((prefix.to_string(), other.to_string())),
    }
}

fn generic_section(title: &str, v: Option<&Value>, cx: &App) -> AnyElement {
    let Some(v) = v.filter(|v| !v.is_null()) else {
        return div().into_any_element();
    };
    let mut rows = Vec::new();
    flatten("", v, 0, &mut rows);
    if rows.is_empty() {
        return div().into_any_element();
    }
    let muted = cx.theme().muted_foreground;
    let more = rows.len().saturating_sub(80);
    v_flex()
        .child(heading(title))
        .children(rows.into_iter().take(80).map(|(k, v)| {
            h_flex()
                .gap_3()
                .py_0p5()
                .text_sm()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(
                    div()
                        .w(px(320.))
                        .flex_shrink_0()
                        .text_color(muted)
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(k),
                )
                .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(v))
        }))
        .when(more > 0, |d| {
            d.child(div().pt_1().text_xs().text_color(muted).child(format!("… {more} more fields in YAML")))
        })
        .into_any_element()
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn failure_conditions_are_bad_when_true() {
        for ty in ["ReplicaFailure", "FailureTarget", "DisruptionTarget", "MemoryPressure", "Failed"] {
            assert!(crate::status::true_is_bad(ty), "{ty}");
        }
        for ty in ["Ready", "Available", "Progressing", "Complete"] {
            assert!(!crate::status::true_is_bad(ty), "{ty}");
        }
    }

    #[test]
    fn health_conditions_are_bad_when_true() {
        for ty in [
            "KernelDeadlock",
            "ReadonlyFilesystem",
            "NamespaceContentRemaining",
            "NonStructuralSchema",
            "NoPodsMatched",
        ] {
            assert!(crate::status::true_is_bad(ty), "{ty}");
            assert!(!crate::status::true_is_good(ty), "{ty}");
        }
        for ty in ["Ready", "Established", "RecommendationProvided"] {
            assert!(crate::status::true_is_good(ty), "{ty}");
        }
    }
}
