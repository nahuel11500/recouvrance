//! The cluster overview's Upgrade check tab: for each cluster, what needs action before it
//! moves to a newer minor version (`crate::upgrade`), most urgent first.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use crate::details::OpenObject;
use crate::k8s;
use crate::latest::Latest;
use crate::problems::Severity;
use crate::upgrade::{Finding, Report};

/// Objects listed under a finding; the rest are counted.
const SHOWN_OBJECTS: usize = 20;

/// One cluster's check.
struct Check {
    context: String,
    client: Client,
    /// The cluster's minor version, once known.
    current: Option<u32>,
    /// The minor version checked against: the cluster's next one until another is picked.
    target: Option<u32>,
    /// None while it runs.
    result: Option<Result<Report, String>>,
    load: Latest,
}

pub struct UpgradeView {
    checks: Vec<Check>,
}

impl EventEmitter<OpenObject> for UpgradeView {}

impl UpgradeView {
    pub fn new(clusters: Vec<(String, Client)>, cx: &mut Context<Self>) -> Self {
        let checks = clusters
            .into_iter()
            .map(|(context, client)| Check {
                context,
                client,
                current: None,
                target: None,
                result: None,
                load: Latest::default(),
            })
            .collect();
        let mut this = Self { checks };
        for i in 0..this.checks.len() {
            this.reload(i, cx);
        }
        this
    }

    /// Check cluster `i` again, against its target: a check still running for another
    /// target is dropped.
    fn reload(&mut self, i: usize, cx: &mut Context<Self>) {
        let Some(check) = self.checks.get_mut(i) else { return };
        check.result = None;
        let (client, target) = (check.client.clone(), check.target);
        // Off the UI thread: it goes through every pod, budget and Helm manifest.
        let work = k8s::run(async move {
            let kinds = k8s::discover(client.clone()).await;
            crate::upgrade::check(client, kinds, target).await
        });
        check.load.spawn(cx, work, move |this, result, cx| {
            let Some(check) = this.checks.get_mut(i) else { return };
            if let Ok(report) = &result {
                (check.current, check.target) = (Some(report.current), Some(report.target));
            }
            check.result = Some(result.map_err(|e| k8s::short_error(&e)));
            cx.notify();
        });
        cx.notify();
    }

    /// Check cluster `i` against the minor version `step` away from its target (never one
    /// it already runs).
    fn step_target(&mut self, i: usize, step: i64, cx: &mut Context<Self>) {
        let Some(check) = self.checks.get_mut(i) else { return };
        let (Some(current), Some(target)) = (check.current, check.target) else { return };
        let next = (i64::from(target) + step).max(i64::from(current) + 1);
        let Ok(next) = u32::try_from(next) else { return };
        if next != target {
            check.target = Some(next);
            self.reload(i, cx);
        }
    }

    fn finding(&self, ci: usize, fi: usize, f: &Finding, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let color = match f.severity {
            Severity::Critical => theme.danger,
            Severity::Warning => theme.warning,
            Severity::Info => theme.muted_foreground,
        };
        let context = self.checks[ci].context.clone();
        let more = f.objects.len().saturating_sub(SHOWN_OBJECTS);
        v_flex()
            .p_3()
            .gap_1()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(div().size(px(8.)).flex_shrink_0().rounded_full().bg(color))
                    .child(div().flex_1().min_w_0().text_sm().font_weight(FontWeight::MEDIUM).child(f.title.clone()))
                    .child(div().text_xs().text_color(theme.muted_foreground).child(f.area.label())),
            )
            .child(div().pl_4().text_sm().child(f.action.clone()))
            .children(
                f.details.iter().map(|d| div().pl_4().text_xs().text_color(theme.muted_foreground).child(d.clone())),
            )
            .children(f.objects.iter().take(SHOWN_OBJECTS).enumerate().map(|(oi, o)| {
                let ev = OpenObject {
                    kind: o.kind.clone(),
                    name: o.name.clone(),
                    namespace: o.namespace.clone(),
                    context: Some(context.clone()),
                };
                // `horizontalpodautoscalers.autoscaling` → `horizontalpodautoscalers`.
                let kind = o.kind.split('.').next().unwrap_or_default().to_string();
                h_flex()
                    .pl_4()
                    .gap_2()
                    .text_sm()
                    .child(div().w(px(200.)).flex_shrink_0().text_xs().text_color(theme.muted_foreground).child(kind))
                    .child(
                        div()
                            .id(SharedString::from(format!("upgrade-object-{ci}-{fi}-{oi}")))
                            .flex_shrink_0()
                            .text_color(theme.blue)
                            .cursor_pointer()
                            .hover(Styled::underline)
                            .on_click(cx.listener(move |_, _, _, cx| cx.emit(ev.clone())))
                            .child(o.path()),
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
                            .child(o.note.clone()),
                    )
            }))
            .when(more > 0, |d| {
                d.child(div().pl_4().text_xs().text_color(theme.muted_foreground).child(format!("+{more} more")))
            })
    }

    fn cluster(&self, ci: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let check = &self.checks[ci];
        let target = check.target.map_or_else(|| "next".to_string(), |t| format!("1.{t}"));
        let current = check.current.map_or_else(String::new, |c| format!("Kubernetes 1.{c} →"));
        let at_lowest = check.current.zip(check.target).is_none_or(|(c, t)| t <= c + 1);
        let idle = check.result.is_some();
        let header = h_flex()
            .gap_2()
            .items_center()
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(check.context.clone()))
            .child(div().text_sm().text_color(theme.muted_foreground).child(current))
            .child(
                Button::new(("upgrade-older", ci))
                    .xsmall()
                    .ghost()
                    .icon(Lucide::ChevronLeft)
                    .disabled(at_lowest || !idle)
                    .on_click(cx.listener(move |this, _, _, cx| this.step_target(ci, -1, cx))),
            )
            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(target))
            .child(
                Button::new(("upgrade-newer", ci))
                    .xsmall()
                    .ghost()
                    .icon(Lucide::ChevronRight)
                    .disabled(check.target.is_none() || !idle)
                    .on_click(cx.listener(move |this, _, _, cx| this.step_target(ci, 1, cx))),
            )
            .child(div().flex_1())
            .child(
                Button::new(("upgrade-refresh", ci))
                    .small()
                    .ghost()
                    .icon(Lucide::RefreshCw)
                    .label("Check again")
                    .disabled(!idle)
                    .on_click(cx.listener(move |this, _, _, cx| this.reload(ci, cx))),
            );
        let body = match &check.result {
            None => div()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("Checking API versions in use, Helm releases, node versions, disruption budgets and add-ons…")
                .into_any_element(),
            Some(Err(e)) => div().text_sm().text_color(theme.danger).child(e.clone()).into_any_element(),
            Some(Ok(report)) => {
                let count = |s: Severity| report.findings.iter().filter(|f| f.severity == s).count();
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
                v_flex()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .child(pill(count(Severity::Critical), "blocking", theme.danger))
                            .child(pill(count(Severity::Warning), "warnings", theme.warning))
                            .child(pill(count(Severity::Info), "to check", theme.muted_foreground)),
                    )
                    .children(report.skipped.iter().map(|s| div().text_sm().text_color(theme.danger).child(s.clone())))
                    .when(report.findings.is_empty(), |d| {
                        if report.skipped.is_empty() {
                            d.child(
                                h_flex()
                                    .gap_2()
                                    .text_color(theme.success)
                                    .child(Icon::new(Lucide::CircleCheck))
                                    .child(format!("Nothing to do found for 1.{}.", report.target)),
                            )
                        } else {
                            d.child(
                                div()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child("Nothing to do found in what could be read."),
                            )
                        }
                    })
                    .children(report.findings.iter().enumerate().map(|(fi, f)| self.finding(ci, fi, f, cx)))
                    .into_any_element()
            }
        };
        v_flex().gap_3().child(header).child(body).into_any_element()
    }
}

impl Render for UpgradeView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let clusters: Vec<AnyElement> = (0..self.checks.len()).map(|ci| self.cluster(ci, cx)).collect();
        v_flex()
            .id("upgrade-check")
            .size_full()
            .overflow_y_scroll()
            .p_4()
            .gap_8()
            .child(
                h_flex()
                    .gap_3()
                    .child(Icon::new(Lucide::CircleArrowUp).size_5().text_color(theme.blue))
                    .child(div().text_xl().font_weight(FontWeight::MEDIUM).child("Upgrade check"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child("What needs action before a cluster moves to a newer minor version"),
                    ),
            )
            .children(clusters)
    }
}
