//! Keeping the resource list live: table watches, metrics and the problem count.

use std::collections::HashSet;
use std::sync::Arc;

use futures::StreamExt as _;
use gpui_kit::component::table::TableDelegate as _;
use gpui_kit::*;

use super::contexts::cluster_namespace;
use super::{AppView, Route, view_cache};
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
        self.watch_errors.clear();
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
        self.defs_by_ctx = vec![Arc::default(); self.clusters.len()];
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
        self.listing = (0..self.clusters.len()).filter(|&ix| watched[ix].is_some()).collect();
        self.refused.clear();
        let loading = !self.listing.is_empty();
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
        self.status = loading.then(|| "Loading…".into());
        if serving.is_empty() {
            self.status = Some(format!("{} is not available on the connected clusters", kind.label).into());
        }
        // Seen recently: show it as it was while the new list loads (whole, not by pages).
        let progressive = cached.is_none();
        if let Some(view) = cached {
            self.rows_by_ctx = view.rows;
            // Kept laid out in the columns of the list shown.
            self.defs_by_ctx = vec![view.columns.clone(); self.clusters.len()];
            // A cluster that no longer serves the kind is not watched: nothing would replace its rows.
            keep_watched(&mut self.rows_by_ctx, &self.listing);
            let merged = self.merged_rows(&view.columns);
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
    /// Both show: a failing cluster's rows may be the old ones the progress notice warns of.
    pub(super) fn status_line(&self) -> Option<SharedString> {
        status_text(self.connect_errors.iter().chain(self.watch_errors.values()), self.status.as_ref())
    }

    /// Keep the list on screen in `views` (if it was complete), before another replaces it.
    pub(super) fn keep_view(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.view_key.take() else { return };
        let forbidden = self.table.read(cx).delegate().forbidden;
        if !self.listing.is_empty() || forbidden || self.rows_by_ctx.iter().all(Vec::is_empty) {
            return;
        }
        let columns = self.table.read(cx).delegate().defs();
        let rows = (self.rows_by_ctx.iter().zip(&self.defs_by_ctx))
            .map(|(rows, defs)| relayout(rows, defs, &columns))
            .collect();
        let view = view_cache::CachedView { columns, rows, at: std::time::Instant::now() };
        self.views.put(key, view);
    }

    /// Every cluster's rows, each with its cluster's index, laid out in `columns` (the
    /// table's).
    fn merged_rows(&self, columns: &[k8s::ColumnDef]) -> Vec<(usize, Arc<k8s::Row>)> {
        (self.rows_by_ctx.iter().zip(&self.defs_by_ctx))
            .enumerate()
            .flat_map(|(i, (rows, defs))| relayout(rows, defs, columns).into_iter().map(move |r| (i, r)))
            .collect()
    }

    /// Every cluster's rows into the table, laid out in its columns.
    fn show_rows(&mut self, cx: &mut Context<Self>) {
        let columns = self.table.read(cx).delegate().defs();
        let merged = self.merged_rows(&columns);
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
        match msg {
            TableMsg::Columns(cols) => {
                // This cluster's rows from before (a relist after a CRD upgrade) move under
                // its new columns, so they stay laid out in what `defs_by_ctx` says.
                let mut moved = false;
                if let (Some(rows), Some(defs)) = (self.rows_by_ctx.get_mut(ctx), self.defs_by_ctx.get_mut(ctx)) {
                    moved = !rows.is_empty() && !same_names(defs, &cols);
                    if moved {
                        *rows = relayout(rows, defs, &cols);
                    }
                    *defs = cols.clone();
                }
                // The table shows the first cluster's columns (or the first to come); the
                // other clusters' rows are laid out under them by column name.
                let shown = self.table.update(cx, |t, cx| {
                    if t.delegate().columns_count(cx) > 0 && ctx != 0 {
                        return false;
                    }
                    let changed = !same_names(&t.delegate().defs(), &cols);
                    // A relist sends them again: keeps the user's sort and column order.
                    t.delegate_mut().update_columns(cols);
                    t.refresh(cx);
                    changed
                });
                if moved || (shown && self.rows_by_ctx.iter().any(|r| !r.is_empty())) {
                    self.show_rows(cx);
                } else if shown {
                    // New columns sort the rows again: the highlight stays on its object.
                    self.follow_selection(cx);
                }
            }
            TableMsg::Rows(rows) => {
                // "Loading…" or "showing the list from 2h ago" holds while another cluster is
                // still listing: its rows are not there yet, or are the old ones.
                if !others_listing(&self.listing, ctx) {
                    self.status = None;
                }
                self.watch_errors.remove(&ctx);
                if let Some(slot) = self.rows_by_ctx.get_mut(ctx) {
                    *slot = rows;
                }
                self.show_rows(cx);
            }
            TableMsg::Listed => {
                self.listing.remove(&ctx);
                if self.listing.is_empty() {
                    self.status = None;
                    self.table.update(cx, |t, _| t.delegate_mut().loading = false);
                }
            }
            TableMsg::Error(e) => {
                let name = self.clusters.get(ctx).map(|c| c.0.clone()).unwrap_or_default();
                self.watch_errors.insert(ctx, format!("{name}: {e} (retrying)").into());
            }
            TableMsg::Forbidden(e) => {
                let name = self.clusters.get(ctx).map(|c| c.0.clone()).unwrap_or_default();
                let text = format!("{name}: your access does not allow listing this type ({e})");
                self.watch_errors.insert(ctx, text.into());
                self.listing.remove(&ctx);
                self.refused.insert(ctx);
                // The watch ends here: rows listed before (or cached) would look live.
                if let Some(slot) = self.rows_by_ctx.get_mut(ctx)
                    && !slot.is_empty()
                {
                    slot.clear();
                    self.show_rows(cx);
                }
                if self.listing.is_empty() {
                    self.status = None;
                    // Forbidden only when every cluster watched refused, whichever answers last.
                    let all = self.refused.len() >= self.watch.as_ref().map_or(0, Vec::len);
                    self.table.update(cx, |t, _| {
                        let d = t.delegate_mut();
                        d.loading = false;
                        d.forbidden = all;
                    });
                }
            }
        }
        cx.notify();
    }
}

/// What failed, then the progress notice, in one line.
fn status_text<'a>(
    failed: impl Iterator<Item = &'a SharedString>,
    status: Option<&'a SharedString>,
) -> Option<SharedString> {
    let parts: Vec<&str> = failed.chain(status).map(AsRef::as_ref).collect();
    (!parts.is_empty()).then(|| parts.join("; ").into())
}

/// Whether a cluster other than `ctx` is still listing.
fn others_listing(listing: &HashSet<usize>, ctx: usize) -> bool {
    listing.iter().any(|&ix| ix != ctx)
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

fn same_names(a: &[k8s::ColumnDef], b: &[k8s::ColumnDef]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.name == y.name)
}

/// Rows whose cells are laid out in `from`, moved under `to` by column name: a column
/// `from` lacks is empty. Rows with no known columns on either side stay as they are.
fn relayout(rows: &[Arc<k8s::Row>], from: &[k8s::ColumnDef], to: &[k8s::ColumnDef]) -> Vec<Arc<k8s::Row>> {
    if from.is_empty() || to.is_empty() || same_names(from, to) {
        return rows.to_vec();
    }
    let at: Vec<Option<usize>> = to.iter().map(|d| from.iter().position(|f| f.name == d.name)).collect();
    rows.iter()
        .map(|r| {
            Arc::new(k8s::Row {
                uid: r.uid.clone(),
                name: r.name.clone(),
                namespace: r.namespace.clone(),
                created: r.created,
                cells: at.iter().map(|i| i.and_then(|i| r.cells.get(i)).cloned().unwrap_or_default()).collect(),
                labels: r.labels.clone(),
                deleting: r.deleting,
                deleted_at: r.deleted_at,
            })
        })
        .collect()
}

/// Empty the rows of the clusters not in `watched`.
fn keep_watched<T>(rows: &mut [Vec<T>], watched: &HashSet<usize>) {
    for (ix, slot) in rows.iter_mut().enumerate() {
        if !watched.contains(&ix) {
            slot.clear();
        }
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

#[cfg(test)]
mod tests {
    use super::{SharedString, TableMsg, coalesce, keep_watched, others_listing, relayout, status_text};

    #[test]
    fn the_old_list_notice_stays_while_another_cluster_lists() {
        let listing = [0, 1].into_iter().collect();
        assert!(others_listing(&listing, 0));
        assert!(!others_listing(&std::iter::once(1).collect(), 1));
        assert!(!others_listing(&Default::default(), 0));
        // A failing cluster's error does not hide the notice: its rows may be the old ones.
        let (error, notice) = (SharedString::from("b: timeout (retrying)"), SharedString::from("Updating…"));
        assert_eq!(
            status_text([&error].into_iter(), Some(&notice)).as_deref(),
            Some("b: timeout (retrying); Updating…")
        );
        assert_eq!(status_text(std::iter::empty(), None), None);
    }

    #[test]
    fn cells_move_under_the_shown_columns() {
        let defs = |names: &[&str]| -> Vec<crate::k8s::ColumnDef> {
            names.iter().map(|n| crate::k8s::ColumnDef { name: n.to_string(), priority: 0 }).collect()
        };
        let mut r = row();
        r.cells = vec!["web".into(), "True".into(), "le".into(), "1d".into()];
        let rows = vec![std::sync::Arc::new(r)];
        // Another operator release: Issuer before Ready, and no Status column.
        let from = defs(&["Name", "Ready", "Issuer", "Age"]);
        let to = defs(&["Name", "Issuer", "Ready", "Status", "Age"]);
        let moved = relayout(&rows, &from, &to);
        assert_eq!(moved[0].cells, ["web", "le", "True", "", "1d"]);
        assert_eq!(moved[0].uid, "u");
        // Same columns, or none known: the rows themselves.
        assert!(std::sync::Arc::ptr_eq(&relayout(&rows, &from, &from)[0], &rows[0]));
        assert!(std::sync::Arc::ptr_eq(&relayout(&rows, &[], &to)[0], &rows[0]));
        assert!(std::sync::Arc::ptr_eq(&relayout(&rows, &from, &[])[0], &rows[0]));
    }

    #[test]
    fn rows_of_clusters_not_watched_are_dropped() {
        let mut rows = vec![vec!["prod-pod"], vec!["staging-pod"], vec!["dev-pod"]];
        keep_watched(&mut rows, &[0, 2].into_iter().collect());
        assert_eq!(rows, [vec!["prod-pod"], vec![], vec!["dev-pod"]]);
    }

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
