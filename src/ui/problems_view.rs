//! Problems page: what is broken across the connected clusters, worst first.

use std::collections::HashMap;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::details::OpenObject;
use crate::k8s::{self, Kind};
use crate::problems::{self, Problem, Severity};
use crate::ui::page_filter::PageFilter;
use crate::ui::page_header::{page_header, scope_text};

pub struct ProblemsView {
    /// Each cluster with the namespaces scanned there.
    clusters: crate::ui::Scope,
    /// The kinds each cluster serves, by context.
    kinds: HashMap<String, Vec<Kind>>,
    /// The namespaces chosen, for the header.
    namespaces: Vec<String>,
    /// (context, problem) once loaded.
    found: Option<Vec<(String, Problem)>>,
    errors: Vec<String>,
    /// Hide Info rows (restarts, cordoned nodes).
    show_info: bool,
    /// One severity only, picked with its count.
    only: Option<Severity>,
    filter: Entity<InputState>,
    /// The filter as last typed, parsed.
    parsed: PageFilter,
    _task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenObject> for ProblemsView {}

impl ProblemsView {
    pub fn new(
        clusters: crate::ui::Scope,
        kinds: HashMap<String, Vec<Kind>>,
        namespaces: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter: text, ns:shop, kind:Pod, ctx:prod"));
        let subs = vec![cx.subscribe(&filter, |this: &mut Self, state, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                this.parsed = PageFilter::parse(&state.read(cx).value());
                cx.notify();
            }
        })];
        let mut this = Self {
            clusters,
            kinds,
            namespaces,
            found: None,
            errors: Vec::new(),
            show_info: false,
            only: None,
            filter,
            parsed: PageFilter::default(),
            _task: None,
            _subs: subs,
        };
        this.reload(cx);
        this
    }

    /// Scan other clusters or namespaces (`namespaces`: the ones chosen, for the header).
    pub fn set_scope(
        &mut self,
        clusters: crate::ui::Scope,
        kinds: HashMap<String, Vec<Kind>>,
        namespaces: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        (self.clusters, self.kinds, self.namespaces, self.found) = (clusters, kinds, namespaces, None);
        self.errors.clear();
        self.reload(cx);
        cx.notify();
    }

    /// Scan again now (F5), keeping the filter and the severity picked.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.found = None;
        self.reload(cx);
        cx.notify();
    }

    fn reload(&mut self, cx: &mut Context<Self>) {
        let (clusters, kinds) = (self.clusters.clone(), self.kinds.clone());
        self._task = Some(cx.spawn(async move |this, cx| {
            loop {
                let exec = cx.background_executor().clone();
                if !crate::app::wait_visible(&this, cx, &exec).await {
                    break;
                }
                // A cluster where none of the namespaces chosen is accessible is left out.
                let scans = clusters.iter().filter(|(_, _, ns)| !ns.is_empty()).map(|(ctx, c, ns)| {
                    // `[None]`: the whole cluster; otherwise each namespace, Nodes once.
                    let ns: Vec<String> = ns.iter().flatten().cloned().collect();
                    let (ctx, c) = (ctx.clone(), c.clone());
                    // Only the kinds this cluster serves: the others would fail with a 404.
                    let kinds = kinds.get(&ctx).cloned().unwrap_or_default();
                    async move {
                        let scan = crate::alerts::scan(&ctx, c, &kinds, &ns).await;
                        (ctx, scan)
                    }
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

    /// Indices in `found` of the problems shown: minor ones only when asked, the severity
    /// picked, and what the filter keeps.
    fn shown(&self) -> Vec<usize> {
        let found = self.found.as_deref().unwrap_or_default();
        (0..found.len()).filter(|&i| keeps(&found[i].0, &found[i].1, &self.parsed, self.only, self.show_info)).collect()
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
        cells(&theme, multi.then_some(ctx.as_str()), p, color)
            .id(("problem", i))
            // An alert about no object in particular has nothing to open.
            .when(!p.kind.is_empty(), |d| {
                d.cursor_pointer()
                    .hover(|s| s.bg(theme.list_hover))
                    .on_click(cx.listener(move |_, _, _, cx| cx.emit(ev.clone())))
            })
            .into_any_element()
    }
}

/// A problem's row: its severity dot, the cluster (with several), kind, namespace, name, message
/// and age. As the page narrows (the assistant dock open, a small window) every text column
/// gives up room: the kind, namespace and cluster first, down to a floor, then the name and the
/// message. The name, the link to click, never goes under `NAME_MIN` (#749). On a wide page it
/// grows up to `NAME_MAX` and the message takes the rest (#660).
fn cells(theme: &component::Theme, context: Option<&str>, p: &Problem, color: Hsla) -> Div {
    // A column with a usual width that gives up room (twice as readily as the name) down to `min`.
    let side = |basis: f32, min: f32| {
        div().flex_basis(px(basis)).flex_shrink(2.).min_w(px(min)).overflow_hidden().text_ellipsis().whitespace_nowrap()
    };
    h_flex()
        .w_full()
        .gap_3()
        .px_2()
        .py_1p5()
        .border_b_1()
        .border_color(theme.border)
        .child(div().size(px(8.)).flex_shrink_0().rounded_full().bg(color))
        .when_some(context, |d, ctx| {
            d.child(side(150., 70.).text_sm().text_color(theme.muted_foreground).child(ctx.to_string()))
        })
        .child(side(160., 80.).text_xs().text_color(theme.muted_foreground).child(p.label.clone()))
        .child(
            side(180., 80.).text_sm().text_color(theme.muted_foreground).child(p.namespace.clone().unwrap_or_default()),
        )
        .child(
            div()
                .debug_selector(|| "problem-name".into())
                .flex_grow(1.)
                .flex_basis(px(NAME_BASIS))
                .min_w(px(NAME_MIN))
                .max_w(px(NAME_MAX))
                .text_sm()
                .text_color(theme.blue)
                .overflow_hidden()
                .text_ellipsis()
                .whitespace_nowrap()
                .child(p.name.clone()),
        )
        .child(
            div()
                .debug_selector(|| "problem-message".into())
                .flex_grow(1.)
                .flex_basis(px(300.))
                .min_w(px(120.))
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
                .child(p.age.map_or_default(k8s::age)),
        )
}

/// The name column's usual width, its floor and its cap.
const NAME_BASIS: f32 = 220.;
const NAME_MIN: f32 = 140.;
const NAME_MAX: f32 = 320.;

/// Whether a problem is shown: of the severity picked (none: any, minor ones only with
/// `show_info`), and matching the filter. `kind:` matches the kind as listed (`Pod alert`,
/// `Service`) or as kubectl names it (`pods`).
fn keeps(context: &str, p: &Problem, f: &PageFilter, only: Option<Severity>, show_info: bool) -> bool {
    let severity_ok = match only {
        Some(s) => p.severity == s,
        None => show_info || p.severity != Severity::Info,
    };
    if !severity_ok {
        return false;
    }
    let ns = p.namespace.as_deref();
    let text = format!("{context} {} {} {}/{} {}", p.label, p.kind, ns.unwrap_or_default(), p.name, p.reason);
    let kinds = format!("{} {}", p.label, p.kind);
    f.matches(context, ns, &kinds, &text)
}

/// What the page says when nothing is found, and whether it is an all-clear: only when
/// every cluster and kind could be read.
fn nothing_found(errors: &[String]) -> (bool, &'static str) {
    if errors.is_empty() {
        (true, "Nothing broken right now.")
    } else {
        (false, "Nothing broken in what could be read.")
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
        let header =
            page_header(Lucide::TriangleAlert, Some(theme.warning), "Problems", scope_text(&self.namespaces), cx)
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
                .child(
                    Button::new("reload-problems")
                        .small()
                        .ghost()
                        .icon(Lucide::RefreshCw)
                        .label("Refresh")
                        .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                );
        let Some(found) = &self.found else {
            return v_flex().size_full().p_4().gap_4().child(header).child(
                div().text_color(theme.muted_foreground).child(
                    "Checking nodes, workloads, pods, jobs, services, ingresses, volumes, references and deletions…",
                ),
            );
        };
        let count = |s: Severity| found.iter().filter(|(_, p)| p.severity == s).count();
        let (crit, warn, info) = (count(Severity::Critical), count(Severity::Warning), count(Severity::Info));
        // A count picks its severity alone; picked again, every severity.
        let only = self.only;
        let pill = |n: usize, label: &str, color: Hsla, severity: Severity, cx: &mut Context<Self>| {
            let picked = only == Some(severity);
            h_flex()
                .id(SharedString::from(format!("only-{label}")))
                .gap_1p5()
                .px_3()
                .py_1()
                .rounded_md()
                .bg(color.opacity(if picked { 0.25 } else { 0.1 }))
                .border_1()
                .border_color(if picked { color } else { color.opacity(0.) })
                .cursor_pointer()
                .hover(|s| s.bg(color.opacity(0.2)))
                .child(div().font_weight(FontWeight::BOLD).text_color(color).child(n.to_string()))
                .child(div().text_sm().child(label.to_string()))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.only = if this.only == Some(severity) { None } else { Some(severity) };
                    cx.notify();
                }))
        };
        let shown = self.shown().len();
        // How many of what the counts and "Show minor" let through the filter keeps.
        let filtered = !self.parsed.is_empty() || self.only.is_some();
        let listed = crit + warn + if self.show_info || self.only == Some(Severity::Info) { info } else { 0 };
        v_flex()
            .size_full()
            .p_4()
            .gap_4()
            .child(header)
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(pill(crit, "critical", theme.danger, Severity::Critical, cx))
                    .child(pill(warn, "warnings", theme.warning, Severity::Warning, cx))
                    .child(pill(info, "minor", theme.muted_foreground, Severity::Info, cx))
                    .child(div().w(px(340.)).ml_2().child(Input::new(&self.filter).small().cleanable(true)))
                    .when(filtered, |d| {
                        d.child(div().text_sm().text_color(theme.muted_foreground).child(format!("{shown} shown")))
                    }),
            )
            .children(self.errors.iter().map(|e| div().text_sm().text_color(theme.danger).child(e.clone())))
            .when(filtered && listed > 0 && shown == 0, |d| {
                d.child(div().text_sm().text_color(theme.muted_foreground).child("Nothing matches the filter."))
            })
            .when(crit + warn + if self.show_info { info } else { 0 } == 0 && !filtered, |d| {
                let (all_clear, text) = nothing_found(&self.errors);
                if all_clear {
                    d.child(
                        h_flex().gap_2().text_color(theme.success).child(Icon::new(Lucide::CircleCheck)).child(text),
                    )
                } else {
                    d.child(div().text_sm().text_color(theme.muted_foreground).child(text))
                }
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
    use crate::ui::page_filter::PageFilter;

    #[test]
    fn the_filter_and_the_severity_picked() {
        let p = |severity, label: &str, kind: &str, ns: &str, name: &str, reason: &str| Problem {
            severity,
            kind: kind.into(),
            label: label.into(),
            namespace: Some(ns.into()),
            name: name.into(),
            reason: reason.into(),
            age: None,
        };
        let alert = p(Severity::Warning, "Pod alert", "pods", "monitoring", "prom-0", "NodeMemoryMajorPagesFaults");
        let svc = p(Severity::Warning, "Service", "services", "monitoring", "nginx", "selects no pod");
        let restarts = p(Severity::Info, "Pod", "pods", "shop", "web", "restarted 3 times");
        let keeps = |pr: &Problem, f: &str, only, info| super::keeps("prod", pr, &PageFilter::parse(f), only, info);
        assert!(keeps(&alert, "", None, false));
        assert!(keeps(&alert, "kind:alert", None, false) && !keeps(&svc, "kind:alert", None, false));
        assert!(keeps(&alert, "kind:pods", None, false), "kubectl's name too");
        assert!(keeps(&svc, "selects ns:monitor ctx:prod", None, false));
        assert!(!keeps(&svc, "ctx:staging", None, false));
        // Minor ones: only when shown, or picked with their count.
        assert!(!keeps(&restarts, "", None, false));
        assert!(keeps(&restarts, "", None, true));
        assert!(keeps(&restarts, "", Some(Severity::Info), false));
        assert!(!keeps(&alert, "", Some(Severity::Critical), true));
        assert!(keeps(&alert, "", Some(Severity::Warning), false));
    }

    #[test]
    fn no_all_clear_when_a_scan_failed() {
        assert_eq!(super::nothing_found(&[]), (true, "Nothing broken right now."));
        let errors = ["prod: connection refused".to_string()];
        assert_eq!(super::nothing_found(&errors), (false, "Nothing broken in what could be read."));
    }

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

#[cfg(all(test, feature = "dev"))]
mod layout_tests {
    use gpui_kit::component::ActiveTheme as _;
    use gpui_kit::{Context, IntoElement, ParentElement as _, Render, Styled as _, TestAppContext, Window, div, px};

    use super::{NAME_MAX, NAME_MIN};
    use crate::problems::{Problem, Severity};

    /// One row of the page, `width` wide, with one cluster or several.
    struct Row {
        width: f32,
        several: bool,
    }

    impl Render for Row {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let theme = cx.theme().clone();
            let p = Problem {
                severity: Severity::Warning,
                kind: "deployments.apps".into(),
                label: "Deployment".into(),
                namespace: Some("payments".into()),
                name: "fraud-check-b668cf779-k6km9".into(),
                reason: "0 of 1 replicas available".into(),
                age: None,
            };
            let ctx = self.several.then_some("kind-staging");
            div().w(px(self.width)).child(super::cells(&theme, ctx, &p, theme.warning))
        }
    }

    /// The width of the name and of the message in a row `width` wide.
    fn widths(cx: &mut TestAppContext, width: f32, several: bool) -> (f32, f32) {
        let (_, cx) = cx.add_window_view(|_, _| Row { width, several });
        cx.update(|window, cx| drop(window.draw(cx)));
        let name = cx.debug_bounds("problem-name").expect("the name is drawn").size.width;
        let message = cx.debug_bounds("problem-message").expect("the message is drawn").size.width;
        (f32::from(name), f32::from(message))
    }

    #[gpui_kit::test]
    fn the_name_stays_readable_as_the_page_narrows(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        // From a page beside the assistant dock to a wide window, one cluster or several.
        for width in [560., 700., 760., 900., 1100., 1400., 1800.] {
            for several in [false, true] {
                let (name, message) = widths(cx, width, several);
                let at = format!("{width}px wide, several clusters: {several}");
                assert!(name >= NAME_MIN - 0.5, "{at}: the name is {name}px wide");
                assert!(name <= NAME_MAX + 0.5, "{at}: the name is {name}px wide");
                assert!(message >= 119.5, "{at}: the message is {message}px wide");
            }
        }
        // A wide page: the name at its cap, the message takes the rest (#660).
        let (name, message) = widths(cx, 1800., false);
        assert!((name - NAME_MAX).abs() < 0.5, "the name is {name}px wide");
        assert!(message > 900., "the message is {message}px wide");
    }
}
