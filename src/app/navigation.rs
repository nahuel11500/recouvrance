//! Back/forward history and tabs.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::base::InteractiveElementExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::scroll::ScrollableMask;
use gpui_kit::component::{ActiveTheme as _, IconName, Sizable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::overviews::Overview;
use super::{AppView, MAC, Route, TabState, VisiblePages, with_keys};
use crate::k8s::Kind;

impl AppView {
    pub(super) fn tab_snapshot(&self, cx: &App) -> TabState {
        TabState {
            history: self.history.clone(),
            pos: self.pos,
            filter: self.filter.read(cx).value().to_string(),
            namespaces: self.namespaces.clone(),
            kind: self.kind.clone(),
            // Opened already: its page is in the history.
            open: None,
        }
    }

    pub(super) fn load_tab(&mut self, t: TabState, window: &mut Window, cx: &mut Context<Self>) {
        self.history = t.history;
        self.pos = t.pos.min(self.history.len().saturating_sub(1));
        // Set the namespaces and the kind first, then restart the watch once: restarting in
        // between would start a list of the old kind in the new namespaces, then drop it.
        let mut restart = false;
        if t.namespaces != self.namespaces {
            let sel = t.namespaces.clone();
            self.ns_picker.update(cx, |p, cx| {
                p.selected = sel.into_iter().collect();
                cx.notify();
            });
            self.choose_namespaces(t.namespaces, cx);
            restart = true;
        }
        if let Some(kind) = new_kind(self.route(), t.kind, self.kind.as_ref()) {
            self.kind = Some(kind);
            restart = true;
        }
        // The tab's filter, for its kind: set before the watch restarts, which applies it.
        self.put_filter(t.filter, window, cx);
        if restart {
            self.restart_watch(cx);
        }
        self.apply_route(cx);
        // A tab reopened on an overview builds it now that it is shown.
        if let Some(page) = t.open {
            self.open_overview(page, window, cx);
        }
    }

    /// A new tab showing the current page (Ctrl+T), or `route` when given.
    pub(super) fn new_tab(&mut self, route: Option<Route>, window: &mut Window, cx: &mut Context<Self>) {
        let snap = self.tab_snapshot(cx);
        let route = route.unwrap_or_else(|| self.route().clone());
        let mut fresh = snap.clone();
        fresh.history = vec![route];
        fresh.pos = 0;
        self.tabs[self.active_tab] = snap;
        self.tabs.insert(self.active_tab + 1, TabState::default());
        self.active_tab += 1;
        self.load_tab(fresh, window, cx);
    }

    pub fn switch_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix == self.active_tab || ix >= self.tabs.len() {
            return;
        }
        self.tabs[self.active_tab] = self.tab_snapshot(cx);
        let next = std::mem::take(&mut self.tabs[ix]);
        self.active_tab = ix;
        self.load_tab(next, window, cx);
    }

    /// Close the tab shown (Ctrl+W). ⌘W on macOS closes the window at its last tab, as Mac apps
    /// do; Linux keeps its last tab.
    pub(super) fn close_tab_or_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if closes_window(self.tabs.len(), MAC) {
            window.remove_window();
        } else {
            self.close_tab(self.active_tab, window, cx);
        }
    }

    pub fn close_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 || ix >= self.tabs.len() {
            return;
        }
        if ix == self.active_tab {
            self.tabs.remove(ix);
            self.active_tab = ix.min(self.tabs.len() - 1);
            let next = std::mem::take(&mut self.tabs[self.active_tab]);
            self.load_tab(next, window, cx);
        } else {
            self.tabs.remove(ix);
            if ix < self.active_tab {
                self.active_tab -= 1;
            }
            self.session_changed(cx);
            cx.notify();
        }
    }

    pub(super) fn route_title(&self, route: &Route, cx: &App) -> String {
        match route {
            Route::Resources(Some(k)) => k.label.clone(),
            Route::Resources(None) => "Resources".into(),
            Route::Detail(d) => d.read(cx).title(),
            Route::Workloads(_)
            | Route::Cluster(_)
            | Route::Problems(_)
            | Route::Timeline(_)
            | Route::Helm(_)
            | Route::Argo(_) => Overview::of(route).map(|(o, _)| o.title().to_string()).unwrap_or_default(),
            Route::Access(_) => "My access".into(),
            Route::HelmInstall(_) => "Helm install".into(),
            Route::Assistant(_) => "Assistant".into(),
            Route::HelmRelease(_) => "Helm release".into(),
            Route::Contexts => "Contexts".into(),
            Route::Settings => "Settings".into(),
            Route::Terminal(_) => "Terminal".into(),
            Route::Apply(_) => "Create / apply".into(),
            Route::Agents => "Agents".into(),
        }
    }

    /// The tab strip, shown once there are two tabs or more.
    pub(super) fn render_tabs(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.tabs.len() < 2 {
            return None;
        }
        let theme = cx.theme().clone();
        let titles: Vec<String> = self
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let (route, kind) = if i == self.active_tab {
                    (Some(self.route()), self.kind.as_ref())
                } else if let Some(page) = t.open {
                    // Reopened on an overview built when shown: its history is still the list.
                    return page.title().to_string();
                } else {
                    (t.history.get(t.pos), t.kind.as_ref())
                };
                match (route, kind) {
                    (Some(Route::Resources(None)), Some(k)) => k.label.clone(),
                    (Some(r), _) => self.route_title(r, cx),
                    (None, _) => String::new(),
                }
            })
            .collect();
        let strip = h_flex()
            .id("tab-strip")
            .w_full()
            .h(px(32.))
            .px_1()
            .gap_0p5()
            .items_end()
            .overflow_x_scroll()
            .lock_scroll_axis()
            .track_scroll(&self.tab_scroll)
            .bg(theme.secondary.opacity(0.5))
            .border_b_1()
            .border_color(theme.border)
            .children(titles.into_iter().enumerate().map(|(i, title)| {
                let active = i == self.active_tab;
                h_flex()
                    .id(("tab", i))
                    .flex_shrink_0()
                    .max_w(px(220.))
                    .h(px(28.))
                    .pl_3()
                    .pr_1()
                    .gap_1()
                    .rounded_t_md()
                    .cursor_pointer()
                    .when(active, |d| d.bg(theme.background).border_1().border_b_0().border_color(theme.border))
                    .when(!active, |d| d.hover(|s| s.bg(theme.list_hover)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .when(!active, |d| d.text_color(theme.muted_foreground))
                            .child(title),
                    )
                    .child(
                        Button::new(("close-tab", i))
                            .xsmall()
                            .ghost()
                            .icon(IconName::Close)
                            .tooltip(with_keys("Close tab ({secondary-w})"))
                            .on_click(cx.listener(move |this, _, w, cx| {
                                this.close_tab(i, w, cx);
                                cx.stop_propagation();
                            })),
                    )
                    .on_click(cx.listener(move |this, _, w, cx| this.switch_tab(i, w, cx)))
            }))
            .child(
                Button::new("new-tab")
                    .xsmall()
                    .ghost()
                    .icon(Lucide::Plus)
                    .tooltip(with_keys("New tab ({secondary-t})"))
                    .on_click(cx.listener(|this, _, w, cx| this.new_tab(None, w, cx))),
            );
        // A plain mouse wheel scrolls the tabs sideways (it has no other axis).
        Some(
            div()
                .relative()
                .w_full()
                .child(strip)
                .child(ScrollableMask::vertical_wheel(&self.tab_scroll))
                .into_any_element(),
        )
    }

    pub(super) fn navigate(&mut self, route: Route, cx: &mut Context<Self>) {
        self.history.truncate(self.pos + 1);
        self.history.push(route);
        // Keep history bounded; details hold live streams.
        if self.history.len() > 50 {
            self.history.remove(0);
        }
        self.pos = self.history.len() - 1;
        self.apply_route(cx);
    }

    pub(super) fn go(&mut self, delta: isize, cx: &mut Context<Self>) {
        let next = self.pos as isize + delta;
        if next >= 0 && (next as usize) < self.history.len() {
            self.pos = next as usize;
            self.apply_route(cx);
        }
    }

    /// Record which page this window shows, and whether the window is hidden (see
    /// `wait_visible`). The list pauses at once when it leaves the screen.
    pub(super) fn mark_visible(&mut self, cx: &mut Context<Self>) {
        let page = match self.route() {
            Route::Detail(e) => Some(e.entity_id()),
            Route::Workloads(e) => Some(e.entity_id()),
            Route::Cluster(e) => Some(e.entity_id()),
            Route::Problems(e) => Some(e.entity_id()),
            Route::Timeline(e) => Some(e.entity_id()),
            Route::Access(e) => Some(e.entity_id()),
            Route::HelmInstall(e) => Some(e.entity_id()),
            Route::Assistant(e) => Some(e.entity_id()),
            Route::Helm(e) => Some(e.entity_id()),
            Route::HelmRelease(e) => Some(e.entity_id()),
            Route::Argo(e) => Some(e.entity_id()),
            Route::Terminal(e) => Some(e.entity_id()),
            Route::Apply(e) => Some(e.entity_id()),
            Route::Resources(_) | Route::Contexts | Route::Settings | Route::Agents => None,
        };
        self.list_pause.set(!self.list_on_screen());
        let (me, hidden) = (cx.entity_id(), self.window_hidden);
        let visible = cx.default_global::<VisiblePages>();
        match page {
            Some(p) => visible.pages.insert(me, p),
            None => visible.pages.remove(&me),
        };
        if hidden {
            visible.hidden.insert(me);
        } else {
            visible.hidden.remove(&me);
        }
    }

    pub(super) fn apply_route(&mut self, cx: &mut Context<Self>) {
        self.mark_visible(cx);
        // Protection may have changed in Settings since the page was last shown.
        if self.route().is_agents() {
            self.agents.update(cx, |a, _| a.refresh_contexts());
        }
        if let Route::Resources(Some(kind)) = self.route().clone()
            && self.kind.as_ref().map(Kind::key) != Some(kind.key())
        {
            self.kind = Some(kind);
            self.restart_watch(cx);
        }
        self.session_changed(cx);
        cx.notify();
    }
}

/// Whether closing the tab shown closes its window instead: at the last tab, on macOS.
fn closes_window(tabs: usize, mac: bool) -> bool {
    mac && tabs < 2
}

/// The kind a tab shows, when it is not `current`: its route's, else the one it kept.
/// Taken from the route, `apply_route` has nothing left to switch (and restart).
fn new_kind(route: &Route, kept: Option<Kind>, current: Option<&Kind>) -> Option<Kind> {
    let kind = match route {
        Route::Resources(Some(k)) => Some(k.clone()),
        _ => kept,
    }?;
    (Some(kind.key()) != current.map(Kind::key)).then_some(kind)
}

#[cfg(test)]
mod tests {
    use super::{Kind, Route, closes_window, new_kind};

    #[test]
    fn cmd_w_closes_the_window_at_the_last_tab_on_macos_only() {
        assert!(closes_window(1, true), "macOS: the last tab's ⌘W closes the window");
        assert!(!closes_window(2, true), "macOS: with two tabs, ⌘W closes one");
        assert!(!closes_window(1, false), "Linux keeps its last tab");
        assert!(!closes_window(3, false));
    }

    fn kind(k: &str) -> Kind {
        Kind::fake("", k)
    }

    #[test]
    fn a_tab_switches_kind_once() {
        let (pods, deploys) = (kind("Pod"), kind("Deployment"));
        let list = Route::Resources(Some(pods.clone()));
        // The route's kind, so showing the route doesn't switch (and restart) again.
        assert_eq!(new_kind(&list, Some(deploys.clone()), Some(&deploys)).map(|k| k.label), Some("Pod".into()));
        assert!(new_kind(&list, Some(pods.clone()), Some(&pods)).is_none());
        assert_eq!(new_kind(&Route::Contexts, Some(pods), Some(&deploys)).map(|k| k.label), Some("Pod".into()));
        assert!(new_kind(&Route::Contexts, None, Some(&deploys)).is_none());
    }
}
