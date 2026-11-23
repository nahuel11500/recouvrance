//! Problems page: what is broken across the connected clusters, worst first. It shows the
//! scans the sidebar badge, every window and the agents share (`problems::shared`): it opens
//! on the latest findings at once, and each check's new ones show as soon as it finishes.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use crate::details::OpenObject;
use crate::k8s::lane::Lane;
use crate::k8s::{self, Kind};
use crate::problems::shared::{self, Ask, Found, Fresh};
use crate::problems::{Problem, Severity};
use crate::ui::page_filter::PageFilter;
use crate::ui::page_header::{page_header, scope_text};

/// How long the page waits at most for new findings before looking again.
const LOOK_AGAIN: Duration = Duration::from_secs(30);

/// The least time between two looks at the findings.
const POLL: Duration = Duration::from_millis(500);

/// What the page says while nothing was scanned yet.
const CHECKING: &str = "Checking nodes, workloads, pods, jobs, services, ingresses, volumes, references and deletions…";

/// A cluster the page shows: the scope scanned, and the namespaces shown from it.
struct Watched {
    context: String,
    client: Client,
    /// The kinds it serves: the others would fail with a 404.
    kinds: Vec<Kind>,
    scope: shared::Scope,
    /// Empty: all of the scope.
    namespaces: Vec<String>,
}

/// The scope a page scans to show `picked`: the one the window's badge scans (`home`) when it
/// covers them, so they share their scans, else `picked` alone.
fn scope_for(home: shared::Scope, picked: shared::Scope) -> shared::Scope {
    if home.covers(&picked) { home } else { picked }
}

/// The clusters of `clusters` the page shows: one where none of the namespaces chosen is
/// accessible is left out.
fn watched(clusters: &crate::ui::Scope, kinds: &HashMap<String, Vec<Kind>>) -> Vec<Watched> {
    (clusters.iter())
        .filter(|(_, _, ns)| !ns.is_empty())
        .map(|(ctx, client, ns)| {
            // `[None]`: the whole cluster; otherwise those namespaces.
            let namespaces: Vec<String> = ns.iter().flatten().cloned().collect();
            let scope = scope_for(shared::Scope::home(ctx), shared::Scope::new(ctx, namespaces.clone()));
            let kinds = kinds.get(ctx).cloned().unwrap_or_default();
            Watched { context: ctx.clone(), client: client.clone(), kinds, scope, namespaces }
        })
        .collect()
}

pub struct ProblemsView {
    /// Each cluster with the namespaces scanned there.
    clusters: crate::ui::Scope,
    /// What is shown of each cluster.
    watched: Arc<Vec<Watched>>,
    /// The findings shown, by cluster: what `found` was made from.
    shown_from: Vec<Option<Arc<Found>>>,
    /// The namespaces chosen, for the header.
    namespaces: Vec<String>,
    /// (context, problem) once a check has finished.
    found: Option<Vec<(String, Problem)>>,
    errors: Vec<String>,
    /// Every cluster scanned to the end at least once: until then, finding nothing means
    /// nothing yet.
    complete: bool,
    /// When the oldest of the findings shown were made.
    scanned: Option<Instant>,
    /// A scan of one of the clusters is running.
    updating: bool,
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
        let watched = Arc::new(watched(&clusters, &kinds));
        let mut this = Self {
            clusters,
            watched,
            shown_from: Vec::new(),
            namespaces,
            found: None,
            errors: Vec::new(),
            complete: false,
            scanned: None,
            updating: false,
            show_info: false,
            only: None,
            filter,
            parsed: PageFilter::default(),
            _task: None,
            _subs: subs,
        };
        this.reload(false, cx);
        this
    }

    /// Show other clusters or namespaces (`namespaces`: the ones chosen, for the header):
    /// their latest findings at once.
    pub fn set_scope(
        &mut self,
        clusters: crate::ui::Scope,
        kinds: HashMap<String, Vec<Kind>>,
        namespaces: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        self.watched = Arc::new(watched(&clusters, &kinds));
        (self.clusters, self.namespaces) = (clusters, namespaces);
        self.shown_from.clear();
        self.reload(false, cx);
        cx.notify();
    }

    /// Scan again now (F5), keeping the rows, the filter and the severity picked. A scan
    /// running is the one waited for.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.reload(true, cx);
        cx.notify();
    }

    /// Show the latest findings, then follow them while the page is on screen: a cluster is
    /// scanned when its findings are due (`shared::every`), or at once for a `refresh`.
    fn reload(&mut self, refresh: bool, cx: &mut Context<Self>) {
        self.show();
        let watched = self.watched.clone();
        self._task = Some(cx.spawn(async move |this, cx| {
            let mut changes = shared::changes();
            let mut wanted = refresh.then(Instant::now);
            loop {
                let exec = cx.background_executor().clone();
                if !crate::app::wait_visible(&this, cx, &exec).await {
                    break;
                }
                let fresh = wanted.take().map_or(Fresh::Paced, Fresh::Since);
                for s in watched.iter() {
                    // Not waited for: the rows follow the scan as its checks finish.
                    let _ = shared::ensure(&s.scope, fresh, || Ask {
                        client: s.client.clone(),
                        kinds: s.kinds.clone(),
                        lane: Lane::Foreground,
                        record: true,
                    });
                }
                let Ok(due) = this.update(cx, |this, cx| {
                    let due = this.show();
                    cx.notify();
                    due
                }) else {
                    break;
                };
                // Until some findings change, or the next scan is due.
                let wait = due.map_or(LOOK_AGAIN, |due| due.saturating_duration_since(Instant::now())).max(POLL);
                let timer = cx.background_executor().timer(wait);
                futures::future::select(Box::pin(changes.changed()), Box::pin(timer)).await;
            }
        }));
    }

    /// Read each cluster's latest findings; when the next scan is due. The rows are made
    /// again only when the findings changed.
    fn show(&mut self) -> Option<Instant> {
        let latest: Vec<Option<Arc<Found>>> = self.watched.iter().map(|s| shared::latest(&s.scope)).collect();
        let done: Vec<shared::Done> = latest.iter().filter_map(|f| f.as_ref()?.done).collect();
        let due = done.iter().map(|d| d.due()).min();
        let same = latest.len() == self.shown_from.len()
            && latest.iter().zip(&self.shown_from).all(|(a, b)| match (a, b) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (a, b) => a.is_none() && b.is_none(),
            });
        if same && self.found.is_some() {
            return due;
        }
        self.complete = done.len() == latest.len();
        self.scanned = done.iter().map(|d| d.ended).min();
        self.updating = latest.iter().flatten().any(|f| f.running.is_some());
        let results: Vec<(String, Vec<Problem>, Option<String>)> = (self.watched.iter().zip(&latest))
            .filter_map(|(s, f)| {
                let f = f.as_ref()?;
                Some((s.context.clone(), f.problems(&s.namespaces), f.skipped()))
            })
            .collect();
        // Nothing yet: the page says it is checking.
        let started = self.complete || !results.is_empty();
        let (found, errors) = merge(results);
        self.found = started.then_some(found);
        self.errors = errors;
        self.shown_from = latest;
        due
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
/// every cluster was scanned to the end (`complete`) and every kind could be read.
fn nothing_found(errors: &[String], complete: bool) -> (bool, &'static str) {
    if !complete {
        (false, CHECKING)
    } else if errors.is_empty() {
        (true, "Nothing broken right now.")
    } else {
        (false, "Nothing broken in what could be read.")
    }
}

/// How old the findings shown are: "checked 2m ago", "updating" while a scan runs.
fn status_text(complete: bool, scanned: Option<Duration>, updating: bool) -> Option<String> {
    let ago = scanned
        .filter(|_| complete)
        .map(|d| format!("Checked {} ago", k8s::duration_text(i64::try_from(d.as_secs()).unwrap_or(i64::MAX))));
    match (ago, updating) {
        (Some(ago), true) => Some(format!("{ago}, updating…")),
        (Some(ago), false) => Some(ago),
        (None, true) => Some("Checking…".into()),
        (None, false) => None,
    }
}

/// Every cluster's problems, worst first, and a line for each cluster with kinds it could
/// not read (context, problems, what it could not read): a kind refused is noted, and the
/// others are still shown.
fn merge(results: Vec<(String, Vec<Problem>, Option<String>)>) -> (Vec<(String, Problem)>, Vec<String>) {
    let mut found = Vec::new();
    let mut errors = Vec::new();
    for (ctx, problems, skipped) in results {
        if let Some(skipped) = skipped {
            errors.push(format!("{ctx}: could not read {skipped}"));
        }
        found.extend(problems.into_iter().map(|p| (ctx.clone(), p)));
    }
    found.sort_by_key(|a| a.1.severity);
    (found, errors)
}

impl Render for ProblemsView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        // Aged when drawn: the findings stay the same between scans.
        let status = status_text(self.complete, self.scanned.map(|at| at.elapsed()), self.updating);
        let header =
            page_header(Lucide::TriangleAlert, Some(theme.warning), "Problems", scope_text(&self.namespaces), cx)
                .child(div().flex_1())
                .when_some(status, |d, status| {
                    d.child(div().text_sm().text_color(theme.muted_foreground).child(status))
                })
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
            return v_flex()
                .size_full()
                .p_4()
                .gap_4()
                .child(header)
                .child(div().text_color(theme.muted_foreground).child(CHECKING));
        };
        let count = |s: Severity| found.iter().filter(|(_, p)| p.severity == s).count();
        let (crit, warn, info) = (count(Severity::Critical), count(Severity::Warning), count(Severity::Info));
        // A count picks its severity alone; picked again, every severity.
        let only = self.only;
        // The count is text in `color`: its tint stays light enough for it to read.
        let fill = crate::app::STATUS_FILL;
        let pill = |n: usize, label: &str, color: Hsla, severity: Severity, cx: &mut Context<Self>| {
            let picked = only == Some(severity);
            h_flex()
                .id(SharedString::from(format!("only-{label}")))
                .gap_1p5()
                .px_3()
                .py_1()
                .rounded_md()
                .bg(color.opacity(if picked { fill } else { 0.1 }))
                .border_1()
                .border_color(if picked { color } else { color.opacity(0.) })
                .cursor_pointer()
                .hover(|s| s.bg(color.opacity(fill)))
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
                let (all_clear, text) = nothing_found(&self.errors, self.complete);
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
    use crate::problems::{Problem, Severity};
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
    fn no_all_clear_when_a_scan_failed_or_is_not_done() {
        assert_eq!(super::nothing_found(&[], true), (true, "Nothing broken right now."));
        let errors = ["prod: connection refused".to_string()];
        assert_eq!(super::nothing_found(&errors, true), (false, "Nothing broken in what could be read."));
        // A first scan still running: nothing found yet is not an all-clear.
        assert_eq!(super::nothing_found(&[], false), (false, super::CHECKING));
    }

    #[test]
    fn the_page_says_how_old_its_rows_are() {
        let s = std::time::Duration::from_secs;
        assert_eq!(super::status_text(true, Some(s(150)), false).unwrap(), "Checked 2m ago");
        assert_eq!(super::status_text(true, Some(s(5)), true).unwrap(), "Checked 5s ago, updating…");
        assert_eq!(super::status_text(false, Some(s(5)), true).unwrap(), "Checking…");
        assert_eq!(super::status_text(false, None, false), None);
    }

    #[test]
    fn the_page_shares_the_badge_s_scan_when_it_covers_the_namespaces() {
        use crate::problems::shared::Scope;
        let scope = |ns: &[&str]| Scope::new("prod", ns.iter().map(ToString::to_string));
        // A cluster read whole: every namespace picked comes from its scan.
        assert_eq!(super::scope_for(scope(&[]), scope(&[])), scope(&[]));
        assert_eq!(super::scope_for(scope(&[]), scope(&["shop"])), scope(&[]));
        // Accessible namespaces set: those among them too, another one on its own.
        assert_eq!(super::scope_for(scope(&["a", "b"]), scope(&["a"])), scope(&["a", "b"]));
        assert_eq!(super::scope_for(scope(&["a", "b"]), scope(&["c"])), scope(&["c"]));
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
        let (found, errors) = super::merge(vec![
            ("one".into(), vec![problem(Severity::Warning, "w")], Some("Nodes (forbidden)".into())),
            ("two".into(), vec![problem(Severity::Critical, "c")], None),
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
