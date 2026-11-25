//! Keeping the resource list live: table watches and metrics.

use std::sync::Arc;

use futures::StreamExt as _;
use gpui_kit::component::table::TableDelegate as _;
use gpui_kit::*;

use super::contexts::cluster_namespace;
use super::live_list::{LiveList, coalesce};
use super::{AppView, Route};
use crate::k8s;
use crate::k8s::TableMsg;

/// The watches of the list on screen: each cluster's table watch and, for pods with usage
/// shown, the watch of their requests and limits. Dropping it stops them.
pub(super) struct ListWatch {
    tasks: Vec<(k8s::TaskGuard, Task<()>)>,
    /// Each cluster's pods' requests and limits, shown again at once when a kept list is back.
    resources: Vec<(usize, k8s::PodRequestsRx)>,
    /// Which clusters showed usage when the watches started: the resources watches follow it.
    metrics_on: Vec<bool>,
}

/// A list left recently, its watches still going (see `kept_lists`): paused like a hidden
/// page's, events still applied, so it comes back current without listing again.
pub(super) struct KeptList {
    /// The `watch_id` its tasks answer to: they touch the table only while it is current.
    id: u64,
    watch: ListWatch,
    list: LiveList,
    /// The columns the table showed it in.
    columns: Arc<Vec<k8s::ColumnDef>>,
    pause: k8s::Pause,
}

impl AppView {
    /// Show the list of the current kind, clusters and namespaces. A list left recently
    /// comes back with its watches, current and without a list request; the list left is
    /// kept the same way.
    pub(super) fn restart_watch(&mut self, cx: &mut Context<Self>) {
        self.switch_watch(false, cx);
    }

    /// List what is shown again from scratch (Refresh): a kept list is not reused.
    pub(super) fn reload_watch(&mut self, cx: &mut Context<Self>) {
        self.switch_watch(true, cx);
    }

    /// The list on screen, out of the view with its watches; None when it has none, or was
    /// refused (its watches ended: nothing would keep it current).
    fn leave_list(&mut self, cx: &mut Context<Self>) -> Option<KeptList> {
        let watch = self.watch.take()?;
        // Its tasks wait from here on (see `wait_list_shown`), its watches hold rows back.
        let pause = std::mem::take(&mut self.list_pause);
        pause.set(true);
        let table = self.table.read(cx).delegate();
        let (columns, forbidden) = (table.defs(), table.forbidden);
        let list = std::mem::take(&mut self.list);
        (!forbidden).then_some(KeptList { id: self.watch_id, watch, list, columns, pause })
    }

    /// Keep `left`, the list that was under `key`, unless it is the list coming on screen
    /// (`next`): then it is returned to go on as it was, except on a reload. A list still
    /// listing is not kept: its rows, and so its cost, aren't known yet.
    fn keep_left(
        &mut self,
        key: Option<String>,
        left: Option<KeptList>,
        next: Option<&str>,
        fresh: bool,
        cx: &mut Context<Self>,
    ) -> Option<KeptList> {
        let (Some(key), Some(left)) = (key, left) else { return None };
        if next == Some(key.as_str()) {
            return (!fresh).then_some(left);
        }
        if !left.list.is_loading() && !left.watch.tasks.is_empty() {
            let rows = left.list.row_count();
            self.kept.put(key, left, rows, std::time::Instant::now());
            self.sweep_kept(cx);
        }
        None
    }

    /// Drop the kept lists left too long ago, every 30 seconds while some are kept.
    fn sweep_kept(&mut self, cx: &mut Context<Self>) {
        if self.kept_sweeping {
            return;
        }
        self.kept_sweeping = true;
        self.kept_sweep = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(30)).await;
                let more = this.update(cx, |this, _| {
                    this.kept.expire(std::time::Instant::now());
                    this.kept_sweeping = !this.kept.is_empty();
                    this.kept_sweeping
                });
                if !matches!(more, Ok(true)) {
                    break;
                }
            }
        }));
    }

    fn switch_watch(&mut self, fresh: bool, cx: &mut Context<Self>) {
        // The lists a connect started early go to the first list after it, or nowhere.
        let early = self.early.take();
        let left_key = self.view_key.clone();
        self.keep_view(cx);
        let left = self.leave_list(cx);
        self.metrics = None;
        // From here on, no task of the list left touches the table.
        self.watch_seq += 1;
        self.watch_id = self.watch_seq;
        let Some(kind) = self.kind.clone() else {
            drop(self.keep_left(left_key, left, None, fresh, cx));
            return;
        };
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
        let custom = self.column_prefs.custom(&kind.kubectl_name());
        let key = list_key(&names, &kind, &watched, &custom);
        let metrics_on: Vec<bool> = self.clusters.iter().map(|(c, _)| crate::settings::metrics_on(c)).collect();
        let kept = self.keep_left(left_key, left, Some(&key), fresh, cx);
        if fresh {
            self.kept.remove(&key);
        }
        let kept = kept
            .or_else(|| if fresh { None } else { self.kept.take(&key, std::time::Instant::now()) })
            // Usage turned on or off since: its watches don't match any more.
            .filter(|k| k.watch.metrics_on == metrics_on);
        let cached = if kept.is_some() {
            None
        } else {
            self.views.get(&key).filter(|v| v.rows.len() == self.clusters.len()).cloned()
        };
        self.view_key = Some(key);
        let (mut kept, kept_columns) = if let Some(k) = kept {
            // Its tasks answer to the id they started with.
            self.watch_id = k.id;
            self.list = k.list;
            (Some((k.watch, k.pause)), Some(k.columns))
        } else {
            // Wait only for the clusters watched: the others never report.
            self.list =
                LiveList::start(self.clusters.len(), (0..self.clusters.len()).filter(|&ix| watched[ix].is_some()));
            (None, None)
        };
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
        // The checked rows' logs, read together.
        if k8s::has_pod_logs(&kind) {
            actions.push("logs");
        }
        // Event rows open the object they are about: no write actions on them.
        if !kind.is("", "Event") {
            actions.push("delete");
        }
        // The table applies what the box shows (each kind's own filter: see `kind_filters`).
        let filter_text = self.filter.read(cx).value().to_string();
        self.table.update(cx, |t, cx| {
            t.delegate_mut().pods = is_pod;
            t.delegate_mut().ingresses = kind.is("networking.k8s.io", "Ingress");
            t.delegate_mut().nodes = kind.is("", "Node");
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
        // Kept from before: its rows as its watches last sent them, current again within half
        // a second of being shown (a paused watch sends what changed meanwhile).
        if let (Some((watch, _)), Some(columns)) = (&kept, kept_columns) {
            let merged = self.list.merged(&columns);
            self.table.update(cx, |t, cx| {
                let d = t.delegate_mut();
                if !columns.is_empty() {
                    d.update_columns(columns);
                }
                d.set_rows(merged);
                for (ix, rx) in &watch.resources {
                    let pods = rx.borrow().as_ref().and_then(|read| Some(read.as_ref().ok()?.pods.clone()));
                    if let Some(pods) = pods {
                        d.set_resources(*ix, pods);
                    }
                }
                t.refresh(cx);
            });
        }
        let restored = kept.is_some();
        // The connect's early lists, when this is the list they started.
        let mut early = early.filter(|e| !restored && e.matches(&kind, &custom));
        let (mut watch, pause) = kept.take().unwrap_or_else(|| {
            let watch = ListWatch { tasks: Vec::new(), resources: Vec::new(), metrics_on: metrics_on.clone() };
            // Theirs, so they pause and go on with the others.
            (watch, early.as_ref().map_or_else(k8s::Pause::default, super::reach::EarlyLists::pause))
        });
        let mut metrics = Vec::new();
        // Set while the list isn't on screen (another page, a hidden window): watches hold
        // their snapshots back, metrics stop polling.
        pause.set(!self.list_on_screen());
        self.list_pause = pause.clone();
        // Before the watches start: with a filter on labels, they list with the metadata.
        self.labels_for_filter(cx);
        let id = self.watch_id;
        for (ix, ((ctx_name, client), ns)) in self.clusters.clone().into_iter().zip(watched).enumerate() {
            let Some(ns) = ns else { continue };
            let usage_on = metrics_on.get(ix).copied().unwrap_or(false);
            if !restored {
                // Set until this cluster has rows on screen (the ones from before count): the
                // usage bars' read of whole pods waits for them, the list gets the API first.
                let rows_due = k8s::Pause::default();
                rows_due.set(progressive);
                // Started with the connect, alongside discovery: its rows may be in already.
                let started = early.as_mut().and_then(|e| e.take(&ctx_name, ns.as_deref()));
                let (guard, rx) = started.unwrap_or_else(|| {
                    k8s::watch_table_paused(
                        client.clone(),
                        kind.clone(),
                        ns.clone(),
                        Arc::new(custom.clone()),
                        pause.clone(),
                        progressive,
                        // The timeline records what this list sees change (no request added).
                        crate::timeline::tap(&ctx_name, &kind, ns.clone()),
                    )
                });
                let (shown, rows_in) = (pause.clone(), rows_due.clone());
                let task = cx.spawn(async move |this, cx| follow_table(this, cx, id, ix, rx, shown, rows_in).await);
                watch.tasks.push((guard, task));
                if usage_on && kind.is("", "Pod") {
                    // Requests and limits, for the usage bars: listed once, then kept by a watch.
                    let (guard, mut rx) = k8s::watch_pod_resources(client.clone(), ns.clone(), pause.clone(), rows_due);
                    watch.resources.push((ix, rx.clone()));
                    let task = cx.spawn(async move |this, cx| {
                        while rx.changed().await.is_ok() {
                            let read = rx.borrow_and_update().clone();
                            // A list that failed, or none yet: the bars keep what they have.
                            let Some(Ok(read)) = read else { continue };
                            let alive = this.update(cx, |this, cx| {
                                // A kept list's: read again when it is back on screen.
                                if this.watch_id == id {
                                    this.table.update(cx, |t, cx| {
                                        t.delegate_mut().set_resources(ix, read.pods);
                                        cx.notify();
                                    });
                                }
                            });
                            if alive.is_err() {
                                break;
                            }
                        }
                    });
                    watch.tasks.push((guard, task));
                }
            }
            if usage_on && kind.is("", "Pod") {
                let (guard, mut rx) = k8s::watch_pod_metrics(client, ns, pause.clone());
                let task = cx.spawn(async move |this, cx| {
                    while let Some(usage) = rx.next().await {
                        let (usage, fresh) = latest(&mut rx, usage);
                        let alive = this.update(cx, |this, cx| {
                            this.table.update(cx, |t, cx| {
                                if t.delegate_mut().set_usage(ix, usage, fresh) {
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
            } else if usage_on && kind.is("", "Node") {
                let (guard, mut rx) = k8s::watch_node_metrics(client, pause.clone());
                let task = cx.spawn(async move |this, cx| {
                    while let Some(usage) = rx.next().await {
                        let (usage, fresh) = latest(&mut rx, usage);
                        let alive = this.update(cx, |this, cx| {
                            this.table.update(cx, |t, cx| {
                                if t.delegate_mut().set_node_usage(ix, usage, fresh) {
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
        self.watch = Some(watch);
        self.metrics = Some(metrics);
        cx.notify();
    }

    /// Whether the resource list is on screen: its page shown, in a window not hidden.
    pub(super) fn list_on_screen(&self) -> bool {
        matches!(self.route(), Route::Resources(_)) && !self.window_hidden
    }

    /// Keep the highlight on the selected object once the rows moved: updates, a
    /// filter, a sort (also by usage, which every metrics poll changes).
    pub(super) fn follow_selection(&mut self, cx: &mut Context<Self>) {
        let moved = self.table.update(cx, crate::table::follow_selection);
        if moved.is_some() {
            self.skip_select = moved;
        }
    }

    /// A filter on labels needs every row's labels, which a big list in every namespace
    /// doesn't download (see `k8s::Pause::want_metadata`): its watches list again with them,
    /// once for the list's life, and the bar says they load meanwhile.
    pub(super) fn labels_for_filter(&mut self, cx: &mut Context<Self>) {
        if self.list_pause.wants_metadata() || !self.table.read(cx).delegate().filter_reads_labels() {
            return;
        }
        self.list_pause.want_metadata();
        if self.list.wait_labels() {
            cx.notify();
        }
    }

    /// What the bar above the list says: what failed (add-on APIs, watches), then progress.
    pub(super) fn status_line(&self) -> Option<SharedString> {
        let failed = self.failed_line(&k8s::short_names::short_name).map(SharedString::from);
        self.list.status_line(failed.as_ref())
    }

    /// The status line's failures with each context's full name, for its hover: None when
    /// the line names none by a shorter one.
    pub(super) fn status_hover(&self) -> Option<SharedString> {
        let full = self.failed_line(&|c: &str| c.to_string())?;
        (Some(&full) != self.failed_line(&k8s::short_names::short_name).as_ref()).then(|| full.into())
    }

    /// What failed besides each context's own failure (said by its line, see `connect_view`):
    /// the connect's notes, then the add-on APIs, each context named by `name`.
    fn failed_line(&self, name: &dyn Fn(&str) -> String) -> Option<String> {
        let contexts: Vec<&str> =
            (self.clusters.iter().map(|(c, _)| c.as_str())).chain(self.conn.iter().map(|(c, _)| c.as_str())).collect();
        let notes = super::conn::notes_line(&self.connect_notes, &contexts, name);
        let skipped = super::contexts::skipped_notice(&self.skipped, self.clusters.len(), name);
        match (notes, skipped) {
            (Some(notes), Some(skipped)) => Some(format!("{notes}; {skipped}")),
            (notes, skipped) => notes.or(skipped),
        }
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
        let name = self.clusters.get(ctx).map_or_default(|c| c.0.clone());
        let shown = {
            let table = self.table.read(cx);
            if table.delegate().columns_count(cx) > 0 { table.delegate().defs() } else { Arc::default() }
        };
        #[cfg(feature = "dev")]
        let listed = matches!(msg, TableMsg::Listed);
        let up = self.list.apply(ctx, &name, msg, &shown);
        #[cfg(feature = "dev")]
        crate::dev::first_rows::seen(&name, up.rows, listed);
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

/// What a list is kept and cached under: its clusters, kind, what each cluster watches
/// (`cluster_namespace`) and its custom columns.
pub(super) fn list_key(
    names: &[String],
    kind: &k8s::Kind,
    watched: &[Option<Option<String>>],
    custom: &[(String, String)],
) -> String {
    format!("{}|{}|{:?}|{:?}", names.join(","), kind.key(), watched, custom)
}

/// The newest value already waiting in a channel (older ones are stale).
fn latest<T>(rx: &mut futures::channel::mpsc::UnboundedReceiver<T>, mut value: T) -> T {
    while let Ok(newer) = rx.try_recv() {
        value = newer;
    }
    value
}

/// Bring what cluster `ix`'s table watch sends to the list `id`, while that list is on
/// screen. Messages that come while it isn't (a kept list, or the list replaced while they
/// were on their way) are held, and applied once it is back: dropping them would lose rows
/// a paused watch sends only once, or the `Listed` that ends the loading.
async fn follow_table(
    this: WeakEntity<AppView>,
    cx: &mut AsyncApp,
    id: u64,
    ix: usize,
    mut rx: futures::channel::mpsc::UnboundedReceiver<TableMsg>,
    shown: k8s::Pause,
    rows_in: k8s::Pause,
) {
    let mut held = Held::default();
    while wait_list_shown(&this, &shown, id, cx).await {
        if held.0.is_empty() {
            let Some(msg) = rx.next().await else { break };
            held.0.push(msg);
        }
        // Everything already waiting goes in one update: only its last rows count.
        while let Ok(msg) = rx.try_recv() {
            held.0.push(msg);
        }
        if held.0.iter().any(|m| matches!(m, TableMsg::Rows(_) | TableMsg::Listed)) {
            rows_in.set(false);
        }
        let applied = this.update(cx, |this, cx| {
            if let Some(batch) = held.release(this.watch_id == id) {
                this.on_table_msgs(ix, batch, cx);
            }
        });
        if applied.is_err() {
            break;
        }
    }
}

/// The messages of one list's watch not applied yet, oldest first.
struct Held<M>(Vec<M>);

impl<M> Default for Held<M> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<M> Held<M> {
    /// All of them, in the order they came, when their list is the current one; otherwise
    /// they stay held (another list is on screen: they must not reach its table).
    fn release(&mut self, current: bool) -> Option<Vec<M>> {
        current.then(|| std::mem::take(&mut self.0))
    }
}

/// Wait until the list `id` is on screen (the current list, its page shown, the window not
/// hidden), pausing its watches meanwhile (like detail pages do with `wait_visible`). False
/// once the view is gone.
async fn wait_list_shown(this: &WeakEntity<AppView>, pause: &k8s::Pause, id: u64, cx: &mut AsyncApp) -> bool {
    loop {
        match this.update(cx, |this, _| this.watch_id == id && this.list_on_screen()) {
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
    use super::Held;

    #[test]
    fn held_messages_wait_for_their_list_and_keep_their_order() {
        let mut held = Held::default();
        held.0.extend([1, 2]);
        // Another list is on screen: nothing reaches the table, nothing is lost.
        assert_eq!(held.release(false), None);
        held.0.push(3);
        assert_eq!(held.release(false), None);
        // Back on screen: everything, oldest first, once.
        assert_eq!(held.release(true), Some(vec![1, 2, 3]));
        assert_eq!(held.release(true), Some(vec![]));
    }
}
