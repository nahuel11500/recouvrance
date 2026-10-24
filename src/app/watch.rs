//! Keeping the resource list live: table watches, metrics and the problem count.

use std::sync::Arc;

use futures::StreamExt as _;
use gpui_kit::component::table::TableDelegate as _;
use gpui_kit::*;

use super::contexts::cluster_namespace;
use super::live_list::{LiveList, coalesce};
use super::{AppView, Route};
use crate::k8s;
use crate::k8s::TableMsg;

impl AppView {
    /// Count problems in the background for the sidebar badge. Big clusters are
    /// scanned less often (the interval grows with how long a scan takes).
    pub(super) fn start_problem_watch(&mut self, cx: &mut Context<Self>) {
        self.problem_counts = None;
        self.problem_gaps = None;
        let (clusters, kinds) = (self.clusters.clone(), self.kinds.clone());
        self._problem_watch = Some(cx.spawn(async move |this, cx| {
            loop {
                let started = std::time::Instant::now();
                // A cluster that may not list cluster-wide is read in its accessible namespaces.
                let scans = clusters.iter().map(|(name, c)| {
                    let accessible = crate::settings::cluster(name).namespaces;
                    let (c, kinds) = (c.clone(), &kinds);
                    async move { crate::problems::scan_namespaces(c, kinds, &accessible).await }
                });
                let results = futures::future::join_all(scans).await;
                let found = results.iter().flat_map(|s| &s.problems);
                let crit = found.clone().filter(|p| p.severity == crate::problems::Severity::Critical).count();
                let warn = found.filter(|p| p.severity == crate::problems::Severity::Warning).count();
                // Kinds a cluster refused or failed to list: the counts leave them out.
                let skipped: Vec<String> = clusters
                    .iter()
                    .zip(&results)
                    .filter_map(|((name, _), s)| Some(format!("{name}: {}", s.skipped_text()?)))
                    .collect();
                let gaps = (!skipped.is_empty())
                    .then(|| format!("Partial count, could not read {}", skipped.join("; ")).into());
                if this
                    .update(cx, |this, cx| {
                        this.problem_counts = Some((crit, warn));
                        this.problem_gaps = gaps;
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                let wait = (started.elapsed() * 8).max(std::time::Duration::from_secs(120));
                cx.background_executor().timer(wait).await;
            }
        }));
    }

    pub(super) fn restart_watch(&mut self, cx: &mut Context<Self>) {
        self.watch = None;
        self.metrics = None;
        self.keep_view(cx);
        let Some(kind) = self.kind.clone() else { return };
        // Not connected: open the cluster switcher instead of doing nothing.
        if self.clusters.is_empty() {
            self.ctx_open = true;
            cx.notify();
            return;
        }
        let show_ns = kind.namespaced && self.namespace.is_none();
        let ns_filter = (self.namespaces.len() > 1).then(|| self.namespaces.iter().cloned().collect());
        let names: Vec<String> = self.clusters.iter().map(|(c, _)| c.clone()).collect();
        // What each cluster watches: nothing when it doesn't serve the kind, one of its
        // accessible namespaces when it may not list cluster-wide. A kind discovery didn't
        // find is served nowhere.
        let serving = self.kind_clusters.get(&kind.key()).cloned().unwrap_or_default();
        let watched: Vec<Option<Option<String>>> = (self.clusters.iter().enumerate())
            .map(|(ix, (ctx_name, _))| {
                if !serving.contains(&ix) {
                    None
                } else if !kind.namespaced {
                    Some(None)
                } else {
                    cluster_namespace(&self.namespaces, &crate::settings::cluster(ctx_name).namespaces)
                }
            })
            .collect();
        let key = format!(
            "{}|{}|{:?}|{:?}",
            names.join(","),
            kind.key(),
            watched,
            self.column_prefs.custom(&kind.kubectl_name())
        );
        let cached = self.views.get(&key).filter(|v| v.rows.len() == self.clusters.len()).cloned();
        self.view_key = Some(key);
        // Wait only for the clusters watched: the others never report.
        self.list = LiveList::start(self.clusters.len(), (0..self.clusters.len()).filter(|&ix| watched[ix].is_some()));
        let loading = self.list.is_loading();
        let is_pod = kind.is("", "Pod");
        let mut actions = Vec::new();
        if kind.is("batch", "CronJob") {
            actions.push("run");
        }
        if kind.ar.group == "apps" && matches!(kind.ar.kind.as_str(), "Deployment" | "StatefulSet" | "ReplicaSet") {
            actions.push("scale");
        }
        if kind.ar.group == "apps" && matches!(kind.ar.kind.as_str(), "Deployment" | "StatefulSet" | "DaemonSet") {
            actions.push("restart");
        }
        // Event rows open the object they are about: no write actions on them.
        if !kind.is("", "Event") {
            actions.push("delete");
        }
        // The filter box keeps its text across kinds; the table applies what it shows.
        let filter_text = self.filter.read(cx).value().to_string();
        self.table.update(cx, |t, cx| {
            t.delegate_mut().pods = is_pod;
            t.delegate_mut().row_actions = actions;
            t.delegate_mut().reset(
                show_ns,
                names,
                self.column_prefs.get(&kind.kubectl_name()),
                self.column_prefs.widths(&kind.kubectl_name()),
            );
            // After the reset, which starts the table over (not loading, not forbidden).
            t.delegate_mut().loading = loading;
            t.delegate_mut().set_namespace_filter(ns_filter);
            t.delegate_mut().set_filter(filter_text);
            // A new list: the row highlighted in the old one (and Enter) points at nothing.
            t.clear_selection(cx);
            t.refresh(cx);
        });
        if serving.is_empty() {
            self.list.notice = Some(format!("{} is not available on the connected clusters", kind.label).into());
        }
        // Seen recently: show it as it was while the new list loads (whole, not by pages).
        let progressive = cached.is_none();
        if let Some(view) = cached {
            let (columns, at) = (view.columns.clone(), view.at);
            self.list.restore(view);
            let merged = self.list.merged(&columns);
            self.table.update(cx, |t, cx| {
                t.delegate_mut().loading = false;
                t.delegate_mut().update_columns(columns);
                t.delegate_mut().set_rows(merged);
                t.refresh(cx);
            });
            if loading {
                let ago =
                    k8s::age(jiff::Timestamp::now() - jiff::SignedDuration::try_from(at.elapsed()).unwrap_or_default());
                self.list.notice = Some(format!("Updating… (showing the list from {ago} ago)").into());
            }
        }
        let mut watches = Vec::new();
        let mut metrics = Vec::new();
        // Set while the list is hidden: watches hold their snapshots back, metrics stop polling.
        let pause = k8s::Pause::default();
        for (ix, ((ctx_name, client), ns)) in self.clusters.clone().into_iter().zip(watched).enumerate() {
            let Some(ns) = ns else { continue };
            let (guard, mut rx) = k8s::watch_table_paused(
                client.clone(),
                kind.clone(),
                ns.clone(),
                Arc::new(self.column_prefs.custom(&kind.kubectl_name())),
                pause.clone(),
                progressive,
            );
            let shown = pause.clone();
            let task = cx.spawn(async move |this, cx| {
                while wait_list_shown(&this, &shown, cx).await {
                    let Some(msg) = rx.next().await else { break };
                    // Everything already waiting goes in one update: only its last rows count.
                    let mut batch = vec![msg];
                    while let Ok(msg) = rx.try_recv() {
                        batch.push(msg);
                    }
                    let alive = this.update(cx, |this, cx| this.on_table_msgs(ix, batch, cx));
                    if alive.is_err() {
                        break;
                    }
                }
            });
            watches.push((guard, task));
            let metrics_on = crate::settings::metrics_on(&ctx_name);
            if metrics_on && kind.is("", "Pod") {
                // Requests and limits, for the usage bars.
                let (guard, mut rx) = k8s::watch_pod_resources(client.clone(), ns.clone(), pause.clone());
                let t = self.table.clone();
                let task = cx.spawn(async move |_, cx| {
                    while let Some(resources) = rx.next().await {
                        let resources = latest(&mut rx, resources);
                        t.update(cx, |t, cx| {
                            t.delegate_mut().set_resources(ix, resources);
                            cx.notify();
                        });
                    }
                });
                metrics.push((guard, task));
                let (guard, mut rx) = k8s::watch_pod_metrics(client, ns, pause.clone());
                let task = cx.spawn(async move |this, cx| {
                    while let Some(usage) = rx.next().await {
                        let usage = latest(&mut rx, usage);
                        let alive = this.update(cx, |this, cx| {
                            this.table.update(cx, |t, cx| {
                                if t.delegate_mut().set_usage(ix, usage) {
                                    t.refresh(cx);
                                }
                                cx.notify();
                            });
                            this.follow_selection(cx);
                        });
                        if alive.is_err() {
                            break;
                        }
                    }
                });
                metrics.push((guard, task));
            } else if metrics_on && kind.is("", "Node") {
                let (guard, mut rx) = k8s::watch_node_metrics(client, pause.clone());
                let task = cx.spawn(async move |this, cx| {
                    while let Some(usage) = rx.next().await {
                        let usage = latest(&mut rx, usage);
                        let alive = this.update(cx, |this, cx| {
                            this.table.update(cx, |t, cx| {
                                if t.delegate_mut().set_node_usage(ix, usage) {
                                    t.refresh(cx);
                                }
                                cx.notify();
                            });
                            this.follow_selection(cx);
                        });
                        if alive.is_err() {
                            break;
                        }
                    }
                });
                metrics.push((guard, task));
            }
        }
        self.watch = Some(watches);
        self.metrics = Some(metrics);
        cx.notify();
    }

    /// Keep the highlight on the selected object once the rows moved: updates, a
    /// filter, a sort (also by usage, which every metrics poll changes).
    pub(super) fn follow_selection(&mut self, cx: &mut Context<Self>) {
        let moved = self.table.update(cx, crate::table::follow_selection);
        if moved.is_some() {
            self.skip_select = moved;
        }
    }

    /// What the bar above the list says: what failed (contexts, watches), then progress.
    pub(super) fn status_line(&self) -> Option<SharedString> {
        self.list.status_line(self.connect_errors.as_ref())
    }

    /// Keep the list on screen in `views` (if it was complete), before another replaces it.
    pub(super) fn keep_view(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.view_key.take() else { return };
        let table = self.table.read(cx).delegate();
        if let Some(view) = self.list.snapshot(table.defs(), table.forbidden) {
            self.views.put(key, view);
        }
    }

    /// Every cluster's rows into the table, laid out in its columns.
    fn show_rows(&mut self, cx: &mut Context<Self>) {
        let columns = self.table.read(cx).delegate().defs();
        let merged = self.list.merged(&columns);
        self.table.update(cx, |t, cx| {
            t.delegate_mut().set_rows(merged);
            cx.notify();
        });
        self.follow_selection(cx);
    }

    /// Messages that piled up from one cluster's watch: every one in order, except
    /// rows that newer rows replace (each costs a filter and a sort of the whole list).
    pub(super) fn on_table_msgs(&mut self, ctx: usize, batch: Vec<TableMsg>, cx: &mut Context<Self>) {
        #[cfg(feature = "dev")]
        let _t = crate::dev::timing::span("watch batch (UI)");
        for msg in coalesce(batch) {
            self.on_table_msg(ctx, msg, cx);
        }
    }

    pub(super) fn on_table_msg(&mut self, ctx: usize, msg: TableMsg, cx: &mut Context<Self>) {
        let name = self.clusters.get(ctx).map(|c| c.0.clone()).unwrap_or_default();
        let shown = {
            let table = self.table.read(cx);
            if table.delegate().columns_count(cx) > 0 { table.delegate().defs() } else { Arc::default() }
        };
        let up = self.list.apply(ctx, &name, msg, &shown);
        if let Some(columns) = up.columns {
            self.table.update(cx, |t, cx| {
                t.delegate_mut().update_columns(columns);
                t.refresh(cx);
            });
        }
        if up.rows {
            self.show_rows(cx);
        } else if up.follow {
            self.follow_selection(cx);
        }
        if up.loaded {
            self.table.update(cx, |t, _| {
                let d = t.delegate_mut();
                d.loading = false;
                if let Some(forbidden) = up.forbidden {
                    d.forbidden = forbidden;
                }
            });
        }
        cx.notify();
    }
}

/// The newest value already waiting in a channel (older ones are stale).
fn latest<T>(rx: &mut futures::channel::mpsc::UnboundedReceiver<T>, mut value: T) -> T {
    while let Ok(newer) = rx.try_recv() {
        value = newer;
    }
    value
}

/// Wait until the resource list is the page shown, pausing its watches meanwhile
/// (like detail pages do with `wait_visible`). False once the view is gone.
async fn wait_list_shown(this: &WeakEntity<AppView>, pause: &k8s::Pause, cx: &mut AsyncApp) -> bool {
    loop {
        match this.update(cx, |this, _| matches!(this.route(), Route::Resources(_))) {
            Err(_) => return false,
            Ok(true) => {
                pause.set(false);
                return true;
            }
            Ok(false) => {
                pause.set(true);
                cx.background_executor().timer(std::time::Duration::from_millis(500)).await;
            }
        }
    }
}
