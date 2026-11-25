//! The Argo CD page: the environments of the Argo CD API servers (the argocd CLI's, and those
//! set in Settings → Argo CD), a row each (an app of apps and the Applications it made, see
//! `k8s::argo::environments`), grouped by project then type (prod, staging…), with what is
//! wrong, the images they run, their last sync and where they deploy (see `envs`). A row
//! unfolds its Applications. Opening one shows the dashboard of its service, a tab per
//! environment (see `dashboard`); a menu opens it in Argo CD's web UI, refreshes or syncs it. Applications picked together (`grouped`) are diffed at once, the same change in
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
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use crate::details::{ArgoAction, OpenOn, confirm_api_action};
use crate::k8s;
use crate::k8s::argo::diagnosis::Res;
use crate::k8s::argo::environments::EnvTypes;
use crate::k8s::argo::grouping::AppKey;
use crate::k8s::argo::windows::{NextSync, Planner};
use crate::k8s::argo::{self, ApiRead, Fleet, FleetApp, Summary, api};
use crate::overview::column_cell as column;
use crate::status::Tone;
use envs::{Env, Filter, GroupBy, Heading, Line};

mod dashboard;
pub(crate) mod envs;
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

/// The page's state as Settings keeps it: folds and unfolded environments sorted, so the
/// same state is always saved the same.
fn page_state(
    filter: &Filter,
    group_by: GroupBy,
    folded: &HashSet<String>,
    expanded: &HashSet<Key>,
) -> crate::settings::ArgoPage {
    let mut folded: Vec<String> = folded.iter().cloned().collect();
    folded.sort();
    let mut expanded: Vec<Key> = expanded.iter().cloned().collect();
    expanded.sort();
    crate::settings::ArgoPage { filter: filter.clone(), group_by, folded, expanded }
}

// MARK: Rows

/// A read of every Application, with its environments and what the page needs worked out
/// once (off the UI thread).
struct Rows {
    fleet: Fleet,
    types: Arc<EnvTypes>,
    envs: Vec<Env>,
    /// Each Application's environment.
    env_of: Vec<usize>,
    /// The environments by project, type and name; and by name.
    by_group: Vec<usize>,
    by_name: Vec<usize>,
    /// Every Application by name: the order picks are listed in.
    flat: Vec<usize>,
    /// The labels of each environment's Applications, for `key=value` in the search.
    labels: Vec<Vec<(String, String)>>,
    projects: Vec<String>,
    /// The types some environment is of, in their order.
    kinds: Vec<usize>,
    /// How many environments need a look, are out of sync, Degraded or failed, and running.
    counts: [usize; 4],
    /// Whether the clusters, and the namespaces, differ enough between rows to be shown.
    show_cluster: bool,
    show_namespace: bool,
    /// When each Application can sync next, as of `now`.
    next: Vec<NextSync>,
    now: jiff::Timestamp,
}

impl Rows {
    fn new(fleet: Fleet, types: Arc<EnvTypes>, plan: &mut Planner) -> Self {
        let now = plan.now();
        let apps = &fleet.apps;
        let parents: Vec<Option<usize>> = apps.iter().map(|a| a.parent).collect();
        let servers: Vec<String> = apps.iter().map(|a| a.server.name()).collect();
        let sort_key = |i: usize| (&apps[i].app.name, &servers[i], &apps[i].app.namespace);
        let cmp = |a: usize, b: usize| sort_key(a).cmp(&sort_key(b));
        let order = tree_order(&parents, cmp);
        let envs = envs::environments(apps, &parents, &order, &types);
        let mut env_of = vec![0; apps.len()];
        for (e, env) in envs.iter().enumerate() {
            for (i, _) in &env.members {
                env_of[*i] = e;
            }
        }
        let mut by_group: Vec<usize> = (0..envs.len()).collect();
        by_group.sort_by(|&a, &b| {
            let (a, b) = (&envs[a], &envs[b]);
            (&a.project, a.kind, &a.name).cmp(&(&b.project, b.kind, &b.name))
        });
        let mut by_name: Vec<usize> = (0..envs.len()).collect();
        by_name.sort_by(|&a, &b| (&envs[a].name, &envs[a].project).cmp(&(&envs[b].name, &envs[b].project)));
        let mut flat: Vec<usize> = (0..apps.len()).collect();
        flat.sort_by(|a, b| cmp(*a, *b));
        let labels = envs
            .iter()
            .map(|e| {
                let mut l: Vec<(String, String)> =
                    e.members.iter().flat_map(|(i, _)| apps[*i].app.labels.iter().cloned()).collect();
                l.sort();
                l.dedup();
                l
            })
            .collect();
        let mut projects: Vec<String> = envs.iter().map(|e| e.project.clone()).collect();
        projects.sort();
        projects.dedup();
        let mut kinds: Vec<usize> = envs.iter().map(|e| e.kind).collect();
        kinds.sort_unstable();
        kinds.dedup();
        let count = |f: &dyn Fn(&Env) -> bool| envs.iter().filter(|e| f(e)).count();
        let counts = [
            count(&|e| e.state.needs_look()),
            count(&|e| e.state.out_of_sync),
            count(&|e| e.state.degraded || e.state.failed),
            count(&|e| e.state.running),
        ];
        let show_cluster = !envs::uniform(envs.iter().map(|e| e.cluster.as_str()));
        let own_namespace = |e: &Env| if e.namespace.is_empty() || e.namespace == e.name { "" } else { "other" };
        let show_namespace = !envs::uniform(envs.iter().map(own_namespace));
        let next = apps.iter().map(|a| fleet.next_sync(a, plan)).collect();
        Self {
            fleet,
            types,
            envs,
            env_of,
            by_group,
            by_name,
            flat,
            labels,
            projects,
            kinds,
            counts,
            show_cluster,
            show_namespace,
            next,
            now,
        }
    }

    /// The environments in the order shown, each with the headings it is under: grouped by
    /// `by`, those with a favorite (`favorite`) first in their group.
    fn listed(&self, by: GroupBy, favorite: impl Fn(usize) -> bool) -> Vec<(Vec<String>, usize)> {
        let order = if by == GroupBy::None { &self.by_name } else { &self.by_group };
        let mut groups: Vec<Vec<(Vec<String>, usize)>> = Vec::new();
        for &e in order {
            let path = by.path(&self.envs[e], &self.types);
            match groups.last_mut() {
                Some(g) if g[0].0 == path => g.push((path, e)),
                _ => groups.push(vec![(path, e)]),
            }
        }
        groups.into_iter().flat_map(|g| crate::favorites::favorites_first(g, |(_, e)| favorite(*e))).collect()
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
    /// The headings folded (`envs::heading_key`), and the environments unfolded (by their
    /// root).
    folded: HashSet<String>,
    expanded: HashSet<Key>,
    /// Saves the page's state once typing in the search pauses.
    remember: Option<Task<()>>,
    /// The lines on screen and their headings, worked out when the data, the filter or a
    /// fold changes.
    lines: Vec<Line>,
    headings: Vec<Heading>,
    /// How many environments the filter keeps, and which.
    kept: usize,
    env_keeps: Vec<bool>,
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
    /// Shows a made-up example (the developer tools'): the feeds are not shown.
    example: bool,
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
        // The page as it was last left (Settings' `argocd_page`).
        let page = crate::settings::with(|s| s.argocd_page.clone());
        let query = page.filter.query.clone();
        let search = cx.new(|cx| {
            let mut st = InputState::new(window, cx).placeholder("Search environments, image tags, or key=value…");
            st.set_value(query, window, cx);
            st
        });
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
                    let query = state.read(cx).value().to_lowercase();
                    if query != this.filter.query {
                        this.filter.query = query;
                        this.refilter(cx);
                        this.remember_soon(cx);
                        cx.notify();
                    }
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
            filter: page.filter,
            group_by: page.group_by,
            search,
            folded: page.folded.into_iter().collect(),
            expanded: page.expanded.into_iter().collect(),
            remember: None,
            lines: Vec::new(),
            headings: Vec::new(),
            kept: 0,
            env_keeps: Vec::new(),
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
            example: false,
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
        if self.example {
            return;
        }
        let feeds: Vec<(Arc<api::Endpoint>, Arc<Mutex<api::FeedState>>)> =
            self.watches.iter().map(|w| (w.endpoint.clone(), w.state.clone())).collect();
        let connected: Vec<String> = self.clusters.iter().map(|(n, _)| n.clone()).collect();
        let config_errors = self.config_errors.clone();
        let (types, servers) = crate::settings::with(|s| (s.argocd_env_types.clone(), s.argocd_servers.clone()));
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
                Rows::new(fleet, Arc::new(EnvTypes::parse(&types)), &mut plan)
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

    /// Show the made-up environments of the developer tools' example: nothing is read.
    #[cfg(feature = "dev")]
    pub fn demo_list(&mut self, cx: &mut Context<Self>) {
        let now = jiff::Timestamp::now();
        let endpoint = Arc::new(api::Endpoint {
            server: argo::server::Server::parse("https://argocd.example.com").expect("a valid address"),
            cli_context: None,
            in_settings: true,
            insecure: false,
            login: Err("a made-up example".into()),
            on: true,
            settings_token: false,
        });
        let mut state = api::FeedState { listed: true, live: true, ..Default::default() };
        for v in argo::environments::example_apps(now) {
            let a = Arc::new(api::ApiApp { summary: Summary::of(&v), value: v });
            state.apps.insert((a.summary.namespace.clone(), a.summary.name.clone()), a);
        }
        let fleet = argo::from_api(vec![ApiRead { endpoint, state }], &[], &[], &Default::default());
        let types = crate::settings::with(|s| s.argocd_env_types.clone());
        self.example = true;
        self.rows = Some(Rows::new(fleet, Arc::new(EnvTypes::parse(&types)), &mut Planner::new(now)));
        self.refilter(cx);
        cx.notify();
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
            self.headings.clear();
            self.kept = 0;
            self.env_keeps.clear();
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
        let env_starred = |e: usize| rows.envs[e].members.iter().any(|(i, _)| starred[*i]);
        let env_keeps: Vec<bool> = rows
            .envs
            .iter()
            .enumerate()
            .map(|(e, env)| {
                let labels: Vec<&(String, String)> = rows.labels[e].iter().collect();
                self.filter.keeps(env, rows.types.name(env.kind), env_starred(e), &labels)
            })
            .collect();
        self.kept = env_keeps.iter().filter(|k| **k).count();
        let listed = rows.listed(self.group_by, env_starred);
        let expanded = &self.expanded;
        let (lines, headings) = envs::lines(
            &listed,
            &env_keeps,
            &self.folded,
            |e| {
                let env = &rows.envs[e];
                (env.members.len() > 1 && expanded.contains(&key(&apps[env.root()]))).then_some(env.members.len())
            },
            |e| rows.envs[e].state.needs_look(),
            |e| rows.envs[e].tone,
        );
        (self.lines, self.headings) = (lines, headings);
        self.keeps = rows.env_of.iter().map(|&e| env_keeps[e]).collect();
        self.env_keeps = env_keeps;
        self.recount_picked();
    }

    fn change_filter(&mut self, change: impl FnOnce(&mut Filter), cx: &mut Context<Self>) {
        change(&mut self.filter);
        self.changed(cx);
    }

    fn set_group_by(&mut self, by: GroupBy, cx: &mut Context<Self>) {
        self.group_by = by;
        self.changed(cx);
    }

    /// The filter, grouping or a fold changed: show it, and remember it.
    fn changed(&mut self, cx: &mut Context<Self>) {
        self.refilter(cx);
        self.remember();
        cx.notify();
    }

    /// Keep how the page is now (Settings' `argocd_page`), to open it so next time.
    fn remember(&mut self) {
        if self.example {
            return;
        }
        self.remember = None;
        let page = page_state(&self.filter, self.group_by, &self.folded, &self.expanded);
        crate::settings::update(|s| s.argocd_page = page);
    }

    /// `remember`, once the search has not changed for a second (not a write per key).
    fn remember_soon(&mut self, cx: &mut Context<Self>) {
        self.remember = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(Duration::from_secs(1)).await;
            let _ = this.update(cx, |this, _| this.remember());
        }));
    }

    /// Star or unstar the Application (kept in favorites.json; the page sorts again when the
    /// favorites change). Unstarring removes it under any name of its Argo CD.
    fn toggle_favorite(&mut self, k: &Key, cx: &mut Context<Self>) {
        let Some(f) = self.find(k).map(favorite) else { return };
        self.favorites.update(cx, |favs, cx| favs.toggle(f, cx));
    }

    /// Fold or unfold a heading.
    fn toggle_heading(&mut self, heading: String, cx: &mut Context<Self>) {
        if !self.folded.remove(&heading) {
            self.folded.insert(heading);
        }
        self.changed(cx);
    }

    /// Show or hide an environment's Applications (`k`: its root).
    fn toggle_expanded(&mut self, k: Key, cx: &mut Context<Self>) {
        if !self.expanded.remove(&k) {
            self.expanded.insert(k);
        }
        self.changed(cx);
    }

    /// Fold every heading, or unfold them all (and the environments).
    fn toggle_all(&mut self, cx: &mut Context<Self>) {
        if self.folded.is_empty() && self.expanded.is_empty() {
            self.folded = self.headings.iter().filter(|h| h.depth == 0).map(|h| h.key.clone()).collect();
        } else {
            self.folded.clear();
            self.expanded.clear();
        }
        self.changed(cx);
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

    /// The star of the Application `k` on a row (`i`): shown on hover, or when it is one.
    fn star(&self, i: usize, k: &Key, starred: bool, cx: &Context<Self>) -> Stateful<Div> {
        let (view, k) = (cx.entity().downgrade(), k.clone());
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
    }

    /// A heading: a project, or a type in it, with how many environments the filter keeps
    /// under it and how many need a look; a click folds it.
    fn heading_row(&self, i: usize, h: &Heading, cx: &Context<Self>) -> AnyElement {
        let folded = self.folded.contains(&h.key);
        let flip = {
            let (key, view) = (h.key.clone(), cx.entity().downgrade());
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let key = key.clone();
                let _ = view.update(cx, |this, cx| this.toggle_heading(key, cx));
            }
        };
        let cx: &App = cx;
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        h_flex()
            .id(("argo-heading", i))
            .w_full()
            .gap_2()
            .px_2()
            .pl(px(8. + INDENT * h.depth as f32))
            .py_0p5()
            .text_sm()
            .border_b_1()
            .border_color(theme.border)
            .when(h.depth == 0, |d| d.bg(theme.muted))
            .cursor_pointer()
            .on_click(flip)
            .child(
                Icon::new(if folded { Lucide::ChevronRight } else { Lucide::ChevronDown }).size_4().text_color(muted),
            )
            .child(div().size(px(7.)).flex_none().rounded_full().bg(h.tone.color(cx)))
            .child(
                div()
                    .when(h.depth == 0, |d| d.font_weight(FontWeight::SEMIBOLD))
                    .when(h.depth > 0, |d| d.font_weight(FontWeight::MEDIUM))
                    .whitespace_nowrap()
                    .child(h.title.clone()),
            )
            .child(div().text_xs().text_color(muted).child(crate::plural::count(h.envs, "environment")))
            .when(h.looks > 0, |d| {
                d.child(div().text_xs().text_color(theme.warning).child(format!("{} to look at", h.looks)))
            })
            .into_any_element()
    }

    /// An environment: its worst color, name and how many Applications, its star (its
    /// root's), what is wrong, its images, its last sync and where it deploys.
    fn env_row(&self, i: usize, e: usize, cx: &Context<Self>) -> AnyElement {
        let Some(rows) = &self.rows else { return div().into_any_element() };
        let Some(env) = rows.envs.get(e) else { return div().into_any_element() };
        let apps = &rows.fleet.apps;
        let root = &apps[env.root()];
        let k = key(root);
        let view = cx.entity().downgrade();
        let several = env.members.len() > 1;
        let expanded = self.expanded.contains(&k);
        let open = {
            let k = key(&apps[env.main]);
            cx.listener(move |this, _, _, cx| this.open_dashboard(&k, cx))
        };
        let pick = {
            let view = view.clone();
            Checkbox::new(("argo-pick-env", i)).checked(self.env_picked(e)).on_click(move |_: &bool, _, cx| {
                let _ = view.update(cx, |this, cx| this.pick_env(e, cx));
            })
        };
        let toggle = {
            let (view, k) = (view.clone(), k.clone());
            Button::new(("argo-expand", i))
                .xsmall()
                .ghost()
                .icon(if expanded { Lucide::ChevronDown } else { Lucide::ChevronRight })
                .accessibility_label(if expanded { "Hide its Applications" } else { "Show its Applications" })
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.toggle_expanded(k.clone(), cx));
                })
        };
        let starred = env.members.iter().any(|(m, _)| self.starred.get(*m).copied().unwrap_or_default());
        let star = self.star(i, &k, starred, cx);
        let cx: &App = cx;
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let badges = state_badges(env.state, cx);
        let (shown, more, tip) = image_summary(env);
        let images = h_flex()
            .id(("argo-images", i))
            .gap_2()
            .overflow_hidden()
            .children(shown.into_iter().map(|(component, tag)| {
                h_flex()
                    .gap_1()
                    .flex_none()
                    .child(div().text_xs().text_color(muted).child(component))
                    .child(div().font_family(theme.mono_font_family.clone()).text_xs().child(tag))
            }))
            .when(more > 0, |d| d.child(div().flex_none().text_xs().text_color(muted).child(format!("+{more}"))))
            .when(!tip.is_empty(), |d| {
                d.tooltip(move |w, cx| component::tooltip::Tooltip::new(tip.clone()).build(w, cx))
            });
        let last_sync = env.last_sync.map(|t| format!("{} ago", k8s::age(t)));
        // The namespace only where it isn't the environment's own name.
        let namespace = if rows.show_namespace && env.namespace != env.name { env.namespace.as_str() } else { "" };
        let destination = match (rows.show_cluster, namespace.is_empty()) {
            (true, false) => argo::destination_text(&env.cluster, namespace),
            (true, true) => env.cluster.clone(),
            (false, _) => namespace.to_string(),
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
                    .w(px(NAME - 48.))
                    .flex_none()
                    .gap_1p5()
                    .overflow_hidden()
                    .child(div().size(px(7.)).flex_none().rounded_full().bg(env.tone.color(cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(theme.blue)
                            .child(env.name.clone()),
                    )
                    .when(several, |d| {
                        d.child(div().flex_none().text_xs().text_color(muted).child(env.members.len().to_string()))
                    })
                    .child(star),
            )
            .child(h_flex().w(px(BADGES)).flex_none().gap_1().overflow_hidden().children(badges))
            .child(div().flex_1().min_w(px(120.)).overflow_hidden().child(images))
            .child(column(90., false).text_color(muted).children(last_sync))
            .when(rows.show_cluster || rows.show_namespace, |d| {
                d.child(column(220., true).text_color(muted).child(destination))
            })
            .when(self.several_sources(), |d| d.child(column(150., true).text_color(muted).child(root.server.name())));
        h_flex()
            .id(("argo-row", i))
            .group(SharedString::from(format!("argo-row-{i}")))
            .w_full()
            .gap_1()
            .px_2()
            .pl(px(8. + INDENT * self.group_depth() as f32))
            .py_0p5()
            .text_sm()
            .border_b_1()
            .border_color(theme.border)
            .hover(|s| s.bg(theme.list_hover))
            .child(div().w(px(20.)).flex_none().child(pick))
            .child(div().w(px(20.)).flex_none().when(several, |d| d.child(toggle)))
            .child(opener)
            .child(div().w(px(MENU)).flex_none().child(row_menu(
                &k,
                root.on.is_some() && !root.dest.namespace.is_empty(),
                Offers::of(&root.app),
                several.then_some(e),
                view,
            )))
            .into_any_element()
    }

    /// How deep environments sit under the headings.
    fn group_depth(&self) -> usize {
        match self.group_by {
            GroupBy::Type => 2,
            GroupBy::Project => 1,
            GroupBy::None => 0,
        }
    }

    /// One Application of an unfolded environment (`e`), at position `m` of its members: its
    /// name under the environment's, sync, health, revision, next and last sync.
    fn app_row(&self, i: usize, e: usize, m: usize, cx: &Context<Self>) -> AnyElement {
        let Some(rows) = &self.rows else { return div().into_any_element() };
        let Some(&(ix, depth)) = rows.envs.get(e).and_then(|env| env.members.get(m)) else {
            return div().into_any_element();
        };
        let env = &rows.envs[e];
        let a = &rows.fleet.apps[ix];
        let app = &a.app;
        let k = key(a);
        let view = cx.entity().downgrade();
        let open = {
            let k = k.clone();
            cx.listener(move |this, _, _, cx| this.open_dashboard(&k, cx))
        };
        let pick = {
            let (view, k) = (view.clone(), k.clone());
            Checkbox::new(("argo-pick", i)).checked(self.selected.contains(&k)).on_click(move |_: &bool, _, cx| {
                let _ = view.update(cx, |this, cx| this.toggle_pick(k.clone(), cx));
            })
        };
        let starred = self.starred.get(ix).copied().unwrap_or_default();
        let star = self.star(i, &k, starred, cx);
        let cx: &App = cx;
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let indent = INDENT * (self.group_depth() + depth.min(6)) as f32;
        let label = if m == 0 {
            format!("{} (app of apps)", app.name)
        } else {
            envs::member_label(&env.name, &app.name).to_string()
        };
        let last_sync = app.operation.as_ref().map(|(phase, _, finished)| {
            let when = finished.as_deref().and_then(|t| t.parse::<jiff::Timestamp>().ok()).map(k8s::age);
            let text = match when {
                Some(when) => format!("{phase}, {when} ago"),
                None => phase.clone(),
            };
            div().text_color(argo::operation_tone(phase).color(cx)).child(text)
        });
        let next = &rows.next[ix];
        let next_color = match next {
            NextSync::At(_) => theme.warning,
            NextSync::NotSoon => theme.danger,
            NextSync::Now | NextSync::NoWindow | NextSync::Unknown(_) => muted,
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
                    .w(px((NAME - 48. - INDENT).max(120.)))
                    .flex_none()
                    .gap_1p5()
                    .overflow_hidden()
                    .child(div().size(px(7.)).flex_none().rounded_full().bg(argo::attention(app).color(cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .text_color(theme.blue)
                            .child(label),
                    )
                    .child(star),
            )
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
                column(110., true)
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .child(app.revision.clone().unwrap_or_default()),
            )
            .child(column(70., false).text_color(muted).child(if app.automated { "Auto" } else { "Manual" }))
            .child(column(110., true).text_color(next_color).child(next.text(rows.now)))
            .child(column(190., true).children(last_sync));
        h_flex()
            .id(("argo-row", i))
            .group(SharedString::from(format!("argo-row-{i}")))
            .w_full()
            .gap_1()
            .px_2()
            .pl(px(8. + indent))
            .py_0p5()
            .text_sm()
            .border_b_1()
            .border_color(theme.border)
            .hover(|s| s.bg(theme.list_hover))
            .child(div().w(px(20.)).flex_none().child(pick))
            .child(div().w(px(20.)).flex_none())
            .child(opener)
            .child(div().w(px(MENU)).flex_none().child(row_menu(
                &k,
                a.on.is_some() && !a.dest.namespace.is_empty(),
                Offers::of(app),
                None,
                view,
            )))
            .into_any_element()
    }

    fn header(&self, cx: &App) -> Div {
        let cell = |w: f32, shrinks: bool, text: &'static str| column(w, shrinks).child(text);
        let (cluster, namespace) = self.rows.as_ref().map_or((false, false), |r| (r.show_cluster, r.show_namespace));
        h_flex()
            .gap_3()
            .px_2()
            .py_1()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().w(px(NAME - 48. + 44.)).flex_none().pl(px(24.)).child("Environment"))
            .child(div().w(px(BADGES)).flex_none().child("State"))
            .child(div().flex_1().min_w(px(120.)).child("Images"))
            .child(cell(90., false, "Last sync"))
            .when(cluster || namespace, |d| d.child(cell(220., true, "Destination")))
            .when(self.several_sources(), |d| d.child(cell(150., true, "Argo CD")))
            // Where the rows have their menu (after a 4 px gap, the header's are 12): the columns
            // get the same room in both, so they shrink alike.
            .child(div().w(px(MENU - 8.)).flex_none())
    }

    /// The search, the state toggles with their counts, the project and type pickers, and the
    /// grouping.
    fn toolbar(&self, cx: &mut Context<Self>) -> Div {
        let [look, out_of_sync, degraded, running] = self.rows.as_ref().map_or([0; 4], |r| r.counts);
        let toggle = |id: &'static str, label: &str, n: usize, on: bool, flip: fn(&mut Filter)| {
            Button::new(id)
                .small()
                .when(on, ButtonVariants::primary)
                .when(!on, Button::outline)
                .label(format!("{label} ({n})"))
                .on_click(cx.listener(move |this, _, _, cx| this.change_filter(flip, cx)))
        };
        let projects = self.rows.as_ref().map_or_else(Vec::new, |r| r.projects.clone());
        let types: Vec<String> = self
            .rows
            .as_ref()
            .map_or_else(Vec::new, |r| r.kinds.iter().map(|&k| r.types.name(k).to_string()).collect());
        let view = cx.entity().downgrade();
        let fold_label = if self.folded.is_empty() && self.expanded.is_empty() { "Fold all" } else { "Unfold all" };
        let f = &self.filter;
        h_flex()
            .gap_2()
            .flex_wrap()
            .child(div().w(px(280.)).child(Input::new(&self.search).small().cleanable(true)))
            .child(toggle("argo-look", "Needs a look", look, f.needs_look, |f| f.needs_look = !f.needs_look))
            .child(toggle("argo-out-of-sync", "Out of sync", out_of_sync, f.out_of_sync, |f| {
                f.out_of_sync = !f.out_of_sync;
            }))
            .child(toggle("argo-degraded", "Degraded or failed", degraded, f.degraded, |f| f.degraded = !f.degraded))
            .child(toggle("argo-running", "Running", running, f.running, |f| f.running = !f.running))
            .child(toggle("argo-favorites", "Favorites", self.starred_count, f.favorites, |f| {
                f.favorites = !f.favorites;
            }))
            .child(picker("argo-project", "All projects", f.project.clone(), projects, view.clone(), |f, v| {
                f.project = v;
            }))
            .child(type_picker(f.kind.clone(), f.on_tag, types, view.clone()))
            .child(div().flex_1())
            .child(group_picker(self.group_by, view))
            .child(
                Button::new("argo-collapse")
                    .small()
                    .ghost()
                    .label(fold_label)
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_all(cx))),
            )
    }
}

/// The room the state badges take on a row.
const BADGES: f32 = 200.;

/// What is wrong with an environment, as badges: out of sync, Degraded, its sync failed,
/// one running.
fn state_badges(s: envs::State, cx: &App) -> Vec<Div> {
    let theme = cx.theme();
    let badge = |text: &'static str, color: Hsla| {
        div()
            .flex_none()
            .px_1p5()
            .rounded(px(4.))
            .border_1()
            .border_color(color.opacity(0.5))
            .text_xs()
            .text_color(color)
            .child(text)
    };
    let mut out = Vec::new();
    if s.failed {
        out.push(badge("Sync failed", theme.danger));
    }
    if s.degraded {
        out.push(badge("Degraded", theme.danger));
    }
    if s.out_of_sync {
        out.push(badge("OutOfSync", theme.warning));
    }
    if s.running {
        out.push(badge("Syncing", Tone::Busy.color(cx)));
    }
    out
}

/// An environment's images on its row: the product's first, at most `IMAGES_SHOWN` (component,
/// short tag; the newest when it runs several), how many more, and every one on hover.
fn image_summary(env: &Env) -> (Vec<(String, String)>, usize, String) {
    let product = env.images.iter().filter(|(own, _)| *own).count();
    let pool = if product > 0 { product } else { env.images.len() };
    let shown: Vec<(String, String)> = env
        .images
        .iter()
        .take(pool.min(envs::IMAGES_SHOWN))
        .map(|(_, i)| {
            let newest = i.tags.last().map_or_default(|t| envs::short_tag(t));
            let tag = if i.tags.len() > 1 { format!("{newest} +{}", i.tags.len() - 1) } else { newest };
            (i.component.clone(), tag)
        })
        .collect();
    let more = env.images.len() - shown.len();
    let tip = env
        .images
        .iter()
        .map(|(own, i)| format!("{}{}:{}", if *own { "" } else { "· " }, i.repo, i.tags.join(", ")))
        .collect::<Vec<_>>()
        .join("\n");
    (shown, more, tip)
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

/// The label of the type filter's "on a version tag" choice.
const ON_TAG: &str = "On a version tag";

/// The environment type to keep: one of `types`, or those whose product runs on version tags.
fn type_picker(
    current: Option<String>,
    on_tag: bool,
    types: Vec<String>,
    view: WeakEntity<ArgoView>,
) -> impl IntoElement {
    let label = match (&current, on_tag) {
        (Some(t), _) => t.clone(),
        (None, true) => ON_TAG.to_string(),
        (None, false) => "All types".to_string(),
    };
    Button::new("argo-type").small().outline().label(label).dropdown_menu(move |menu, _, _| {
        let pick = |kind: Option<String>, tag: bool| {
            let view = view.clone();
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                let kind = kind.clone();
                let _ = view.update(cx, |this, cx| {
                    this.change_filter(
                        |f| {
                            f.kind = kind;
                            f.on_tag = tag;
                        },
                        cx,
                    );
                });
            }
        };
        let menu = menu
            .scrollable(true)
            .max_h(px(420.))
            .item(PopupMenuItem::new("All types").checked(current.is_none() && !on_tag).on_click(pick(None, false)))
            .item(PopupMenuItem::new(ON_TAG).checked(current.is_none() && on_tag).on_click(pick(None, true)))
            .separator();
        types.iter().fold(menu, |menu, t| {
            menu.item(
                PopupMenuItem::new(t.clone())
                    .checked(current.as_ref() == Some(t))
                    .on_click(pick(Some(t.clone()), false)),
            )
        })
    })
}

/// How the list is grouped: by project then type, by project, or not.
fn group_picker(current: GroupBy, view: WeakEntity<ArgoView>) -> impl IntoElement {
    Button::new("argo-group-by").small().ghost().icon(Lucide::Layers).label(current.label()).dropdown_menu(
        move |menu, _, _| {
            [GroupBy::Type, GroupBy::Project, GroupBy::None].into_iter().fold(menu, |menu, by| {
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
/// pick its environment's Applications (`group`: the environment, when it has several), refresh,
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
                        let _ = view.update(cx, |this, cx| this.pick_env(ix, cx));
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
                menu.item(PopupMenuItem::new("Pick its Applications").icon(Lucide::ListChecks).on_click(pick_group))
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
                let kept = (self.filter != Filter::default()).then_some(self.kept);
                let summary = envs::summary(rows.envs.len(), kept, rows.fleet.apps.len(), &rows.fleet.reading);
                let type_errors = rows.types.errors.clone();
                v_flex()
                    .size_full()
                    .child(div().px_2().pb_2().text_sm().child(summary))
                    .children((!type_errors.is_empty()).then(|| {
                        div().px_2().pb_2().text_xs().text_color(theme.warning).child(format!(
                            "Environment types that don't read (Settings → Argo CD), left out: {}",
                            type_errors.join("; ")
                        ))
                    }))
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
                                            Line::Heading(h) => match this.headings.get(h) {
                                                Some(h) => this.heading_row(i, h, cx),
                                                None => div().into_any_element(),
                                            },
                                            Line::Env(e) => this.env_row(i, e, cx),
                                            Line::App(e, m) => this.app_row(i, e, m, cx),
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
    use super::tree_order;

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
    }
}

/// Rules that hold for any parents, however tangled.
#[cfg(test)]
mod properties {
    use super::tree_order;
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
        }
    }
}
