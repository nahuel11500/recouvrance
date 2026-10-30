//! The Argo CD page: every Application of the contexts that serve Argo CD, each under the app
//! of apps that made it, with where it deploys mapped to a kubeconfig context. Opening one
//! shows its page on Argo CD's cluster; its menu refreshes or syncs it.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::time::Instant;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use crate::details::{ArgoAction, OpenNamespace, OpenOnContext, confirm_argo_action};
use crate::k8s::argo::{self, Fleet, FleetApp, Summary};
use crate::k8s::{self, ObjectRef};
use crate::status::Tone;

// MARK: Grouping

/// Each Application under the one that made it, siblings in the order `cmp` gives:
/// (index, depth). One in a loop of parents starts a tree of its own; every index is there
/// exactly once.
pub fn tree_order(parents: &[Option<usize>], cmp: impl Fn(usize, usize) -> Ordering) -> Vec<(usize, usize)> {
    fn walk(root: usize, children: &[Vec<usize>], seen: &mut [bool], out: &mut Vec<(usize, usize)>) {
        let mut stack = vec![(root, 0)];
        while let Some((i, depth)) = stack.pop() {
            if std::mem::replace(&mut seen[i], true) {
                continue;
            }
            out.push((i, depth));
            stack.extend(children[i].iter().rev().map(|&c| (c, depth + 1)));
        }
    }
    let n = parents.len();
    let mut children = vec![Vec::new(); n];
    let mut roots = Vec::new();
    for (i, p) in parents.iter().enumerate() {
        match p.filter(|&p| p < n && p != i) {
            Some(p) => children[p].push(i),
            None => roots.push(i),
        }
    }
    for c in &mut children {
        c.sort_by(|a, b| cmp(*a, *b));
    }
    roots.sort_by(|a, b| cmp(*a, *b));
    let (mut seen, mut out) = (vec![false; n], Vec::with_capacity(n));
    for r in roots {
        walk(r, &children, &mut seen, &mut out);
    }
    // Parents in a loop: none of them is a root.
    let mut rest: Vec<usize> = (0..n).filter(|&i| !seen[i]).collect();
    rest.sort_by(|a, b| cmp(*a, *b));
    for r in rest {
        walk(r, &children, &mut seen, &mut out);
    }
    out
}

/// The Applications above `i`, nearest first, stopping where parents loop.
fn ancestors(parents: &[Option<usize>], i: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut at = i;
    while let Some(p) = parents.get(at).copied().flatten() {
        if p >= parents.len() || p == i || out.contains(&p) {
            break;
        }
        out.push(p);
        at = p;
    }
    out
}

/// A row on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shown {
    pub ix: usize,
    pub depth: usize,
    /// Shown only as the parent of rows the filter keeps.
    pub context_only: bool,
}

/// The rows of `order` (see `tree_order`) that `keeps` keeps, with the Applications above
/// them for context, and nothing under a collapsed one.
pub fn shown_rows(
    order: &[(usize, usize)],
    parents: &[Option<usize>],
    keeps: &[bool],
    collapsed: impl Fn(usize) -> bool,
) -> Vec<Shown> {
    let mut wanted = keeps.to_vec();
    for i in (0..keeps.len()).filter(|&i| keeps[i]) {
        for p in ancestors(parents, i) {
            wanted[p] = true;
        }
    }
    let mut out = Vec::new();
    let mut hidden_below: Option<usize> = None;
    for &(ix, depth) in order {
        if hidden_below.is_some_and(|d| depth > d) {
            continue;
        }
        hidden_below = None;
        if !wanted[ix] {
            continue;
        }
        out.push(Shown { ix, depth, context_only: !keeps[ix] });
        if collapsed(ix) {
            hidden_below = Some(depth);
        }
    }
    out
}

// MARK: Filters

/// What the page keeps: an Application in any of the states picked (none picked: all), in
/// the project and on the destination picked, matching the search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    pub out_of_sync: bool,
    pub degraded: bool,
    pub failed: bool,
    pub project: Option<String>,
    pub destination: Option<String>,
    /// In lowercase.
    pub query: String,
}

impl Filter {
    /// Whether it keeps `app`, shown on `destination` (see `destination_key`), whose
    /// searchable text in lowercase is `search`.
    pub fn keeps(&self, app: &Summary, destination: &str, search: &str) -> bool {
        let any_state = self.out_of_sync || self.degraded || self.failed;
        let state = !any_state
            || (self.out_of_sync && app.sync == "OutOfSync")
            || (self.degraded && app.health == "Degraded")
            || (self.failed && argo::sync_failed(app));
        state
            && self.project.as_ref().is_none_or(|p| *p == app.project)
            && self.destination.as_ref().is_none_or(|d| d == destination)
            && search.contains(&self.query)
    }
}

/// Where an Application is shown to deploy, and filtered by: its context, else the cluster
/// it names.
fn destination_key(a: &FleetApp) -> &str {
    a.on.as_deref().unwrap_or(&a.dest.cluster)
}

/// What the search looks in, in lowercase.
fn searchable(a: &FleetApp) -> String {
    let app = &a.app;
    let parts = [
        app.name.as_str(),
        &app.namespace,
        &app.project,
        destination_key(a),
        &a.dest.namespace,
        &a.hub.context,
        app.revision.as_deref().unwrap_or_default(),
    ];
    parts.join(" ").to_lowercase()
}

/// An Application, the same across reads: its hub, namespace and name.
type Key = (String, String, String);

fn key(a: &FleetApp) -> Key {
    (a.hub.context.clone(), a.app.namespace.clone(), a.app.name.clone())
}

// MARK: Rows

/// A read of every Application, with what the page needs worked out once (off the UI thread).
struct Rows {
    fleet: Fleet,
    order: Vec<(usize, usize)>,
    parents: Vec<Option<usize>>,
    /// How many Applications each one made, and how many under it are red.
    children: Vec<usize>,
    failing: Vec<usize>,
    tones: Vec<Tone>,
    search: Vec<String>,
    projects: Vec<String>,
    destinations: Vec<String>,
    /// How many are out of sync, Degraded, and with a failed sync.
    counts: (usize, usize, usize),
}

impl Rows {
    fn new(fleet: Fleet) -> Self {
        let apps = &fleet.apps;
        let parents: Vec<Option<usize>> = apps.iter().map(|a| a.parent).collect();
        let sort_key = |i: usize| (&apps[i].app.name, &apps[i].hub.context, &apps[i].app.namespace);
        let order = tree_order(&parents, |a, b| sort_key(a).cmp(&sort_key(b)));
        let tones: Vec<Tone> = apps.iter().map(|a| argo::attention(&a.app)).collect();
        let (mut children, mut failing) = (vec![0; apps.len()], vec![0; apps.len()]);
        for (i, tone) in tones.iter().enumerate() {
            if let Some(p) = ancestors(&parents, i).first() {
                children[*p] += 1;
            }
            if *tone == Tone::Bad {
                for p in ancestors(&parents, i) {
                    failing[p] += 1;
                }
            }
        }
        let mut projects: Vec<String> = apps.iter().map(|a| a.app.project.clone()).collect();
        projects.sort();
        projects.dedup();
        let mut destinations: Vec<String> = apps.iter().map(|a| destination_key(a).to_string()).collect();
        destinations.sort();
        destinations.dedup();
        let count = |f: &dyn Fn(&Summary) -> bool| apps.iter().filter(|a| f(&a.app)).count();
        let counts = (
            count(&|s: &Summary| s.sync == "OutOfSync"),
            count(&|s: &Summary| s.health == "Degraded"),
            count(&argo::sync_failed),
        );
        let search = apps.iter().map(searchable).collect();
        Self { fleet, order, parents, children, failing, tones, search, projects, destinations, counts }
    }
}

// MARK: Page

/// Column widths.
const NAME: f32 = 340.;
const INDENT: f32 = 16.;

pub struct ArgoView {
    clusters: Vec<(String, Client)>,
    rows: Option<Rows>,
    /// When the read shown started: an older one finishing later never replaces it.
    started: Option<Instant>,
    filter: Filter,
    search: Entity<InputState>,
    /// Applications whose children are hidden.
    collapsed: HashSet<Key>,
    /// The rows on screen, worked out when the data, the filter or a group changes.
    shown: Vec<Shown>,
    /// How many Applications the filter keeps.
    kept: usize,
    /// Reads while the page is shown.
    _poll: Task<()>,
    /// A read right after a write.
    after_write: crate::latest::Latest,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenOnContext> for ArgoView {}
impl EventEmitter<OpenNamespace> for ArgoView {}

impl ArgoView {
    pub fn new(clusters: Vec<(String, Client)>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search Applications…"));
        let subs = vec![cx.subscribe(&search, |this, state, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                this.filter.query = state.read(cx).value().to_lowercase();
                this.refilter();
                cx.notify();
            }
        })];
        let poll = {
            let clusters = clusters.clone();
            cx.spawn(async move |this, cx| {
                loop {
                    let exec = cx.background_executor().clone();
                    if !crate::app::wait_visible(&this, cx, &exec).await {
                        break;
                    }
                    let started = Instant::now();
                    let rows = read(clusters.clone(), &exec).await;
                    if this.update(cx, |this, cx| this.set_rows(rows, started, cx)).is_err() {
                        break;
                    }
                    exec.timer(crate::settings::secs(crate::settings::get().metrics_refresh)).await;
                }
            })
        };
        Self {
            clusters,
            rows: None,
            started: None,
            filter: Filter::default(),
            search,
            collapsed: HashSet::new(),
            shown: Vec::new(),
            kept: 0,
            _poll: poll,
            after_write: Default::default(),
            _subs: subs,
        }
    }

    /// Show a read, unless one that started later is shown already.
    fn set_rows(&mut self, rows: Rows, started: Instant, cx: &mut Context<Self>) {
        if self.started.is_some_and(|shown| shown > started) {
            return;
        }
        self.started = Some(started);
        self.rows = Some(rows);
        self.refilter();
        cx.notify();
    }

    /// Read again now (after a write).
    fn reload(&mut self, cx: &mut Context<Self>) {
        let (clusters, exec, started) = (self.clusters.clone(), cx.background_executor().clone(), Instant::now());
        self.after_write.spawn(cx, async move { read(clusters, &exec).await }, move |this, rows, cx| {
            this.set_rows(rows, started, cx);
        });
    }

    fn refilter(&mut self) {
        let Some(rows) = &self.rows else {
            self.shown.clear();
            self.kept = 0;
            return;
        };
        let apps = &rows.fleet.apps;
        let keeps: Vec<bool> = apps
            .iter()
            .enumerate()
            .map(|(i, a)| self.filter.keeps(&a.app, destination_key(a), &rows.search[i]))
            .collect();
        self.kept = keeps.iter().filter(|k| **k).count();
        let collapsed = &self.collapsed;
        self.shown = shown_rows(&rows.order, &rows.parents, &keeps, |i| collapsed.contains(&key(&apps[i])));
    }

    fn change_filter(&mut self, change: impl FnOnce(&mut Filter), cx: &mut Context<Self>) {
        change(&mut self.filter);
        self.refilter();
        cx.notify();
    }

    fn toggle_group(&mut self, k: Key, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&k) {
            self.collapsed.insert(k);
        }
        self.refilter();
        cx.notify();
    }

    /// Fold every app of apps, or unfold them all.
    fn toggle_all(&mut self, cx: &mut Context<Self>) {
        if self.collapsed.is_empty() {
            if let Some(rows) = &self.rows {
                let apps = &rows.fleet.apps;
                self.collapsed = (0..apps.len()).filter(|&i| rows.children[i] > 0).map(|i| key(&apps[i])).collect();
            }
        } else {
            self.collapsed.clear();
        }
        self.refilter();
        cx.notify();
    }

    fn find(&self, k: &Key) -> Option<&FleetApp> {
        self.rows.as_ref()?.fleet.apps.iter().find(|a| key(a) == *k)
    }

    /// Open the Application's page, on Argo CD's cluster.
    fn open(&mut self, k: &Key, cx: &mut Context<Self>) {
        let Some(a) = self.find(k) else { return };
        let obj = ObjectRef {
            kind: argo::application_kind(),
            name: a.app.name.clone(),
            namespace: Some(a.app.namespace.clone()),
            uid: String::new(),
        };
        let ev = OpenOnContext { context: a.hub.context.clone(), client: a.hub.client.clone(), obj };
        cx.emit(ev);
    }

    /// Open the namespace it deploys into, on the context that reaches its cluster.
    fn open_destination(&mut self, k: &Key, cx: &mut Context<Self>) {
        let Some(a) = self.find(k) else { return };
        let Some(context) = a.on.clone().filter(|_| !a.dest.namespace.is_empty()) else { return };
        let ev = OpenNamespace { context, namespace: a.dest.namespace.clone() };
        cx.emit(ev);
    }

    /// Confirm, then refresh or sync it (write mode and the hub's protection apply).
    fn act(&mut self, k: &Key, action: ArgoAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(a) = self.find(k).cloned() else { return };
        let view = cx.entity().downgrade();
        let done = move |_: &mut Window, cx: &mut App| {
            let _ = view.update(cx, Self::reload);
        };
        confirm_argo_action(&a.hub, &a.app.namespace, &a.app.name, action, done, window, cx);
    }

    fn several_hubs(&self) -> bool {
        self.rows.as_ref().is_some_and(|r| r.fleet.hubs.len() > 1)
    }

    fn row(&self, i: usize, s: Shown, cx: &Context<Self>) -> AnyElement {
        let Some(rows) = &self.rows else { return div().into_any_element() };
        let a = &rows.fleet.apps[s.ix];
        let app = &a.app;
        let k = key(a);
        let view = cx.entity().downgrade();
        let open_key = k.clone();
        let open = cx.listener(move |this, _, _, cx| this.open(&open_key, cx));
        let cx: &App = cx;
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let indent = INDENT * s.depth.min(8) as f32;
        let grouped = rows.children[s.ix] > 0;
        let collapsed = self.collapsed.contains(&k);
        let toggle = {
            let (view, k) = (view.clone(), k.clone());
            Button::new(("argo-group", i))
                .xsmall()
                .ghost()
                .icon(if collapsed { Lucide::ChevronRight } else { Lucide::ChevronDown })
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.toggle_group(k.clone(), cx));
                })
        };
        let last_sync = app.operation.as_ref().map(|(phase, _, finished)| {
            let when = finished.as_deref().and_then(|t| t.parse::<jiff::Timestamp>().ok()).map(k8s::age);
            let text = match when {
                Some(when) => format!("{phase}, {when} ago"),
                None => phase.clone(),
            };
            div().text_color(tone_color(argo::operation_tone(phase), cx)).child(text)
        });
        let destination = match &a.on {
            Some(context) => div().child(argo::destination_text(context, &a.dest.namespace)),
            None => div().text_color(muted).child(argo::destination_text(&a.dest.cluster, &a.dest.namespace)),
        };
        let cell = |w: f32| div().w(px(w)).flex_none().overflow_hidden().text_ellipsis().whitespace_nowrap();
        let opener = h_flex()
            .id(("argo-open", i))
            .flex_1()
            .min_w_0()
            .gap_3()
            .cursor_pointer()
            .on_click(open)
            .child(
                h_flex()
                    .w(px((NAME - indent - 24.).max(120.)))
                    .flex_none()
                    .gap_1p5()
                    .overflow_hidden()
                    .child(div().size(px(7.)).flex_none().rounded_full().bg(tone_color(rows.tones[s.ix], cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(theme.blue)
                            .child(app.name.clone()),
                    )
                    .when(grouped, |d| {
                        d.child(div().flex_none().text_xs().text_color(muted).child(rows.children[s.ix].to_string()))
                    })
                    .when(rows.failing[s.ix] > 0, |d| {
                        let n = rows.failing[s.ix];
                        d.child(div().flex_none().text_xs().text_color(theme.danger).child(format!("{n} red")))
                    }),
            )
            .child(cell(130.).text_color(muted).child(app.project.clone()))
            .child(
                cell(90.).text_color(tone_color(argo::sync_tone(&app.sync, app.automated), cx)).child(app.sync.clone()),
            )
            .child(
                cell(100.)
                    .text_color(tone_color(argo::listed_health_tone(&app.health, app.automated), cx))
                    .child(app.health.clone()),
            )
            .child(
                cell(90.)
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .child(app.revision.clone().unwrap_or_default()),
            )
            .child(cell(260.).child(destination))
            .child(cell(70.).text_color(muted).child(if app.automated { "On" } else { "Off" }))
            .child(cell(190.).children(last_sync))
            .when(self.several_hubs(), |d| d.child(cell(150.).text_color(muted).child(a.hub.context.clone())));
        h_flex()
            .id(("argo-row", i))
            .w_full()
            .gap_1()
            .px_2()
            .py_0p5()
            .text_sm()
            .border_b_1()
            .border_color(theme.border)
            .hover(|s| s.bg(theme.list_hover))
            .when(s.context_only, |d| d.opacity(0.55))
            .child(div().w(px(indent)).flex_none())
            .child(div().w(px(20.)).flex_none().when(grouped, |d| d.child(toggle)))
            .child(opener)
            .child(row_menu(&k, a.on.is_some() && !a.dest.namespace.is_empty(), view))
            .into_any_element()
    }

    fn header(&self, cx: &App) -> Div {
        let cell = |w: f32, text: &'static str| div().w(px(w)).flex_none().child(text);
        h_flex()
            .gap_3()
            .px_2()
            .py_1()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().w(px(NAME + 4.)).flex_none().child("Application"))
            .child(cell(130., "Project"))
            .child(cell(90., "Sync"))
            .child(cell(100., "Health"))
            .child(cell(90., "Revision"))
            .child(cell(260., "Destination"))
            .child(cell(70., "Auto-sync"))
            .child(cell(190., "Last sync"))
            .when(self.several_hubs(), |d| d.child(cell(150., "Argo CD on")))
    }

    /// The search, the state toggles with their counts, and the project and destination pickers.
    fn toolbar(&self, cx: &mut Context<Self>) -> Div {
        let (out_of_sync, degraded, failed) = self.rows.as_ref().map_or((0, 0, 0), |r| r.counts);
        let toggle = |id: &'static str, label: &str, n: usize, on: bool, flip: fn(&mut Filter)| {
            Button::new(id)
                .small()
                .when(on, ButtonVariants::primary)
                .when(!on, Button::outline)
                .label(format!("{label} ({n})"))
                .on_click(cx.listener(move |this, _, _, cx| this.change_filter(flip, cx)))
        };
        let projects = self.rows.as_ref().map_or_else(Vec::new, |r| r.projects.clone());
        let destinations = self.rows.as_ref().map_or_else(Vec::new, |r| r.destinations.clone());
        let view = cx.entity().downgrade();
        let collapse_label = if self.collapsed.is_empty() { "Collapse all" } else { "Expand all" };
        h_flex()
            .gap_2()
            .flex_wrap()
            .child(div().w(px(280.)).child(Input::new(&self.search).small().cleanable(true)))
            .child(toggle("argo-out-of-sync", "Out of sync", out_of_sync, self.filter.out_of_sync, |f| {
                f.out_of_sync = !f.out_of_sync;
            }))
            .child(toggle("argo-degraded", "Degraded", degraded, self.filter.degraded, |f| f.degraded = !f.degraded))
            .child(toggle("argo-failed", "Failed sync", failed, self.filter.failed, |f| f.failed = !f.failed))
            .child(picker(
                "argo-project",
                "All projects",
                self.filter.project.clone(),
                projects,
                view.clone(),
                |f, v| {
                    f.project = v;
                },
            ))
            .child(picker(
                "argo-destination",
                "All destinations",
                self.filter.destination.clone(),
                destinations,
                view,
                |f, v| f.destination = v,
            ))
            .child(div().flex_1())
            .child(
                Button::new("argo-collapse")
                    .small()
                    .ghost()
                    .label(collapse_label)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_all(cx))),
            )
    }
}

/// A button picking one value of a filter (or all of them) from a menu.
fn picker(
    id: &'static str,
    all: &'static str,
    current: Option<String>,
    values: Vec<String>,
    view: WeakEntity<ArgoView>,
    set: fn(&mut Filter, Option<String>),
) -> impl IntoElement {
    Button::new(id).small().outline().label(current.clone().unwrap_or_else(|| all.to_string())).dropdown_menu(
        move |menu, _, _| {
            let pick = |value: Option<String>| {
                let view = view.clone();
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let value = value.clone();
                    let _ = view.update(cx, |this, cx| this.change_filter(|f| set(f, value), cx));
                }
            };
            let menu = menu
                .scrollable(true)
                .max_h(px(420.))
                .item(PopupMenuItem::new(all).checked(current.is_none()).on_click(pick(None)));
            values.iter().fold(menu, |menu, v| {
                menu.item(
                    PopupMenuItem::new(v.clone()).checked(current.as_ref() == Some(v)).on_click(pick(Some(v.clone()))),
                )
            })
        },
    )
}

/// A row's menu: open it or where it deploys, refresh, hard refresh, sync. Its actions find the
/// Application again by its key: the list may have been read again since it was drawn.
fn row_menu(k: &Key, has_destination: bool, view: WeakEntity<ArgoView>) -> impl IntoElement {
    let k = k.clone();
    let id = SharedString::from(format!("argo-menu-{}-{}-{}", k.0, k.1, k.2));
    Button::new(id).xsmall().ghost().icon(Lucide::Ellipsis).dropdown_menu(move |menu: PopupMenu, _, _| {
        let open = {
            let (view, k) = (view.clone(), k.clone());
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let _ = view.update(cx, |this, cx| this.open(&k, cx));
            }
        };
        let destination = {
            let (view, k) = (view.clone(), k.clone());
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let _ = view.update(cx, |this, cx| this.open_destination(&k, cx));
            }
        };
        let act = |action: ArgoAction| {
            let (view, k) = (view.clone(), k.clone());
            move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                let _ = view.update(cx, |this, cx| this.act(&k, action, window, cx));
            }
        };
        menu.item(PopupMenuItem::new("Open").icon(Lucide::PanelRightOpen).on_click(open))
            .item(
                PopupMenuItem::new("Open destination namespace")
                    .icon(Lucide::FolderOpen)
                    .disabled(!has_destination)
                    .on_click(destination),
            )
            .separator()
            .item(PopupMenuItem::new("Refresh…").icon(Lucide::RefreshCw).on_click(act(ArgoAction::Refresh)))
            .item(
                PopupMenuItem::new("Hard refresh…").icon(Lucide::RefreshCcwDot).on_click(act(ArgoAction::HardRefresh)),
            )
            .item(PopupMenuItem::new("Sync…").icon(Lucide::GitBranch).on_click(act(ArgoAction::Sync)))
    })
}

/// Read every Application, then work out the rows off the UI thread.
async fn read(clusters: Vec<(String, Client)>, exec: &BackgroundExecutor) -> Rows {
    let fleet = argo::fleet(clusters).await;
    exec.spawn(async move { Rows::new(fleet) }).await
}

fn tone_color(tone: Tone, cx: &App) -> Hsla {
    let t = cx.theme();
    match tone {
        Tone::Good => t.success,
        Tone::Busy => t.warning,
        Tone::Bad => t.danger,
        Tone::Done => t.muted_foreground,
    }
}

impl Render for ArgoView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let body: AnyElement = match &self.rows {
            None => div().p_4().text_color(theme.muted_foreground).child("Reading Argo CD…").into_any_element(),
            Some(rows) if rows.fleet.hubs.is_empty() => v_flex()
                .p_4()
                .gap_2()
                .text_color(theme.muted_foreground)
                .child(
                    "No context serves Argo CD's Applications. If Argo CD runs on a cluster whose context needs a \
                     separate login, name it in `argocd_contexts` (see the configuration docs).",
                )
                .into_any_element(),
            Some(rows) => {
                let count = self.shown.len();
                let total = rows.fleet.apps.len();
                let summary = if self.filter == Filter::default() {
                    format!("{total} Applications")
                } else {
                    format!("{} of {total} Applications", self.kept)
                };
                v_flex()
                    .size_full()
                    .child(div().px_2().pb_2().text_sm().child(summary))
                    .child(self.header(cx))
                    .child(
                        // Only the rows on screen are laid out: a hub can hold hundreds of Applications.
                        uniform_list(
                            "argo-apps",
                            count,
                            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                range.filter_map(|i| this.shown.get(i).copied().map(|s| this.row(i, s, cx))).collect()
                            }),
                        )
                        .flex_1()
                        .min_h_0(),
                    )
                    .into_any_element()
            }
        };
        let errors = self.rows.as_ref().map_or_else(Vec::new, |r| r.fleet.errors.clone());
        v_flex()
            .size_full()
            .p_2()
            .gap_2()
            .child(self.toolbar(cx))
            .when(!errors.is_empty(), |d| {
                d.child(
                    v_flex()
                        .px_2()
                        .text_xs()
                        .text_color(theme.danger)
                        .child("Some Argo CD clusters could not be read:")
                        .children(errors.into_iter().map(|e| div().child(e))),
                )
            })
            .child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's own `test` attribute would shadow the standard one.
    use super::{Filter, Shown, ancestors, shown_rows, tree_order};
    use crate::k8s::argo::Summary;

    fn by_index(a: usize, b: usize) -> std::cmp::Ordering {
        a.cmp(&b)
    }

    #[test]
    fn applications_are_grouped_under_their_app_of_apps() {
        // 0: root, made 2 and 3; 3 made 1; 4 stands alone.
        let parents = [None, Some(3), Some(0), Some(0), None];
        assert_eq!(tree_order(&parents, by_index), [(0, 0), (2, 1), (3, 1), (1, 2), (4, 0)]);
        // Siblings follow the order given (by name on the page).
        let reversed = tree_order(&parents, |a, b| b.cmp(&a));
        assert_eq!(reversed, [(4, 0), (0, 0), (3, 1), (1, 2), (2, 1)]);
    }

    #[test]
    fn a_loop_of_parents_still_shows_each_application_once() {
        // 0 and 1 name each other, 2 names itself, 3 names one that isn't there.
        let parents = [Some(1), Some(0), Some(2), Some(9)];
        let order = tree_order(&parents, by_index);
        let mut seen: Vec<usize> = order.iter().map(|(i, _)| *i).collect();
        seen.sort_unstable();
        assert_eq!(seen, [0, 1, 2, 3]);
        assert_eq!(ancestors(&parents, 0), [1]);
        assert!(ancestors(&parents, 2).is_empty());
        assert!(ancestors(&parents, 3).is_empty());
    }

    #[test]
    fn a_kept_child_shows_its_parents_and_a_collapsed_group_hides_its_children() {
        let parents = [None, Some(0), Some(1), Some(0), None];
        let order = tree_order(&parents, by_index);
        // Only 2 kept: 0 and 1 above it are shown for context.
        let keeps = [false, false, true, false, false];
        let shown = shown_rows(&order, &parents, &keeps, |_| false);
        let row = |ix, depth, context_only| Shown { ix, depth, context_only };
        assert_eq!(shown, [row(0, 0, true), row(1, 1, true), row(2, 2, false)]);
        // Everything kept, 1 folded: 2 is hidden, its sibling 3 is not.
        let all = [true; 5];
        let ix = |s: Vec<Shown>| s.into_iter().map(|s| s.ix).collect::<Vec<_>>();
        assert_eq!(ix(shown_rows(&order, &parents, &all, |i| i == 1)), [0, 1, 3, 4]);
        assert_eq!(ix(shown_rows(&order, &parents, &all, |i| i == 0)), [0, 4]);
    }

    fn app(sync: &str, health: &str, phase: Option<&str>, project: &str) -> Summary {
        Summary {
            sync: sync.into(),
            health: health.into(),
            operation: phase.map(|p| (p.to_string(), None, None)),
            project: project.into(),
            ..Default::default()
        }
    }

    #[test]
    fn state_filters_add_up_and_the_others_narrow() {
        let out_of_sync = app("OutOfSync", "Healthy", Some("Succeeded"), "shop");
        let degraded = app("Synced", "Degraded", None, "shop");
        let failed = app("Synced", "Healthy", Some("Failed"), "data");
        let fine = app("Synced", "Healthy", Some("Succeeded"), "data");
        let keeps = |f: &Filter, a: &Summary| f.keeps(a, "prod", "web shop prod");
        assert!([&out_of_sync, &degraded, &failed, &fine].iter().all(|a| keeps(&Filter::default(), a)));
        let problems = Filter { out_of_sync: true, failed: true, ..Default::default() };
        assert!(keeps(&problems, &out_of_sync) && keeps(&problems, &failed));
        assert!(!keeps(&problems, &degraded) && !keeps(&problems, &fine));
        let shop = Filter { project: Some("shop".into()), ..problems };
        assert!(keeps(&shop, &out_of_sync) && !keeps(&shop, &failed));
        let elsewhere = Filter { destination: Some("staging".into()), ..Default::default() };
        assert!(!keeps(&elsewhere, &fine));
        let search = |q: &str| Filter { query: q.into(), ..Default::default() };
        assert!(keeps(&search("shop"), &fine) && !keeps(&search("billing"), &fine));
    }
}

/// Rules that hold for any parents, however tangled.
#[cfg(test)]
mod properties {
    use super::{shown_rows, tree_order};
    use proptest::prelude::*;

    proptest! {
        /// Every Application is listed once, under its parent when it has one outside a loop.
        #[test]
        fn every_application_is_shown_once(parents in proptest::collection::vec(proptest::option::of(0usize..12), 0..12)) {
            let order = tree_order(&parents, |a, b| a.cmp(&b));
            let mut ixs: Vec<usize> = order.iter().map(|(i, _)| *i).collect();
            ixs.sort_unstable();
            prop_assert_eq!(ixs, (0..parents.len()).collect::<Vec<_>>());
            // A child right after a shallower row is one level below it.
            for pair in order.windows(2) {
                prop_assert!(pair[1].1 <= pair[0].1 + 1);
            }
            // With nothing filtered or folded, the page shows the same rows.
            let all = vec![true; parents.len()];
            let shown: Vec<usize> = shown_rows(&order, &parents, &all, |_| false).into_iter().map(|s| s.ix).collect();
            prop_assert_eq!(shown, order.iter().map(|(i, _)| *i).collect::<Vec<_>>());
        }
    }
}
