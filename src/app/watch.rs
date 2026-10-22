//! Keeping the resource list live: table watches, metrics and the problem count.

#[allow(unused_imports)]
use super::*;

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
                let scans = clusters.iter().map(|(_, c)| crate::problems::scan_partial(c.clone(), &kinds, None));
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
        self.rows_by_ctx = vec![Vec::new(); self.clusters.len()];
        let key = format!(
            "{}|{}|{:?}|{:?}",
            names.join(","),
            kind.key(),
            self.namespace,
            self.column_prefs.custom(&kind.kubectl_name())
        );
        let cached = self.views.get(&key).filter(|v| v.rows.len() == self.clusters.len()).cloned();
        self.view_key = Some(key);
        // Wait only for the clusters that serve this kind: the others never report.
        let serving = self.kind_clusters.get(&kind.key()).cloned();
        self.listing = (0..self.clusters.len()).filter(|ix| serving.as_ref().is_none_or(|s| s.contains(ix))).collect();
        let loading = !self.listing.is_empty();
        self.table.update(cx, |t, _| {
            let d = t.delegate_mut();
            d.loading = loading;
            d.forbidden = false;
        });
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
            t.delegate_mut().set_namespace_filter(ns_filter);
            t.delegate_mut().set_filter(filter_text);
            t.refresh(cx);
        });
        self.status = loading.then(|| "Loading…".into());
        // Seen recently: show it as it was while the new list loads (whole, not by pages).
        let progressive = cached.is_none();
        if let Some(view) = cached {
            self.rows_by_ctx = view.rows;
            let merged = self.merged_rows();
            self.table.update(cx, |t, cx| {
                t.delegate_mut().loading = false;
                t.delegate_mut().update_columns(view.columns);
                t.delegate_mut().set_rows(merged);
                t.refresh(cx);
            });
            if loading {
                let ago = k8s::age(
                    jiff::Timestamp::now() - jiff::SignedDuration::try_from(view.at.elapsed()).unwrap_or_default(),
                );
                self.status = Some(format!("Updating… (showing the list from {ago} ago)").into());
            }
        }
        let mut watches = Vec::new();
        let mut metrics = Vec::new();
        // Set while the list is hidden: watches hold their snapshots back, metrics stop polling.
        let pause = k8s::Pause::default();
        for (ix, (ctx_name, client)) in self.clusters.clone().into_iter().enumerate() {
            if serving.as_ref().is_some_and(|s| !s.contains(&ix)) {
                continue; // this cluster doesn't have the kind
            }
            let (guard, mut rx) = k8s::watch_table_paused(
                client.clone(),
                kind.clone(),
                self.namespace.clone(),
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
            let table = self.table.clone();
            let metrics_on = crate::settings::metrics_on(&ctx_name);
            if metrics_on && kind.is("", "Pod") {
                let (guard, mut rx) = k8s::watch_pod_metrics(client, self.namespace.clone(), pause.clone());
                let task = cx.spawn(async move |_, cx| {
                    while let Some(usage) = rx.next().await {
                        let usage = latest(&mut rx, usage);
                        table.update(cx, |t, cx| {
                            if t.delegate_mut().set_usage(ix, usage) {
                                t.refresh(cx);
                            }
                            cx.notify();
                        });
                    }
                });
                metrics.push((guard, task));
            } else if metrics_on && kind.is("", "Node") {
                let (guard, mut rx) = k8s::watch_node_metrics(client, pause.clone());
                let task = cx.spawn(async move |_, cx| {
                    while let Some(usage) = rx.next().await {
                        let usage = latest(&mut rx, usage);
                        table.update(cx, |t, cx| {
                            if t.delegate_mut().set_node_usage(ix, usage) {
                                t.refresh(cx);
                            }
                            cx.notify();
                        });
                    }
                });
                metrics.push((guard, task));
            }
        }
        self.watch = Some(watches);
        self.metrics = Some(metrics);
        cx.notify();
    }

    /// Keep the list on screen in `views` (if it was complete), before another replaces it.
    fn keep_view(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.view_key.take() else { return };
        let forbidden = self.table.read(cx).delegate().forbidden;
        if !self.listing.is_empty() || forbidden || self.rows_by_ctx.iter().all(Vec::is_empty) {
            return;
        }
        let columns = self.table.read(cx).delegate().defs();
        let view = view_cache::CachedView { columns, rows: self.rows_by_ctx.clone(), at: std::time::Instant::now() };
        self.views.put(key, view);
    }

    /// Every cluster's rows, each with its cluster's index.
    fn merged_rows(&self) -> Vec<(usize, Arc<k8s::Row>)> {
        self.rows_by_ctx.iter().enumerate().flat_map(|(i, rows)| rows.iter().map(move |r| (i, r.clone()))).collect()
    }

    /// Messages that piled up from one cluster's watch: every one in order, except
    /// rows that newer rows replace (each costs a filter and a sort of the whole list).
    pub(super) fn on_table_msgs(&mut self, ctx: usize, batch: Vec<TableMsg>, cx: &mut Context<Self>) {
        for msg in coalesce(batch) {
            self.on_table_msg(ctx, msg, cx);
        }
    }

    pub(super) fn on_table_msg(&mut self, ctx: usize, msg: TableMsg, cx: &mut Context<Self>) {
        match msg {
            // Every cluster serves the same columns for a kind; take the first we get.
            TableMsg::Columns(cols) => self.table.update(cx, |t, cx| {
                if t.delegate().columns_count(cx) == 0 || ctx == 0 {
                    // A relist sends them again: keeps the user's sort and column order.
                    t.delegate_mut().update_columns(cols);
                    t.refresh(cx);
                }
            }),
            TableMsg::Rows(rows) => {
                self.status = None;
                if let Some(slot) = self.rows_by_ctx.get_mut(ctx) {
                    *slot = rows;
                }
                let merged = self.merged_rows();
                let moved = self.table.update(cx, |t, cx| {
                    let menu_open = t.right_clicked_row().is_some();
                    t.delegate_mut().set_rows(merged);
                    // Keep the selection on the same object as rows move. The table
                    // reports it as a selection: the app skips that one (not a click).
                    let uid = t.delegate().selected_uid.clone();
                    let moved = match uid.and_then(|uid| t.delegate().visible_index_of(&uid)) {
                        Some(ix) if t.selected_row() != Some(ix) => {
                            t.set_selected_row(ix, cx);
                            Some(ix)
                        }
                        _ => None,
                    };
                    // The row menu's highlight follows its row too (selecting clears it).
                    if menu_open {
                        let ix = t.delegate().menu_row.as_ref().and_then(|(_, uid)| t.delegate().visible_index_of(uid));
                        t.set_right_clicked_row(ix, cx);
                    }
                    cx.notify();
                    moved
                });
                if moved.is_some() {
                    self.skip_select = moved;
                }
            }
            TableMsg::Listed => {
                self.listing.remove(&ctx);
                if self.listing.is_empty() {
                    self.table.update(cx, |t, _| t.delegate_mut().loading = false);
                }
            }
            TableMsg::Error(e) => {
                let name = self.clusters.get(ctx).map(|c| c.0.clone()).unwrap_or_default();
                self.status = Some(format!("{name}: {e} (retrying)").into());
            }
            TableMsg::Forbidden(e) => {
                let name = self.clusters.get(ctx).map(|c| c.0.clone()).unwrap_or_default();
                self.status = Some(format!("{name}: your access does not allow listing this type ({e})").into());
                self.listing.remove(&ctx);
                if self.listing.is_empty() {
                    self.table.update(cx, |t, _| {
                        let d = t.delegate_mut();
                        d.loading = false;
                        d.forbidden = true;
                    });
                }
            }
        }
        cx.notify();
    }
}

/// Drop rows that newer rows in the same batch replace; everything else keeps its order.
fn coalesce(batch: Vec<TableMsg>) -> Vec<TableMsg> {
    let last_rows = batch.iter().rposition(|m| matches!(m, TableMsg::Rows(_)));
    batch
        .into_iter()
        .enumerate()
        .filter(|(i, m)| !matches!(m, TableMsg::Rows(_)) || Some(*i) == last_rows)
        .map(|(_, m)| m)
        .collect()
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

#[cfg(test)]
mod tests {
    use super::{TableMsg, coalesce};

    fn row() -> crate::k8s::Row {
        crate::k8s::Row {
            uid: "u".into(),
            name: "n".into(),
            namespace: None,
            created: None,
            cells: Vec::new(),
            labels: "".into(),
            deleting: false,
            deleted_at: None,
        }
    }

    #[test]
    fn coalesces_rows() {
        let rows = |n: usize| TableMsg::Rows((0..n).map(|_| std::sync::Arc::new(row())).collect());
        let batch = vec![rows(1), TableMsg::Columns(Default::default()), rows(2), TableMsg::Listed, rows(3)];
        let kept: Vec<String> = coalesce(batch)
            .iter()
            .map(|m| match m {
                TableMsg::Rows(r) => format!("rows{}", r.len()),
                TableMsg::Columns(_) => "columns".into(),
                TableMsg::Listed => "listed".into(),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(kept, ["columns", "listed", "rows3"]);
        assert_eq!(coalesce(vec![TableMsg::Error("x".into())]).len(), 1);
    }
}
