//! Back/forward history and tabs.

#[allow(unused_imports)]
use super::*;

impl AppView {
    pub(super) fn tab_snapshot(&self, cx: &App) -> TabState {
        TabState {
            history: self.history.clone(),
            pos: self.pos,
            filter: self.filter.read(cx).value().to_string(),
            namespaces: self.namespaces.clone(),
            kind: self.kind.clone(),
        }
    }

    pub(super) fn load_tab(&mut self, t: TabState, window: &mut Window, cx: &mut Context<Self>) {
        self.history = t.history;
        self.pos = t.pos.min(self.history.len().saturating_sub(1));
        self.filter.update(cx, |s, cx| s.set_value(t.filter.clone(), window, cx));
        self.table.update(cx, |tb, cx| {
            tb.delegate_mut().set_filter(t.filter);
            cx.notify();
        });
        // `set_value` sends no change event: move the highlight here.
        self.follow_selection(cx);
        if t.namespaces != self.namespaces {
            let sel = t.namespaces.clone();
            self.ns_picker.update(cx, |p, cx| {
                p.selected = sel.into_iter().collect();
                cx.notify();
            });
            self.set_selected_namespaces(t.namespaces, cx);
        }
        if t.kind.is_some() && t.kind.as_ref().map(|k| k.key()) != self.kind.as_ref().map(|k| k.key()) {
            self.kind = t.kind;
            self.restart_watch(cx);
        }
        self.apply_route(cx);
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
            cx.notify();
        }
    }

    pub(super) fn route_title(&self, route: &Route, cx: &App) -> String {
        match route {
            Route::Resources(Some(k)) => k.label.clone(),
            Route::Resources(None) => "Resources".into(),
            Route::Detail(d) => d.read(cx).title(),
            Route::Workloads(_) => "Workloads".into(),
            Route::Cluster(_) => "Cluster".into(),
            Route::Problems(_) => "Problems".into(),
            Route::Access(_) => "My access".into(),
            Route::HelmInstall(_) => "Helm install".into(),
            Route::Assistant(_) => "Assistant".into(),
            Route::Helm(_) => "Helm releases".into(),
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
        Some(
            h_flex()
                .id("tab-strip")
                .w_full()
                .h(px(32.))
                .px_1()
                .gap_0p5()
                .items_end()
                .overflow_x_scroll()
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
                                .tooltip("Close tab (Ctrl+W)")
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
                        .tooltip("New tab (Ctrl+T)")
                        .on_click(cx.listener(|this, _, w, cx| this.new_tab(None, w, cx))),
                )
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

    /// Record which page this window shows (see `wait_visible`).
    pub(super) fn mark_visible(&mut self, cx: &mut Context<Self>) {
        let page = match self.route() {
            Route::Detail(e) => Some(e.entity_id()),
            Route::Workloads(e) => Some(e.entity_id()),
            Route::Cluster(e) => Some(e.entity_id()),
            Route::Problems(e) => Some(e.entity_id()),
            Route::Access(e) => Some(e.entity_id()),
            Route::HelmInstall(e) => Some(e.entity_id()),
            Route::Assistant(e) => Some(e.entity_id()),
            Route::Helm(e) => Some(e.entity_id()),
            Route::HelmRelease(e) => Some(e.entity_id()),
            Route::Terminal(e) => Some(e.entity_id()),
            Route::Apply(e) => Some(e.entity_id()),
            Route::Resources(_) | Route::Contexts | Route::Settings | Route::Agents => None,
        };
        let me = cx.entity_id();
        let pages = &mut cx.default_global::<VisiblePages>().0;
        match page {
            Some(p) => pages.insert(me, p),
            None => pages.remove(&me),
        };
    }

    pub(super) fn apply_route(&mut self, cx: &mut Context<Self>) {
        self.mark_visible(cx);
        // Protection may have changed in Settings since the page was last shown.
        if self.route().is_agents() {
            self.agents.update(cx, |a, _| a.refresh_contexts());
        }
        if let Route::Resources(Some(kind)) = self.route().clone()
            && self.kind.as_ref().map(|k| k.key()) != Some(kind.key())
        {
            self.kind = Some(kind);
            self.restart_watch(cx);
        }
        cx.notify();
    }
}
