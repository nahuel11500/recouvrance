//! Reopening where the app was left. Each set of contexts keeps its tabs (page, resource
//! type, namespaces, filter) and each type's filter in settings.json; the first window that
//! connects to that set reopens them. The window keeps its place and size.
//!
//! Saved a moment after each change and when the window closes or the app quits, never while
//! connecting: a launch whose reconnect fails keeps what was saved. Nor after a connect that
//! reached only some of the clusters, or could not list one's namespaces: the types and
//! namespaces only that one has would read as gone, and be saved so. With two windows on one
//! set, only one saves it (`SessionOwners`).

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use gpui_kit::*;

use super::overviews::Overview;
use super::{AppView, Route, TabState};
use crate::k8s::Kind;
use crate::settings::{SavedPage, SavedTab, Session, WindowPlace};

/// How long the app waits for changes to stop before saving them.
const SAVE_AFTER: Duration = Duration::from_secs(2);

/// Set once the first window has opened: only that one reopens where the app was left. A
/// window opened after it starts fresh, as a new window does.
pub(super) struct FirstWindowOpened;

impl Global for FirstWindowOpened {}

/// Which window saves each set of contexts (by `settings::session_set`). Two windows on one
/// set would else save over each other, the last to quit winning. The one that reopened the
/// set, or connected to it first, saves it, until the user changes something in another.
#[derive(Default)]
pub(super) struct SessionOwners(HashMap<Vec<String>, EntityId>);

impl Global for SessionOwners {}

impl SessionOwners {
    pub(super) fn owner(&self, set: &[String]) -> Option<EntityId> {
        self.0.get(set).copied()
    }

    fn owns(&self, set: &[String], window: EntityId) -> bool {
        self.owner(set) == Some(window)
    }

    fn take(&mut self, set: &[String], window: EntityId) {
        if !set.is_empty() {
            self.0.insert(set.to_vec(), window);
        }
    }

    /// The window leaves the set: closed, or connected to another.
    fn leave(&mut self, set: &[String], window: EntityId) {
        if self.owns(set, window) {
            self.0.remove(set);
        }
    }

    /// After `window` connected to `set`: it saves the set when it reopened it or no other
    /// window did before (`before`). What the connect itself changed doesn't count, and a
    /// connect where a cluster `failed` changes nothing: that window can't save the set, and
    /// must not stop a healthy one from saving it. Whether `window` saves the set now.
    fn settle(
        &mut self,
        set: &[String],
        window: EntityId,
        before: Option<EntityId>,
        reopened: bool,
        failed: bool,
    ) -> bool {
        if !failed {
            let owner = if reopened { window } else { before.unwrap_or(window) };
            self.take(set, owner);
        }
        self.owns(set, window)
    }
}

/// Whether a connect reopens where its set of contexts was left. Only the first window does,
/// for the first set it connects to; it stays to do until a connect to that set reaches every
/// cluster. A connect where one failed reopens what it can, but its types, namespaces and
/// filters only the failed cluster has fall back to the defaults: once the set connects in
/// full, it is reopened again from what was saved, not carried on from the reduced tabs.
#[derive(Debug, PartialEq)]
pub(super) enum Restore {
    Off,
    /// The first window, before its first connect.
    FirstConnect,
    /// The set to reopen (`settings::session_set`).
    Pending(Vec<String>),
}

impl Restore {
    /// A connect to `set` starts: the first one is the set to reopen.
    pub(super) fn connecting(&mut self, set: &[String]) {
        if *self == Self::FirstConnect {
            *self = if set.is_empty() { Self::Off } else { Self::Pending(set.to_vec()) };
        }
    }

    /// The connect to `set` finished, with a cluster `failed` or not: whether it reopens the
    /// set's session. One that reached every cluster is the last to.
    pub(super) fn finished(&mut self, set: &[String], failed: bool) -> bool {
        let Self::Pending(pending) = self else { return false };
        if pending.as_slice() != set {
            return false;
        }
        if !failed {
            *self = Self::Off;
        }
        true
    }

    /// The clusters a partial connect to `set` left connecting all joined it, and none
    /// failed: the connect is whole after all. Whether its session is reopened again, from
    /// what was saved: when it waited for a whole connect, and the tabs are `unchanged`
    /// since the partial one (the user's own changes are kept, not replaced).
    pub(super) fn whole(&mut self, set: &[String], unchanged: bool) -> bool {
        self.finished(set, false) && unchanged
    }
}

/// What a window's session may be saved by. None of it is saved without a set of contexts,
/// while connecting or not connected (a failed or unfinished connect never overwrites what
/// was saved), after a connect where a cluster `failed` (what only it has would read as
/// gone), or by a window that doesn't `own` the set (see `SessionOwners`).
#[derive(Clone, Copy)]
struct SaveGate {
    has_set: bool,
    connecting: bool,
    connected: bool,
    failed: bool,
    owns: bool,
}

impl SaveGate {
    fn saves(self) -> bool {
        self.has_set && !self.connecting && self.connected && !self.failed && self.owns
    }
}

impl AppView {
    /// The tabs, namespaces and filters saved for the contexts just connected, put in place
    /// before the list starts (the caller starts it once). The page the tab shown opens on,
    /// or none when nothing was saved for this set.
    pub(super) fn restore_session(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<SavedPage> {
        let saved = crate::settings::session(&self.session_key)?;
        let known = self.ns_picker.read(cx).known().to_vec();
        let mut restored = restored(&saved, &self.kinds, &known, self.kind.as_ref())?;
        self.kind_filters.restore(restored.filters);
        let shown = std::mem::take(&mut restored.tabs[restored.active]);
        self.tabs = restored.tabs;
        self.active_tab = restored.active;
        self.history = shown.history;
        self.pos = 0;
        self.kind = shown.kind;
        if shown.namespaces != self.namespaces {
            let picked = shown.namespaces.clone();
            self.ns_picker.update(cx, |p, cx| {
                p.selected = picked.into_iter().collect();
                cx.notify();
            });
            // One at a time where only some namespaces are accessible.
            self.choose_namespaces(shown.namespaces, cx);
        }
        self.put_filter(shown.filter, window, cx);
        Some(shown.open.map_or(SavedPage::List, saved_page))
    }

    /// Where this window is now on its set of contexts; none when it may not be saved (see
    /// `SaveGate`; a cluster failed when `connect_partial`: it didn't answer, or its
    /// namespaces didn't list, or it joined after the first rows).
    fn session_snapshot(&self, cx: &App) -> Option<Session> {
        let gate = SaveGate {
            has_set: !self.session_key.is_empty(),
            connecting: self.connecting,
            connected: !self.clusters.is_empty(),
            failed: self.connect_partial(),
            owns: cx.try_global::<SessionOwners>().is_some_and(|o| o.owns(&self.session_key, self.view_id)),
        };
        if !gate.saves() {
            return None;
        }
        let shown = self.tab_snapshot(cx);
        let tabs = self.saved_tabs(&shown);
        let by_key: HashMap<String, String> = self.kinds.iter().map(|k| (k.key(), k.kubectl_name())).collect();
        let filters = (self.kind_filters.all(&shown.filter).into_iter())
            .filter_map(|(key, text)| Some((by_key.get(&key)?.clone(), text)))
            .collect();
        Some(Session { contexts: self.session_key.clone(), tabs, active_tab: self.active_tab, filters })
    }

    /// The tabs as they would be saved, the one shown as `shown` (`tab_snapshot`).
    fn saved_tabs(&self, shown: &TabState) -> Vec<SavedTab> {
        (0..self.tabs.len()).map(|i| saved_tab(if i == self.active_tab { shown } else { &self.tabs[i] })).collect()
    }

    /// The tabs as they are now, to tell later whether the user changed them.
    pub(super) fn tabs_now(&self, cx: &App) -> (Vec<SavedTab>, usize) {
        (self.saved_tabs(&self.tab_snapshot(cx)), self.active_tab)
    }

    /// Every cluster left connecting by the last connect has joined, and none failed: the
    /// set is whole after all, and its session is saved again. When the connect reopened it
    /// without what only the late clusters have, it is reopened again from what was saved,
    /// unless the user changed the tabs meanwhile: theirs are kept then.
    pub(super) fn joined_whole(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.joined_late {
            return;
        }
        let unchanged = self.partial_tabs.take().is_some_and(|tabs| tabs == self.tabs_now(cx));
        self.joined_late = false;
        let owner_before = cx.try_global::<SessionOwners>().and_then(|o| o.owner(&self.session_key));
        let reopened =
            if self.restore.whole(&self.session_key, unchanged) { self.restore_session(window, cx) } else { None };
        if let Some(page) = reopened {
            self.restart_watch(cx);
            if let Some(page) = overview(page) {
                self.open_overview(page, window, cx);
            }
        }
        self.settle_session(owner_before, reopened.is_some(), cx);
    }

    /// Save where this window is now, at once (the window closes, the app quits, another set
    /// of contexts is connected).
    pub(super) fn save_session_now(&mut self, cx: &App) {
        self._save_session = None;
        if let Some(session) = self.session_snapshot(cx) {
            crate::settings::save_session(session);
        }
    }

    /// Leave the set of contexts connected: save where this window is on it, and let another
    /// window on it save it from now on.
    pub(super) fn leave_session(&mut self, cx: &mut App) {
        self.save_session_now(cx);
        let set = std::mem::take(&mut self.session_key);
        if cx.has_global::<SessionOwners>() {
            cx.global_mut::<SessionOwners>().leave(&set, self.view_id);
        }
    }

    /// Whether the last connect left out part of the set: a cluster failed or is still
    /// connecting (`connect_errors`), or one joined after the first rows (`joined_late`).
    fn connect_partial(&self) -> bool {
        self.connect_errors.is_some() || self.joined_late
    }

    /// The connect has finished: which window saves its set from now on (see
    /// `SessionOwners::settle`). When it is this one, the set is saved once things settle: a
    /// set connected for the first time is kept even if nothing changes before the app quits.
    pub(super) fn settle_session(&mut self, before: Option<EntityId>, reopened: bool, cx: &mut Context<Self>) {
        let failed = self.connect_partial();
        let owns =
            cx.default_global::<SessionOwners>().settle(&self.session_key, self.view_id, before, reopened, failed);
        if owns && !failed {
            self.session_changed(cx);
        }
    }

    /// Something saved changed: save it once changes stop for a moment. The file is written
    /// off the UI thread. The user changed it here: this window saves its set from now on,
    /// unless a cluster failed in its connect (it can't save, and another window may).
    pub(super) fn session_changed(&mut self, cx: &mut Context<Self>) {
        if self.session_key.is_empty() || self.connecting {
            return;
        }
        if !self.connect_partial() {
            cx.default_global::<SessionOwners>().take(&self.session_key, self.view_id);
        }
        // Replacing the task cancels the wait before: one save per burst of changes.
        self._save_session = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
            let Ok(Some(session)) = this.update(cx, |this, cx| this.session_snapshot(cx)) else { return };
            cx.background_spawn(async move { crate::settings::save_session(session) }).await;
        }));
    }

    /// The window moved, was resized or maximized: remember where, once it stops.
    ///
    /// The bounds are read once it stops, not as it moves: on X11 the new size of a window
    /// being maximized can come before its maximized state (the desktop picks the order, and
    /// gpui reports a change of state alone to no one), so the size reported first reads as
    /// a windowed place the size of the screen.
    pub(super) fn window_moved(&mut self, window: &Window, cx: &mut Context<Self>) {
        self._save_window = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
            let Ok(place) = this.update_in(cx, |this, window, _| {
                // Maximized before this window was ever windowed: the place saved last time.
                let place = place_of(window.window_bounds(), this.windowed.or_else(crate::settings::window_place));
                if !place.maximized {
                    this.windowed = Some(place);
                }
                place
            }) else {
                return;
            };
            cx.background_spawn(async move { crate::settings::save_window(place) }).await;
        }));
    }
}

/// Where the first window opens: where the app was left when that is still on a display, else
/// centered at the size it had (made to fit the display), else the default size, centered.
pub(super) fn first_window_bounds(cx: &App) -> WindowBounds {
    let default = size(px(crate::settings::WINDOW_SIZE.0), px(crate::settings::WINDOW_SIZE.1));
    let Some(place) = crate::settings::window_place() else {
        return WindowBounds::Windowed(Bounds::centered(None, default, cx));
    };
    let saved = Bounds { origin: point(px(place.x), px(place.y)), size: size(px(place.width), px(place.height)) };
    let displays: Vec<Bounds<Pixels>> = cx.displays().iter().map(|d| d.visible_bounds()).collect();
    let bounds = if on_a_display(&place, &displays) { saved } else { Bounds::centered(None, saved.size, cx) };
    if place.maximized { WindowBounds::Maximized(bounds) } else { WindowBounds::Windowed(bounds) }
}

/// Enough of the title bar to grab it: a window less on screen than this is moved back.
const GRAB: (f32, f32) = (120., 40.);

/// Whether enough of the window's top shows on one of the displays to drag it. A display
/// unplugged since, or a resolution made smaller, can leave it out of reach.
fn on_a_display(place: &WindowPlace, displays: &[Bounds<Pixels>]) -> bool {
    displays.iter().any(|d| {
        let (dx, dy) = (f32::from(d.origin.x), f32::from(d.origin.y));
        let (dw, dh) = (f32::from(d.size.width), f32::from(d.size.height));
        let left = place.x.max(dx);
        let right = (place.x + place.width).min(dx + dw);
        // The title bar on this display. Half of it may be above the top: a window put at the
        // very top can report its shadow's edge there.
        right - left >= GRAB.0 && place.y + GRAB.1 / 2. >= dy && place.y + GRAB.1 <= dy + dh
    })
}

/// The window's place to save. Maximized or fullscreen, it keeps the place it had `windowed`:
/// X11 gives the screen's size there, not the size to restore (Wayland gives that one).
fn place_of(bounds: WindowBounds, windowed: Option<WindowPlace>) -> WindowPlace {
    let (b, maximized) = match bounds {
        WindowBounds::Windowed(b) => (b, false),
        WindowBounds::Maximized(b) | WindowBounds::Fullscreen(b) => match windowed {
            Some(place) => return WindowPlace { maximized: true, ..place },
            None => (b, true),
        },
    };
    WindowPlace {
        x: f32::from(b.origin.x),
        y: f32::from(b.origin.y),
        width: f32::from(b.size.width),
        height: f32::from(b.size.height),
        maximized,
    }
}

/// A saved session made to fit the clusters connected now.
struct Restored {
    tabs: Vec<TabState>,
    /// Within `tabs`.
    active: usize,
    /// Each type's filter, by `Kind::key`.
    filters: HashMap<String, String>,
}

/// The tabs of `saved` on the clusters connected now: a resource type they don't serve shows
/// `fallback` (its filter dropped, as it was typed for another), a namespace not in `known` is
/// dropped, a page that doesn't read is the list. None when nothing was saved.
fn restored(saved: &Session, kinds: &[Kind], known: &[String], fallback: Option<&Kind>) -> Option<Restored> {
    let named = |name: &str| kinds.iter().find(|k| k.kubectl_name() == name);
    let tabs: Vec<TabState> = (saved.tabs.iter())
        .map(|t| {
            let own = t.kind.as_deref().and_then(named);
            let kind = own.or(fallback).cloned();
            TabState {
                history: vec![Route::Resources(kind.clone())],
                pos: 0,
                filter: if own.is_some() { t.filter.clone() } else { String::new() },
                namespaces: t.namespaces.iter().filter(|ns| known.contains(ns)).cloned().collect(),
                kind,
                open: overview(t.page),
            }
        })
        .collect();
    let active = saved.active_tab.min(tabs.len().checked_sub(1)?);
    let filters = (saved.filters.iter()).filter_map(|(name, text)| Some((named(name)?.key(), text.clone()))).collect();
    Some(Restored { tabs, active, filters })
}

/// A tab as it is saved: the page it shows, or the list or overview it went to it from.
fn saved_tab(t: &TabState) -> SavedTab {
    let page = t.open.map_or_else(
        || {
            (t.history.iter().take(t.pos + 1).rev())
                .find_map(|r| match r {
                    Route::Resources(_) => Some(SavedPage::List),
                    r => Overview::of(r).map(|(o, _)| saved_page(o)),
                })
                .unwrap_or_default()
        },
        saved_page,
    );
    SavedTab {
        page,
        kind: t.kind.as_ref().map(Kind::kubectl_name),
        namespaces: t.namespaces.clone(),
        filter: t.filter.clone(),
    }
}

fn saved_page(o: Overview) -> SavedPage {
    match o {
        Overview::Workloads => SavedPage::Workloads,
        Overview::Cluster => SavedPage::Cluster,
        Overview::Problems => SavedPage::Problems,
        Overview::Timeline => SavedPage::Timeline,
        Overview::Helm => SavedPage::Helm,
        Overview::Argo => SavedPage::Argo,
    }
}

/// The overview a saved page opens; none for the list.
pub(super) fn overview(page: SavedPage) -> Option<Overview> {
    Some(match page {
        SavedPage::List => return None,
        SavedPage::Workloads => Overview::Workloads,
        SavedPage::Cluster => Overview::Cluster,
        SavedPage::Problems => Overview::Problems,
        SavedPage::Timeline => Overview::Timeline,
        SavedPage::Helm => Overview::Helm,
        SavedPage::Argo => Overview::Argo,
    })
}

/// The filters to save from `by_kind`, with `typed` for the type the box shows now; empty
/// ones are left out.
pub(super) fn filters_to_save(
    by_kind: &HashMap<String, String>,
    shown_for: Option<&String>,
    typed: &str,
) -> BTreeMap<String, String> {
    let mut all: BTreeMap<String, String> = by_kind.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    if let Some(kind) = shown_for {
        all.insert(kind.clone(), typed.to_string());
    }
    all.retain(|_, text| !text.trim().is_empty());
    all
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use gpui_kit::{Bounds, EntityId, WindowBounds, point, px, size};

    use super::{
        Kind, Overview, Restore, Route, SaveGate, SavedPage, SavedTab, Session, SessionOwners, TabState, WindowPlace,
        filters_to_save, on_a_display, place_of, restored, saved_tab,
    };

    fn kind(group: &str, k: &str) -> Kind {
        Kind::fake(group, k)
    }

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    fn tab(page: SavedPage, kind: &str, namespaces: &[&str], filter: &str) -> SavedTab {
        SavedTab { page, kind: Some(kind.into()), namespaces: v(namespaces), filter: filter.into() }
    }

    #[test]
    fn a_session_reopens_on_what_the_clusters_still_have() {
        let (pods, deploys) = (kind("", "Pod"), kind("apps", "Deployment"));
        let saved = Session {
            contexts: v(&["prod"]),
            tabs: vec![
                tab(SavedPage::List, "deployments.apps", &["team-a", "gone"], "web"),
                // A type the clusters no longer serve: the default list, unfiltered.
                tab(SavedPage::Problems, "certificates.cert-manager.io", &["team-a"], "expired"),
            ],
            active_tab: 7,
            filters: [("deployments.apps".to_string(), "web".to_string()), ("gone.example.com".into(), "x".into())]
                .into(),
        };
        let kinds = [pods.clone(), deploys.clone()];
        let r = restored(&saved, &kinds, &v(&["team-a", "team-b"]), Some(&pods)).unwrap();
        assert_eq!(r.active, 1, "the last tab when the one saved is past the end");
        let [first, second] = &r.tabs[..] else { panic!("two tabs") };
        assert_eq!(first.kind.as_ref().map(Kind::key), Some(deploys.key()));
        assert!(matches!(&first.history[..], [Route::Resources(Some(k))] if k.key() == deploys.key()));
        assert_eq!((first.namespaces.clone(), first.filter.as_str(), first.open), (v(&["team-a"]), "web", None));
        assert_eq!(second.kind.as_ref().map(Kind::key), Some(pods.key()));
        assert_eq!((second.filter.as_str(), second.open), ("", Some(Overview::Problems)));
        assert_eq!(r.filters.len(), 1);
        assert_eq!(r.filters[&deploys.key()], "web");
        // Nothing saved: nothing to reopen.
        assert!(restored(&Session::default(), &kinds, &[], Some(&pods)).is_none());
    }

    #[test]
    fn a_tab_saves_its_list_or_the_overview_it_came_from() {
        let pods = kind("", "Pod");
        let mut t = TabState {
            history: vec![Route::Resources(Some(pods.clone())), Route::Contexts, Route::Settings],
            pos: 2,
            filter: "api".into(),
            namespaces: v(&["team-a"]),
            kind: Some(pods),
            open: None,
        };
        let saved = saved_tab(&t);
        assert_eq!(saved, tab(SavedPage::List, "pods", &["team-a"], "api"));
        // An overview not opened yet is saved as it was.
        t.open = Some(Overview::Argo);
        assert_eq!(saved_tab(&t).page, SavedPage::Argo);
        // Nothing to go back to: the list.
        let t = TabState { history: vec![Route::Contexts], ..TabState::default() };
        assert_eq!(saved_tab(&t).page, SavedPage::List);
    }

    #[test]
    fn saved_tabs_reopen_as_they_were() {
        let (pods, deploys) = (kind("", "Pod"), kind("apps", "Deployment"));
        let kinds = [pods.clone(), deploys];
        let tabs = vec![
            tab(SavedPage::List, "deployments.apps", &["team-a"], "web"),
            tab(SavedPage::Workloads, "pods", &[], ""),
            tab(SavedPage::Helm, "pods", &["team-a", "team-b"], "x"),
        ];
        let saved = Session { contexts: v(&["a", "b"]), tabs: tabs.clone(), active_tab: 1, ..Session::default() };
        let r = restored(&saved, &kinds, &v(&["team-a", "team-b"]), Some(&pods)).unwrap();
        let again: Vec<SavedTab> = r.tabs.iter().map(saved_tab).collect();
        assert_eq!(again, tabs);
    }

    #[test]
    fn filters_saved_leave_out_empty_ones_and_take_the_box_for_its_type() {
        let by_kind: HashMap<String, String> =
            [("v1/pods".to_string(), "old".to_string()), ("v1/nodes".into(), " ".into())].into();
        let saved = filters_to_save(&by_kind, Some(&"v1/pods".to_string()), "web");
        assert_eq!(saved, BTreeMap::from([("v1/pods".to_string(), "web".to_string())]));
        assert!(filters_to_save(&by_kind, Some(&"v1/pods".to_string()), "").is_empty());
    }

    fn place(x: f32, y: f32, width: f32, height: f32) -> WindowPlace {
        WindowPlace { x, y, width, height, maximized: false }
    }

    #[test]
    fn a_window_out_of_reach_is_moved_back() {
        let screens = [
            Bounds { origin: point(px(0.), px(0.)), size: size(px(1920.), px(1080.)) },
            Bounds { origin: point(px(1920.), px(0.)), size: size(px(2560.), px(1440.)) },
        ];
        assert!(on_a_display(&place(100., 100., 1500., 920.), &screens));
        // At the very top, its shadow's edge just above.
        assert!(on_a_display(&place(100., -10., 1500., 920.), &screens));
        assert!(on_a_display(&place(3000., 200., 1500., 920.), &screens[1..]));
        // The second display unplugged.
        assert!(!on_a_display(&place(3000., 200., 1500., 920.), &screens[..1]));
        // Its title bar above the top, or only a sliver at the side.
        assert!(!on_a_display(&place(100., -300., 1500., 920.), &screens));
        assert!(!on_a_display(&place(-1450., 100., 1500., 920.), &screens[..1]));
        assert!(!on_a_display(&place(100., 100., 1500., 920.), &[]));
    }

    #[test]
    fn a_maximized_window_saves_its_restore_size() {
        let b = Bounds { origin: point(px(10.), px(20.)), size: size(px(1200.), px(800.)) };
        let before = place(50., 60., 1000., 700.);
        assert_eq!(place_of(WindowBounds::Windowed(b), Some(before)), place(10., 20., 1200., 800.));
        // X11 reports the screen's size when maximized: the place it had windowed is kept.
        let screen = Bounds { origin: point(px(0.), px(0.)), size: size(px(1920.), px(1080.)) };
        for bounds in [WindowBounds::Maximized(screen), WindowBounds::Fullscreen(screen)] {
            assert_eq!(place_of(bounds, Some(before)), WindowPlace { maximized: true, ..before });
        }
        // Never windowed, nothing saved: what the desktop gives.
        let max = place_of(WindowBounds::Maximized(b), None);
        assert!(max.maximized);
        assert_eq!(WindowPlace { maximized: false, ..max }, place(10., 20., 1200., 800.));
    }

    #[test]
    fn one_window_saves_each_set_of_contexts() {
        let (first, second): (EntityId, EntityId) = (1u64.into(), 2u64.into());
        let (set, other) = (v(&["hub", "prod"]), v(&["dev"]));
        let mut owners = SessionOwners::default();
        // The first window reopens the set; a second one connects to it and starts fresh:
        // its own connect (the overview it opens at start) doesn't make it the one saving.
        assert!(owners.settle(&set, first, None, true, false));
        owners.take(&set, second);
        assert!(!owners.settle(&set, second, Some(first), false, false));
        assert_eq!(owners.owner(&set), Some(first));
        // The user changes something in the second: it saves the set from now on.
        owners.take(&set, second);
        assert!(owners.owns(&set, second) && !owners.owns(&set, first));
        // The first leaving doesn't free what it no longer saves; the second leaving does.
        owners.leave(&set, first);
        assert_eq!(owners.owner(&set), Some(second));
        owners.leave(&set, second);
        assert_eq!(owners.owner(&set), None);
        // A set nobody saved: the window connecting to it saves it, even with nothing reopened.
        assert!(owners.settle(&other, second, None, false, false));
        assert_eq!(owners.owner(&other), Some(second));
        // No set (contexts given by a developer hook): nothing is saved.
        owners.take(&[], first);
        assert_eq!(owners.owner(&[]), None);
    }

    #[test]
    fn a_session_is_saved_only_when_every_condition_holds() {
        let ok = SaveGate { has_set: true, connecting: false, connected: true, failed: false, owns: true };
        assert!(ok.saves());
        for gate in [
            SaveGate { has_set: false, ..ok },
            SaveGate { connecting: true, ..ok },
            SaveGate { connected: false, ..ok },
            // A cluster didn't answer: what only it has would be saved as gone.
            SaveGate { failed: true, ..ok },
            SaveGate { owns: false, ..ok },
        ] {
            assert!(!gate.saves());
        }
    }

    #[test]
    fn a_set_is_reopened_until_it_connects_in_full() {
        let (set, other) = (v(&["hub", "prod"]), v(&["dev"]));
        // A window that isn't the first never reopens.
        let mut off = Restore::Off;
        off.connecting(&set);
        assert!(!off.finished(&set, false));
        // The first connect is the set to reopen: it reopens what it can while a cluster
        // fails, and stays to do.
        let mut restore = Restore::FirstConnect;
        restore.connecting(&set);
        assert!(restore.finished(&set, true));
        assert_eq!(restore, Restore::Pending(set.clone()));
        // Another set connected meanwhile is not reopened, and doesn't end it.
        restore.connecting(&other);
        assert!(!restore.finished(&other, false));
        // The set connects in full: reopened from what was saved, for the last time.
        restore.connecting(&set);
        assert!(restore.finished(&set, false));
        assert_eq!(restore, Restore::Off);
        assert!(!restore.finished(&set, false));
        // No set (contexts given by a developer hook): nothing to reopen.
        let mut none = Restore::FirstConnect;
        none.connecting(&[]);
        assert_eq!(none, Restore::Off);
    }

    /// #915: a launch where one cluster of the set doesn't answer, then a reconnect of the
    /// same set that reaches it. The reduced tabs of the first connect are never saved, nor
    /// can that window take the set from a healthy one; the reconnect reopens what was saved.
    #[test]
    fn a_partial_connect_never_saves_and_the_full_one_reopens() {
        let (window, other): (EntityId, EntityId) = (1u64.into(), 2u64.into());
        let set = v(&["hub", "prod"]);
        let mut owners = SessionOwners::default();
        let mut restore = Restore::FirstConnect;
        let gate = |owners: &SessionOwners, failed: bool| SaveGate {
            has_set: true,
            connecting: false,
            connected: true,
            failed,
            owns: owners.owns(&set, window),
        };
        // A healthy window has the set.
        owners.settle(&set, other, None, false, false);
        // The partial connect: it reopens what it can, but takes nothing and saves nothing.
        restore.connecting(&set);
        assert!(restore.finished(&set, true));
        assert!(!owners.settle(&set, window, Some(other), true, true));
        assert_eq!(owners.owner(&set), Some(other));
        assert!(!gate(&owners, true).saves());
        // Nor once the healthy one closed and nobody saves the set: it doesn't take it, and
        // even owning it, a failed connect saves nothing.
        owners.leave(&set, other);
        assert!(!owners.settle(&set, window, None, true, true));
        owners.take(&set, window);
        assert!(!gate(&owners, true).saves());
        // The full reconnect: reopened from the file, then this window saves the set.
        restore.connecting(&set);
        assert!(restore.finished(&set, false));
        assert!(owners.settle(&set, window, None, true, false));
        assert!(gate(&owners, false).saves());
    }

    /// A cluster always a few seconds behind the others joins after the first rows on every
    /// launch. Once it is in and none failed, the set saves again: else it would never be
    /// saved, nor a set connected for the first time saved at all.
    #[test]
    fn a_set_whole_after_a_late_join_saves_again() {
        let window: EntityId = 1u64.into();
        let set = v(&["hub", "prod"]);
        let gate = |owners: &SessionOwners, failed: bool| SaveGate {
            has_set: true,
            connecting: false,
            connected: true,
            failed,
            owns: owners.owns(&set, window),
        };
        let waiting = v(&["prod"]);
        // The first rows, "prod" still connecting: partial, nothing saved.
        let mut owners = SessionOwners::default();
        let mut restore = Restore::FirstConnect;
        restore.connecting(&set);
        assert!(super::super::reach::connect_status(&[], &waiting).is_some());
        assert!(restore.finished(&set, true));
        assert!(!owners.settle(&set, window, None, true, true));
        assert!(!gate(&owners, true).saves());
        // "prod" joins, none failed: whole. Nothing changed since, so the session is reopened
        // again, whole, for the last time, and saved from then on.
        assert!(super::super::reach::connect_status(&[], &[]).is_none());
        assert!(restore.whole(&set, true));
        assert_eq!(restore, Restore::Off);
        assert!(owners.settle(&set, window, None, true, false));
        assert!(gate(&owners, false).saves());
        // The user changed the tabs meanwhile: theirs are kept (not reopened), and saved.
        let (mut owners, mut restore) = (SessionOwners::default(), Restore::Pending(set.clone()));
        assert!(!restore.whole(&set, false));
        assert_eq!(restore, Restore::Off);
        assert!(owners.settle(&set, window, None, false, false));
        assert!(gate(&owners, false).saves());
        // A set connected for the first time, or by a window that doesn't reopen: nothing to
        // reopen, saved once whole.
        let (mut owners, mut restore) = (SessionOwners::default(), Restore::Off);
        assert!(!restore.whole(&set, true));
        assert!(owners.settle(&set, window, None, false, false));
        assert!(gate(&owners, false).saves());
    }
}
