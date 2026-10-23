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
        self.client = None;
        self.clusters.clear();
        self.kinds.clear();
        self.watch = None;
        self.metrics = None;
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
                // Union of every cluster's kinds; remember where each one exists.
                let discovered =
                    futures::future::join_all(clusters.iter().map(|(_, c)| k8s::discover(c.clone()))).await;
                let mut kinds: Vec<Kind> = Vec::new();
                let mut kind_clusters: HashMap<String, Vec<usize>> = HashMap::new();
                for (ix, found) in discovered.into_iter().enumerate() {
                    let found = found.unwrap_or_else(|e| {
                        errors.push(format!("{}: {e:#}", clusters[ix].0));
                        Vec::new()
                    });
                    for k in found {
                        let entry = kind_clusters.entry(k.key()).or_default();
                        if entry.is_empty() {
                            kinds.push(k);
                        }
                        entry.push(ix);
                    }
                }
                if kinds.is_empty() {
                    anyhow::bail!("discovery failed on every cluster: {}", errors.join("; "));
                }
                // Built-in sections first, then custom resources by group.
                k8s::sort_for_sidebar(&mut kinds);
                // Namespace listing may be forbidden; that is not fatal.
                let mut namespaces = Vec::new();
                for (name, c) in &clusters {
                    // Accessible namespaces set for this cluster replace the listing.
                    let accessible = crate::settings::cluster(name).namespaces;
                    if accessible.is_empty() {
                        namespaces.extend(k8s::namespaces(c.clone()).await.unwrap_or_default());
                    } else {
                        namespaces.extend(accessible);
                    }
                }
                namespaces.sort();
                namespaces.dedup();
                anyhow::Ok((client, kinds, kind_clusters, namespaces))
            }
            .await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.context.as_deref() != Some(primary.as_str()) {
                    return; // the user switched again meanwhile
                }
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
                seen = now;
                let Ok(contexts) = k8s::load_contexts() else { continue };
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
        self.all_contexts = names.clone();
        self.ctx_picker.update(cx, |p, cx| {
            p.all = names;
            cx.notify();
        });
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

    /// Namespaces configured as accessible for the connected clusters (empty: all).
    pub(super) fn accessible_namespaces(&self) -> Vec<String> {
        self.clusters.iter().flat_map(|(c, _)| crate::settings::cluster(c).namespaces).collect()
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
    use super::one_namespace;

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
}
