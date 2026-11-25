//! The Argo CD page: every Application of the Argo CD API servers (the argocd CLI's, and those
//! set in Settings → Argo CD), each under the app of apps that made it or with the other
//! environments of its service, with where it deploys mapped to a kubeconfig context and when
//! it can sync next. Opening one, or a service, shows the dashboard of its service, a tab per
//! environment (see `dashboard`); its menu opens it in Argo CD's web UI, refreshes or syncs
//! it. Applications picked together (`grouped`) are diffed at once, the same change in
//! several of them shown once, or synced at once (`sync`). Every operation seen is kept in a
//! local journal (`journal`).
//!
//! Argo CD is read only by its API: each server's Applications are kept live from Argo CD's
//! stream (see `k8s::argo::api`), which stops while no window shows the page.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use crate::details::{ArgoAction, OpenOn, confirm_api_action};
use crate::k8s;
use crate::k8s::argo::diagnosis::Res;
use crate::k8s::argo::grouping::{self, AppKey};
use crate::k8s::argo::windows::{NextSync, Planner};
use crate::k8s::argo::{self, ApiRead, Fleet, FleetApp, Summary, api};
use crate::overview::column_cell as column;
use crate::status::Tone;

mod dashboard;
mod grouped;
mod journal;
mod sync;

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

/// A line of the list: an Application, or the heading of a service's Applications.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Line {
    App(Shown),
    /// The service at this index of the services.
    Service(usize),
}

/// The lines of the services `groups` (each with its Applications): a service with one
/// Application is that Application's row; one with several, a heading and its Applications
/// the filter keeps under it (none when it is collapsed). A service none of whose
/// Applications is kept is left out.
pub fn service_lines(groups: &[(String, Vec<usize>)], keeps: &[bool], collapsed: impl Fn(usize) -> bool) -> Vec<Line> {
    let mut out = Vec::new();
    for (g, (_, members)) in groups.iter().enumerate() {
        let kept: Vec<usize> = members.iter().copied().filter(|&i| keeps.get(i).copied().unwrap_or(false)).collect();
        if kept.is_empty() {
            continue;
        }
        if let [only] = members.as_slice() {
            out.push(Line::App(Shown { ix: *only, depth: 0, context_only: false }));
            continue;
        }
        out.push(Line::Service(g));
        if !collapsed(g) {
            out.extend(kept.into_iter().map(|ix| Line::App(Shown { ix, depth: 1, context_only: false })));
        }
    }
    out
}

/// `lines` with the groups holding a favorite first: a group is a top-level line (an
/// Application not under another, or a service's heading) with the lines under it, and moves
/// whole; the groups keep their order otherwise. `is_favorite` says it of a line (of a
/// service's heading: whether one of its Applications is, folded or not).
pub fn favorites_first(lines: Vec<Line>, is_favorite: impl Fn(&Line) -> bool) -> Vec<Line> {
    let mut groups: Vec<Vec<Line>> = Vec::new();
    for line in lines {
        match groups.last_mut() {
            Some(group) if matches!(line, Line::App(s) if s.depth > 0) => group.push(line),
            _ => groups.push(vec![line]),
        }
    }
    let groups = crate::favorites::favorites_first(groups, |g| g.iter().any(&is_favorite));
    groups.into_iter().flatten().collect()
}

/// How the list groups its Applications.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GroupBy {
    /// Under the app of apps that made each.
    #[default]
    Parent,
    /// With the other environments of their service (`grouping::service_of`).
    Service,
    /// Not grouped: by name.
    None,
}

impl GroupBy {
    fn label(self) -> &'static str {
        match self {
            Self::Parent => "Group: app of apps",
            Self::Service => "Group: service",
            Self::None => "Not grouped",
        }
    }
}

// MARK: Filters

/// What the page keeps: an Application in any of the states picked (none picked: all), in
/// the project, on the destination, with the sync, health, auto-sync and labels picked,
/// matching the search.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Filter {
    pub out_of_sync: bool,
    pub degraded: bool,
    pub failed: bool,
    /// Only the favorites (see `crate::favorites::FavApp`).
    pub favorites: bool,
    pub project: Option<String>,
    pub destination: Option<String>,
    pub sync: Option<String>,
    pub health: Option<String>,
    pub auto_sync: Option<bool>,
    /// Labels it must all have.
    pub labels: Vec<(String, String)>,
    /// In lowercase.
    pub query: String,
}

impl Filter {
    /// Whether it keeps `app`, shown on `destination` (see `destination_key`), whose
    /// searchable text in lowercase is `search`, a favorite or not.
    pub fn keeps(&self, app: &Summary, destination: &str, search: &str, favorite: bool) -> bool {
        let any_state = self.out_of_sync || self.degraded || self.failed;
        let state = !any_state
            || (self.out_of_sync && app.sync == "OutOfSync")
            || (self.degraded && app.health == "Degraded")
            || (self.failed && argo::sync_failed(app));
        state
            && (favorite || !self.favorites)
            && self.project.as_ref().is_none_or(|p| *p == app.project)
            && self.destination.as_ref().is_none_or(|d| d == destination)
            && self.sync.as_ref().is_none_or(|s| *s == app.sync)
            && self.health.as_ref().is_none_or(|h| *h == app.health)
            && self.auto_sync.is_none_or(|on| on == app.automated)
            && self.labels.iter().all(|l| app.labels.contains(l))
            && query_keeps(&self.query, app, search)
    }
}

/// Whether each word of the search (in lowercase) is in the searchable text, a word
/// `key=value` being a label it has instead (`key=` any value of that label).
pub fn query_keeps(query: &str, app: &Summary, search: &str) -> bool {
    query.split_whitespace().all(|word| match word.split_once('=') {
        Some((key, value)) if !key.is_empty() => {
            app.labels.iter().any(|(k, v)| k.to_lowercase() == key && (value.is_empty() || v.to_lowercase() == value))
        }
        _ => search.contains(word),
    })
}

/// Where an Application is shown to deploy, and filtered by: its context, else the cluster
/// it names.
fn destination_key(a: &FleetApp) -> &str {
    a.on.as_deref().unwrap_or(&a.dest.cluster)
}

/// What the search looks in, in lowercase.
fn searchable(a: &FleetApp) -> String {
    let app = &a.app;
    let source = a.server.name();
    let parts = [
        app.name.as_str(),
        &app.namespace,
        &app.project,
        destination_key(a),
        &a.dest.namespace,
        &source,
        app.revision.as_deref().unwrap_or_default(),
    ];
    parts.join(" ").to_lowercase()
}

/// An Application, the same across reads: its Argo CD, namespace and name.
type Key = AppKey;

fn key(a: &FleetApp) -> Key {
    (a.instance.clone(), a.app.namespace.clone(), a.app.name.clone())
}

/// `a` as a favorite: by its Argo CD instance (`FleetApp::instance`), namespace and name.
fn favorite(a: &FleetApp) -> crate::favorites::Favorite {
    crate::favorites::Favorite::Application(crate::favorites::FavApp {
        argocd: a.instance.clone(),
        namespace: a.app.namespace.clone(),
        name: a.app.name.clone(),
        context: None,
    })
}

// MARK: Rows

/// Most labels offered to filter by.
const MAX_LABELS: usize = 500;

/// A read of every Application, with what the page needs worked out once (off the UI thread).
struct Rows {
    fleet: Fleet,
    order: Vec<(usize, usize)>,
    /// Every Application by name, for the list not grouped.
    flat: Vec<usize>,
    parents: Vec<Option<usize>>,
    /// How many Applications each one made, and how many under it are red.
    children: Vec<usize>,
    failing: Vec<usize>,
    tones: Vec<Tone>,
    search: Vec<String>,
    projects: Vec<String>,
    destinations: Vec<String>,
    healths: Vec<String>,
    /// The labels present (`key`, `value`), to filter by.
    labels: Vec<(String, String)>,
    /// How many are out of sync, Degraded, and with a failed sync.
    counts: (usize, usize, usize),
    /// The services, each with its Applications by name.
    services: Vec<(String, Vec<usize>)>,
    /// When each can sync next, as of `now`.
    next: Vec<NextSync>,
    now: jiff::Timestamp,
}

impl Rows {
    fn new(fleet: Fleet, label_key: &str, plan: &mut Planner) -> Self {
        let now = plan.now();
        let apps = &fleet.apps;
        let parents: Vec<Option<usize>> = apps.iter().map(|a| a.parent).collect();
        let labels_of: Vec<String> = apps.iter().map(|a| a.server.name()).collect();
        let sort_key = |i: usize| (&apps[i].app.name, &labels_of[i], &apps[i].app.namespace);
        let cmp = |a: usize, b: usize| sort_key(a).cmp(&sort_key(b));
        let order = tree_order(&parents, cmp);
        let mut flat: Vec<usize> = (0..apps.len()).collect();
        flat.sort_by(|a, b| cmp(*a, *b));
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
        let sorted = |mut v: Vec<String>| {
            v.sort();
            v.dedup();
            v
        };
        let projects = sorted(apps.iter().map(|a| a.app.project.clone()).collect());
        let destinations = sorted(apps.iter().map(|a| destination_key(a).to_string()).collect());
        let healths = sorted(apps.iter().map(|a| a.app.health.clone()).collect());
        let mut labels: Vec<(String, String)> = apps.iter().flat_map(|a| a.app.labels.iter().cloned()).collect();
        labels.sort();
        labels.dedup();
        labels.truncate(MAX_LABELS);
        let count = |f: &dyn Fn(&Summary) -> bool| apps.iter().filter(|a| f(&a.app)).count();
        let counts = (
            count(&|s: &Summary| s.sync == "OutOfSync"),
            count(&|s: &Summary| s.health == "Degraded"),
            count(&argo::sync_failed),
        );
        let search = apps.iter().map(searchable).collect();
        let names: Vec<String> =
            apps.iter().map(|a| grouping::service_of(&a.app.name, &a.app.labels, label_key)).collect();
        let mut services = grouping::by_service(&names);
        for (_, members) in &mut services {
            members.sort_by(|a, b| cmp(*a, *b));
        }
        let next = apps.iter().map(|a| fleet.next_sync(a, plan)).collect();
        Self {
            fleet,
            order,
            flat,
            parents,
            children,
            failing,
            tones,
            search,
            projects,
            destinations,
            healths,
            labels,
            counts,
            services,
            next,
            now,
        }
    }
}

/// The color of a group: red when one of them is, amber when one is on its way, green when
/// all are.
fn group_tone(tones: impl Iterator<Item = Tone>) -> Tone {
    let tones: Vec<Tone> = tones.collect();
    if tones.contains(&Tone::Bad) {
        Tone::Bad
    } else if tones.contains(&Tone::Busy) {
        Tone::Busy
    } else if !tones.is_empty() && tones.iter().all(|t| *t == Tone::Good) {
        Tone::Good
    } else {
        Tone::Done
    }
}

// MARK: Page

/// Column widths.
const NAME: f32 = 340.;
const INDENT: f32 = 16.;
/// The space kept for a row's menu, so no column draws under it. The header keeps that room
/// too, so its columns shrink like the rows' in a narrow window (`column`).
const MENU: f32 = 24.;
/// How often the API servers' feeds are looked at for news, and the longest the next sync
/// times go without being worked out again.
const TICK: Duration = Duration::from_secs(1);
const RECOMPUTE: Duration = Duration::from_secs(60);

pub struct ArgoView {
    clusters: Vec<(String, Client)>,
    /// When each Application can sync next, kept for a minute (see `rebuild`).
    plan: Arc<Mutex<Planner>>,
    /// The API servers' feeds this page holds while it is shown.
    watches: Vec<api::Watch>,
    /// What could not be used of the API servers' config.
    config_errors: Vec<String>,
    rows: Option<Rows>,
    /// Works out the rows off the UI thread; a newer one replaces the one running.
    build: crate::latest::Latest,
    filter: Filter,
    group_by: GroupBy,
    search: Entity<InputState>,
    /// Applications whose children are hidden, and services whose Applications are.
    collapsed: HashSet<Key>,
    collapsed_services: HashSet<String>,
    /// The lines on screen, worked out when the data, the filter or a group changes.
    lines: Vec<Line>,
    /// How many Applications the filter keeps.
    kept: usize,
    favorites: Entity<crate::favorites::Favorites>,
    /// Which Applications (by index in the rows shown) are favorites, and how many.
    starred: Vec<bool>,
    starred_count: usize,
    /// Holds the API servers' feeds while the page is shown.
    _feeds: Task<()>,
    /// The dashboard of a service, shown instead of the list while it is open.
    dashboard: Option<dashboard::Dashboard>,
    /// The Applications picked, by Argo CD, namespace and name (see `grouped`); how many of
    /// them the filter keeps, and how many it hides.
    selected: HashSet<Key>,
    picked: usize,
    picked_hidden: usize,
    /// Which Applications (by index in the rows) the filter keeps.
    keeps: Vec<bool>,
    /// The grouped diff of the Applications picked, shown instead of the list while open.
    grouped: Option<grouped::GroupedDiff>,
    /// The sync of the Applications picked, shown instead of the list while open (and kept,
    /// hidden, while it runs).
    sync: Option<sync::GroupedSync>,
    /// The sync journal, shown over the rest while open; the Application words to filter it by.
    journal: Option<journal::JournalPage>,
    journal_search: Entity<InputState>,
    /// The words a sync's steps are filtered by (status, kind, name), in the journal and the
    /// dashboard's Last sync; lowercase.
    steps_search: Entity<InputState>,
    steps_query: String,
    /// An Application asked for from another page (`show_app`), whose dashboard opens once
    /// it is listed.
    asked: Option<Key>,
    _subs: Vec<Subscription>,
}

/// Ask the app to open the Argo CD page on an Application's dashboard: by its Argo CD
/// (`Endpoint::instance`), namespace and name.
#[derive(Clone, Debug)]
pub struct OpenArgoApp {
    pub instance: String,
    pub namespace: String,
    pub name: String,
}

impl EventEmitter<OpenOn> for ArgoView {}

impl ArgoView {
    pub fn new(
        clusters: Vec<(String, Client)>,
        favorites: Entity<crate::favorites::Favorites>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search Applications, or key=value…"));
        let journal_search = cx.new(|cx| InputState::new(window, cx).placeholder("Application…"));
        let steps_search = cx.new(|cx| InputState::new(window, cx).placeholder("Steps: status, kind, name…"));
        let subs = vec![
            cx.subscribe(&steps_search, |this, state, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.steps_query = state.read(cx).value().to_lowercase();
                    cx.notify();
                }
            }),
            cx.subscribe(&journal_search, |this, state, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    let text = state.read(cx).value().to_string();
                    this.journal_typed(&text, cx);
                }
            }),
            cx.subscribe(&search, |this, state, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.filter.query = state.read(cx).value().to_lowercase();
                    this.refilter(cx);
                    cx.notify();
                }
            }),
            // A star put here, on an Application's page or in the sidebar.
            cx.observe(&favorites, |this, _, cx| {
                this.refilter(cx);
                cx.notify();
            }),
        ];
        let feeds = cx.spawn(async move |this, cx| {
            loop {
                let exec = cx.background_executor().clone();
                if !crate::app::wait_visible(&this, cx, &exec).await {
                    break;
                }
                let generation = api::generation();
                let stamp = exec.spawn(async { api::config_stamp() }).await;
                let endpoints = exec.spawn(async { api::endpoints() }).await;
                let watches: Vec<api::Watch> = endpoints.list.iter().filter(|e| e.on).map(api::watch).collect();
                let held = this.update(cx, |this, cx| {
                    this.config_errors = endpoints.errors;
                    this.watches = watches;
                    this.rebuild(cx);
                });
                if held.is_err() {
                    break;
                }
                let (mut seen, mut built) = (api::changes(), Instant::now());
                loop {
                    exec.timer(TICK).await;
                    let Ok(shown) = this.update(cx, |_, cx| crate::app::is_visible(cx.entity_id(), cx)) else { return };
                    // A new `argocd login`, or a token set: read the servers again.
                    let relogged = exec.spawn(async { api::config_stamp() }).await != stamp;
                    if !shown || relogged || api::generation() != generation {
                        // Hidden, or the servers changed: the feeds stop unless another page holds them.
                        // Hidden, the dashboard's read stops too; it reads again once shown.
                        let _ = this.update(cx, |this, cx| {
                            this.watches.clear();
                            if !shown {
                                this.dashboard_hidden(cx);
                            }
                        });
                        break;
                    }
                    let now = api::changes();
                    if now != seen || built.elapsed() >= RECOMPUTE {
                        (seen, built) = (now, Instant::now());
                        let _ = this.update(cx, Self::rebuild);
                    }
                }
            }
        });
        Self {
            clusters,
            plan: Arc::new(Mutex::new(Planner::new(jiff::Timestamp::UNIX_EPOCH))),
            watches: Vec::new(),
            config_errors: Vec::new(),
            rows: None,
            build: Default::default(),
            filter: Filter::default(),
            group_by: GroupBy::default(),
            search,
            collapsed: HashSet::new(),
            collapsed_services: HashSet::new(),
            lines: Vec::new(),
            kept: 0,
            favorites,
            starred: Vec::new(),
            starred_count: 0,
            _feeds: feeds,
            dashboard: None,
            selected: HashSet::new(),
            picked: 0,
            picked_hidden: 0,
            keeps: Vec::new(),
            grouped: None,
            sync: None,
            journal: None,
            journal_search,
            steps_search,
            steps_query: String::new(),
            asked: None,
            _subs: subs,
        }
    }

    /// Open the dashboard of the Application `app` (from the Problems page), over what the
    /// page shows; once it is listed when it isn't yet.
    pub fn show_app(&mut self, app: &OpenArgoApp, cx: &mut Context<Self>) {
        self.asked = Some((app.instance.clone(), app.namespace.clone(), app.name.clone()));
        self.open_asked(cx);
        cx.notify();
    }

    /// Open the dashboard asked for by `show_app` if its Application is listed now.
    fn open_asked(&mut self, cx: &mut Context<Self>) {
        let Some(k) = self.asked.clone() else { return };
        let listed = self.rows.as_ref().is_some_and(|r| r.fleet.apps.iter().any(|a| key(a) == k));
        if !listed {
            return;
        }
        self.asked = None;
        self.journal = None;
        self.grouped = None;
        if let Some(sync) = &mut self.sync {
            sync.shown = false;
        }
        self.open_dashboard(&k, cx);
    }

    /// Work out the rows again from the API servers' feeds, off the UI thread.
    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let feeds: Vec<(Arc<api::Endpoint>, Arc<Mutex<api::FeedState>>)> =
            self.watches.iter().map(|w| (w.endpoint.clone(), w.state.clone())).collect();
        let connected: Vec<String> = self.clusters.iter().map(|(n, _)| n.clone()).collect();
        let config_errors = self.config_errors.clone();
        let (label_key, servers) = crate::settings::with(|s| (s.argocd_group_label.clone(), s.argocd_servers.clone()));
        let plan = self.plan.clone();
        let exec = cx.background_executor().clone();
        let work = async move {
            exec.spawn(async move {
                let reads = feeds
                    .into_iter()
                    .map(|(endpoint, state)| ApiRead {
                        endpoint,
                        state: state.lock().unwrap_or_else(PoisonError::into_inner).clone(),
                    })
                    .collect();
                let mut fleet = argo::from_api(reads, &argo::targets(), &connected, &servers);
                fleet.errors.extend(config_errors);
                // When each can sync next is worked out anew each minute; in between, the
                // answers kept are reused (the same windows give the same answer).
                let mut plan = plan.lock().unwrap_or_else(PoisonError::into_inner);
                let now = jiff::Timestamp::now();
                if now.as_second() - plan.now().as_second() >= RECOMPUTE.as_secs() as i64 {
                    *plan = Planner::new(now);
                }
                Rows::new(fleet, &label_key, &mut plan)
            })
            .await
        };
        self.build.spawn(cx, work, |this, rows, cx| {
            this.rows = Some(rows);
            this.refilter(cx);
            this.dashboard_changed(cx);
            this.open_asked(cx);
            cx.notify();
        });
    }

    /// What the page shows when no Argo CD server is logged in to or set: how to add one.
    fn no_server(&self, fleet: &Fleet, cx: &App) -> AnyElement {
        let theme = cx.theme();
        v_flex()
            .p_4()
            .gap_3()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child("No Argo CD server is logged in to or set. Argo CD is read only through its API.")
            .child(
                "Log in with `argocd login <server>` (the argocd CLI's servers are read), or add the server's \
                 address and a token in Settings → Argo CD.",
            )
            .children(fleet.errors.iter().map(|e| div().text_color(theme.danger).child(e.clone())))
            .into_any_element()
    }

    fn refilter(&mut self, cx: &App) {
        let Some(rows) = &self.rows else {
            self.lines.clear();
            self.kept = 0;
            self.starred.clear();
            self.starred_count = 0;
            self.keeps.clear();
            (self.picked, self.picked_hidden) = (0, 0);
            return;
        };
        let apps = &rows.fleet.apps;
        let favorites = self.favorites.read(cx);
        self.starred = apps.iter().map(|a| favorites.has(&favorite(a))).collect();
        self.starred_count = self.starred.iter().filter(|s| **s).count();
        let starred = &self.starred;
        let keeps: Vec<bool> = apps
            .iter()
            .enumerate()
            .map(|(i, a)| self.filter.keeps(&a.app, destination_key(a), &rows.search[i], starred[i]))
            .collect();
        self.kept = keeps.iter().filter(|k| **k).count();
        let (collapsed, services) = (&self.collapsed, &self.collapsed_services);
        let lines = match self.group_by {
            GroupBy::Parent => shown_rows(&rows.order, &rows.parents, &keeps, |i| collapsed.contains(&key(&apps[i])))
                .into_iter()
                .map(Line::App)
                .collect(),
            GroupBy::Service => service_lines(&rows.services, &keeps, |g| services.contains(&rows.services[g].0)),
            GroupBy::None => rows
                .flat
                .iter()
                .filter(|&&i| keeps[i])
                .map(|&ix| Line::App(Shown { ix, depth: 0, context_only: false }))
                .collect(),
        };
        // Favorites first, in every grouping: with the app of apps or service they are in.
        self.lines = favorites_first(lines, |line| match line {
            Line::App(s) => starred[s.ix],
            Line::Service(g) => rows.services[*g].1.iter().any(|&i| starred[i]),
        });
        self.keeps = keeps;
        self.recount_picked();
    }

    fn change_filter(&mut self, change: impl FnOnce(&mut Filter), cx: &mut Context<Self>) {
        change(&mut self.filter);
        self.refilter(cx);
        cx.notify();
    }

    fn set_group_by(&mut self, by: GroupBy, cx: &mut Context<Self>) {
        self.group_by = by;
        self.refilter(cx);
        cx.notify();
    }

    /// Star or unstar the Application (kept in favorites.json; the page sorts again when the
    /// favorites change). Unstarring removes it under any name of its Argo CD.
    fn toggle_favorite(&mut self, k: &Key, cx: &mut Context<Self>) {
        let Some(f) = self.find(k).map(favorite) else { return };
        self.favorites.update(cx, |favs, cx| favs.toggle(f, cx));
    }

    fn toggle_group(&mut self, k: Key, cx: &mut Context<Self>) {
        if !self.collapsed.remove(&k) {
            self.collapsed.insert(k);
        }
        self.refilter(cx);
        cx.notify();
    }

    fn toggle_service(&mut self, service: String, cx: &mut Context<Self>) {
        if !self.collapsed_services.remove(&service) {
            self.collapsed_services.insert(service);
        }
        self.refilter(cx);
        cx.notify();
    }

    /// Whether some group is folded, the way the list is grouped.
    fn any_collapsed(&self) -> bool {
        match self.group_by {
            GroupBy::Parent => !self.collapsed.is_empty(),
            GroupBy::Service => !self.collapsed_services.is_empty(),
            GroupBy::None => false,
        }
    }

    /// Fold every group, or unfold them all.
    fn toggle_all(&mut self, cx: &mut Context<Self>) {
        let fold = !self.any_collapsed();
        match (self.group_by, &self.rows) {
            (GroupBy::Parent, Some(rows)) if fold => {
                let apps = &rows.fleet.apps;
                self.collapsed = (0..apps.len()).filter(|&i| rows.children[i] > 0).map(|i| key(&apps[i])).collect();
            }
            (GroupBy::Service, Some(rows)) if fold => {
                self.collapsed_services =
                    rows.services.iter().filter(|(_, m)| m.len() > 1).map(|(s, _)| s.clone()).collect();
            }
            (GroupBy::Parent, _) => self.collapsed.clear(),
            (GroupBy::Service, _) => self.collapsed_services.clear(),
            _ => {}
        }
        self.refilter(cx);
        cx.notify();
    }

    fn find(&self, k: &Key) -> Option<&FleetApp> {
        self.rows.as_ref()?.fleet.apps.iter().find(|a| key(a) == *k)
    }

    /// Open the Application in Argo CD's web UI.
    fn open(&mut self, k: &Key, cx: &mut Context<Self>) {
        let Some(a) = self.find(k) else { return };
        crate::forwards::open_web_link(&a.server.web_url(&a.app.namespace, &a.app.name), cx);
    }

    /// Open the namespace it deploys into, on the context that reaches its cluster.
    fn open_destination(&mut self, k: &Key, cx: &mut Context<Self>) {
        let Some(a) = self.find(k) else { return };
        let Some(context) = a.on.clone().filter(|_| !a.dest.namespace.is_empty()) else { return };
        let ev = OpenOn { context, res: Res::namespace(&a.dest.namespace) };
        cx.emit(ev);
    }

    /// Confirm, then refresh or sync it by Argo CD's API (write mode applies, confirmed as a
    /// protected context's).
    fn act(&mut self, k: &Key, action: ArgoAction, window: &mut Window, cx: &mut Context<Self>) {
        let Some(a) = self.find(k).cloned() else { return };
        confirm_api_action(&a.server, &a.app.namespace, &a.app.name, action, window, cx);
    }

    /// Several servers are read: each row says which.
    fn several_sources(&self) -> bool {
        self.rows.as_ref().is_some_and(|r| r.fleet.servers.len() > 1)
    }

    fn row(&self, i: usize, s: Shown, cx: &Context<Self>) -> AnyElement {
        let Some(rows) = &self.rows else { return div().into_any_element() };
        let a = &rows.fleet.apps[s.ix];
        let app = &a.app;
        let k = key(a);
        let view = cx.entity().downgrade();
        let open_key = k.clone();
        let open = cx.listener(move |this, _, _, cx| this.open_dashboard(&open_key, cx));
        let cx: &App = cx;
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let indent = INDENT * s.depth.min(8) as f32;
        let grouped = self.group_by == GroupBy::Parent && rows.children[s.ix] > 0;
        let collapsed = self.collapsed.contains(&k);
        let starred = self.starred.get(s.ix).copied().unwrap_or_default();
        let pick = {
            let (view, k) = (view.clone(), k.clone());
            Checkbox::new(("argo-pick", i)).checked(self.selected.contains(&k)).on_click(move |_: &bool, _, cx| {
                let _ = view.update(cx, |this, cx| this.toggle_pick(k.clone(), cx));
            })
        };
        let star = {
            let (view, k) = (view.clone(), k.clone());
            div()
                .id(("argo-star", i))
                .flex_none()
                .cursor_pointer()
                .when(!starred, |d| {
                    d.opacity(0.).group_hover(SharedString::from(format!("argo-row-{i}")), |s| s.opacity(0.5))
                })
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    let _ = view.update(cx, |this, cx| this.toggle_favorite(&k, cx));
                })
                .child(if starred {
                    Icon::new(component::IconName::StarFill).size_3p5().text_color(yellow())
                } else {
                    Icon::new(Lucide::Star).size_3p5()
                })
        };
        let toggle = {
            let (view, k) = (view.clone(), k.clone());
            Button::new(("argo-group", i))
                .xsmall()
                .ghost()
                .icon(if collapsed { Lucide::ChevronRight } else { Lucide::ChevronDown })
                .accessibility_label(if collapsed { "Open the group" } else { "Fold the group" })
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
            div().text_color(argo::operation_tone(phase).color(cx)).child(text)
        });
        let destination = match &a.on {
            Some(context) => div().child(argo::destination_text(context, &a.dest.namespace)),
            None => div().text_color(muted).child(argo::destination_text(&a.dest.cluster, &a.dest.namespace)),
        };
        let next = &rows.next[s.ix];
        let next_color = match next {
            NextSync::Now | NextSync::NoWindow => muted,
            NextSync::At(_) => theme.warning,
            NextSync::NotSoon => theme.danger,
            NextSync::Unknown(_) => muted,
        };
        let opener = h_flex()
            .id(("argo-open", i))
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .gap_3()
            .cursor_pointer()
            .on_click(open)
            .child(
                h_flex()
                    .w(px((NAME - indent - 48.).max(120.)))
                    .flex_none()
                    .gap_1p5()
                    .overflow_hidden()
                    .child(div().size(px(7.)).flex_none().rounded_full().bg(rows.tones[s.ix].color(cx)))
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
                    .when(grouped && rows.failing[s.ix] > 0, |d| {
                        let n = rows.failing[s.ix];
                        d.child(div().flex_none().text_xs().text_color(theme.danger).child(format!("{n} red")))
                    })
                    .child(star),
            )
            .child(column(130., true).text_color(muted).child(app.project.clone()))
            .child(
                column(90., false)
                    .text_color(argo::sync_tone(&app.sync, app.automated).color(cx))
                    .child(app.sync.clone()),
            )
            .child(
                column(100., false)
                    .text_color(argo::listed_health_tone(&app.health, app.automated).color(cx))
                    .child(app.health.clone()),
            )
            .child(
                column(90., false)
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .child(app.revision.clone().unwrap_or_default()),
            )
            .child(column(260., true).child(destination))
            .child(column(70., false).text_color(muted).child(if app.automated { "On" } else { "Off" }))
            .child(column(110., true).text_color(next_color).child(next.text(rows.now)))
            .child(column(190., true).children(last_sync))
            .when(self.several_sources(), |d| d.child(column(150., true).text_color(muted).child(a.server.name())));
        h_flex()
            .id(("argo-row", i))
            .group(SharedString::from(format!("argo-row-{i}")))
            .w_full()
            .gap_1()
            .px_2()
            .py_0p5()
            .text_sm()
            .border_b_1()
            .border_color(theme.border)
            .hover(|s| s.bg(theme.list_hover))
            .when(s.context_only, |d| d.opacity(0.55))
            .child(div().w(px(20.)).flex_none().child(pick))
            .child(div().w(px(indent)).flex_none())
            .child(div().w(px(20.)).flex_none().when(grouped, |d| d.child(toggle)))
            .child(opener)
            .child(div().w(px(MENU)).flex_none().child(row_menu(
                &k,
                a.on.is_some() && !a.dest.namespace.is_empty(),
                Offers::of(app),
                grouped.then_some(s.ix),
                view,
            )))
            .into_any_element()
    }

    /// The heading of a service's Applications: its name, how many there are, how many are
    /// synced and red, and the worst of their colors.
    fn service_row(&self, i: usize, g: usize, cx: &Context<Self>) -> AnyElement {
        let Some(rows) = &self.rows else { return div().into_any_element() };
        let Some((service, members)) = rows.services.get(g) else { return div().into_any_element() };
        let view = cx.entity().downgrade();
        let collapsed = self.collapsed_services.contains(service);
        let flip = {
            let (service, view) = (service.clone(), view.clone());
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let service = service.clone();
                let _ = view.update(cx, |this, cx| this.toggle_service(service, cx));
            }
        };
        let pick = {
            let view = view.clone();
            Checkbox::new(("argo-pick-service", i)).checked(self.group_picked(g, true)).on_click(
                move |_: &bool, _, cx| {
                    let _ = view.update(cx, |this, cx| this.pick_group(g, true, cx));
                },
            )
        };
        let open = move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
            let _ = view.update(cx, |this, cx| this.open_service(g, cx));
        };
        let cx: &App = cx;
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let tone = group_tone(members.iter().map(|&m| rows.tones[m]));
        let red = members.iter().filter(|&&m| rows.tones[m] == Tone::Bad).count();
        let synced = members.iter().filter(|&&m| rows.fleet.apps[m].app.sync == "Synced").count();
        h_flex()
            .id(("argo-service", i))
            .w_full()
            .gap_1()
            .px_2()
            .py_0p5()
            .text_sm()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.muted)
            .cursor_pointer()
            .child(div().w(px(20.)).flex_none().child(pick))
            .child(
                div().id(("argo-service-fold", i)).w(px(20.)).flex_none().on_click(flip).child(
                    Icon::new(if collapsed { Lucide::ChevronRight } else { Lucide::ChevronDown })
                        .size_4()
                        .text_color(muted),
                ),
            )
            .child(
                h_flex()
                    .id(("argo-service-open", i))
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .overflow_hidden()
                    .on_click(open)
                    .child(div().size(px(7.)).flex_none().rounded_full().bg(tone.color(cx)))
                    .child(div().font_weight(FontWeight::MEDIUM).whitespace_nowrap().child(service.clone()))
                    .child(div().text_xs().text_color(muted).child(crate::plural::count(members.len(), "Application")))
                    .child(div().text_xs().text_color(muted).child(format!("{synced} synced")))
                    .when(red > 0, |d| d.child(div().text_xs().text_color(theme.danger).child(format!("{red} red")))),
            )
            .into_any_element()
    }

    fn header(&self, cx: &App) -> Div {
        let cell = |w: f32, shrinks: bool, text: &'static str| column(w, shrinks).child(text);
        h_flex()
            .gap_3()
            .px_2()
            .py_1()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().w(px(NAME + 4.)).flex_none().pl(px(24.)).child("Application"))
            .child(cell(130., true, "Project"))
            .child(cell(90., false, "Sync"))
            .child(cell(100., false, "Health"))
            .child(cell(90., false, "Revision"))
            .child(cell(260., true, "Destination"))
            .child(cell(70., false, "Auto-sync"))
            .child(cell(110., true, "Next sync"))
            .child(cell(190., true, "Last sync"))
            .when(self.several_sources(), |d| d.child(cell(150., true, "Argo CD")))
            // Where the rows have their menu (after a 4 px gap, the header's are 12): the columns
            // get the same room in both, so they shrink alike.
            .child(div().w(px(MENU - 8.)).flex_none())
    }

    /// The search, the state toggles with their counts, the pickers, and the grouping.
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
        let list = |f: fn(&Rows) -> &Vec<String>| self.rows.as_ref().map_or_else(Vec::new, |r| f(r).clone());
        let projects = list(|r| &r.projects);
        let destinations = list(|r| &r.destinations);
        let healths = list(|r| &r.healths);
        let labels = self.rows.as_ref().map_or_else(Vec::new, |r| r.labels.clone());
        let view = cx.entity().downgrade();
        let collapse_label = if self.any_collapsed() { "Expand all" } else { "Collapse all" };
        let f = &self.filter;
        let auto_sync = f.auto_sync.map(|on| if on { AUTO_ON } else { AUTO_OFF }.to_string());
        h_flex()
            .gap_2()
            .flex_wrap()
            .child(div().w(px(280.)).child(Input::new(&self.search).small().cleanable(true)))
            .child(toggle("argo-out-of-sync", "Out of sync", out_of_sync, f.out_of_sync, |f| {
                f.out_of_sync = !f.out_of_sync;
            }))
            .child(toggle("argo-degraded", "Degraded", degraded, f.degraded, |f| f.degraded = !f.degraded))
            .child(toggle("argo-failed", "Failed sync", failed, f.failed, |f| f.failed = !f.failed))
            .child(toggle("argo-favorites", "Favorites", self.starred_count, f.favorites, |f| {
                f.favorites = !f.favorites;
            }))
            .child(picker("argo-project", "All projects", f.project.clone(), projects, view.clone(), |f, v| {
                f.project = v;
            }))
            .child(picker(
                "argo-destination",
                "All destinations",
                f.destination.clone(),
                destinations,
                view.clone(),
                |f, v| f.destination = v,
            ))
            .child(picker(
                "argo-sync",
                "Any sync",
                f.sync.clone(),
                ["Synced", "OutOfSync", "Unknown"].map(String::from).to_vec(),
                view.clone(),
                |f, v| f.sync = v,
            ))
            .child(picker("argo-health", "Any health", f.health.clone(), healths, view.clone(), |f, v| f.health = v))
            .child(picker(
                "argo-auto-sync",
                "Auto-sync on or off",
                auto_sync,
                [AUTO_ON, AUTO_OFF].map(String::from).to_vec(),
                view.clone(),
                |f, v| f.auto_sync = v.map(|v| v == AUTO_ON),
            ))
            .child(label_picker(f.labels.clone(), labels, view.clone()))
            .child(div().flex_1())
            .child(group_picker(self.group_by, view))
            .child(
                Button::new("argo-collapse")
                    .small()
                    .ghost()
                    .label(collapse_label)
                    .disabled(self.group_by == GroupBy::None)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_all(cx))),
            )
    }
}

const AUTO_ON: &str = "Auto-sync on";
const AUTO_OFF: &str = "Auto-sync off";

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

/// The labels to filter by, from those the Applications have: each click adds or removes
/// one (`key=value` typed in the search does the same).
fn label_picker(
    picked: Vec<(String, String)>,
    present: Vec<(String, String)>,
    view: WeakEntity<ArgoView>,
) -> impl IntoElement {
    let text = match picked.as_slice() {
        [] => "Any label".to_string(),
        [(k, v)] => format!("{k}={v}"),
        many => crate::plural::count(many.len(), "label"),
    };
    Button::new("argo-labels").small().outline().label(text).dropdown_menu(move |menu, _, _| {
        let clear = {
            let view = view.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let _ = view.update(cx, |this, cx| this.change_filter(|f| f.labels.clear(), cx));
            }
        };
        let menu = menu
            .scrollable(true)
            .max_h(px(420.))
            .item(PopupMenuItem::new("Any label").checked(picked.is_empty()).on_click(clear));
        present.iter().fold(menu, |menu, label| {
            let flip = {
                let (view, label) = (view.clone(), label.clone());
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let label = label.clone();
                    let _ = view.update(cx, |this, cx| {
                        this.change_filter(
                            |f| match f.labels.iter().position(|l| *l == label) {
                                Some(at) => {
                                    f.labels.remove(at);
                                }
                                None => f.labels.push(label),
                            },
                            cx,
                        );
                    });
                }
            };
            menu.item(
                PopupMenuItem::new(format!("{}={}", label.0, label.1)).checked(picked.contains(label)).on_click(flip),
            )
        })
    })
}

/// How the list is grouped: under apps of apps, by service, or not.
fn group_picker(current: GroupBy, view: WeakEntity<ArgoView>) -> impl IntoElement {
    Button::new("argo-group-by").small().ghost().icon(Lucide::Layers).label(current.label()).dropdown_menu(
        move |menu, _, _| {
            [GroupBy::Parent, GroupBy::Service, GroupBy::None].into_iter().fold(menu, |menu, by| {
                let view = view.clone();
                menu.item(PopupMenuItem::new(by.label()).checked(by == current).on_click(
                    move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                        let _ = view.update(cx, |this, cx| this.set_group_by(by, cx));
                    },
                ))
            })
        },
    )
}

/// What a row's menu offers beyond Refresh and Sync, from the Application as listed. Its
/// dialog reads the Application again and says what Argo CD would refuse.
#[derive(Clone, Copy)]
struct Offers {
    /// An operation runs: it can be terminated.
    terminate: bool,
    /// Auto-sync is off and a deployment precedes the newest: Argo CD allows a rollback.
    roll_back: bool,
}

impl Offers {
    fn of(app: &Summary) -> Self {
        Self { terminate: app.in_progress, roll_back: !app.automated && app.history.len() >= 2 }
    }
}

/// A row's menu: open it in Argo CD's web UI or where it deploys,
/// pick it with the Applications under it (`group`: its index, when it made some), refresh,
/// hard refresh, sync, terminate, roll back. Its actions find the Application again by its
/// key: the list may have been read again since it was drawn.
fn row_menu(
    k: &Key,
    has_destination: bool,
    offers: Offers,
    group: Option<usize>,
    view: WeakEntity<ArgoView>,
) -> impl IntoElement {
    let k = k.clone();
    let id = SharedString::from(format!("argo-menu-{}-{}-{}", k.0, k.1, k.2));
    Button::new(id).xsmall().ghost().icon(Lucide::Ellipsis).accessibility_label("More actions").dropdown_menu(
        move |menu: PopupMenu, _, _| {
            let open = {
                let (view, k) = (view.clone(), k.clone());
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let _ = view.update(cx, |this, cx| this.open(&k, cx));
                }
            };
            let board = {
                let (view, k) = (view.clone(), k.clone());
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let _ = view.update(cx, |this, cx| this.open_dashboard(&k, cx));
                }
            };
            let destination = {
                let (view, k) = (view.clone(), k.clone());
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    let _ = view.update(cx, |this, cx| this.open_destination(&k, cx));
                }
            };
            let pick_group = {
                let view = view.clone();
                move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                    if let Some(ix) = group {
                        let _ = view.update(cx, |this, cx| this.pick_group(ix, false, cx));
                    }
                }
            };
            let act = |action: ArgoAction| {
                let (view, k) = (view.clone(), k.clone());
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    let _ = view.update(cx, |this, cx| this.act(&k, action, window, cx));
                }
            };
            let open_item = PopupMenuItem::new("Open in Argo CD").icon(Lucide::ExternalLink);
            let menu = menu
                .item(PopupMenuItem::new("Dashboard").icon(Lucide::LayoutDashboard).on_click(board))
                .item(open_item.on_click(open))
                .item(
                    PopupMenuItem::new("Open destination namespace")
                        .icon(Lucide::FolderOpen)
                        .disabled(!has_destination)
                        .on_click(destination),
                );
            let menu = if group.is_some() {
                menu.item(PopupMenuItem::new("Pick the group").icon(Lucide::ListChecks).on_click(pick_group))
            } else {
                menu
            };
            menu.separator()
                .item(PopupMenuItem::new("Refresh…").icon(Lucide::RefreshCw).on_click(act(ArgoAction::Refresh)))
                .item(
                    PopupMenuItem::new("Hard refresh…")
                        .icon(Lucide::RefreshCcwDot)
                        .on_click(act(ArgoAction::HardRefresh)),
                )
                .item(PopupMenuItem::new("Sync…").icon(Lucide::GitBranch).on_click(act(ArgoAction::Sync)))
                .item(
                    PopupMenuItem::new("Terminate…")
                        .icon(Lucide::CircleStop)
                        .disabled(!offers.terminate)
                        .on_click(act(ArgoAction::Terminate)),
                )
                .item(
                    PopupMenuItem::new("Roll back to the previous deployment…")
                        .icon(Lucide::Undo2)
                        .disabled(!offers.roll_back)
                        .on_click(act(ArgoAction::Rollback(None))),
                )
        },
    )
}

impl Render for ArgoView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.journal.is_some() {
            return v_flex().size_full().p_2().child(self.journal_view(cx));
        }
        if self.sync.as_ref().is_some_and(|g| g.shown) {
            return v_flex().size_full().p_2().child(self.sync_view(cx));
        }
        if self.grouped.is_some() {
            return v_flex().size_full().p_2().child(self.grouped_view(cx));
        }
        if self.dashboard.is_some() {
            return v_flex().size_full().p_2().child(self.dashboard_view(cx));
        }
        let theme = cx.theme().clone();
        let body: AnyElement = match &self.rows {
            None => div().p_4().text_color(theme.muted_foreground).child("Reading Argo CD…").into_any_element(),
            Some(rows) if rows.fleet.servers.is_empty() => self.no_server(&rows.fleet, cx),
            Some(rows) => {
                let count = self.lines.len();
                let summary = rows.fleet.summary((self.filter != Filter::default()).then_some(self.kept));
                v_flex()
                    .size_full()
                    .child(div().px_2().pb_2().text_sm().child(summary))
                    .child(div().pb_1().child(self.selection_bar(cx)))
                    .child(self.header(cx))
                    .child(
                        // Only the rows on screen are laid out: an Argo CD can hold hundreds of Applications.
                        uniform_list(
                            "argo-apps",
                            count,
                            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                range
                                    .filter_map(|i| {
                                        this.lines.get(i).copied().map(|line| match line {
                                            Line::App(s) => this.row(i, s, cx),
                                            Line::Service(g) => this.service_row(i, g, cx),
                                        })
                                    })
                                    .collect()
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
                        .child("Some Argo CD servers could not be read:")
                        .children(errors.into_iter().map(|e| div().child(e))),
                )
            })
            .child(div().flex_1().min_h_0().child(body))
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's own `test` attribute would shadow the standard one.
    use super::{Filter, Line, Shown, ancestors, favorites_first, group_tone, service_lines, shown_rows, tree_order};
    use crate::k8s::argo::Summary;
    use crate::status::Tone;

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

    #[test]
    fn a_service_shows_a_heading_over_its_environments() {
        let groups =
            vec![("api".to_string(), vec![1, 3]), ("solo".to_string(), vec![4]), ("web".to_string(), vec![0, 2])];
        let app = |ix, depth| Line::App(Shown { ix, depth, context_only: false });
        let all = [true; 5];
        assert_eq!(
            service_lines(&groups, &all, |_| false),
            [Line::Service(0), app(1, 1), app(3, 1), app(4, 0), Line::Service(2), app(0, 1), app(2, 1)]
        );
        // Folded: the heading alone. Filtered: only what is kept, and no heading over nothing.
        assert_eq!(service_lines(&groups, &all, |g| g == 0)[..2], [Line::Service(0), app(4, 0)]);
        let keeps = [false, false, false, true, false];
        assert_eq!(service_lines(&groups, &keeps, |_| false), [Line::Service(0), app(3, 1)]);
    }

    #[test]
    fn a_group_is_as_red_as_its_worst() {
        assert_eq!(group_tone([Tone::Good, Tone::Bad, Tone::Busy].into_iter()), Tone::Bad);
        assert_eq!(group_tone([Tone::Good, Tone::Busy].into_iter()), Tone::Busy);
        assert_eq!(group_tone([Tone::Good, Tone::Good].into_iter()), Tone::Good);
        assert_eq!(group_tone([Tone::Good, Tone::Done].into_iter()), Tone::Done);
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
        let keeps = |f: &Filter, a: &Summary| f.keeps(a, "prod", "web shop prod", false);
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
        // Each word on its own.
        assert!(keeps(&search("prod web"), &fine) && !keeps(&search("prod billing"), &fine));
        // Favorites only: the others go, whatever else is picked.
        let favorites = Filter { favorites: true, ..Default::default() };
        assert!(favorites.keeps(&fine, "prod", "web", true));
        assert!(!favorites.keeps(&fine, "prod", "web", false));
        assert!(Filter::default().keeps(&fine, "prod", "web", false));
    }

    /// A favorite's whole group comes first: under its app of apps, or its service's heading,
    /// even folded; the others follow in their order.
    #[test]
    fn favorites_and_their_groups_come_first() {
        // 0 made 1 and 2; 3 alone; 4 made 5.
        let parents = [None, Some(0), Some(0), None, None, Some(4)];
        let order = tree_order(&parents, by_index);
        let shown: Vec<Line> = shown_rows(&order, &parents, &[true; 6], |_| false).into_iter().map(Line::App).collect();
        let ix = |lines: Vec<Line>| {
            lines.into_iter().filter_map(|l| if let Line::App(s) = l { Some(s.ix) } else { None }).collect::<Vec<_>>()
        };
        let fav = |f: fn(usize) -> bool| move |l: &Line| matches!(l, Line::App(s) if f(s.ix));
        assert_eq!(ix(favorites_first(shown.clone(), fav(|i| i == 5))), [4, 5, 0, 1, 2, 3]);
        assert_eq!(ix(favorites_first(shown.clone(), fav(|i| i == 3 || i == 1))), [0, 1, 2, 3, 4, 5]);
        assert_eq!(ix(favorites_first(shown, fav(|_| false))), [0, 1, 2, 3, 4, 5]);
        // Services: the heading moves with its Applications.
        let lines = vec![
            Line::Service(0),
            Line::App(Shown { ix: 0, depth: 1, context_only: false }),
            Line::App(Shown { ix: 1, depth: 0, context_only: false }),
            Line::Service(1),
        ];
        let moved = favorites_first(lines, |l| matches!(l, Line::Service(1)));
        assert_eq!(moved[0], Line::Service(1));
        assert_eq!(moved[1], Line::Service(0));
    }

    #[test]
    fn sync_health_auto_sync_and_labels_narrow() {
        let mut a = app("OutOfSync", "Progressing", None, "shop");
        a.automated = true;
        a.labels = vec![("env".into(), "Prod".into()), ("team".into(), "web".into())];
        let keeps = |f: Filter| f.keeps(&a, "prod", "web", false);
        assert!(keeps(Filter { sync: Some("OutOfSync".into()), ..Default::default() }));
        assert!(!keeps(Filter { sync: Some("Synced".into()), ..Default::default() }));
        assert!(!keeps(Filter { health: Some("Healthy".into()), ..Default::default() }));
        assert!(keeps(Filter { auto_sync: Some(true), ..Default::default() }));
        assert!(!keeps(Filter { auto_sync: Some(false), ..Default::default() }));
        let label = |k: &str, v: &str| (k.to_string(), v.to_string());
        assert!(keeps(Filter { labels: vec![label("env", "Prod"), label("team", "web")], ..Default::default() }));
        assert!(!keeps(Filter { labels: vec![label("env", "prod")], ..Default::default() }));
        // Typed in the search, in lowercase: key=value, or key= for any value.
        let search = |q: &str| Filter { query: q.into(), ..Default::default() };
        assert!(keeps(search("env=prod")) && keeps(search("team= web")) && keeps(search("env=")));
        assert!(!keeps(search("env=staging")) && !keeps(search("owner=")));
    }
}

/// Rules that hold for any parents, however tangled.
#[cfg(test)]
mod properties {
    use super::{Line, favorites_first, service_lines, shown_rows, tree_order};
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

        /// Favorites first moves whole groups: every line stays once, under the same heading
        /// or Application, and the groups with a favorite come before the others.
        #[test]
        fn favorites_first_keeps_every_group_whole(
            parents in proptest::collection::vec(proptest::option::of(0usize..12), 0..12),
            favs in proptest::collection::btree_set(0usize..12, 0..4),
        ) {
            let order = tree_order(&parents, |a, b| a.cmp(&b));
            let shown: Vec<Line> = shown_rows(&order, &parents, &vec![true; parents.len()], |_| false).into_iter().map(Line::App).collect();
            let is_fav = |l: &Line| matches!(l, Line::App(s) if favs.contains(&s.ix));
            let moved = favorites_first(shown.clone(), is_fav);
            let ixs = |lines: &[Line]| {
                let mut v: Vec<usize> = lines.iter().filter_map(|l| if let Line::App(s) = l { Some(s.ix) } else { None }).collect();
                v.sort_unstable();
                v
            };
            prop_assert_eq!(ixs(&shown), ixs(&moved));
            let depth = |l: &Line| if let Line::App(s) = l { s.depth } else { 0 };
            for pair in moved.windows(2) {
                prop_assert!(depth(&pair[1]) <= depth(&pair[0]) + 1);
            }
            let mut groups: Vec<bool> = Vec::new();
            for l in &moved {
                if depth(l) == 0 || groups.is_empty() {
                    groups.push(is_fav(l));
                } else if is_fav(l) {
                    *groups.last_mut().unwrap() = true;
                }
            }
            let n = groups.iter().take_while(|g| **g).count();
            prop_assert!(groups[n..].iter().all(|g| !g));
        }

        /// Grouped by service, every Application kept is shown once, and none other.
        #[test]
        fn every_kept_application_is_in_its_service_once(
            services in proptest::collection::vec(0usize..5, 0..20),
            keeps in proptest::collection::vec(any::<bool>(), 20),
        ) {
            let names: Vec<String> = services.iter().map(|s| format!("svc{s}")).collect();
            let groups = crate::k8s::argo::grouping::by_service(&names);
            let lines = service_lines(&groups, &keeps[..names.len()], |_| false);
            let mut shown: Vec<usize> = lines.iter().filter_map(|l| match l { Line::App(s) => Some(s.ix), Line::Service(_) => None }).collect();
            shown.sort_unstable();
            let want: Vec<usize> = (0..names.len()).filter(|&i| keeps[i]).collect();
            prop_assert_eq!(shown, want);
        }
    }
}
