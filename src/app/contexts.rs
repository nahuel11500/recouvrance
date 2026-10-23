//! Connecting to contexts, namespaces, and reloading kubeconfigs.

#[allow(unused_imports)]
use super::*;

impl AppView {
    /// Connect to one or more contexts; tables merge them with a Context column.
    pub(super) fn connect_many(&mut self, contexts: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(primary) = contexts.first().cloned() else { return };
        if dev_var("RECOUVRANCE_CONTEXT").is_none() {
            let remembered = contexts.clone();
            crate::settings::update(|s| {
                crate::settings::push_recent(&mut s.recent_contexts, &remembered, 10);
                s.last_contexts = remembered;
            });
        }
        let connected = contexts.clone();
        self.ctx_picker.update(cx, |p, cx| p.set_connected(connected, cx));
        let ctx_name = primary.clone();
        self.ns_picker.update(cx, |p, _| p.context = ctx_name);
        self.context = Some(primary.clone());
        // Keep the list on screen while the table still has its columns (reset below).
        self.keep_view(cx);
        self.client = None;
        self.clusters.clear();
        self.kinds.clear();
        self.watch = None;
        self.metrics = None;
        // Watch errors name the old clusters, and would hide "Connecting…" and new failures.
        self.watch_errors.clear();
        self.listing.clear();
        self.refused.clear();
        // The problem badge counted the old clusters.
        self._problem_watch = None;
        self.problem_counts = None;
        self.problem_gaps = None;
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
        self.index_built = None;
        self._index_tasks.clear();
        self.status = Some(format!("Connecting to {}…", contexts.join(", ")).into());
        self.connect_errors = None;
        self.connecting = true;
        self.table.update(cx, |t, cx| {
            t.delegate_mut().reset(false, Vec::new(), None, Default::default());
            t.refresh(cx);
        });
        cx.notify();

        self._connect = Some(cx.spawn_in(window, async move |this, cx| {
            let connects = contexts.iter().map(|c| k8s::connect(c.clone()));
            let results = futures::future::join_all(connects).await;
            let mut clusters = Vec::new();
            let mut errors = Vec::new();
            for (ctx, r) in contexts.iter().zip(results) {
                match r {
                    Ok(client) => clusters.push((ctx.clone(), client)),
                    Err(e) => errors.push(format!("{ctx}: {e:#}")),
                }
            }
            let result = async {
                let (_, client) = clusters.first().cloned().ok_or_else(|| anyhow::anyhow!(errors.join("; ")))?;
                // Connecting does not reach the server: discovery is the first request, and
                // an unreachable cluster fails there.
                let discovered =
                    futures::future::join_all(clusters.iter().map(|(_, c)| k8s::discover(c.clone()))).await;
                let names: Vec<&str> = clusters.iter().map(|(n, _)| n.as_str()).collect();
                let (mut kinds, kind_clusters) = merge_kinds(&names, discovered, &mut errors);
                if kinds.is_empty() {
                    anyhow::bail!("discovery failed on every cluster: {}", errors.join("; "));
                }
                // Built-in sections first, then custom resources by group.
                k8s::sort_for_sidebar(&mut kinds);
                // Namespace listing may be forbidden; that is not fatal. All clusters at once,
                // skipping those whose discovery failed: they would only wait out the timeout again.
                let reached: HashSet<usize> = kind_clusters.values().flatten().copied().collect();
                let listed =
                    clusters.iter().enumerate().filter(|(ix, _)| reached.contains(ix)).map(|(_, (name, c))| {
                        // Accessible namespaces set for this cluster replace the listing.
                        let accessible = crate::settings::cluster(name).namespaces;
                        let c = c.clone();
                        async move {
                            if accessible.is_empty() {
                                k8s::namespaces(c).await.unwrap_or_default()
                            } else {
                                accessible
                            }
                        }
                    });
                let mut namespaces: Vec<String> =
                    futures::future::join_all(listed).await.into_iter().flatten().collect();
                namespaces.sort();
                namespaces.dedup();
                anyhow::Ok((client, kinds, kind_clusters, namespaces))
            }
            .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.context.as_deref() != Some(primary.as_str()) {
                    return; // the user switched again meanwhile
                }
                this.connecting = false;
                match result {
                    Ok((client, kinds, kind_clusters, namespaces)) => {
                        this.kind_clusters = kind_clusters;
                        this.connect_errors = (!errors.is_empty()).then(|| errors.join("; ").into());
                        this.client = Some(client);
                        this.clusters = clusters;
                        let keep = this.kind.as_ref().map(|k| k.key());
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
                            if i == active {
                                continue;
                            }
                            tab.kind = tab
                                .kind
                                .take()
                                .and_then(|k| kinds.iter().find(|x| x.key() == k.key()).cloned())
                                .or_else(|| fallback.clone());
                            tab.namespaces.retain(|ns| namespaces.contains(ns));
                        }
                        this.settings_view.update(cx, |s, _| s.set_kinds(kinds.clone()));
                        this.kinds = kinds;
                        this.start_problem_watch(cx);
                        this.set_namespaces(namespaces, window, cx);
                        this.restart_watch(cx);
                        if dev_var("RECOUVRANCE_PAGE").is_none() {
                            match crate::settings::get().initial_view {
                                crate::settings::InitialView::Workloads => this.open_workloads(window, cx),
                                crate::settings::InitialView::Cluster => this.open_cluster(window, cx),
                                crate::settings::InitialView::Pods => {}
                            }
                        }
                        match dev_var("RECOUVRANCE_PAGE").as_deref() {
                            Some("workloads") => this.open_workloads(window, cx),
                            Some("helm") => this.open_helm(window, cx),
                            Some("agents") => this.navigate(Route::Agents, cx),
                            Some("cluster") => this.open_cluster(window, cx),
                            Some("settings") => this.navigate(Route::Settings, cx),
                            Some("terminal") => this.open_terminal(window, cx),
                            Some("apply") => this.open_apply(window, cx),
                            Some("problems") => this.open_problems(window, cx),
                            Some("access") => this.open_access(window, cx),
                            Some("helm-install") => this.open_helm_install(window, cx),
                            Some("assistant") => this.open_assistant(window, cx),
                            _ => {}
                        }
                    }
                    Err(e) => this.status = Some(format!("{e:#}").into()),
                }
                cx.notify();
            });
        }));
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
        self.all_contexts = names.clone();
        self.ctx_picker.update(cx, |p, cx| p.set_all(names, cx));
        self.settings_view.update(cx, |s, cx| s.sync_clusters(window, cx));
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
                errors.push(format!("{name}: {e:#}"));
                continue;
            }
        };
        for k in found {
            let entry = kind_clusters.entry(k.key()).or_default();
            if entry.is_empty() {
                kinds.push(k);
            }
            entry.push(ix);
        }
    }
    (kinds, kind_clusters)
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
    use super::{Kind, Route, all_restricted, cluster_namespace, merge_kinds, one_namespace, served_lists};

    fn kind(k: &str) -> Kind {
        Kind {
            ar: kube::discovery::ApiResource {
                group: "".into(),
                version: "v1".into(),
                api_version: "v1".into(),
                kind: k.into(),
                plural: format!("{}s", k.to_lowercase()),
            },
            namespaced: true,
            section: "",
            label: k.into(),
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
        list.iter().map(|s| s.to_string()).collect()
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
}
