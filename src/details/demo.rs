//! Hooks for the off-screen snapshot harness (feature `snapshot`).

use gpui_kit::*;

use super::{DetailPanel, DetailTab};
use crate::k8s;

impl DetailPanel {
    /// QA crawler: errors shown on this page, and the tabs it offers.
    pub fn qa_state(&self) -> (Option<String>, Option<String>, bool) {
        let events = match &self.events {
            Some(Err(e)) => Some(e.to_string()),
            _ => None,
        };
        (self.yaml_error.as_ref().map(ToString::to_string), events, self.object.is_some())
    }

    pub fn qa_tabs(&self) -> Vec<&'static str> {
        let mut t = vec!["overview"];
        if self.obj.kind.is("", "Pod") {
            t.push("container");
        }
        if self.obj.kind.ar.group == "apps" || self.obj.kind.is("batch", "Job") || self.obj.kind.is("", "Pod") {
            t.push("map");
        }
        if self.obj.kind.is("apps", "Deployment")
            || self.obj.kind.is("apps", "StatefulSet")
            || self.obj.kind.is("apps", "DaemonSet")
        {
            t.push("revisions");
        }
        if self.logs.is_some() {
            t.push("logs");
        }
        t.extend(["yaml", "compare", "history", "events"]);
        t
    }

    #[cfg(feature = "dev")]
    pub fn demo_terminal(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if let Some(t) = self.terminal.clone() {
            self.tab = DetailTab::Shell;
            t.update(cx, |t, cx| t.demo(bytes, cx));
        }
    }

    #[cfg(feature = "dev")]
    /// Snapshot hooks: `rollback` opens the confirmation for the oldest revision,
    /// `rollback!` runs it.
    pub fn demo(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let oldest = || {
            let rev = |r: &serde_json::Value| {
                r.pointer("/metadata/annotations/deployment.kubernetes.io~1revision")
                    .and_then(|v| v.as_str())
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(0)
            };
            self.related.revisions.iter().min_by_key(|r| rev(r)).cloned()
        };
        match name {
            "rollback" => {
                if let Some(name) =
                    oldest().and_then(|r| r.pointer("/metadata/name").and_then(|v| v.as_str()).map(String::from))
                {
                    self.confirm_rollback(name, window, cx);
                }
            }
            "rollback!" => {
                if let Some(t) = oldest().and_then(|r| r.pointer("/spec/template").cloned()) {
                    let fut = k8s::rollback(
                        self.client.clone(),
                        self.obj.kind.clone(),
                        self.obj.namespace.clone(),
                        self.obj.name.clone(),
                        t,
                        false,
                    );
                    self.run_write("Roll back", fut, window, cx);
                }
            }
            other if other.starts_with("reveal-key:") => self.toggle_secret_key(&other[11..], cx),
            "edit-data" => self.toggle_data_editor(window, cx),
            "sa-kubeconfig" => self.save_sa_kubeconfig(1, window, cx),
            "env" => self.toggle_env(cx),
            other if other.starts_with("logfilter:") => {
                if let Some(l) = self.logs.clone() {
                    let q = other[10..].to_string();
                    l.update(cx, |l, cx| {
                        l.search.update(cx, |s, cx| s.set_value(q, window, cx));
                        l.recompute_matches(cx);
                    });
                }
            }
            other if other.starts_with("download:") => {
                let fut = k8s::download_from_container(
                    self.client.clone(),
                    self.obj.namespace.clone().unwrap_or_default(),
                    self.obj.name.clone(),
                    None,
                    other[9..].to_string(),
                );
                self.run_write(
                    "Download",
                    async move { fut.await.map(|p| format!("Saved {}", p.display())) },
                    window,
                    cx,
                );
            }
            other if other.starts_with("upload:") => {
                let (local, dir) = other[7..].split_once('|').unwrap_or((&other[7..], "/tmp"));
                let fut = k8s::upload_to_container(
                    self.client.clone(),
                    self.obj.namespace.clone().unwrap_or_default(),
                    self.obj.name.clone(),
                    None,
                    local.into(),
                    dir.to_string(),
                );
                self.run_write("Upload", async move { fut.await.map(|d| format!("Uploaded to {d}")) }, window, cx);
            }
            other if other.starts_with("pf-opts:") => {
                let saved = crate::settings::SavedForward {
                    context: self.context.clone(),
                    namespace: self.obj.namespace.clone().unwrap_or_default(),
                    service: None,
                    pod: self.obj.name.clone(),
                    remote: other[8..].parse().unwrap_or(80),
                    local: None,
                    address: None,
                    https: false,
                    open_browser: false,
                };
                self.pf_options_dialog(saved, window, cx);
            }
            "attach" => {
                if let Some(t) = self.terminal.clone() {
                    self.tab = DetailTab::Shell;
                    t.update(cx, |t, cx| t.start_attach(window, cx));
                }
            }
            other if other.starts_with("bind:") => self.bind_dialog(None, Some(other[5..].to_string()), window, cx),
            other if other.starts_with("add-subject:") => {
                let b = self.object.clone();
                self.bind_dialog(b, Some(other[12..].to_string()), window, cx);
            }
            other if other.starts_with("remove-subject:") => {
                self.confirm_remove_subject(other[15..].parse().unwrap_or(0), window, cx);
            }
            "managed" => {
                self.show_managed = true;
                self.refresh_yaml(window, cx);
            }
            other if other.starts_with("docs:") => {
                self.toggle_field_docs(true, cx);
                self.tab = DetailTab::Yaml;
                let line: u32 = other[5..].parse().unwrap_or(0);
                self.yaml
                    .update(cx, |e, cx| e.set_cursor_position(component::input::Position::new(line, 4), window, cx));
            }
            other if other.starts_with("fill:") => {
                if let Some(e) = self.data_editor.clone() {
                    e.update(cx, |e, cx| e.demo_fill(&other[5..], window, cx));
                }
            }
            "save-data" => {
                if let Some(e) = self.data_editor.clone() {
                    e.update(cx, |e, cx| e.save(window, cx));
                }
            }
            other => {
                if let (Some(q), Some(logs)) = (other.strip_prefix("find:"), self.logs.clone()) {
                    let (regex, q) = match q.strip_prefix("re:") {
                        Some(q) => (true, q),
                        None => (false, q),
                    };
                    logs.update(cx, |l, cx| {
                        l.find_regex = regex;
                        l.find.update(cx, |s, cx| s.set_value(q.to_string(), window, cx));
                        l.compile_find(cx);
                        l.step_find(1, cx);
                    });
                }
            }
        }
    }

    #[cfg(feature = "dev")]
    pub fn demo_run_cron(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = format!("Run CronJob {} now?", self.obj.name);
        self.confirm_action(
            title,
            "Creates a Job from its template, like `kubectl create job --from=cronjob/…`.",
            "Run",
            |p, client| {
                Box::pin(k8s::trigger_cronjob(client, p.obj.namespace.clone().unwrap_or_default(), p.obj.name.clone()))
            },
            window,
            cx,
        );
    }

    #[cfg(feature = "dev")]
    pub fn demo_plugin(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let k = self.obj.kind.clone();
        if let Some(p) = crate::plugins::for_kind(&k.kubectl_name(), &k.ar.plural, cx).get(ix).cloned() {
            self.run_plugin(p, window, cx);
        }
    }
}
