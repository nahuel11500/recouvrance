//! Hooks for the off-screen snapshot harness (feature `snapshot`).

#[allow(unused_imports)]
use super::*;

impl AppView {
    /// QA crawler: every resource type, by kubectl name.
    pub fn qa_kinds(&self) -> Vec<String> {
        self.kinds.iter().map(|k| k.kubectl_name()).collect()
    }

    pub fn qa_select(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some(k) = k8s::find_kind(&self.kinds, name).cloned() {
            self.select_kind(k, cx);
        }
    }

    /// (rows, still listing, status text) of the resource list.
    pub fn qa_list(&self, cx: &App) -> (usize, bool, Option<String>) {
        let rows = self.table.read(cx).delegate().total();
        (rows, !self.listing.is_empty(), self.status.as_ref().map(|s| s.to_string()))
    }

    pub fn qa_back(&mut self, cx: &mut Context<Self>) {
        self.go(-1, cx);
    }

    pub fn qa_detail(&self) -> Option<Entity<DetailPanel>> {
        self.detail().cloned()
    }

    pub fn qa_page(&mut self, page: &str, window: &mut Window, cx: &mut Context<Self>) {
        match page {
            "workloads" => self.open_workloads(window, cx),
            "cluster" => self.open_cluster(window, cx),
            "problems" => self.open_problems(window, cx),
            "access" => self.open_access(window, cx),
            "apply" => self.open_apply(window, cx),
            "helm" => self.open_helm(window, cx),
            "assistant" => self.open_assistant(window, cx),
            "settings" => self.navigate(Route::Settings, cx),
            "agents" => self.navigate(Route::Agents, cx),
            "contexts" => self.navigate(Route::Contexts, cx),
            _ => {}
        }
    }

    #[cfg(feature = "dev")]
    pub fn demo_type(&mut self, text: &str, cx: &mut Context<Self>) {
        if let Route::Terminal(t) = self.route() {
            t.read(cx).type_text(text);
        }
    }

    #[cfg(feature = "dev")]
    pub fn demo_compare(&mut self, ctx: String, cx: &mut Context<Self>) {
        if let Some(d) = self.detail().cloned() {
            d.update(cx, |d, cx| {
                d.select_tab_named("compare", cx);
                d.compare_with_cluster(ctx, cx);
            });
        }
    }

    #[cfg(feature = "dev")]
    pub fn demo_run_cron(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = self.detail().cloned() {
            d.update(cx, |d, cx| d.demo_run_cron(window, cx));
        }
    }

    #[cfg(feature = "dev")]
    pub fn demo_filter(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.filter.update(cx, |s, cx| s.set_value(text.to_string(), window, cx));
        self.table.update(cx, |t, cx| {
            t.delegate_mut().set_filter(text.to_string());
            cx.notify();
        });
    }

    /// Check the first `n` rows, then optionally open a bulk confirmation (never confirmed).
    #[cfg(feature = "dev")]
    pub fn demo_mark(&mut self, n: usize, bulk: Option<&'static str>, window: &mut Window, cx: &mut Context<Self>) {
        self.table.update(cx, |t, cx| {
            let d = t.delegate_mut();
            let uids: Vec<String> = (0..n).filter_map(|i| d.row(i).map(|r| r.uid.clone())).collect();
            d.marked.extend(uids);
            cx.notify();
        });
        if let Some(a) = bulk {
            self.bulk_action(a, window, cx);
        }
    }

    #[cfg(feature = "dev")]
    pub fn demo_view(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(v) = self.favorites.read(cx).views().iter().find(|v| v.name == name).cloned() {
            self.apply_view(&v, window, cx);
        }
    }

    #[cfg(feature = "dev")]
    pub fn demo_popover(&mut self, which: &str, cx: &mut Context<Self>) {
        match which {
            "ctx" => self.ctx_open = true,
            _ => self.ns_open = true,
        }
        cx.notify();
    }

    #[cfg(feature = "dev")]
    pub fn demo_restore_forwards(&mut self, cx: &mut Context<Self>) {
        self.restore_forwards(cx);
    }

    #[cfg(feature = "dev")]
    pub fn demo_helm(&mut self, query: &str, dry_run: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Route::HelmInstall(p) = self.route().clone() {
            p.update(cx, |p, cx| p.demo(query, dry_run, window, cx));
        }
    }

    #[cfg(feature = "dev")]
    pub fn demo_plugin(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = self.detail().cloned() {
            d.update(cx, |d, cx| d.demo_plugin(ix, window, cx));
        }
    }

    #[cfg(feature = "dev")]
    pub fn demo_ask(&mut self, q: &str, cx: &mut Context<Self>) {
        if let Route::Assistant(p) = self.route().clone() {
            p.update(cx, |p, cx| p.send(q.to_string(), cx));
        }
    }

    #[cfg(feature = "dev")]
    pub fn demo_rules_code(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.agents.update(cx, |a, cx| a.demo_code(window, cx));
    }

    /// Named app actions for off-screen checks.
    #[cfg(feature = "dev")]
    pub fn demo_act(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        match name {
            "csv" => self.export_csv(window, cx),
            "refresh" => self.refresh_page(window, cx),
            "sidebar" => {
                self.sidebar_hidden = !self.sidebar_hidden;
                cx.notify();
            }
            "yaml" => {
                if let Some(d) = self.detail().cloned() {
                    d.update(cx, |d, cx| d.download_yaml(window, cx));
                }
            }
            "filter-menu" => {
                crate::settings::remember_filter("status:Running restarts>0");
                crate::settings::update(|s| s.saved_searches = vec!["cpu>100m".into()]);
            }
            other if other.starts_with("filter:") => self.apply_filter_text(other[7..].to_string(), window, cx),
            other if other.starts_with("add-ns:") => {
                let ns = other[7..].to_string();
                self.ns_picker.update(cx, |p, cx| p.add_namespace(ns, cx));
            }
            "ns-open" => self.demo_popover("ns", cx),
            other if other.starts_with("ns-search:") => {
                let text = other["ns-search:".len()..].to_string();
                self.ns_picker.update(cx, |p, cx| p.demo_search(&text, window, cx));
            }
            "newtab" => self.new_tab(None, window, cx),
            other if other.starts_with("page:") => self.qa_page(&other[5..], window, cx),
            other if other.starts_with("kubeconfig-paste:") => {
                let text = std::fs::read_to_string(&other["kubeconfig-paste:".len()..]).unwrap_or_default();
                self.settings_view.update(cx, |s, cx| s.demo_paste_kubeconfig(&text, window, cx));
            }
            other if other.starts_with("tab:") => self.switch_tab(other[4..].parse().unwrap_or(0), window, cx),
            other if other.starts_with("closetab:") => self.close_tab(other[9..].parse().unwrap_or(0), window, cx),
            other if other.starts_with("opentab:") => {
                self.open_detail(other[8..].parse().unwrap_or(0), window, cx);
                let detail = self.route().clone();
                self.go(-1, cx);
                self.history.truncate(self.pos + 1);
                self.new_tab(Some(detail), window, cx);
            }
            other if other.starts_with("kind:") => {
                if let Some(k) = k8s::find_kind(&self.kinds, &other[5..]).cloned() {
                    self.select_kind(k, cx);
                }
            }
            "who" => {
                if let Route::Access(page) = self.route().clone() {
                    page.update(cx, |p, cx| p.ask_who(cx));
                }
            }
            other if other.starts_with("apply:") => {
                let Route::Apply(page) = self.route().clone() else { return };
                let arg = &other[6..];
                page.update(cx, |p, cx| match arg.split_once('=') {
                    Some(("tpl", name)) => p.use_template(name, window, cx),
                    Some(("text", t)) => p.set_text(t.replace("\\n", "\n"), window, cx),
                    Some(("file", path)) => p.load_paths(&[std::path::PathBuf::from(path)], window, cx),
                    Some(("form", kind)) => p.show_form(kind, window, cx),
                    Some(("fill", spec)) => {
                        if let Some(f) = p.form() {
                            f.update(cx, |f, cx| f.demo_fill(spec, window, cx));
                        }
                    }
                    Some(("save", _)) => {
                        if let Some(f) = p.form() {
                            f.update(cx, |f, cx| f.save(window, cx));
                        }
                    }
                    Some(("run", dry)) => p.run(dry == "dry", window, cx),
                    Some(("url", u)) => {
                        p.set_url(u, window, cx);
                        p.load_url(window, cx);
                    }
                    Some(("editor", _)) => p.open_external(window, cx),
                    Some(("sform", line)) => {
                        p.cursor_to(line.parse().unwrap_or(0), window, cx);
                        p.toggle_schema_form(window, cx);
                    }
                    Some(("sopen", path)) => {
                        if let Some(f) = p.schema_form() {
                            f.update(cx, |f, cx| {
                                f.open(
                                    path.split('.').filter(|s| !s.is_empty()).map(String::from).collect(),
                                    window,
                                    cx,
                                )
                            });
                        }
                    }
                    Some(("sset", kv)) => {
                        if let (Some(f), Some((k, v))) = (p.schema_form(), kv.split_once(':')) {
                            f.update(cx, |f, cx| f.demo_set(k, v, window, cx));
                        }
                    }
                    Some(("swrite", _)) => {
                        if let Some(f) = p.schema_form() {
                            f.update(cx, |f, cx| f.write(window, cx));
                        }
                    }
                    Some(("docs", line)) => {
                        p.toggle_docs(cx);
                        p.cursor_to(line.parse().unwrap_or(0), window, cx);
                    }
                    _ => {}
                });
            }
            other => {
                if let (Some(rest), Some(d)) = (other.strip_prefix("detail:"), self.detail().cloned()) {
                    d.update(cx, |d, cx| d.demo(rest, window, cx));
                }
            }
        }
    }

    #[cfg(feature = "dev")]
    pub fn demo_action(&mut self, action: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(d) = self.detail().cloned() {
            d.update(cx, |d, cx| d.quick_action(action, window, cx));
        }
    }

    #[cfg(feature = "dev")]
    pub fn demo_palette(&mut self, query: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.open_palette(window, cx);
        if let Some(p) = self.palette.clone() {
            p.update(cx, |p, cx| p.set_query(query, window, cx));
        }
    }

    #[cfg(feature = "dev")]
    pub fn select_tab(&mut self, name: &str, cx: &mut Context<Self>) {
        if let Some(d) = self.detail().cloned() {
            d.update(cx, |d, cx| d.select_tab_named(name, cx));
        }
    }

    #[cfg(feature = "dev")]
    pub fn demo_terminal(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if let Some(d) = self.detail().cloned() {
            d.update(cx, |d, cx| d.demo_terminal(bytes, cx));
        }
    }
}
