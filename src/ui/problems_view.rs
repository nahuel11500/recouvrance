//! Problems page: what is broken across the connected clusters, worst first.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use crate::details::OpenObject;
use crate::k8s::{self, Kind};
use crate::problems::{self, Problem, Severity};

pub struct ProblemsView {
    clusters: Vec<(String, Client)>,
    kinds: Vec<Kind>,
    namespace: Option<String>,
    /// (context, problem) once loaded.
    found: Option<Vec<(String, Problem)>>,
    errors: Vec<String>,
    /// Hide Info rows (restarts, cordoned nodes).
    show_info: bool,
    _task: Option<Task<()>>,
}

impl EventEmitter<OpenObject> for ProblemsView {}

impl ProblemsView {
    pub fn new(
        clusters: Vec<(String, Client)>,
        kinds: Vec<Kind>,
        namespace: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this =
            Self { clusters, kinds, namespace, found: None, errors: Vec::new(), show_info: false, _task: None };
        this.reload(cx);
        this
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let (clusters, kinds, ns) = (self.clusters.clone(), self.kinds.clone(), self.namespace.clone());
        self._task = Some(cx.spawn(async move |this, cx| {
            loop {
                let exec = cx.background_executor().clone();
                if !crate::app::wait_visible(&this, cx, &exec).await {
                    break;
                }
                let scans = clusters.iter().map(|(ctx, c)| {
                    let (ctx, c, kinds, ns) = (ctx.clone(), c.clone(), kinds.clone(), ns.clone());
                    async move { (ctx, problems::scan_partial(c, &kinds, ns).await) }
                });
                let (found, errors) = merge(futures::future::join_all(scans).await);
                if this
                    .update(cx, |this, cx| {
                        this.found = Some(found);
                        this.errors = errors;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(crate::settings::secs(crate::settings::get().overview_refresh)).await;
            }
        }));
    }

    /// Indices in `found` of the problems shown (minor ones only when asked).
    fn shown(&self) -> Vec<usize> {
        let found = self.found.as_deref().unwrap_or_default();
        (0..found.len()).filter(|&i| self.show_info || found[i].1.severity != Severity::Info).collect()
    }

    fn row(&self, i: usize, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let multi = self.clusters.len() > 1;
        let (ctx, p) = &self.found.as_deref().unwrap_or_default()[ix];
        let color = match p.severity {
            Severity::Critical => theme.danger,
            Severity::Warning => theme.warning,
            Severity::Info => theme.muted_foreground,
        };
        let ev = OpenObject {
            kind: p.kind.clone(),
            name: p.name.clone(),
            namespace: p.namespace.clone(),
            context: Some(ctx.clone()),
        };
        h_flex()
            .id(("problem", i))
            .w_full()
            .gap_3()
            .px_2()
            .py_1p5()
            .border_b_1()
            .border_color(theme.border)
            .cursor_pointer()
            .hover(|s| s.bg(theme.list_hover))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(ev.clone())))
            .child(div().size(px(8.)).flex_shrink_0().rounded_full().bg(color))
            .when(multi, |d| {
                d.child(
                    div()
                        .w(px(150.))
                        .flex_shrink_0()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .overflow_hidden()
                        .text_ellipsis()
                        .whitespace_nowrap()
                        .child(ctx.clone()),
                )
            })
            .child(
                div().w(px(110.)).flex_shrink_0().text_xs().text_color(theme.muted_foreground).child(p.label.clone()),
            )
            .child(
                div()
                    .w(px(180.))
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(p.namespace.clone().unwrap_or_default()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(theme.blue)
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(p.name.clone()),
            )
            .child(
                div()
                    .w(px(300.))
                    .flex_shrink_0()
                    .text_sm()
                    .text_color(color)
                    .overflow_hidden()
                    .text_ellipsis()
                    .whitespace_nowrap()
                    .child(p.reason.clone()),
            )
            .child(
                div()
                    .w(px(50.))
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(p.age.map(k8s::age).unwrap_or_default()),
            )
            .into_any_element()
    }
}

/// Every cluster's problems, worst first, and a line for each cluster with kinds it could
/// not read: a kind refused is noted, and the others are still shown.
fn merge(results: Vec<(String, problems::Scan)>) -> (Vec<(String, Problem)>, Vec<String>) {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    for (ctx, scan) in results {
        if let Some(skipped) = scan.skipped_text() {
            errors.push(format!("{ctx}: could not read {skipped}"));
        }
        found.extend(scan.problems.into_iter().map(|p| (ctx.clone(), p)));
    }
    found.sort_by_key(|a| a.1.severity);
    (found, errors)
}

impl Render for ProblemsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let header = h_flex()
            .gap_3()
            .child(Icon::new(Lucide::TriangleAlert).size_5().text_color(theme.warning))
            .child(div().text_xl().font_weight(FontWeight::MEDIUM).child("Problems"))
            .child(div().text_sm().text_color(theme.muted_foreground).child(match &self.namespace {
                Some(ns) => format!("in {ns}"),
                None => "all namespaces".into(),
            }))
            .child(div().flex_1())
            .child(
                Button::new("show-info")
                    .small()
                    .ghost()
                    .label(if self.show_info { "Hide minor" } else { "Show minor" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_info = !this.show_info;
                        cx.notify();
                    })),
            )
            .child(Button::new("reload-problems").small().ghost().icon(Lucide::RefreshCw).label("Refresh").on_click(
                cx.listener(|this, _, _, cx| {
                    this.found = None;
                    this.reload(cx);
                    cx.notify();
                }),
            ));
        let Some(found) = &self.found else {
            return v_flex().size_full().p_4().gap_4().child(header).child(
                div().text_color(theme.muted_foreground).child("Checking nodes, workloads, pods, jobs and volumes…"),
            );
        };
        let count = |s: Severity| found.iter().filter(|(_, p)| p.severity == s).count();
        let (crit, warn, info) = (count(Severity::Critical), count(Severity::Warning), count(Severity::Info));
        let pill = |n: usize, label: &str, color: Hsla| {
            h_flex()
                .gap_1p5()
                .px_3()
                .py_1()
                .rounded_md()
                .bg(color.opacity(0.1))
                .child(div().font_weight(FontWeight::BOLD).text_color(color).child(n.to_string()))
                .child(div().text_sm().child(label.to_string()))
        };
        let shown = self.shown().len();
        v_flex()
            .size_full()
            .p_4()
            .gap_4()
            .child(header)
            .child(
                h_flex()
                    .gap_2()
                    .child(pill(crit, "critical", theme.danger))
                    .child(pill(warn, "warnings", theme.warning))
                    .child(pill(info, "minor", theme.muted_foreground)),
            )
            .children(self.errors.iter().map(|e| div().text_sm().text_color(theme.danger).child(e.clone())))
            .when(crit + warn + if self.show_info { info } else { 0 } == 0, |d| {
                d.child(
                    h_flex()
                        .gap_2()
                        .text_color(theme.success)
                        .child(Icon::new(Lucide::CircleCheck))
                        .child("Nothing broken right now."),
                )
            })
            .child(
                // Only the rows on screen are laid out: hundreds of problems made the page stutter.
                uniform_list(
                    "problems-list",
                    shown,
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        let shown = this.shown();
                        range.filter_map(|i| shown.get(i).map(|&ix| this.row(i, ix, cx))).collect()
                    }),
                )
                .flex_1()
                .min_h_0(),
            )
    }
}

#[cfg(test)]
mod tests {
    use crate::problems::{Problem, Scan, Severity};

    #[test]
    fn a_kind_refused_keeps_the_others() {
        let problem = |severity, name: &str| Problem {
            severity,
            kind: "pods".into(),
            label: "Pod".into(),
            namespace: Some("a".into()),
            name: name.into(),
            reason: "CrashLoopBackOff".into(),
            age: None,
        };
        let forbidden: anyhow::Error =
            kube::Error::Api(kube::core::Status::failure("forbidden", "Forbidden").with_code(403).boxed()).into();
        let (found, errors) = super::merge(vec![
            (
                "one".into(),
                Scan { problems: vec![problem(Severity::Warning, "w")], skipped: vec![("Nodes".into(), forbidden)] },
            ),
            ("two".into(), Scan { problems: vec![problem(Severity::Critical, "c")], skipped: Vec::new() }),
        ]);
        let names: Vec<(&str, &str)> = found.iter().map(|(ctx, p)| (ctx.as_str(), p.name.as_str())).collect();
        assert_eq!(names, [("two", "c"), ("one", "w")]);
        assert_eq!(errors, ["one: could not read Nodes (forbidden)"]);
    }
}
