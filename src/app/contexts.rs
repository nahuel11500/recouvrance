//! Connecting to contexts, namespaces, and reloading kubeconfigs.

use std::collections::{HashMap, HashSet};

use futures::StreamExt as _;
use futures::stream::FuturesUnordered;

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;

use super::{AppView, Route, TabState, dev_var, reach};
use crate::k8s;
use crate::k8s::Kind;

impl AppView {
    /// Connect to one or more contexts; tables merge them with a Context column.
    pub(super) fn connect_many(&mut self, contexts: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(primary) = contexts.first().cloned() else { return };
        #[cfg(feature = "dev")]
        crate::dev::first_rows::connecting(&contexts);
        // Where the contexts connected until now were left, before it is reset below.
        self.leave_session(cx);
        if dev_var("RECOUVRANCE_CONTEXT").is_none() {
            let remembered = contexts.clone();
            crate::settings::update(|s| {
                crate::settings::push_recent(&mut s.recent_contexts, &remembered, 10);
                s.last_contexts = remembered;
            });
            self.session_key = crate::settings::session_set(&contexts);
        }
        self.restore.connecting(&self.session_key);
        let connected = contexts.clone();
        self.ctx_picker.update(cx, |p, cx| p.set_connected(connected, cx));
        let (ctx_name, all) = (primary.clone(), contexts.clone());
        self.ns_picker.update(cx, |p, _| {
            p.context = ctx_name;
            p.contexts = all;
        });
        self.context = Some(primary.clone());
        // A favorite waiting for another connect is not opened on this one.
        self.after_connect = None;
        // Keep the list on screen while the table still has its columns (reset below).
        self.keep_view(cx);
        self.client = None;
        self.clusters.clear();
        self.kinds.clear();
        self.watch = None;
        // Kept lists watch the old clusters.
        self.kept.clear();
        self.metrics = None;
        self.recording = Vec::new();
        // Watch errors name the old clusters, and would hide "Connecting…" and new failures.
        self.list = super::live_list::LiveList::default();
        // The problem badge counted the old clusters.
        self.problems.clear();
        // Details belong to the old clusters, in every tab: the other tabs go
        // back to their resource list (kinds and namespaces checked once connected).
        self.history = vec![Route::Resources(self.kind.clone())];
        self.pos = 0;
        let active = self.active_tab;
        for (i, tab) in self.tabs.iter_mut().enumerate() {
            if i != active {
                tab.history = vec![Route::Resources(None)];
                tab.pos = 0;
            }
        }
        self.page_scopes.clear();
        // The palette's object index points at the old clusters.
        self.search_index.clear();
        // Each context says it is connecting where the list goes (see `render_connect`).
        self.connect_errors = None;
        self.connect_notes = None;
        self.conn = super::conn::ConnStates::connecting(&contexts);
        self.conn_changed(cx);
        self.connecting = true;
        self.table.update(cx, |t, cx| {
            t.delegate_mut().reset(false, Vec::new(), None, Default::default());
            t.refresh(cx);
        });
        cx.notify();

        // The Pods list, when it is the one shown first, starts with each client.
        let early = self.early_list(&contexts);
        self.early = None;
        self.joined_late = false;
        self.partial_tabs = None;

        self._connect = Some(cx.spawn_in(window, async move |this, cx| {
            // Each cluster on its own: connect, then discovery and namespaces together.
            let executor = cx.background_executor().clone();
            let mut pending: FuturesUnordered<_> = (contexts.iter().cloned().enumerate())
                .map(|(ix, ctx)| {
                    let early = early.clone();
                    async move { (ix, reach::reach(ctx, early).await) }
                })
                .collect();
            // A cluster that is slow, unreachable or waiting on a login doesn't hold back the
            // others' rows: it joins when it answers (below).
            let mut wave = reach::first_wave(
                &mut pending,
                |(_, r): &(usize, Result<reach::Reached, String>)| reach::answered(r),
                || executor.timer(reach::LATE_AFTER),
            )
            .await;
            // In the order the contexts were given: the first one's client is the main one.
            wave.sort_by_key(|(ix, _)| *ix);
            // How each one went, before its result is taken apart below.
            let settled: Vec<(String, Option<String>)> =
                wave.iter().map(|(ix, r)| (contexts[*ix].clone(), reach::failure(&contexts[*ix], r))).collect();
            let done: HashSet<usize> = wave.iter().map(|(ix, _)| *ix).collect();
            let mut waiting: Vec<String> =
                (contexts.iter().enumerate()).filter(|(ix, _)| !done.contains(ix)).map(|(_, c)| c.clone()).collect();
            let mut clusters = Vec::new();
            let mut discovered = Vec::new();
            let mut listed = Vec::new();
            let mut early_lists = HashMap::new();
            let mut errors = Vec::new();
            for (_, r) in wave {
                match r {
                    Ok(r) => {
                        if let Some(list) = r.early {
                            early_lists.insert(r.ctx.clone(), list);
                        }
                        clusters.push((r.ctx, r.client));
                        discovered.push(r.kinds);
                        listed.push(r.namespaces);
                    }
                    Err(e) => errors.push(e),
                }
            }
            let result = async {
                let (_, client) = clusters.first().cloned().ok_or_else(|| anyhow::anyhow!(errors.join("; ")))?;
                // Connecting does not reach the server: discovery is the first request, and
                // an unreachable cluster fails there.
                let names: Vec<&str> = clusters.iter().map(|(n, _)| n.as_str()).collect();
                let (mut kinds, kind_clusters) = merge_kinds(&names, discovered, &mut errors);
                if kinds.is_empty() {
                    anyhow::bail!("discovery failed on every cluster: {}", errors.join("; "));
                }
                // Built-in sections first, then custom resources by group.
                k8s::sort_for_sidebar(&mut kinds);
                // Namespaces of the clusters whose discovery answered: on the others, listing
                // them failed the same way, already reported.
                let reached: HashSet<usize> = kind_clusters.values().flatten().copied().collect();
                let mut namespaces: Vec<String> = Vec::new();
                for (ix, listed) in listed.into_iter().enumerate() {
                    if !reached.contains(&ix) {
                        continue;
                    }
                    match listed {
                        Ok(names) => namespaces.extend(names),
                        Err(e) => errors.push(e),
                    }
                }
                namespaces.sort();
                namespaces.dedup();
                anyhow::Ok((client, kinds, kind_clusters, namespaces))
            }
            .await;
            let applied = this.update_in(cx, |this, window, cx| {
                if this.context.as_deref() != Some(primary.as_str()) {
                    return false; // the user switched again meanwhile
                }
                this.connecting = false;
                for (ctx, failure) in &settled {
                    this.conn.settle(ctx, failure.as_deref());
                }
                let connected = match result {
                    Ok((client, kinds, kind_clusters, namespaces)) => {
                        // Who saved this set before: what this connect does below is no change
                        // of the user's (see `session::SessionOwners`).
                        let owner_before =
                            (cx.try_global::<super::session::SessionOwners>()).and_then(|o| o.owner(&this.session_key));
                        this.kind_clusters = kind_clusters;
                        this.connect_errors = reach::connect_status(&errors, &waiting);
                        this.connect_notes = super::conn::notes(&errors, &this.conn);
                        // The first context may have failed: namespace visits and the ones added
                        // by hand belong to the cluster that is first now.
                        if let Some((first, _)) = clusters.first() {
                            let first = first.clone();
                            this.context = Some(first.clone());
                            // Favorite namespaces: those of the clusters that answered.
                            let connected: Vec<String> = clusters.iter().map(|(c, _)| c.clone()).collect();
                            this.ns_picker.update(cx, |p, _| {
                                p.context = first;
                                p.contexts = connected;
                            });
                        }
                        this.client = Some(client);
                        // Copies of pods left behind by a run of the app that was killed.
                        for (name, client) in &clusters {
                            k8s::pod_copy::clean_up(name, client.clone(), crate::settings::cluster(name).namespaces);
                        }
                        this.clusters = clusters;
                        let keep = this.kind.as_ref().map(Kind::key);
                        let wanted = dev_var("RECOUVRANCE_KIND").and_then(|q| k8s::find_kind(&kinds, &q).cloned());
                        this.kind = wanted
                            .or_else(|| keep.and_then(|key| kinds.iter().find(|k| k.key() == key).cloned()))
                            .or_else(|| kinds.iter().find(|k| k.is("", "Pod")).cloned());
                        // Back must not return to a list the new clusters don't have.
                        served_lists(&mut this.history, &kinds, this.kind.as_ref());
                        // Other tabs keep a kind and namespaces only where the new clusters have them.
                        let active = this.active_tab;
                        let fallback = this.kind.clone();
                        for (i, tab) in this.tabs.iter_mut().enumerate() {
                            if i != active {
                                served_tab(tab, &kinds, &namespaces, fallback.as_ref());
                            }
                        }
                        this.settings_view.update(cx, |s, _| s.set_kinds(kinds.clone()));
                        this.kinds = kinds;
                        // The timeline: the connected contexts' records, their own watches.
                        for (c, _) in &this.clusters {
                            crate::timeline::open(c);
                        }
                        this.recording =
                            crate::timeline::start_watches(&this.clusters, &this.kinds, &this.kind_clusters);
                        this.set_namespaces(namespaces, window, cx);
                        // The first window's first set reopens where it was left, again until a
                        // connect reaches every cluster: its namespaces before the list starts,
                        // so the list starts once.
                        let partial = !errors.is_empty() || !waiting.is_empty();
                        let reopened = if this.restore.finished(&this.session_key, partial) {
                            this.restore_session(window, cx)
                        } else {
                            None
                        };
                        // The Pods lists started with the connect, for the list to take over.
                        this.early = early.clone().map(|of| reach::EarlyLists { of, lists: early_lists });
                        this.restart_watch(cx);
                        // After the list, whose rows its first count waits for.
                        this.problems.start(this.clusters.clone(), this.cluster_kinds(), cx);
                        let restored = reopened.is_some();
                        match reopened {
                            Some(page) => {
                                if let Some(page) = super::session::overview(page) {
                                    this.open_overview(page, window, cx);
                                }
                            }
                            None if dev_var("RECOUVRANCE_PAGE").is_none() => {
                                match crate::settings::get().initial_view {
                                    crate::settings::InitialView::Workloads => this.open_workloads(window, cx),
                                    crate::settings::InitialView::Cluster => this.open_cluster(window, cx),
                                    crate::settings::InitialView::Pods => {}
                                }
                            }
                            None => {}
                        }
                        match dev_var("RECOUVRANCE_PAGE").as_deref() {
                            Some("workloads") => this.open_workloads(window, cx),
                            Some("helm") => this.open_helm(window, cx),
                            Some("argo") => this.open_argo(window, cx),
                            Some("agents") => this.navigate(Route::Agents, cx),
                            Some("cluster") => this.open_cluster(window, cx),
                            Some("settings") => this.navigate(Route::Settings, cx),
                            Some("terminal") => this.open_terminal(window, cx),
                            Some("apply") => this.open_apply(window, cx),
                            Some("problems") => this.open_problems(window, cx),
                            Some("timeline") => this.open_timeline(window, cx),
                            Some("access") => this.open_access(window, cx),
                            Some("helm-install") => this.open_helm_install(window, cx),
                            Some("assistant") => this.open_assistant(window, cx),
                            Some("assistant-dock") => this.toggle_assistant(window, cx),
                            _ => {}
                        }
                        this.settle_session(owner_before, restored, cx);
                        // A favorite on a context that wasn't connected opens now (once), or
                        // when its context joins if it is still connecting.
                        if let Some(favorite) = this.after_connect.take() {
                            if favorite.context().is_some_and(|c| waiting.iter().any(|w| w == c)) {
                                this.after_connect = Some(favorite);
                            } else {
                                this.open_favorite(favorite, false, window, cx);
                            }
                        }
                        // Where the user starts from while clusters are still connecting.
                        if !waiting.is_empty() {
                            this.partial_tabs = Some(this.tabs_now(cx));
                        }
                        true
                    }
                    Err(e) => {
                        this.list.notice = None;
                        this.connect_errors = Some(format!("{e:#}").into());
                        // Each context says why it failed where the list goes; one that has
                        // no reason of its own (it served no API) takes the connect's.
                        this.conn.give_up(&contexts, &k8s::short_error(&e));
                        this.connect_notes = None;
                        if let Some(favorite) = this.after_connect.take() {
                            window.push_notification(
                                Notification::warning(format!(
                                    "{} not opened: {} did not connect",
                                    favorite.title(),
                                    favorite.context().unwrap_or("its context")
                                )),
                                cx,
                            );
                        }
                        false
                    }
                };
                this.conn_changed(cx);
                cx.notify();
                connected
            });
            if !matches!(applied, Ok(true)) {
                return;
            }
            // Clusters that answer after the first rows join them, each when it does.
            while let Some((ix, r)) = pending.next().await {
                let alive = this.update_in(cx, |this, window, cx| {
                    waiting.retain(|c| *c != contexts[ix]);
                    this.conn.settle(&contexts[ix], reach::failure(&contexts[ix], &r).as_deref());
                    match r {
                        Ok(r) if r.kinds.is_ok() => this.join_late(r, early.clone(), &mut errors, window, cx),
                        Ok(r) => {
                            if let Err(e) = &r.kinds {
                                errors.push(format!("{}: {}", r.ctx, k8s::error_chain(e)));
                            }
                        }
                        Err(e) => errors.push(e),
                    }
                    this.connect_errors = reach::connect_status(&errors, &waiting);
                    this.connect_notes = super::conn::notes(&errors, &this.conn);
                    this.conn_changed(cx);
                    // The last one in and none failed: the connect is whole after all.
                    if waiting.is_empty() && errors.is_empty() {
                        this.joined_whole(window, cx);
                    }
                    // The favorite waiting for this context (it says so when it failed).
                    if this.after_connect.as_ref().is_some_and(|f| f.context() == Some(contexts[ix].as_str()))
                        && let Some(favorite) = this.after_connect.take()
                    {
                        this.open_favorite(favorite, false, window, cx);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    /// The context picker marks each context of the set as it stands now.
    pub(super) fn conn_changed(&mut self, cx: &mut Context<Self>) {
        let states = self.conn.clone();
        self.ctx_picker.update(cx, |p, cx| p.set_states(states, cx));
    }

    /// Connect to the set again: its contexts that failed, with those that answered.
    pub(super) fn retry_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let set = self.ctx_picker.read(cx).connected.clone();
        self.connect_many(set, window, cx);
    }

    pub(super) fn set_namespaces(&mut self, namespaces: Vec<String>, _: &mut Window, cx: &mut Context<Self>) {
        self.ns_picker.update(cx, |p, cx| p.set_namespaces(namespaces.clone(), cx));
        // Keep the selection when switching to a cluster that has the same namespaces.
        let mut kept: Vec<String> = self.namespaces.iter().filter(|ns| namespaces.contains(ns)).cloned().collect();
        if let Some(list) = dev_var("RECOUVRANCE_NS") {
            kept = list.split(',').map(String::from).filter(|ns| namespaces.contains(ns)).collect();
        }
        // Without the right to list cluster-wide, watch one accessible namespace at a time.
        if kept.len() != 1
            && let Some(first) = self.accessible_namespaces().into_iter().next()
        {
            kept = vec![kept.first().cloned().unwrap_or(first)];
        }
        self.ns_picker.update(cx, |p, _| p.selected = kept.iter().cloned().collect());
        self.namespaces = kept;
        self.namespace = (self.namespaces.len() == 1).then(|| self.namespaces[0].clone());
        self.update_assistant_scope(cx);
    }

    /// Reload the contexts when a kubeconfig file (or a watched folder) changes.
    pub(super) fn watch_kubeconfigs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self._kubeconfig_watch = Some(cx.spawn_in(window, async move |this, cx| {
            let mut seen = k8s::kubeconfig_signature();
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(3)).await;
                let now = k8s::kubeconfig_signature();
                if now == seen {
                    continue;
                }
                // Not the cached list, which may predate the change; on a failed read, try again.
                let Ok(contexts) = k8s::reload_contexts() else { continue };
                // A file may be half-written: wait until it stays the same for a while.
                if !contexts.settled {
                    continue;
                }
                seen = now;
                let alive = this.update_in(cx, |this, window, cx| this.contexts_reloaded(contexts.names, window, cx));
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    pub fn contexts_reloaded(&mut self, names: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let added = names.iter().filter(|n| !self.all_contexts.contains(n)).count();
        let removed = self.all_contexts.iter().filter(|n| !names.contains(n)).count();
        self.picked.retain(|c| names.contains(c));
        self.all_contexts.clone_from(&names);
        crate::k8s::short_names::set_contexts(&names);
        self.ctx_picker.update(cx, |p, cx| p.set_all(names, cx));
        self.settings_view.update(cx, |s, cx| s.sync_clusters(window, cx));
        self.agents.update(cx, |a, _| a.refresh_contexts());
        let msg = match (added, removed) {
            (0, 0) => "Kubeconfig reloaded".to_string(),
            (a, 0) => format!("Kubeconfig reloaded: {a} new context{}", if a > 1 { "s" } else { "" }),
            (0, r) => format!("Kubeconfig reloaded: {r} context{} gone", if r > 1 { "s" } else { "" }),
            (a, r) => format!("Kubeconfig reloaded: {a} new, {r} gone"),
        };
        window.push_notification(Notification::info(msg), cx);
        cx.notify();
    }

    /// Namespaces configured as accessible, when every connected cluster has some: then
    /// none may list cluster-wide and the picker keeps one at a time (empty: all). Among
    /// clusters that may, one with a list watches one of its own (see `cluster_namespace`).
    pub(super) fn accessible_namespaces(&self) -> Vec<String> {
        all_restricted(self.clusters.iter().map(|(c, _)| crate::settings::cluster(c).namespaces))
    }

    /// One namespace is watched directly; several are watched cluster-wide and filtered here.
    pub(super) fn set_selected_namespaces(&mut self, namespaces: Vec<String>, cx: &mut Context<Self>) {
        self.choose_namespaces(namespaces, cx);
        self.restart_watch(cx);
    }

    /// Select namespaces (one at a time when only some are accessible) without
    /// restarting the watch.
    pub(super) fn choose_namespaces(&mut self, mut namespaces: Vec<String>, cx: &mut Context<Self>) {
        let accessible = self.accessible_namespaces();
        if namespaces.len() != 1
            && let Some(one) = one_namespace(&namespaces, &self.namespaces, &accessible)
        {
            namespaces = vec![one];
            let sel = namespaces.clone();
            self.ns_picker.update(cx, |p, cx| {
                p.selected = sel.into_iter().collect();
                cx.notify();
            });
        }
        self.namespaces = namespaces;
        self.namespace = (self.namespaces.len() == 1).then(|| self.namespaces[0].clone());
        self.update_assistant_scope(cx);
        self.rescope_page(cx);
        self.session_changed(cx);
    }

    /// Each connected cluster with the namespaces a page lists there.
    pub(super) fn namespace_scope(&self) -> crate::ui::Scope {
        (self.clusters.iter())
            .map(|(ctx, client)| {
                let ns = cluster_namespaces(&self.namespaces, &crate::settings::cluster(ctx).namespaces);
                (ctx.clone(), client.clone(), ns)
            })
            .collect()
    }

    /// Each connected cluster's own kinds, by context: a scan given another cluster's
    /// kinds would ask for a kind this one doesn't serve (Argo CD on the hub only).
    pub(super) fn cluster_kinds(&self) -> HashMap<String, Vec<Kind>> {
        let names: Vec<&str> = self.clusters.iter().map(|(n, _)| n.as_str()).collect();
        kinds_per_cluster(&names, &self.kinds, &self.kind_clusters)
    }

    /// The kinds the connected cluster at `ix` of `clusters` serves (none when its discovery
    /// failed): `kinds` also holds the other clusters' own.
    pub(super) fn served_kinds(&self, ix: usize) -> impl Iterator<Item = &Kind> {
        served_by(&self.kinds, &self.kind_clusters, ix)
    }
}

/// The kinds each of `names` serves, from where each kind exists (`merge_kinds`). A
/// cluster that serves none (its discovery failed) gets an empty list.
fn kinds_per_cluster(
    names: &[&str],
    kinds: &[Kind],
    kind_clusters: &HashMap<String, Vec<usize>>,
) -> HashMap<String, Vec<Kind>> {
    (names.iter().enumerate())
        .map(|(ix, name)| (name.to_string(), served_by(kinds, kind_clusters, ix).cloned().collect()))
        .collect()
}

/// The kinds the cluster at `ix` serves, from where each kind exists (`merge_kinds`).
fn served_by<'a>(
    kinds: &'a [Kind],
    kind_clusters: &'a HashMap<String, Vec<usize>>,
    ix: usize,
) -> impl Iterator<Item = &'a Kind> {
    kinds.iter().filter(move |k| kind_clusters.get(&k.key()).is_some_and(|c| c.contains(&ix)))
}

/// A cluster's namespaces as its listing answered, or what failed, to report. Not being
/// allowed to list them is an answer: they are picked by hand there. A timeout or a server
/// error is not: the namespaces picked there may still exist, and the connect counts as
/// partial (a session is not saved over, see `session`).
pub(super) fn listed_namespaces(ctx: &str, listed: Result<Vec<String>>) -> Result<Vec<String>, String> {
    match listed {
        Ok(names) => Ok(names),
        Err(e) if k8s::api_status(&e) == Some(403) => Ok(Vec::new()),
        Err(e) => Err(format!("{ctx}: listing namespaces: {}", k8s::error_chain(&e))),
    }
}

/// Union of every cluster's kinds, with where each one exists. A cluster whose
/// discovery failed adds `name: error` to `errors`.
fn merge_kinds(
    names: &[&str],
    discovered: Vec<Result<Vec<Kind>>>,
    errors: &mut Vec<String>,
) -> (Vec<Kind>, HashMap<String, Vec<usize>>) {
    let mut kinds: Vec<Kind> = Vec::new();
    let mut kind_clusters: HashMap<String, Vec<usize>> = HashMap::new();
    for (ix, (name, found)) in names.iter().zip(discovered).enumerate() {
        let found = match found {
            Ok(found) => found,
            Err(e) => {
                errors.push(format!("{name}: {}", k8s::error_chain(&e)));
                continue;
            }
        };
        add_kinds(&mut kinds, &mut kind_clusters, ix, found);
    }
    (kinds, kind_clusters)
}

/// The kinds the cluster at `ix` serves added to `kinds`, and where each exists: a cluster
/// that joins after the others (see `reach`) as `merge_kinds` adds each one.
pub(super) fn add_kinds(
    kinds: &mut Vec<Kind>,
    kind_clusters: &mut HashMap<String, Vec<usize>>,
    ix: usize,
    found: Vec<Kind>,
) {
    for k in found {
        let entry = kind_clusters.entry(k.key()).or_default();
        if entry.is_empty() {
            kinds.push(k);
        }
        entry.push(ix);
    }
}

/// Lists in a history that name a kind none of the clusters serve show `fallback` instead.
fn served_lists(history: &mut [Route], kinds: &[Kind], fallback: Option<&Kind>) {
    for route in history {
        if let Route::Resources(Some(k)) = route
            && !kinds.iter().any(|x| x.key() == k.key())
        {
            *route = Route::Resources(fallback.cloned());
        }
    }
}

/// A tab not shown keeps its kind, lists and namespaces only where the new clusters have
/// them: a tab switched away from while connecting still names the old clusters' kind.
fn served_tab(tab: &mut TabState, kinds: &[Kind], namespaces: &[String], fallback: Option<&Kind>) {
    tab.kind =
        tab.kind.take().and_then(|k| kinds.iter().find(|x| x.key() == k.key()).cloned()).or_else(|| fallback.cloned());
    served_lists(&mut tab.history, kinds, tab.kind.as_ref());
    tab.namespaces.retain(|ns| namespaces.contains(ns));
}

/// Every list joined, or nothing when one of them is empty (that cluster may list all).
fn all_restricted(lists: impl IntoIterator<Item = Vec<String>>) -> Vec<String> {
    let mut joined = Vec::new();
    for list in lists {
        if list.is_empty() {
            return Vec::new();
        }
        joined.extend(list);
    }
    joined
}

/// The namespace a cluster's list watches, given the namespaces chosen (several or none:
/// all) and the ones accessible on that cluster: a cluster that may not list cluster-wide
/// watches one of its own. `Some(None)` watches all; `None` leaves the cluster out, as none
/// of the namespaces chosen is accessible there.
pub(super) fn cluster_namespace(chosen: &[String], accessible: &[String]) -> Option<Option<String>> {
    if let [one] = chosen {
        return Some(Some(one.clone()));
    }
    if accessible.is_empty() {
        return Some(None);
    }
    if chosen.is_empty() {
        return Some(accessible.first().cloned());
    }
    accessible.iter().find(|ns| chosen.contains(ns)).map(|ns| Some(ns.clone()))
}

/// The namespaces a page that lists each one by itself reads on a cluster, given the ones
/// chosen (none: all) and the ones accessible there (none: all). `None` lists them all at
/// once; an empty list leaves the cluster out, as none of those chosen is accessible there.
/// One namespace chosen is read everywhere, as `cluster_namespace` watches it.
pub(super) fn cluster_namespaces(chosen: &[String], accessible: &[String]) -> Vec<Option<String>> {
    match (chosen, accessible.is_empty()) {
        ([], true) => vec![None],
        ([], false) => accessible.iter().cloned().map(Some).collect(),
        ([one], _) => vec![Some(one.clone())],
        _ => (chosen.iter()).filter(|ns| accessible.is_empty() || accessible.contains(ns)).cloned().map(Some).collect(),
    }
}

/// The one namespace to show when only some are accessible: the one just ticked,
/// else the last one asked for, else the first accessible one. None when all are.
fn one_namespace(wanted: &[String], previous: &[String], accessible: &[String]) -> Option<String> {
    if accessible.is_empty() {
        return None;
    }
    wanted.iter().find(|ns| !previous.contains(ns)).or(wanted.last()).or(accessible.first()).cloned()
}

#[cfg(test)]
mod tests {
    use super::{
        Kind, Route, TabState, all_restricted, cluster_namespace, cluster_namespaces, kinds_per_cluster,
        listed_namespaces, merge_kinds, one_namespace, served_by, served_lists, served_tab,
    };
    use crate::k8s::argo::diagnosis::{Res, known_kind};

    fn kind(k: &str) -> Kind {
        Kind::fake("", k)
    }

    #[test]
    fn an_argo_cd_resource_opens_with_a_kind_its_own_cluster_serves() {
        let at = |group: &str, kind: &str, version: &str| {
            let mut k = Kind::fake(group, kind);
            k.ar.version = version.into();
            k.ar.api_version = format!("{group}/{version}");
            k
        };
        let cert = Kind::fake("cert-manager.io", "Certificate");
        let (hpa1, hpa2) =
            (at("autoscaling", "HorizontalPodAutoscaler", "v1"), at("autoscaling", "HorizontalPodAutoscaler", "v2"));
        // The hub serves the CRD and HPAs at v1 and v2, the spoke HPAs at v1 only.
        let found = vec![Ok(vec![cert.clone(), hpa1.clone(), hpa2.clone()]), Ok(vec![hpa1.clone()])];
        let (kinds, where_) = merge_kinds(&["hub", "spoke"], found, &mut Vec::new());
        let open = |ix: usize, group: &str, kind: &str, version: &str| {
            let res = Res {
                group: group.into(),
                kind: kind.into(),
                name: "x".into(),
                version: version.into(),
                ..Default::default()
            };
            known_kind(served_by(&kinds, &where_, ix), &res).map(|k| k.key())
        };
        assert_eq!(open(0, "cert-manager.io", "Certificate", "v1"), Some(cert.key()));
        // The hub's CRD isn't the spoke's: looked up on the spoke instead of opened not found.
        assert_eq!(open(1, "cert-manager.io", "Certificate", "v1"), None);
        assert_eq!(open(0, "autoscaling", "HorizontalPodAutoscaler", "v2"), Some(hpa2.key()));
        // A version only the hub serves: the one the spoke serves.
        assert_eq!(open(1, "autoscaling", "HorizontalPodAutoscaler", "v2"), Some(hpa1.key()));
    }

    #[test]
    fn a_namespace_listing_that_failed_is_reported_but_a_refusal_is_not() {
        let api = |code| -> anyhow::Error {
            kube::Error::Api(kube::core::Status::failure("x", "x").with_code(code).boxed()).into()
        };
        let listed = listed_namespaces("a", Ok(vec!["team-a".into()]));
        assert_eq!(listed, Ok(vec!["team-a".to_string()]));
        // Not allowed to list them: picked by hand, nothing failed.
        assert_eq!(listed_namespaces("a", Err(api(403))), Ok(Vec::new()));
        // A timeout or a server error: the namespaces picked there may still exist.
        for failed in [api(500), api(401), anyhow::anyhow!("timed out")] {
            let e = listed_namespaces("hub", Err(failed)).unwrap_err();
            assert!(e.starts_with("hub: listing namespaces: "), "{e}");
        }
    }

    #[test]
    fn a_cluster_that_fails_discovery_is_reported() {
        let mut errors = vec!["c: no such context".to_string()];
        let found = vec![Ok(vec![kind("Pod")]), Err(anyhow::anyhow!("connection refused"))];
        let (kinds, where_) = merge_kinds(&["a", "b"], found, &mut errors);
        assert_eq!(kinds.len(), 1);
        assert_eq!(where_[&kinds[0].key()], [0]);
        assert_eq!(errors, ["c: no such context", "b: connection refused"]);
    }

    #[test]
    fn each_cluster_gets_only_the_kinds_it_serves() {
        // The hub serves Pods and Applications, the spoke Pods only, the third failed discovery.
        let (pod, app) = (kind("Pod"), kind("Application"));
        let found = vec![Ok(vec![pod.clone(), app.clone()]), Ok(vec![pod.clone()]), Err(anyhow::anyhow!("down"))];
        let (kinds, where_) = merge_kinds(&["hub", "spoke", "down"], found, &mut Vec::new());
        let own = kinds_per_cluster(&["hub", "spoke", "down"], &kinds, &where_);
        let keys = |c: &str| own[c].iter().map(Kind::key).collect::<Vec<_>>();
        assert_eq!(keys("hub"), [pod.key(), app.key()]);
        assert_eq!(keys("spoke"), [pod.key()]);
        assert!(own["down"].is_empty());
        // One cluster: every kind is its own.
        let (kinds, where_) = merge_kinds(&["a"], vec![Ok(vec![pod, app])], &mut Vec::new());
        assert_eq!(kinds_per_cluster(&["a"], &kinds, &where_)["a"].len(), 2);
    }

    #[test]
    fn a_tab_left_while_connecting_drops_a_kind_the_clusters_dont_serve() {
        let (pods, certs) = (kind("Pod"), kind("Certificate"));
        let mut tab = TabState {
            history: vec![Route::Resources(Some(certs.clone()))],
            kind: Some(certs),
            namespaces: vec!["a".into(), "gone".into()],
            ..Default::default()
        };
        served_tab(&mut tab, std::slice::from_ref(&pods), &["a".to_string()], Some(&pods));
        assert_eq!(tab.kind.as_ref().map(|k| k.label.as_str()), Some("Pod"));
        assert!(matches!(&tab.history[..], [Route::Resources(Some(k))] if k.label == "Pod"));
        assert_eq!(tab.namespaces, ["a"]);
    }

    #[test]
    fn back_goes_to_a_list_the_clusters_serve() {
        let (pods, certs) = (kind("Pod"), kind("Certificate"));
        let mut history = vec![Route::Resources(Some(certs)), Route::Resources(None), Route::Contexts];
        served_lists(&mut history, std::slice::from_ref(&pods), Some(&pods));
        let lists: Vec<Option<String>> = history
            .iter()
            .map(|r| match r {
                Route::Resources(k) => k.as_ref().map(|k| k.label.clone()),
                _ => Some("other".into()),
            })
            .collect();
        assert_eq!(lists, [Some("Pod".into()), None, Some("other".into())]);
    }

    fn v(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn the_namespace_just_ticked_wins() {
        let accessible = v(&["a", "b", "c"]);
        assert_eq!(one_namespace(&v(&["a", "c"]), &v(&["c"]), &accessible).as_deref(), Some("a"));
        assert_eq!(one_namespace(&v(&["a", "c"]), &v(&["a"]), &accessible).as_deref(), Some("c"));
        assert_eq!(one_namespace(&v(&["a", "c"]), &v(&["a", "c"]), &accessible).as_deref(), Some("c"));
        assert_eq!(one_namespace(&[], &v(&["c"]), &accessible).as_deref(), Some("a"));
    }

    #[test]
    fn several_namespaces_stay_when_all_are_accessible() {
        assert_eq!(one_namespace(&v(&["a", "c"]), &v(&["c"]), &[]), None);
        assert_eq!(one_namespace(&[], &v(&["c"]), &[]), None);
    }

    #[test]
    fn one_namespace_at_a_time_only_when_every_cluster_has_a_list() {
        assert_eq!(all_restricted([v(&["team-a"]), v(&[])]), Vec::<String>::new());
        assert_eq!(all_restricted([v(&["team-a"]), v(&["team-b"])]), v(&["team-a", "team-b"]));
        assert_eq!(all_restricted([v(&[])]), Vec::<String>::new());
    }

    #[test]
    fn a_cluster_with_a_list_watches_one_of_its_namespaces() {
        let team = v(&["team-a", "team-b"]);
        // A cluster that may list everything watches all, or the one namespace chosen.
        assert_eq!(cluster_namespace(&[], &[]), Some(None));
        assert_eq!(cluster_namespace(&v(&["x", "y"]), &[]), Some(None));
        assert_eq!(cluster_namespace(&v(&["x"]), &[]), Some(Some("x".into())));
        // One with a list never watches cluster-wide.
        assert_eq!(cluster_namespace(&[], &team), Some(Some("team-a".into())));
        assert_eq!(cluster_namespace(&v(&["x", "team-b"]), &team), Some(Some("team-b".into())));
        assert_eq!(cluster_namespace(&v(&["x", "y"]), &team), None);
        assert_eq!(cluster_namespace(&v(&["x"]), &team), Some(Some("x".into())));
    }

    #[test]
    fn helm_lists_each_namespace_chosen() {
        let some = |l: &[&str]| l.iter().map(|s| Some(s.to_string())).collect::<Vec<_>>();
        let team = v(&["team-a", "team-b"]);
        assert_eq!(cluster_namespaces(&[], &[]), vec![None]);
        assert_eq!(cluster_namespaces(&v(&["kube-system", "monitoring"]), &[]), some(&["kube-system", "monitoring"]));
        assert_eq!(cluster_namespaces(&v(&["x"]), &[]), some(&["x"]));
        assert_eq!(cluster_namespaces(&[], &team), some(&["team-a", "team-b"]));
        assert_eq!(cluster_namespaces(&v(&["x", "team-b"]), &team), some(&["team-b"]));
        assert_eq!(cluster_namespaces(&v(&["x", "y"]), &team), some(&[]));
        assert_eq!(cluster_namespaces(&v(&["x"]), &team), some(&["x"]));
    }
}

/// The namespaces Helm reads on a cluster, for every choice and accessible list.
#[cfg(test)]
mod properties {
    use super::{Kind, add_kinds, cluster_namespaces, merge_kinds};
    use proptest::prelude::*;

    proptest! {
        /// A cluster that answers after the first rows adds its kinds, and where each one
        /// exists, as if it had answered with the others: the kinds a list or a scan asks a
        /// cluster for don't depend on when it connected.
        #[test]
        fn a_late_cluster_merges_like_the_others(
            lists in prop::collection::vec(prop::collection::vec(0usize..6, 0..5), 1..4),
        ) {
            const KINDS: [&str; 6] = ["Pod", "Service", "Secret", "Job", "Lease", "Node"];
            let found: Vec<Vec<Kind>> =
                lists.iter().map(|l| l.iter().map(|&i| Kind::fake("", KINDS[i])).collect()).collect();
            let names: Vec<String> = (0..found.len()).map(|i| format!("c{i}")).collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let (all, all_where) = merge_kinds(&names, found.iter().cloned().map(Ok).collect(), &mut Vec::new());
            let last = found.len() - 1;
            let (mut kinds, mut where_) =
                merge_kinds(&names[..last], found[..last].iter().cloned().map(Ok).collect(), &mut Vec::new());
            add_kinds(&mut kinds, &mut where_, last, found[last].clone());
            let keys = |kinds: &[Kind]| kinds.iter().map(Kind::key).collect::<Vec<_>>();
            prop_assert_eq!(keys(&kinds), keys(&all));
            prop_assert_eq!(where_, all_where);
        }


        /// Namespaces chosen are never widened to all of them, and a cluster with a list
        /// of accessible namespaces is never read cluster-wide.
        #[test]
        fn helm_never_reads_more_than_chosen(
            chosen in prop::collection::vec("[a-c]", 0..4),
            accessible in prop::collection::vec("[a-c]", 0..4),
        ) {
            let read = cluster_namespaces(&chosen, &accessible);
            if !chosen.is_empty() || !accessible.is_empty() {
                prop_assert!(read.iter().all(Option::is_some));
            }
            for ns in read.iter().flatten() {
                let from = if chosen.is_empty() { &accessible } else { &chosen };
                prop_assert!(from.contains(ns), "{ns} read, not in {from:?}");
            }
        }
    }
}
