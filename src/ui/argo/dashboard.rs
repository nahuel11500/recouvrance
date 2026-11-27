//! The Application dashboard of the Argo CD page: a service's Applications, one tab per
//! environment, and the cards of the one shown (see `k8s::argo::dashboard`).
//!
//! Only the tab shown is read: the Application whole, its resource tree, its diff and its
//! commits, by Argo CD's API. It is read again when its row changes (the stream), every few
//! seconds while a sync runs, and on Refresh; a
//! newer read replaces the one running, and leaving the tab or the page drops it.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::Timestamp;

use super::{ArgoView, Key, key};
use crate::k8s;
use crate::k8s::argo::components;
use crate::k8s::argo::dashboard::{self, Cards, Problem};
use crate::k8s::argo::diagnosis::span_text;
use crate::k8s::argo::sync_diff::{self, Entry};
use crate::k8s::argo::{self, api, short_revision};
use crate::smartdiff::{self, ObjectKey, ResourceChange, SmartDiff};
use crate::status::Tone;
use crate::ui::selectable::selectable;
pub(super) use dialogs::prune_banner;
use dialogs::{Detail, detail_button};
pub(super) use env_page::sync_table;
use env_page::{DetailsTab, View, ViewFilters};
pub(super) use versions::changes_table;

mod columns;
mod dialogs;
mod env_page;
mod sync_dialog;
mod versions;

/// A status tile's width, before it grows to share its row: four side by side on a wide
/// window, two pairs on a narrow one.
const TILE: f32 = 230.;
/// A tile's width, at least.
const TILE_MIN: f32 = 190.;
/// The Versions and History panels' widths, before they grow to share their row.
const VERSIONS: f32 = 520.;
const HISTORY: f32 = 360.;
/// An environment's entry, at most.
const ENV_MAX: f32 = 360.;
/// How often the tab shown is read while a sync runs.
const RUNNING_EVERY: Duration = Duration::from_secs(3);
/// Problems listed in the Problems panel, one line each, before `+N more`.
const PROBLEMS_SHOWN: usize = 6;
/// Names listed in a line, at most.
const NAMES_SHOWN: usize = 4;
/// Lines of one resource's diff shown, at most.
const DIFF_LINES: usize = 400;
/// Lines of a summary shown, at most.
const GROUPS_SHOWN: usize = 50;
/// Changes said on hovering the Sync diff card, at most.
const CHANGES_TIP: usize = 12;
/// Resources listed, at most.
const ROWS_SHOWN: usize = 200;

/// A read of the manifests' diff of two deployments.
type DiffRead = std::pin::Pin<Box<dyn Future<Output = Result<Arc<SmartDiff>, String>>>>;

/// Two deployments of the history compared (by id, the older first).
struct Compare {
    from: i64,
    to: i64,
    /// What their sources asked for; None while worked out.
    sources: Option<Arc<SmartDiff>>,
    /// What Argo CD renders at their revisions; None while read.
    manifests: Option<Result<Arc<SmartDiff>, String>>,
    /// What to know about how they are rendered (a source with no revision recorded).
    notes: Vec<String>,
    /// Noise shown.
    noise: bool,
    /// The resources whose diff is unfolded.
    open: HashSet<ObjectKey>,
}

/// Where a smart diff shows on the dashboard: the comparison of two deployments, or what a
/// sync would change. Each has its own noise switch and resources unfolded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DiffAt {
    Compare,
    Sync,
}

impl DiffAt {
    fn id(self) -> &'static str {
        match self {
            Self::Compare => "argo-dash-cmp",
            Self::Sync => "argo-dash-sync",
        }
    }
}

/// The dashboard open on the page.
pub(super) struct Dashboard {
    /// The environment: its name, type and Argo CD server's name.
    title: String,
    kind: String,
    server: String,
    /// Its Applications, one section each: its workload first, its root last.
    members: Vec<Key>,
    tab: usize,
    /// The cards last read, and of which Application.
    cards: Option<(Key, Result<Arc<Cards>, String>)>,
    /// Why the last read failed, or gave only the list's copy, while older cards of the same
    /// Application read whole stay shown: one line over them.
    failed: Option<String>,
    /// The Application's row when it was read: a change of it reads it again.
    read_for: Option<(Key, argo::Summary)>,
    loading: bool,
    /// Its row changed while a read ran: read it again when that one ends.
    dirty: bool,
    updated: Option<Timestamp>,
    load: crate::latest::Latest,
    /// Reads it again while a sync runs, once the page is shown.
    tick: Option<Task<()>>,
    /// The parts unfolded: "history" (its list, on the History view), "images", "deployed";
    /// and the Resources view's categories folded (`"res-fold-<category>"`).
    open: HashSet<String>,
    /// The Sync diff dialog's components unfolded (`details`), and its resources whose YAML
    /// diff is unfolded.
    diff_details: HashSet<String>,
    sync_open: HashSet<ObjectKey>,
    /// Made-up environments and their cards (the developer tools' example): nothing is read.
    example: Option<Vec<(String, Arc<Cards>)>>,
    /// A deployment picked, waiting for a second to compare it with.
    picked: Option<i64>,
    /// The two deployments compared.
    compare: Option<Compare>,
    /// Their sources' diff, worked out off the UI thread.
    sources_load: crate::latest::Latest,
    /// Their manifests, read when the comparison opens.
    manifests_load: crate::latest::Latest,
    /// What the Versions card reads: earlier deployments' images, the registries' tags.
    versions: versions::State,
    /// The view shown under the action bar, the Details' tab, and the views' filters.
    view: View,
    details_tab: DetailsTab,
    filters: ViewFilters,
    /// The assistant can be asked (Claude Code or an API key is set up), as seen on opening.
    assistant: bool,
}

impl Dashboard {
    fn new(title: String, members: Vec<Key>, tab: usize) -> Self {
        Self {
            title,
            kind: String::new(),
            server: String::new(),
            members,
            tab,
            cards: None,
            failed: None,
            read_for: None,
            loading: false,
            dirty: false,
            updated: None,
            load: Default::default(),
            tick: None,
            open: HashSet::new(),
            diff_details: HashSet::new(),
            sync_open: HashSet::new(),
            example: None,
            picked: None,
            compare: None,
            sources_load: Default::default(),
            manifests_load: Default::default(),
            versions: Default::default(),
            view: View::default(),
            details_tab: DetailsTab::default(),
            filters: ViewFilters::default(),
            assistant: crate::assistant::backend().is_some(),
        }
    }

    /// Close the comparison, its reads dropped.
    fn close_compare(&mut self) {
        self.compare = None;
        self.picked = None;
        self.sources_load.cancel();
        self.manifests_load.cancel();
    }

    /// The comparison of `from` and `to`, when it is the one open (a result for another is
    /// left).
    fn comparing(&mut self, from: i64, to: i64) -> Option<&mut Compare> {
        self.compare.as_mut().filter(|c| (c.from, c.to) == (from, to))
    }

    fn current(&self) -> Option<&Key> {
        self.members.get(self.tab)
    }

    /// The cards of the tab shown; None while its first read runs.
    fn shown(&self) -> Option<Result<Arc<Cards>, String>> {
        if let Some(tabs) = &self.example {
            return tabs.get(self.tab).map(|(_, c)| Ok(c.clone()));
        }
        let (k, cards) = self.cards.as_ref()?;
        (Some(k) == self.current()).then(|| cards.clone())
    }

    fn is_open(&self, part: &str) -> bool {
        self.open.contains(part)
    }

    /// Take the result of a read of the tab `k`, done at `now`: kept unless another tab is
    /// shown by then (`Stale`). What follows: a read again when its row changed meanwhile
    /// (`Reload`), one in a few seconds while a sync runs (`Tick`), or nothing.
    fn take_read(&mut self, k: Key, cards: Result<Arc<Cards>, String>, now: Timestamp) -> Next {
        if self.current() != Some(&k) {
            return Next::Stale;
        }
        let running =
            cards.as_ref().is_ok_and(|c| c.summary.in_progress || c.last_sync.as_ref().is_some_and(|s| s.running));
        self.loading = false;
        // A failed read, or one of the list's copy only, keeps the cards of this Application
        // read whole before: what's held stays, with why in one line.
        let whole = |c: &Result<Arc<Cards>, String>| c.as_ref().is_ok_and(|c| c.note.is_none());
        let kept = self.cards.as_ref().is_some_and(|(kk, c)| *kk == k && whole(c));
        match &cards {
            Err(e) if kept => self.failed = Some(kept_line(e)),
            Ok(c) if kept && c.note.is_some() => self.failed = c.note.as_deref().map(kept_line),
            _ => {
                self.failed = None;
                self.updated = Some(now);
                self.cards = Some((k, cards));
            }
        }
        if std::mem::take(&mut self.dirty) {
            Next::Reload
        } else if running {
            Next::Tick
        } else {
            Next::Rest
        }
    }
}

/// The line over cards kept from an older read, when the last one failed with `why`.
fn kept_line(why: &str) -> String {
    format!("Not read now ({}): shown as last read.", dashboard::first_line(why))
}

/// What follows a read of the dashboard (see `Dashboard::take_read`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Next {
    Stale,
    Reload,
    Tick,
    Rest,
}

/// Run `then` on the view `after` this long, once its page is on screen: a hidden page waits
/// until it shows again (nothing is read for a page no one sees), instead of skipping it for
/// good. Dropping the task drops it.
fn when_shown<V: 'static>(
    cx: &mut Context<V>,
    after: Duration,
    then: impl FnOnce(&mut V, &mut Context<V>) + 'static,
) -> Task<()> {
    cx.spawn(async move |this, cx| {
        let exec = cx.background_executor().clone();
        exec.timer(after).await;
        if crate::app::wait_visible(&this, cx, &exec).await {
            let _ = this.update(cx, then);
        }
    })
}

impl ArgoView {
    /// Open the dashboard of the environment of the Application `k`, on its section.
    pub(super) fn open_dashboard(&mut self, k: &Key, cx: &mut Context<Self>) {
        let Some(rows) = &self.rows else { return };
        let apps = &rows.fleet.apps;
        let Some(ix) = apps.iter().position(|a| key(a) == *k) else { return };
        let Some(env) = rows.env_of.get(ix).and_then(|&e| rows.envs.get(e)) else { return };
        let members = super::envs::section_order(env);
        let tab = members.iter().position(|&m| m == ix).unwrap_or(0);
        let mut d = Dashboard::new(env.name.clone(), members.iter().map(|&m| key(&apps[m])).collect(), tab);
        d.kind = rows.types.name(env.kind).to_string();
        d.server = apps[env.root()].server.name();
        self.dashboard = Some(d);
        self.load_dashboard(cx);
    }

    /// Where the environment shown is among those the list keeps (from 0), and how many.
    fn env_position(&self) -> Option<(usize, usize)> {
        let rows = self.rows.as_ref()?;
        let k = self.dashboard.as_ref()?.members.first()?;
        let ix = rows.fleet.apps.iter().position(|a| key(a) == *k)?;
        let e = *rows.env_of.get(ix)?;
        Some((self.env_order.iter().position(|&o| o == e)?, self.env_order.len()))
    }

    /// Open the environment before (`-1`) or after (`1`) the one shown, in the list's order.
    fn step_env(&mut self, by: isize, cx: &mut Context<Self>) {
        let Some((at, n)) = self.env_position() else { return };
        let Some(to) = at.checked_add_signed(by).filter(|&t| t < n) else { return };
        let Some(rows) = &self.rows else { return };
        let Some(env) = rows.envs.get(self.env_order[to]) else { return };
        let k = key(&rows.fleet.apps[env.main]);
        self.open_dashboard(&k, cx);
    }

    pub(super) fn close_dashboard(&mut self, cx: &mut Context<Self>) {
        // Its reads are dropped with it.
        self.dashboard = None;
        cx.notify();
    }

    fn select_dashboard_tab(&mut self, tab: usize, cx: &mut Context<Self>) {
        let Some(d) = &mut self.dashboard else { return };
        if d.tab == tab {
            return;
        }
        d.tab = tab;
        d.failed = None;
        d.open.clear();
        d.sync_open.clear();
        d.diff_details.clear();
        d.close_compare();
        d.tick = None;
        d.dirty = false;
        self.load_dashboard(cx);
        cx.notify();
    }

    fn toggle_part(&mut self, part: String, cx: &mut Context<Self>) {
        if let Some(d) = &mut self.dashboard
            && !d.open.remove(&part)
        {
            d.open.insert(part);
        }
        cx.notify();
    }

    /// Read the tab shown, off the UI thread, by Argo CD's API. Its cards are
    /// worked out there too. A result for another tab than the one shown by then is left.
    pub(super) fn load_dashboard(&mut self, cx: &mut Context<Self>) {
        let Some(k) = self.dashboard.as_ref().filter(|d| d.example.is_none()).and_then(|d| d.current().cloned()) else {
            return;
        };
        let Some(a) = self.find(&k).cloned() else {
            if let Some(d) = &mut self.dashboard {
                d.load.cancel();
                d.tick = None;
                (d.loading, d.dirty) = (false, false);
                d.cards = Some((k, Err("This Application is no longer listed.".into())));
            }
            cx.notify();
            return;
        };
        let (ns, name) = (a.app.namespace.clone(), a.app.name.clone());
        let read = api::dashboard(a.server.clone(), ns, name);
        let exec = cx.background_executor().clone();
        let seen_key = k.clone();
        let work = async move {
            let read = read.await?;
            let cards = exec.spawn(async move {
                let seen = dashboard::seen_images(&seen_key, &read.app);
                Cards::of(read, seen.as_deref(), Timestamp::now())
            });
            Ok::<_, String>(Arc::new(cards.await))
        };
        let Some(d) = &mut self.dashboard else { return };
        (d.loading, d.dirty) = (true, false);
        d.tick = None;
        d.read_for = Some((k.clone(), a.app));
        d.load.spawn(cx, work, move |this, cards: Result<Arc<Cards>, String>, cx| {
            this.dashboard_read(k, cards, cx);
        });
        cx.notify();
    }

    /// A read of the tab `k` ended: show it, unless another tab is shown by then; read it again
    /// when its row changed meanwhile, or in a few seconds while a sync runs.
    fn dashboard_read(&mut self, k: Key, cards: Result<Arc<Cards>, String>, cx: &mut Context<Self>) {
        // What the tree holds in trouble below it counts for its environment on the list.
        if let Ok(c) = &cards
            && c.problems.from_tree
        {
            let changed =
                if c.state.below.is_some() { self.tree_bad.insert(k.clone()) } else { self.tree_bad.remove(&k) };
            if changed && let Some(rows) = &mut self.rows {
                rows.set_tree_verdicts(&self.tree_bad);
                self.refilter(cx);
            }
        }
        let Some(d) = &mut self.dashboard else { return };
        let next = d.take_read(k, cards, Timestamp::now());
        cx.notify();
        self.ask_versions(cx);
        let Some(d) = &mut self.dashboard else { return };
        match next {
            Next::Stale | Next::Rest => {}
            Next::Reload => self.load_dashboard(cx),
            // A sync running: its steps move without its row changing.
            Next::Tick => d.tick = Some(when_shown(cx, RUNNING_EVERY, Self::load_dashboard)),
        }
    }

    /// The page is no longer shown: the read running is dropped, and the tab is read again once
    /// the page shows (its next rows).
    pub(super) fn dashboard_hidden(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.dashboard.as_mut().filter(|d| d.example.is_none()) else { return };
        d.load.cancel();
        (d.loading, d.dirty) = (false, false);
        d.read_for = None;
        cx.notify();
    }

    /// The page's rows were read again: read the tab shown again when its row changed.
    pub(super) fn dashboard_changed(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.dashboard.as_ref().filter(|d| d.example.is_none()) else { return };
        let Some(k) = d.current().cloned() else { return };
        let now = self.find(&k).map(|a| a.app.clone());
        let before = d.read_for.as_ref().filter(|(rk, _)| *rk == k).map(|(_, s)| s.clone());
        if now.is_none() || now == before {
            return;
        }
        // A read is running: it ends first (a busy Application changes more often than three
        // requests take), then the tab is read again.
        if d.loading {
            if let Some(d) = &mut self.dashboard {
                d.dirty = true;
            }
            return;
        }
        self.load_dashboard(cx);
    }

    /// The read of the manifests rendered at the deployments `from` and `to` of the tab shown:
    /// by Argo CD's API.
    fn manifests_read(&self, spec: u64, from: &Entry, to: &Entry) -> DiffRead {
        let fail = |why: &str| -> DiffRead {
            let why = why.to_string();
            Box::pin(async move { Err(why) })
        };
        let Some(d) = &self.dashboard else { return fail("No dashboard open.") };
        if d.example.is_some() {
            return fail("A made-up example: no manifests are rendered.");
        }
        let (Some(rf), Some(rt)) = (from.revision.clone(), to.revision.clone()) else {
            return fail("A deployment compared has no revision recorded: its manifests can't be rendered.");
        };
        let Some(a) = d.current().and_then(|k| self.find(k)) else {
            return fail("This Application is no longer listed.");
        };
        let (ns, name) = (a.app.namespace.clone(), a.app.name.clone());
        Box::pin(api::manifests_diff(a.server.clone(), ns, name, spec, rf, rt))
    }

    /// Compare the deployments `from` and `to` (ids, the older first) of the tab shown: their
    /// sources, and their manifests read now (each revision's kept once read). Both are worked
    /// out off the UI thread; a newer comparison replaces this one's reads.
    fn open_compare(&mut self, from: i64, to: i64, cx: &mut Context<Self>) {
        let Some(Ok(cards)) = self.dashboard.as_ref().and_then(Dashboard::shown) else { return };
        let entry = |id: i64| cards.history.entries.iter().find(|e| e.id == id).cloned();
        let (Some(a), Some(b)) = (entry(from), entry(to)) else { return };
        // Rendered with the Application's sources now: only when they have the shape they had.
        let spec = cards.history.spec;
        let (manifests, notes): (DiffRead, Vec<String>) = match sync_diff::plan(spec, &a, &b) {
            Ok(notes) => (self.manifests_read(spec.hash, &a, &b), notes),
            Err(why) => (Box::pin(async move { Err(why) }), Vec::new()),
        };
        let Some(d) = &mut self.dashboard else { return };
        d.picked = None;
        d.compare =
            Some(Compare { from, to, sources: None, manifests: None, notes, noise: false, open: HashSet::new() });
        let sources = cx.background_executor().spawn(async move {
            let diff = smartdiff::compare_values(&a.sources, &b.sources);
            diff.prepare();
            Arc::new(diff)
        });
        d.sources_load.spawn(cx, sources, move |this: &mut Self, diff, cx| {
            if let Some(c) = this.dashboard.as_mut().and_then(|d| d.comparing(from, to)) {
                c.sources = Some(diff);
                cx.notify();
            }
        });
        d.manifests_load.spawn(cx, manifests, move |this: &mut Self, diff, cx| {
            if let Some(c) = this.dashboard.as_mut().and_then(|d| d.comparing(from, to)) {
                c.manifests = Some(diff);
                cx.notify();
            }
        });
        cx.notify();
    }

    /// The diff of a deployment against the one before: opened, or closed when it is open.
    fn toggle_compare(&mut self, from: i64, to: i64, cx: &mut Context<Self>) {
        let Some(d) = &mut self.dashboard else { return };
        if d.comparing(from, to).is_some() {
            d.close_compare();
            cx.notify();
        } else {
            self.open_compare(from, to, cx);
        }
    }

    /// Pick a deployment to compare: the second picked opens the comparison of the two.
    fn pick(&mut self, id: i64, cx: &mut Context<Self>) {
        let Some(d) = &mut self.dashboard else { return };
        let picked = d.picked;
        match picked {
            Some(p) if p == id => d.picked = None,
            Some(p) => return self.open_compare(p.min(id), p.max(id), cx),
            None => d.picked = Some(id),
        }
        cx.notify();
    }

    /// Show or hide the noise of the smart diff `at` (`key` None), or unfold or fold the diff of
    /// its resource `key`.
    fn toggle_smart(&mut self, at: DiffAt, key: Option<ObjectKey>, cx: &mut Context<Self>) {
        let Some(d) = &mut self.dashboard else { return };
        // The Sync diff hides its noise for good: said in a sentence.
        let (noise, open) = match at {
            DiffAt::Compare => match d.compare.as_mut() {
                Some(c) => (Some(&mut c.noise), &mut c.open),
                None => return,
            },
            DiffAt::Sync => (None, &mut d.sync_open),
        };
        match key {
            None => {
                if let Some(noise) = noise {
                    *noise = !*noise;
                }
            }
            Some(k) => {
                if !open.remove(&k) {
                    open.insert(k);
                }
            }
        }
        cx.notify();
    }

    /// The made-up example of the developer tools: three environments, nothing read.
    #[cfg(feature = "dev")]
    pub fn demo_dashboard(&mut self, cx: &mut Context<Self>) {
        use dashboard::example;
        let now = Timestamp::now();
        // An environment's three Applications, each shown with one of the example's reads.
        let sections = ["workload", "managed-namespace", "shop-prod-acme (app of apps)"];
        let tabs = sections
            .iter()
            .zip([example::ENVIRONMENTS[1], example::ENVIRONMENTS[0], example::ENVIRONMENTS[2]])
            .map(|(name, env)| (name.to_string(), Arc::new(Cards::of(example::read(env, now), None, now))))
            .collect();
        // A made-up newer release of `api` in its registry: its chip turns red.
        crate::registry::pretend(
            "registry.example.com/shop/api",
            "1.4.1",
            crate::registry::Status::Newer {
                tag: "1.5.1".into(),
                version: "1.5.1".into(),
                published: Timestamp::from_second(now.as_second() - 45 * 60).ok(),
            },
        );
        let mut d = Dashboard::new("shop-prod-acme (example, no real data)".into(), Vec::new(), 0);
        d.kind = "prod".into();
        d.server = "argocd.example.com".into();
        d.example = Some(tabs);
        d.updated = Some(now);
        self.dashboard = Some(d);
        cx.notify();
    }

    /// The made-up example on the view `view` (`resources`, `sync`, `history`, `details`;
    /// `all`: the Overview with every part unfolded).
    #[cfg(feature = "dev")]
    pub fn demo_dashboard_view(&mut self, view: &str, cx: &mut Context<Self>) {
        if self.dashboard.as_ref().is_none_or(|d| d.example.is_none()) {
            self.demo_dashboard(cx);
        }
        let Some(d) = &mut self.dashboard else { return };
        d.view = match view {
            "resources" => View::Resources,
            "sync" => View::SyncStatus,
            "history" => View::History,
            "details" => View::Details,
            _ => View::Overview,
        };
        if view == "all" {
            d.open.extend(["images", "deployed"].map(String::from));
        }
        cx.notify();
    }

    /// The made-up example with one of its details open in its dialog: `why`, `diff`
    /// (every component unfolded), `problems`, `pods`, `history`.
    #[cfg(feature = "dev")]
    pub fn demo_dashboard_dialog(&mut self, which: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.dashboard.as_ref().is_none_or(|d| d.example.is_none()) {
            self.demo_dashboard(cx);
        }
        let detail = match which {
            "diff" => Detail::SyncDiff,
            "problems" => Detail::Problems,
            "pods" => Detail::Pods,
            "history" => Detail::History,
            _ => Detail::Why,
        };
        self.open_detail(detail, window, cx);
        if detail == Detail::SyncDiff
            && let Some(d) = &mut self.dashboard
            && let Some(Ok(c)) = d.shown()
        {
            d.diff_details.extend(c.components.iter().map(|l| l.name.clone()));
        }
        cx.notify();
    }

    /// The made-up example after a selective sync of the workload (two of its resources, by
    /// hand, an hour ago), newer than its newest deployment.
    #[cfg(feature = "dev")]
    pub fn demo_dashboard_selective(&mut self, cx: &mut Context<Self>) {
        self.demo_dashboard(cx);
        let Some(d) = &mut self.dashboard else { return };
        let now = Timestamp::now();
        if let Some((_, cards)) = d.example.as_mut().and_then(|t| t.get_mut(0)) {
            let mut read = dashboard::example::read(dashboard::example::ENVIRONMENTS[1], now);
            let ago = |m: i64| Timestamp::from_second(now.as_second() - m * 60).unwrap_or(now).to_string();
            read.app["status"]["operationState"] = serde_json::json!({
                "phase": "Succeeded",
                "message": "successfully synced (all tasks run)",
                "startedAt": ago(61),
                "finishedAt": ago(60),
                "operation": {
                    "initiatedBy": {"username": "jdoe"},
                    "sync": {"revision": "ebf7503b6f0c4e2a9d8c7b6a5f4e3d2c1b0a9f8e", "syncStrategy": {"apply": {}},
                             "resources": [{"kind": "Deployment", "name": "api"}, {"kind": "ConfigMap", "name": "api-config"}]},
                },
                "syncResult": {"revision": "ebf7503b6f0c4e2a9d8c7b6a5f4e3d2c1b0a9f8e", "resources": [
                    {"kind": "Deployment", "name": "api", "namespace": "shop", "status": "Synced", "message": "deployment.apps/api configured"},
                    {"kind": "ConfigMap", "name": "api-config", "namespace": "shop", "status": "Synced", "message": "configured"},
                ]},
            });
            *cards = Arc::new(Cards::of(read, None, now));
        }
        cx.notify();
    }

    /// The made-up example after a login expired: the tab shown keeps its last read with why in
    /// one line, and the next one is shown as the Applications list has it.
    #[cfg(feature = "dev")]
    pub fn demo_dashboard_expired(&mut self, cx: &mut Context<Self>) {
        self.demo_dashboard(cx);
        let Some(d) = &mut self.dashboard else { return };
        let why = "Argo CD at argocd.example.com doesn't take this token as a login: expired or revoked?";
        d.failed = Some(kept_line(why));
        let now = Timestamp::now();
        if let Some((_, cards)) = d.example.as_mut().and_then(|t| t.get_mut(1)) {
            let app = api::lean(&dashboard::example::read(dashboard::example::ENVIRONMENTS[0], now).app);
            *cards = Arc::new(Cards::of(dashboard::Read::held(app, why), None, now));
        }
        cx.notify();
    }

    // MARK: Rendering

    /// The dashboard: its header, an entry per environment, and the cards of the one shown.
    pub(super) fn dashboard_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(d) = &self.dashboard else { return div().into_any_element() };
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let updated = d.updated.map(|t| {
            let at = t.to_zoned_datetime(jiff::tz::TimeZone::system()).strftime("%H:%M:%S").to_string();
            format!("Updated at {at}")
        });
        let header = h_flex()
            .gap_2()
            .child(
                Button::new("argo-dash-back")
                    .small()
                    .ghost()
                    .icon(Lucide::ArrowLeft)
                    .label("Environments")
                    .on_click(cx.listener(|this, _, _, cx| this.close_dashboard(cx))),
            )
            .child(div().min_w_0().truncate().text_lg().font_weight(FontWeight::SEMIBOLD).child(d.title.clone()))
            .when(!d.kind.is_empty(), |h| {
                h.child(
                    div()
                        .flex_none()
                        .px_1p5()
                        .rounded(px(4.))
                        .border_1()
                        .border_color(theme.border)
                        .text_xs()
                        .child(d.kind.clone()),
                )
            })
            .when(!d.server.is_empty(), |h| {
                h.child(div().flex_none().text_xs().text_color(muted).child(d.server.clone()))
            })
            .child(div().flex_1())
            .children(self.env_position().map(|(at, n)| {
                h_flex()
                    .flex_none()
                    .gap_1()
                    .child(
                        Button::new("argo-dash-prev")
                            .small()
                            .ghost()
                            .icon(Lucide::ChevronLeft)
                            .accessibility_label("Previous environment")
                            .disabled(at == 0)
                            .on_click(cx.listener(|this, _, _, cx| this.step_env(-1, cx))),
                    )
                    .child(div().text_xs().text_color(muted).child(format!("{} / {n}", at + 1)))
                    .child(
                        Button::new("argo-dash-next")
                            .small()
                            .ghost()
                            .icon(Lucide::ChevronRight)
                            .accessibility_label("Next environment")
                            .disabled(at + 1 >= n)
                            .on_click(cx.listener(|this, _, _, cx| this.step_env(1, cx))),
                    )
            }))
            .children(updated.map(|u| div().flex_none().text_xs().text_color(muted).child(u)))
            // Reads the page again from Argo CD; Refresh in the action bar asks Argo CD to
            // compare with git again.
            .child(
                with_tip("argo-dash-reload-tip", "Reload: read this page again from Argo CD".into()).child(
                    Button::new("argo-dash-refresh")
                        .small()
                        .ghost()
                        .icon(Lucide::RotateCw)
                        .accessibility_label("Reload")
                        .loading(d.loading)
                        .disabled(d.example.is_some())
                        .on_click(cx.listener(|this, _, _, cx| this.load_dashboard(cx))),
                ),
            );
        let environments = self.environments(d, cx);
        let shown = d.shown();
        let shown_cards = shown.as_ref().and_then(|c| c.as_ref().ok()).cloned();
        let actions = self.action_bar(d, shown_cards.as_deref(), cx);
        let body = match shown {
            None => div().p_4().text_color(muted).child("Reading the Application…").into_any_element(),
            Some(Err(e)) => div().p_4().text_color(theme.danger).child(selectable(e)).into_any_element(),
            Some(Ok(cards)) => self.view_body(d, &cards, cx),
        };
        let failed = d.failed.clone().filter(|_| d.shown().is_some_and(|c| c.is_ok()));
        v_flex()
            .size_full()
            .gap_2()
            .child(header)
            .child(environments)
            .child(actions)
            .children(failed.map(|f| div().text_xs().text_color(theme.warning).child(selectable(f))))
            .child(div().id("argo-dash-body").flex_1().min_h_0().overflow_y_scroll().child(body))
            .into_any_element()
    }

    /// An entry per Application of the environment (its sections: the workload first, the root
    /// last), on more lines when the window is narrow: its state's dot, its name under the
    /// environment's, its sync and health, and where it is read when two would read the same
    /// otherwise. A click shows its cards.
    fn environments(&self, d: &Dashboard, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let entries: Vec<(String, Option<String>, Tone)> = if let Some(tabs) = &d.example {
            tabs.iter().map(|(n, c)| (n.clone(), None, c.state.tone)).collect()
        } else {
            // Two of the same name (another namespace or Argo CD), or several sources: each
            // says where it is read.
            let clash = d.members.iter().any(|k| d.members.iter().filter(|o| o.2 == k.2).count() > 1);
            let sourced = clash || self.several_sources();
            d.members
                .iter()
                .map(|k| {
                    let found = self.find(k);
                    // An object in trouble below it (its tree, as read) makes it red too.
                    let tone = match found {
                        _ if self.tree_bad.contains(k) => Tone::Bad,
                        Some(a) => argo::attention(&a.app),
                        None => Tone::Done,
                    };
                    // Named under the environment (`workload`); its root (named as it) as such.
                    let root = k.2 == d.title && d.members.len() > 1;
                    let label = if root {
                        format!("{} (app of apps)", k.2)
                    } else {
                        super::envs::member_label(&d.title, &k.2).to_string()
                    };
                    let state = found.map(|a| format!("{} · {}", a.app.sync, a.app.health));
                    let detail = match (state, found.filter(|_| sourced).map(|a| a.server.name())) {
                        (Some(s), Some(server)) => Some(format!("{s} · {server}")),
                        (s, server) => s.or(server),
                    };
                    (label, detail, tone)
                })
                .collect()
        };
        h_flex().flex_wrap().gap_1().children(entries.into_iter().enumerate().map(|(i, (name, source, tone))| {
            let selected = i == d.tab;
            h_flex()
                .id(("argo-dash-env", i))
                .max_w(px(ENV_MAX))
                .min_w_0()
                .gap_1p5()
                .px_2()
                .py_0p5()
                .rounded_md()
                .border_1()
                .text_sm()
                .cursor_pointer()
                .border_color(if selected { theme.foreground.opacity(0.45) } else { theme.border })
                .when(selected, |h| h.bg(theme.secondary).font_weight(FontWeight::MEDIUM))
                .when(!selected, |h| h.text_color(theme.muted_foreground).hover(|s| s.bg(theme.secondary)))
                .child(div().flex_none().size(px(8.)).rounded_full().bg(tone.color(cx)))
                .child(div().min_w_0().truncate().child(name))
                .children(
                    source.map(|s| div().min_w_0().truncate().text_xs().text_color(theme.muted_foreground).child(s)),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.select_dashboard_tab(i, cx)))
        }))
    }

    /// The three status tiles (they stack on a narrow window), the Problems panel always
    /// under them, then the Versions and History panels. A card says little: its details
    /// open in a dialog, from its bordered button.
    fn cards(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> AnyElement {
        let tiles = h_flex()
            .flex_wrap()
            .items_stretch()
            .gap_3()
            .child(self.state_card(c, cx))
            .child(self.last_sync_card(d, c, cx))
            .child(self.diff_card(c, cx));
        let problems = self.problems_panel(d, c, cx);
        v_flex()
            .gap_3()
            .pb_4()
            .when_some(c.note.as_ref(), |v, why| {
                let line = format!("Not read now ({why}): shown as the Applications list has it.");
                v.child(div().text_xs().text_color(cx.theme().warning).child(selectable(line)))
            })
            .child(tiles)
            .child(problems)
            .child(
                h_flex()
                    .flex_wrap()
                    .items_start()
                    .gap_3()
                    .child(self.versions_card(d, c, cx).flex_grow(3.).flex_basis(px(VERSIONS)))
                    .child(self.history_card(d, c, cx).flex_grow(2.).flex_basis(px(HISTORY))),
            )
            .into_any_element()
    }

    /// A link that unfolds a part of a card, or folds it.
    fn toggle_link(&self, id: &str, part: &str, label: &str, d: &Dashboard, cx: &mut Context<Self>) -> Stateful<Div> {
        let open = d.is_open(part);
        let part = part.to_string();
        link(SharedString::from(format!("argo-dash-{id}")), if open { "hide" } else { label }, cx)
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_part(part.clone(), cx)))
    }

    /// The button that opens `detail` in its dialog.
    fn detail_opener(&self, id: &'static str, label: &str, detail: Detail, cx: &mut Context<Self>) -> Button {
        detail_button(id, label).on_click(cx.listener(move |this, _, window, cx| this.open_detail(detail, window, cx)))
    }

    /// The sync state in big, its color; one line of cause (the first resource Degraded and how
    /// many others, or the health); `Why? ↗` when the diagnosis finds something.
    fn state_card(&self, c: &Cards, cx: &mut Context<Self>) -> Div {
        let s = &c.state;
        let theme = cx.theme().clone();
        let sync_tone = match s.sync.as_str() {
            "Sync Failed" => Tone::Bad,
            "Syncing" => Tone::Busy,
            other => argo::sync_tone(other, c.summary.automated),
        };
        let health_color = argo::listed_health_tone(&s.health, c.summary.automated).color(cx);
        let cause = match (s.degraded.first(), &s.below) {
            (Some(first), _) => {
                let more = match s.degraded.len() {
                    1 => String::new(),
                    n => format!(" +{}", n - 1),
                };
                Some(format!("{first} Degraded{more}"))
            }
            (None, Some(below)) => Some(below.clone()),
            (None, None) => None,
        };
        let calm = c.findings.is_empty() && cause.is_none() && s.health == "Healthy" && c.clean_is_known();
        let line = match cause {
            Some(text) => div().min_w_0().truncate().text_color(theme.danger).child(selectable(text)),
            None if calm => div().text_color(theme.muted_foreground).child("Healthy · Nothing wrong."),
            None => div().min_w_0().truncate().text_color(health_color).child(selectable(s.health.clone())),
        };
        let line = with_tip("argo-dash-state-line", state_tip(s)).min_w_0().text_sm().child(line);
        let why = (!c.findings.is_empty()).then(|| self.detail_opener("argo-dash-why", "Why?", Detail::Why, cx));
        tile("State", Some(s.tone), cx)
            .child(figure(s.sync.clone(), sync_tone.color(cx)))
            .child(line)
            .child(div().flex_1())
            .children(why.map(|w| h_flex().child(w)))
    }

    fn last_sync_card(&self, _d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let Some(s) = &c.last_sync else {
            return tile("Last sync", None, cx).child(figure("Never".into(), muted));
        };
        let now = Timestamp::now();
        let mut facts = Vec::new();
        if let Some(t) = s.finished.or(s.started) {
            facts.push(format!("{} ago", k8s::age(t)));
        }
        if let Some(by) = &s.by {
            facts.push(format!("by {by}"));
        }
        if let Some(secs) = s.seconds(now) {
            facts.push(if s.running {
                format!("running for {}", span_text(secs))
            } else {
                format!("took {}", span_text(secs))
            });
        }
        if let Some(r) = &s.revision {
            facts.push(short_revision(r));
        }
        // A selective sync: some resources only, not added to Argo CD's history.
        if let Some((n, total)) = s.selective {
            facts.push(format!("selective: {n} of {}", crate::plural::count(total, "resource")));
        }
        let progress = (s.running && !s.steps.is_empty()).then(|| {
            let share = s.done as f32 / s.steps.len() as f32;
            v_flex().gap_0p5().child(div().text_xs().child(format!("{} of {} steps", s.done, s.steps.len()))).child(
                div()
                    .h(px(4.))
                    .w_full()
                    .rounded_full()
                    .bg(theme.muted)
                    .child(div().h_full().w(relative(share)).rounded_full().bg(theme.warning)),
            )
        });
        // Argo CD says the same of every sync that went well: only another message is shown.
        let message = s.message.clone().filter(|_| s.tone != Tone::Good);
        // One entry to the sync: its phases, what it waits for, the table and the journal.
        let open = self.sync_dialog_button(s.running, cx);
        tile("Last sync", Some(s.tone), cx)
            .child(figure(s.phase.clone(), s.tone.color(cx)))
            .child(div().text_xs().text_color(muted).child(selectable(facts.join(" · "))))
            .when_some(message, |v, m| v.child(div().text_xs().line_clamp(3).child(selectable(m))))
            .children(progress)
            .child(h_flex().child(open))
    }

    /// How many changes a sync would make, by component, then their list in one line
    /// (`~ api · + search · – old`); the prune verdict under it whenever something differs;
    /// `show ↗` for the Sync diff dialog.
    fn diff_card(&self, c: &Cards, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let diff = &c.diff;
        let total = diff.total();
        // Read from the Application's status, not Argo CD's diff: said on hover, with why.
        let tip = match &c.git_diff {
            Some(Err(e)) => Some(format!("Argo CD's diff couldn't be read ({e}): from the Application's status.")),
            _ if !diff.from_argo && total > 0 => Some("From the Application's status.".to_string()),
            _ => None,
        };
        let from_status = tip.map(|tip| hint("argo-dash-diff-source", "from status", tip, cx));
        // A list's copy holds no resources: what differs isn't known, only that something does.
        if total == 0 && c.note.is_some() && c.summary.sync != "Synced" {
            return tile("Sync diff", None, cx)
                .child(figure(c.summary.sync.clone(), muted))
                .child(div().text_sm().text_color(muted).child("What differs wasn't read."));
        }
        if total == 0 && c.components.is_empty() {
            return tile("Sync diff", Some(Tone::Good), cx)
                .child(figure("In sync".into(), theme.success))
                .child(div().text_sm().text_color(muted).child("Nothing to sync: the cluster matches git."))
                .child(div().flex_1())
                .children(from_status.map(|h| h_flex().child(h)));
        }
        let n = c.components.len().max(1);
        let tone = if c.summary.automated { Tone::Busy } else { Tone::Done };
        let list = if c.components.is_empty() { diff.text() } else { components::one_line(&c.components) };
        // On hover, what changes, each change once with where.
        let mut said: Vec<String> = c.changes.iter().take(CHANGES_TIP).map(|g| g.text.clone()).collect();
        if c.changes.len() > CHANGES_TIP {
            said.push(format!("+{} more", c.changes.len() - CHANGES_TIP));
        }
        let list_tip = if said.is_empty() { list.clone() } else { format!("{list}\n\n{}", said.join("\n")) };
        let show = self.detail_opener("argo-dash-diff", "show", Detail::SyncDiff, cx);
        tile("Sync diff", Some(tone), cx)
            .child(figure(
                crate::plural::count(n, "change"),
                if tone == Tone::Done { theme.foreground } else { tone.color(cx) },
            ))
            .child(
                with_tip("argo-dash-diff-list", list_tip).min_w_0().truncate().text_sm().text_color(muted).child(list),
            )
            .child(prune_banner("argo-dash-prune", &c.prune, true, cx))
            .child(div().flex_1())
            .child(h_flex().gap_2().items_center().child(show).children(from_status))
    }

    /// The problems, always under the tiles: one line each (status, name, kind, pods, for how
    /// long; why on hover), the worst first; the first few, then `+N more ↗` for all of them
    /// in a dialog. Its header asks the assistant about them all; its foot counts the pods and
    /// opens them. With none, one line: nothing wrong (when that is known), or why it isn't
    /// known.
    fn problems_panel(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> Div {
        let p = &c.problems;
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let known_clean = c.clean_is_known();
        let k = d.current().cloned();
        let reaches = k.as_ref().and_then(|k| self.find(k)).is_some_and(|a| a.on.is_some());
        let pods = (p.from_tree && !p.pods.is_empty())
            .then(|| self.detail_opener("argo-dash-all-pods", "all pods", Detail::Pods, cx));
        let tree_unread =
            c.tree_error.clone().filter(|_| !p.from_tree).map(|e| hint("argo-dash-tree-error", "pods unread", e, cx));
        let title = || div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(muted).child("PROBLEMS");
        if p.rows.is_empty() {
            let line = h_flex()
                .gap_2()
                .items_center()
                .px_3()
                .py_2()
                .rounded_lg()
                .border_1()
                .border_color(theme.border)
                .text_sm()
                .child(title());
            if known_clean {
                return line
                    .child(div().flex_none().size(px(8.)).rounded_full().bg(theme.success))
                    .child(div().child("Nothing wrong."))
                    .when(p.from_tree, |h| {
                        h.child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("{}, all healthy", crate::plural::count(p.pods.len(), "pod"))),
                        )
                    })
                    .child(div().flex_1())
                    .children(pods)
                    .children(tree_unread);
            }
            let why = c
                .tree_error
                .clone()
                .or_else(|| c.note.as_ref().map(|n| format!("Not read now ({n})")))
                .unwrap_or_else(|| {
                    "Argo CD keeps no health per resource in the Application, and its resource tree wasn't read".into()
                });
            return line
                .child(div().flex_none().size(px(8.)).rounded_full().bg(muted))
                .child(div().text_color(muted).child("Problems unknown"))
                .child(hint("argo-dash-problems-why", "why?", format!("{why}."), cx));
        }
        let tone = if p.rows.iter().any(|r| r.tone == Tone::Bad) { Tone::Bad } else { Tone::Busy };
        let shown = p.rows.len().min(PROBLEMS_SHOWN);
        let k_all = k;
        let ask_all = self
            .ask_button("argo-dash-ask-all", d)
            .label("Ask the assistant")
            .on_click(cx.listener(move |this, _, _, cx| this.ask_problems(k_all.as_ref(), None, cx)));
        let header = h_flex()
            .gap_2()
            .items_center()
            .flex_wrap()
            .pb_1()
            .child(title())
            .child(chip(&p.rows.len().to_string(), tone, cx))
            .children(tree_unread)
            .child(div().flex_1())
            .child(ask_all);
        let rows: Vec<Stateful<Div>> =
            p.rows.iter().take(shown).enumerate().map(|(i, r)| self.problem_row(d, i, r, reaches, cx)).collect();
        let more_label = format!("+{} more", p.rows.len() - shown);
        let more = (p.rows.len() > shown)
            .then(|| self.detail_opener("argo-dash-problems-more", &more_label, Detail::Problems, cx));
        let counts = p.from_tree.then(|| {
            div().text_xs().text_color(muted).child(format!(
                "{} · {} in trouble",
                crate::plural::count(p.pods.len(), "pod"),
                p.troubled
            ))
        });
        let footer = (more.is_some() || counts.is_some() || pods.is_some())
            .then(|| h_flex().gap_2().items_center().pt_1().children(more).children(counts).children(pods));
        frame_toned(Some(tone), cx).w_full().gap_0().child(header).children(rows).children(footer)
    }

    /// The button asking the assistant: it says on hover what it asks, or why it can't be
    /// asked (no assistant set up), then shows disabled.
    fn ask_button(&self, id: impl Into<ElementId>, d: &Dashboard) -> Button {
        let tip = if d.assistant {
            "Ask the assistant why: it reads the cluster (read-only) and answers with the cause and the fix"
        } else {
            "Set up the assistant first (Settings → Assistant: Claude Code or an API key)"
        };
        Button::new(id)
            .xsmall()
            .outline()
            .icon(Lucide::Sparkles)
            .accessibility_label("Ask the assistant")
            .tooltip(tip)
            .disabled(!d.assistant)
    }

    /// Ask the assistant about the problems of the tab shown (`k`): all of them, or the one at
    /// `one`.
    fn ask_problems(&mut self, k: Option<&Key>, one: Option<usize>, cx: &mut Context<Self>) {
        let Some(d) = &self.dashboard else { return };
        let Some(Ok(cards)) = d.shown() else { return };
        let rows: Vec<&Problem> = match one {
            Some(i) => cards.problems.rows.get(i).into_iter().collect(),
            None => cards.problems.rows.iter().collect(),
        };
        if rows.is_empty() {
            return;
        }
        let det = &cards.details;
        let application = match (k, &d.example) {
            (Some(k), _) => k.2.clone(),
            (None, Some(tabs)) => tabs.get(d.tab).map_or_else(|| d.title.clone(), |(n, _)| n.clone()),
            (None, None) => d.title.clone(),
        };
        let scope = dashboard::AskScope {
            environment: d.title.clone(),
            application,
            cluster: if det.cluster.is_empty() { det.server.clone() } else { det.cluster.clone() },
            namespace: det.namespace.clone(),
            context: k.and_then(|k| self.find(k)).and_then(|a| a.on.clone()),
        };
        let question = dashboard::problems_question(&scope, &rows, Timestamp::now());
        cx.emit(super::AskAssistant { question });
    }

    /// A problem in one line: its status, its name whole (a click opens it when a context
    /// reaches its cluster), its kind, its pods in trouble and for how long, the assistant
    /// asked about it; why on hover. In a dialog too: what it opens closes the dialog first.
    fn problem_row(
        &self,
        d: &Dashboard,
        i: usize,
        r: &Problem,
        reaches: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let mut facts = vec![r.res.kind.clone()];
        if let Some((bad, all)) = r.pods {
            facts.push(format!("{bad} of {}", crate::plural::count(all, "pod")));
        }
        if let Some(since) = r.since {
            facts.push(format!("for {}", span_text(Timestamp::now().as_second() - since.as_second())));
        }
        let k = d.current().cloned();
        let name =
            div().id(("argo-dash-problem", i)).flex_none().max_w(px(360.)).truncate().font_weight(FontWeight::MEDIUM);
        let name = if reaches {
            let (k, res) = (k.clone(), r.res.clone());
            name.text_color(theme.blue).cursor_pointer().hover(Styled::underline).child(r.res.name.clone()).on_click(
                cx.listener(move |this, _, window, cx| {
                    window.close_all_dialogs(cx);
                    if let Some(k) = &k {
                        this.open_deployed(k, res.clone(), cx);
                    }
                }),
            )
        } else {
            name.child(selectable(r.res.name.clone()))
        };
        let ask = self.ask_button(("argo-dash-ask", i), d).ghost().on_click(cx.listener(move |this, _, window, cx| {
            window.close_all_dialogs(cx);
            this.ask_problems(k.as_ref(), Some(i), cx);
        }));
        let tip = r.why.clone().unwrap_or_else(|| format!("{} {}: {}", r.res.kind, r.res.name, r.status));
        with_tip(("argo-dash-problem-row", i), tip)
            .flex()
            .flex_row()
            .gap_2()
            .items_center()
            .min_w_0()
            .py_1()
            .border_t_1()
            .border_color(theme.border.opacity(0.6))
            .text_sm()
            .child(chip(&r.status, r.tone, cx))
            .child(name)
            .child(div().flex_1().min_w_0().truncate().text_xs().text_color(muted).child(selectable(facts.join(" · "))))
            .child(ask)
    }

    /// "journal": the sync journal of the Application shown, every operation seen of it, even
    /// those Argo CD's history no longer has.
    fn journal_link(&self, id: &'static str, d: &Dashboard, cx: &mut Context<Self>) -> Option<Stateful<Div>> {
        let name = d.current()?.2.clone();
        Some(
            link(id, "journal", cx)
                .on_click(cx.listener(move |this, _, window, cx| this.open_journal(Some(name.clone()), window, cx))),
        )
    }

    /// The product's images of the Application `k` (see `envs::environments`): those of its
    /// environment whose tags differ between the project's environments or that are named
    /// after the project, and those named after the environment's product (its name's first
    /// word).
    fn own_images(&self, k: &Key) -> HashSet<String> {
        let Some(rows) = &self.rows else { return HashSet::new() };
        let Some(env) = rows.fleet.apps.iter().position(|a| key(a) == *k).and_then(|i| rows.envs.get(rows.env_of[i]))
        else {
            return HashSet::new();
        };
        let product = env.name.split('-').next().unwrap_or_default();
        env.images
            .iter()
            .filter(|(own, i)| *own || argo::environments::named_after(&i.repo, product))
            .map(|(_, i)| i.repo.clone())
            .collect()
    }

    /// The deployments as dots (a failed sync red), their counts, and `open ↗` for their list
    /// in a dialog; on the History view, the list itself.
    fn history_card(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> Div {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let h = &c.history;
        let tone = if h.dots.last().is_some_and(|dot| !dot.ok) { Some(Tone::Bad) } else { None };
        let listed = d.view == View::History;
        let open = (!h.dots.is_empty() && !listed)
            .then(|| self.detail_opener("argo-dash-history", "open", Detail::History, cx));
        let journal = self.journal_link("argo-dash-journal-history", d, cx);
        card("History", tone, cx)
            .when(h.dots.is_empty(), |v| v.child(div().text_sm().text_color(muted).child("No deployment yet.")))
            .child(h_flex().flex_wrap().gap_1().children(
                h.dots.iter().map(|dot| {
                    div().size(px(12.)).rounded_full().bg(if dot.ok { theme.success } else { theme.danger })
                }),
            ))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("{} succeeded · {} failed", h.succeeded, h.failed))
                    .children(open)
                    .children(journal),
            )
            .when(listed, |v| v.children(self.history_rows(d, c, cx)))
    }

    /// The deployments, newest first: each compared with the one before (`diff`) or picked
    /// to compare with another.
    fn history_rows(&self, d: &Dashboard, c: &Cards, cx: &mut Context<Self>) -> Vec<Div> {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let h = &c.history;
        let mut out = Vec::new();
        if h.entries.len() > 1 {
            out.push(div().text_xs().text_color(muted).child("Pick two to compare them."));
        }
        out.extend(h.dots.iter().rev().map(|dot| {
            // The deployment before this one, to diff against.
            let before = dot.id.and_then(|id| {
                let i = h.entries.iter().position(|e| e.id == id)?;
                Some((h.entries.get(i.checked_sub(1)?)?.id, id))
            });
            let pickable = dot.id.filter(|id| h.entries.iter().any(|e| e.id == *id));
            let mut parts = vec![match dot.id {
                Some(id) => format!("#{id}"),
                None => "failed sync".to_string(),
            }];
            if !dot.revision.is_empty() {
                parts.push(short_revision(&dot.revision));
            }
            if let Some(t) = dot.at {
                parts.push(format!("{} ago", k8s::age(t)));
            }
            if let Some(by) = &dot.by {
                parts.push(format!("by {by}"));
            }
            let shown = |from: i64, to: i64| d.compare.as_ref().is_some_and(|c| (c.from, c.to) == (from, to));
            h_flex()
                .gap_1p5()
                .text_xs()
                .child(div().size(px(7.)).flex_none().rounded_full().bg(if dot.ok {
                    theme.success
                } else {
                    theme.danger
                }))
                .child(div().flex_1().min_w_0().truncate().child(selectable(parts.join(" · "))))
                .when_some(before, |row, (from, to)| {
                    let label = if shown(from, to) { "hide diff" } else { "diff" };
                    row.child(
                        link(("argo-dash-hdiff", to.unsigned_abs() as usize), label, cx)
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_compare(from, to, cx))),
                    )
                })
                .when_some(pickable, |row, id| {
                    let label = if d.picked == Some(id) { "picked" } else { "pick" };
                    row.child(
                        link(("argo-dash-pick", id.unsigned_abs() as usize), label, cx)
                            .on_click(cx.listener(move |this, _, _, cx| this.pick(id, cx))),
                    )
                })
        }));
        out
    }
}

/// What hovering the State card's line says: the health, Argo CD's message, what is
/// Degraded, and what counts beside the resources Argo CD manages.
fn state_tip(s: &dashboard::State) -> String {
    let mut parts = vec![format!("Health: {}", s.health)];
    if let Some(m) = &s.health_message {
        parts.push(m.clone());
    }
    if !s.degraded.is_empty() {
        parts.push(format!("Degraded: {}", s.degraded.join(", ")));
    }
    if let Some(below) = &s.below {
        parts.push(format!(
            "{below}: Argo CD counts only the resources it manages; these are objects they made (the Jobs of a \
             CronJob, a Deployment's pods), listed under Problems."
        ));
    }
    parts.join("\n")
}

impl ArgoView {
    /// Two deployments compared, full width under the cards: what their sources asked for, then
    /// what Argo CD renders at their revisions (summary first, noise folded, each resource's
    /// diff unfolded on a click).
    fn compare_view(&self, c: &Compare, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let heading =
            |text: &str| div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(muted).child(text.to_uppercase());
        let note = |text: &str| div().text_xs().text_color(muted).child(selectable(text.to_string()));
        let sources = match &c.sources {
            None => note("Comparing…").into_any_element(),
            Some(s) => match s.resources.first() {
                None => note("Same sources: same revisions, values and parameters.").into_any_element(),
                Some(r) => v_flex()
                    .gap_1()
                    .children(s.groups.iter().map(|g| div().text_sm().child(selectable(g.text()))))
                    .child(lines_block(r.lines(false), (&format!("#{}", c.from), &format!("#{}", c.to)), cx))
                    .into_any_element(),
            },
        };
        let manifests = match &c.manifests {
            None => note("Reading the manifests Argo CD renders at both revisions…").into_any_element(),
            Some(Err(e)) => div().text_xs().text_color(theme.danger).child(selectable(e.clone())).into_any_element(),
            Some(Ok(m)) => self.manifests_view(c, m, cx),
        };
        v_flex()
            .gap_2()
            .p_3()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(format!("Deployment #{} → #{}", c.from, c.to)),
                    )
                    .child(link("argo-dash-compare-close", "close", cx).on_click(cx.listener(|this, _, _, cx| {
                        if let Some(d) = &mut this.dashboard {
                            d.close_compare();
                        }
                        cx.notify();
                    }))),
            )
            .child(heading("Sources"))
            .child(sources)
            .child(heading("Rendered manifests"))
            .children(c.notes.iter().map(String::as_str).map(note))
            .child(manifests)
            .into_any_element()
    }

    /// The manifests' smart diff of a comparison.
    fn manifests_view(&self, c: &Compare, m: &SmartDiff, cx: &mut Context<Self>) -> AnyElement {
        let (from, to) = (format!("Deployment #{}", c.from), format!("Deployment #{}", c.to));
        let shown = Smart {
            at: DiffAt::Compare,
            intro: Some(
                "Argo CD renders both from the Application's sources as they are now, at each deployment's \
                 revision: a change of its own parameters shows under Sources.",
            ),
            empty: "Same rendered manifests at both revisions.",
            same: m.unchanged,
            noise: c.noise,
            open: &c.open,
            sides: (&from, &to),
        };
        self.smart_view(&shown, m, cx)
    }

    /// A smart diff: the summary (each change once, with where, images first), the noise hidden
    /// and counted, then a row per resource with what changes in it, that unfolds its diff.
    fn smart_view(&self, v: &Smart, m: &SmartDiff, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let at = v.at;
        let noise = m.noise();
        let empty = if m.is_empty() {
            Some(v.empty)
        } else if m.groups.is_empty() {
            Some("Only noise changed.")
        } else {
            None
        };
        let by_rule: Vec<String> = m.noise_by_rule().iter().take(4).map(|(r, n)| format!("{r} {n}")).collect();
        let noise_line = format!(
            "{} {} ({})",
            crate::plural::count(noise, "noise change"),
            if v.noise { "shown" } else { "hidden" },
            by_rule.join(", ")
        );
        let rows: Vec<&ResourceChange> = m.resources.iter().filter(|r| v.noise || !r.facts.is_empty()).collect();
        let more_rows = rows.len().saturating_sub(ROWS_SHOWN);
        // Each change in a line with where: the changes grouped across resources, else the
        // diff's own groups.
        let groups: Vec<(String, String)> = m.groups.iter().map(|g| (g.text(), names(&g.resources))).collect();
        let more_groups = groups.len().saturating_sub(GROUPS_SHOWN);
        let changed = m.resources.iter().filter(|r| !r.facts.is_empty()).count();
        let counts = format!(
            "{} changed, {} the same or noise only",
            crate::plural::count(changed, "resource"),
            v.same + m.resources.len() - changed
        );
        v_flex()
            .gap_1()
            .children(v.intro.map(|i| div().text_xs().text_color(muted).child(i)))
            .children(empty.map(|e| div().text_sm().text_color(muted).child(e)))
            .children(groups.into_iter().take(GROUPS_SHOWN).map(|(text, names)| {
                h_flex()
                    .gap_2()
                    .child(div().text_sm().child(selectable(text)))
                    .child(div().text_xs().text_color(muted).child(selectable(names)))
            }))
            .when(more_groups > 0, |v| {
                v.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("… {} more", crate::plural::count(more_groups, "change"))),
                )
            })
            .when(!m.is_empty(), |v| v.child(div().text_xs().text_color(muted).child(counts)))
            .when(noise > 0, |w| {
                w.child(
                    h_flex().gap_2().text_xs().text_color(muted).child(selectable(noise_line)).child(
                        link(
                            SharedString::from(format!("{}-noise", at.id())),
                            if v.noise { "hide" } else { "show" },
                            cx,
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_smart(at, None, cx))),
                    ),
                )
            })
            .children(rows.into_iter().take(ROWS_SHOWN).enumerate().map(|(i, r)| {
                let open = v.open.contains(&r.key);
                let key = r.key.clone();
                // Images first.
                let mut facts: Vec<&smartdiff::Fact> = r.facts.iter().collect();
                facts.sort_by_key(|f| f.rank());
                let mut what: Vec<String> = facts.iter().take(3).map(|f| f.text()).collect();
                if r.facts.len() > 3 {
                    what.push(format!("+{} more", r.facts.len() - 3));
                }
                if r.facts.is_empty() {
                    what.push(format!("noise only ({})", r.noise.len()));
                }
                let label = format!("{} {}", if open { "▾" } else { "▸" }, r.key.label());
                v_flex()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1p5()
                            .text_xs()
                            .child(link(SharedString::from(format!("{}-res-{i}", at.id())), &label, cx).on_click(
                                cx.listener(move |this, _, _, cx| this.toggle_smart(at, Some(key.clone()), cx)),
                            ))
                            .child(div().text_color(muted).child(selectable(what.join("; ")))),
                    )
                    .when(open, |w| w.child(lines_block(r.lines(v.noise), v.sides, cx)))
            }))
            .when(more_rows > 0, |v| {
                v.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(format!("… {} more", crate::plural::count(more_rows, "resource"))),
                )
            })
            .into_any_element()
    }
}

/// A smart diff as shown (see `ArgoView::smart_view`).
struct Smart<'a> {
    at: DiffAt,
    intro: Option<&'static str>,
    /// Said when nothing differs.
    empty: &'static str,
    /// Resources the same on both sides, beside those the diff holds.
    same: usize,
    noise: bool,
    open: &'a HashSet<ObjectKey>,
    /// The two sides compared, as named over their columns.
    sides: (&'a str, &'a str),
}

/// Some resources by kind and name, the first few.
fn names(keys: &[ObjectKey]) -> String {
    let mut out: Vec<String> = keys.iter().take(NAMES_SHOWN).map(|k| format!("{} {}", k.kind, k.name)).collect();
    if keys.len() > NAMES_SHOWN {
        out.push(format!("+{} more", keys.len() - NAMES_SHOWN));
    }
    out.join(", ")
}

/// A diff's lines in two aligned columns, `sides` (before, after) named at their top; at most
/// `DIFF_LINES`, then how many more.
pub(super) fn lines_block(lines: &[crate::history::DiffLine], sides: (&str, &str), cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    if lines.is_empty() {
        return v_flex().child(div().text_xs().text_color(muted).child("No line differs once noise is hidden."));
    }
    let more = lines.len().saturating_sub(DIFF_LINES);
    v_flex().gap_0p5().child(columns::block(&lines[..lines.len().min(DIFF_LINES)], sides, cx)).when(more > 0, |v| {
        v.child(div().text_xs().text_color(muted).child(format!("… {} more", crate::plural::count(more, "line"))))
    })
}

/// Something with `tip` shown on hover.
fn with_tip(id: impl Into<ElementId>, tip: String) -> Stateful<Div> {
    let tip = SharedString::from(tip);
    div().id(id).tooltip(move |w, cx| component::tooltip::Tooltip::new(tip.clone()).build(w, cx))
}

/// A few muted words that say more on hover.
fn hint(id: impl Into<ElementId>, text: &str, tip: String, cx: &App) -> Stateful<Div> {
    with_tip(id, tip)
        .flex_none()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .underline()
        .text_decoration_color(cx.theme().muted_foreground.opacity(0.5))
        .child(text.to_string())
}

/// A card's box, its background tinted by `tone` when it says something is right or wrong.
fn frame_toned(tone: Option<Tone>, cx: &App) -> Div {
    let theme = cx.theme();
    let tone = tone.filter(|t| *t != Tone::Done);
    v_flex()
        .min_w_0()
        .gap_1p5()
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(tone.map_or(theme.border, |t| t.color(cx).opacity(0.5)))
        .bg(tone.map_or(theme.background, |t| t.color(cx).opacity(0.08)))
}

/// A card's box, untinted.
fn frame(cx: &App) -> Div {
    frame_toned(None, cx)
}

/// A card: its title, its background tinted by `tone`.
fn card(title: &str, tone: Option<Tone>, cx: &App) -> Div {
    let muted = cx.theme().muted_foreground;
    frame_toned(tone, cx).child(
        div().truncate().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(muted).child(title.to_uppercase()),
    )
}

/// One of the three status tiles: a card that shares its row with the others.
fn tile(title: &str, tone: Option<Tone>, cx: &App) -> Div {
    card(title, tone, cx).flex_grow(1.).flex_basis(px(TILE)).min_w(px(TILE_MIN))
}

/// A tile's headline, in its tone.
fn figure(text: String, color: Hsla) -> Div {
    div().truncate().text_xl().font_weight(FontWeight::BOLD).text_color(color).child(text)
}

/// A status in its tone, boxed.
pub(super) fn chip(text: &str, tone: Tone, cx: &App) -> Div {
    let color = tone.color(cx);
    div().flex_none().px_1p5().rounded_md().text_xs().text_color(color).bg(color.opacity(0.12)).child(text.to_string())
}

/// A word that does something when clicked.
pub(super) fn link(id: impl Into<ElementId>, text: &str, cx: &App) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .text_xs()
        .text_color(cx.theme().blue)
        .cursor_pointer()
        .hover(Styled::underline)
        .child(text.to_string())
}

// View tests need the toolkit's test support, which the `dev` feature turns on.
#[cfg(all(test, feature = "dev"))]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use gpui_kit::{AppContext as _, Context, Task, TestAppContext};
    use jiff::Timestamp;

    use super::{Dashboard, Next, when_shown};
    use crate::k8s::argo::dashboard::{Cards, example};

    fn key(name: &str) -> super::Key {
        ("argocd.example.com".into(), "argocd".into(), name.into())
    }

    fn cards(env: &str) -> Arc<Cards> {
        let now = Timestamp::now();
        Arc::new(Cards::of(example::read(env, now), None, now))
    }

    /// A read of a tab left before it ended is dropped; the tab shown keeps its own.
    #[test]
    fn a_read_of_another_tab_is_dropped() {
        let mut d = Dashboard::new("shop".into(), vec![key("example-staging"), key("example-prod")], 1);
        d.loading = true;
        assert_eq!(d.take_read(key("example-staging"), Ok(cards("example-staging")), Timestamp::now()), Next::Stale);
        assert!(d.shown().is_none() && d.loading, "nothing shown, the tab's own read still runs");
        assert_eq!(d.take_read(key("example-prod"), Ok(cards("example-prod")), Timestamp::now()), Next::Rest);
        assert!(d.shown().is_some_and(|c| c.is_ok_and(|c| c.summary.name == "example-prod")));
        assert!(!d.loading);
    }

    /// A row that changed during a read reads again once it ends; a sync running ticks.
    #[test]
    fn what_follows_a_read() {
        let mut d = Dashboard::new("shop".into(), vec![key("example-staging")], 0);
        d.dirty = true;
        assert_eq!(d.take_read(key("example-staging"), Ok(cards("example-staging")), Timestamp::now()), Next::Reload);
        assert!(!d.dirty);
        let mut running = (*cards("example-staging")).clone();
        running.summary.in_progress = true;
        assert_eq!(d.take_read(key("example-staging"), Ok(Arc::new(running)), Timestamp::now()), Next::Tick);
        assert_eq!(d.take_read(key("example-staging"), Err("down".into()), Timestamp::now()), Next::Rest);
    }

    /// A read that fails, or gives only the list's copy, keeps the cards read whole before,
    /// with why in one line; the next read whole replaces them. With nothing read whole before,
    /// the list's copy is shown, and an error is the error.
    #[test]
    fn a_failed_read_keeps_what_was_read() {
        let k = key("example-prod");
        let mut d = Dashboard::new("shop".into(), vec![k.clone()], 0);
        let held = || {
            let app = example::read("example-prod", Timestamp::now()).app;
            let read = crate::k8s::argo::dashboard::Read::held(app, "401 Unauthorized\nmore detail");
            Arc::new(Cards::of(read, None, Timestamp::now()))
        };
        d.take_read(k.clone(), Ok(held()), Timestamp::now());
        assert!(d.shown().is_some_and(|c| c.is_ok_and(|c| c.note.as_deref() == Some("401 Unauthorized"))));
        assert!(d.failed.is_none(), "the list's copy says why itself");
        d.take_read(k.clone(), Ok(cards("example-prod")), Timestamp::now());
        assert!(d.shown().is_some_and(|c| c.is_ok_and(|c| c.note.is_none())) && d.failed.is_none());
        d.take_read(k.clone(), Err("token expired\nat …".into()), Timestamp::now());
        assert!(d.shown().is_some_and(|c| c.is_ok_and(|c| c.note.is_none())), "kept");
        assert_eq!(d.failed.as_deref(), Some("Not read now (token expired): shown as last read."));
        d.take_read(k.clone(), Ok(held()), Timestamp::now());
        assert!(d.shown().is_some_and(|c| c.is_ok_and(|c| c.note.is_none())), "kept over the list's copy");
        assert_eq!(d.failed.as_deref(), Some("Not read now (401 Unauthorized): shown as last read."));
        d.take_read(k.clone(), Ok(cards("example-prod")), Timestamp::now());
        assert!(d.failed.is_none());
        let mut other = Dashboard::new("shop".into(), vec![k.clone()], 0);
        other.take_read(k, Err("down".into()), Timestamp::now());
        assert!(other.shown().is_some_and(|c| c.is_err()) && other.failed.is_none());
    }

    #[derive(Default)]
    struct Page {
        reads: usize,
        tick: Option<Task<()>>,
    }

    impl Page {
        fn read(&mut self, _: &mut Context<Self>) {
            self.reads += 1;
        }
    }

    /// The re-read while a sync runs waits for a hidden page to show, rather than being lost.
    #[gpui_kit::test]
    fn a_tick_waits_for_the_page_to_show(cx: &mut TestAppContext) {
        let page = cx.new(|_| Page::default());
        page.update(cx, |p, cx| p.tick = Some(when_shown(cx, Duration::from_secs(3), Page::read)));
        cx.executor().advance_clock(Duration::from_secs(10));
        cx.run_until_parked();
        assert_eq!(page.read_with(cx, |p, _| p.reads), 0, "hidden: not read");
        let id = page.entity_id();
        cx.update(|cx| crate::app::show_for_test(cx, id, true));
        cx.executor().advance_clock(Duration::from_secs(1));
        cx.run_until_parked();
        assert_eq!(page.read_with(cx, |p, _| p.reads), 1, "shown: read once");
        // Dropped before its time: nothing.
        page.update(cx, |p, cx| p.tick = Some(when_shown(cx, Duration::from_secs(3), Page::read)));
        page.update(cx, |p, _| p.tick = None);
        cx.executor().advance_clock(Duration::from_secs(5));
        cx.run_until_parked();
        assert_eq!(page.read_with(cx, |p, _| p.reads), 1);
        cx.update(|cx| crate::app::show_for_test(cx, id, false));
    }
}
