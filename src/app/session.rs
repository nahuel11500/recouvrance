//! Reopening where the app was left. Each set of contexts keeps its tabs (page, resource
//! type, namespaces, filter) and each type's filter in settings.json; the first window that
//! connects to that set reopens them. The window keeps its place and size.
//!
//! Saved a moment after each change and when the window closes or the app quits, never while
//! connecting: a launch whose reconnect fails keeps what was saved.

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

    /// Where this window is now on its set of contexts; none while it is connecting or not
    /// connected, so a failed or unfinished connect never overwrites what was saved.
    fn session_snapshot(&self, cx: &App) -> Option<Session> {
        if self.session_key.is_empty() || self.connecting || self.clusters.is_empty() {
            return None;
        }
        let shown = self.tab_snapshot(cx);
        let tabs = (0..self.tabs.len())
            .map(|i| saved_tab(if i == self.active_tab { &shown } else { &self.tabs[i] }))
            .collect();
        let by_key: HashMap<String, String> = self.kinds.iter().map(|k| (k.key(), k.kubectl_name())).collect();
        let filters = (self.kind_filters.all(&shown.filter).into_iter())
            .filter_map(|(key, text)| Some((by_key.get(&key)?.clone(), text)))
            .collect();
        Some(Session { contexts: self.session_key.clone(), tabs, active_tab: self.active_tab, filters })
    }

    /// Save where this window is now, at once (the window closes, the app quits, another set
    /// of contexts is connected).
    pub(super) fn save_session_now(&mut self, cx: &App) {
        self._save_session = None;
        if let Some(session) = self.session_snapshot(cx) {
            crate::settings::save_session(session);
        }
    }

    /// Something saved changed: save it once changes stop for a moment. The file is written
    /// off the UI thread.
    pub(super) fn session_changed(&mut self, cx: &mut Context<Self>) {
        if self.session_key.is_empty() || self.connecting {
            return;
        }
        // Replacing the task cancels the wait before: one save per burst of changes.
        self._save_session = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
            let Ok(Some(session)) = this.update(cx, |this, cx| this.session_snapshot(cx)) else { return };
            cx.background_spawn(async move { crate::settings::save_session(session) }).await;
        }));
    }

    /// The window moved, was resized or maximized: remember where, once it stops.
    pub(super) fn window_moved(&mut self, window: &Window, cx: &mut Context<Self>) {
        let place = place_of(window.window_bounds());
        self._save_window = Some(cx.spawn(async move |_, cx| {
            cx.background_executor().timer(SAVE_AFTER).await;
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

/// The window's place to save: its restore size when maximized or fullscreen.
fn place_of(bounds: WindowBounds) -> WindowPlace {
    let (b, maximized) = match bounds {
        WindowBounds::Windowed(b) => (b, false),
        WindowBounds::Maximized(b) | WindowBounds::Fullscreen(b) => (b, true),
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

    use gpui_kit::{Bounds, WindowBounds, point, px, size};

    use super::{
        Kind, Overview, Route, SavedPage, SavedTab, Session, TabState, WindowPlace, filters_to_save, on_a_display,
        place_of, restored, saved_tab,
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
        assert_eq!(place_of(WindowBounds::Windowed(b)), place(10., 20., 1200., 800.));
        let max = place_of(WindowBounds::Maximized(b));
        assert!(max.maximized);
        assert_eq!(WindowPlace { maximized: false, ..max }, place(10., 20., 1200., 800.));
        assert!(place_of(WindowBounds::Fullscreen(b)).maximized);
    }
}
