//! Settings page: startup, appearance, refresh rates, logs, kubeconfig,
//! agents and shortcuts.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, ThemeMode, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::settings::{self, ConfirmStyle, DeleteConfirm, InitialView, Startup};
use crate::ui::a11y::Control as _;

/// A section of the page, picked on its left.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    General,
    Contexts,
    Clusters,
    ArgoCd,
    Appearance,
    Refresh,
    Logs,
    Timeline,
    Kubeconfig,
    Agents,
    Assistant,
    Plugins,
    Shortcuts,
    Requests,
    About,
}

const SECTIONS: [(Section, &str, Lucide); 15] = [
    (Section::General, "General", Lucide::Settings),
    (Section::Contexts, "Contexts", Lucide::Ship),
    (Section::Clusters, "Per cluster", Lucide::Server),
    (Section::ArgoCd, "Argo CD", Lucide::GitBranch),
    (Section::Appearance, "Appearance", Lucide::Palette),
    (Section::Refresh, "Refresh rates", Lucide::RefreshCw),
    (Section::Logs, "Log viewer", Lucide::ScrollText),
    (Section::Timeline, "Timeline", Lucide::ClockArrowDown),
    (Section::Kubeconfig, "Kubeconfig", Lucide::FileText),
    (Section::Agents, "Agents (MCP)", Lucide::Bot),
    (Section::Assistant, "Assistant", Lucide::Sparkles),
    (Section::Plugins, "Plugins", Lucide::Puzzle),
    (Section::Shortcuts, "Shortcuts", Lucide::Keyboard),
    (Section::Requests, "API requests", Lucide::Activity),
    (Section::About, "About", Lucide::Info),
];

pub struct SettingsView {
    section: Section,
    metrics: Entity<InputState>,
    detail: Entity<InputState>,
    overview: Entity<InputState>,
    tail: Entity<InputState>,
    /// The timeline: kinds watched whole, MB and days kept per context.
    timeline_kinds: Entity<InputState>,
    timeline_mb: Entity<InputState>,
    timeline_days: Entity<InputState>,
    kubeconfig: Entity<crate::ui::kubeconfig_sources::KubeconfigSources>,
    mcp_port: Entity<InputState>,
    /// Why the port typed is not saved.
    mcp_port_error: Option<SharedString>,
    /// The assistant's team notes, and its code folders (one a line).
    notes: Entity<TextareaState>,
    repos: Entity<TextareaState>,
    /// Prometheus override per context (`namespace/service:port`).
    prometheus: Vec<(String, Entity<InputState>)>,
    /// Per context: node shell image and accessible namespaces.
    cluster_inputs: Vec<(String, Entity<InputState>, Entity<InputState>)>,
    /// Per context: the user and groups writes may run as (`kubectl --as`).
    run_as_inputs: Vec<(String, Entity<InputState>, Entity<InputState>)>,
    /// Per context: its Argo CD API server, and a token to log in with (never shown back).
    argo_inputs: Vec<(String, Entity<InputState>, Entity<InputState>)>,
    /// The contexts with an Argo CD token saved, read when the rows are made.
    argo_tokens: std::collections::BTreeSet<String>,
    /// Why the token typed for a context could not be saved.
    argo_token_error: Option<(String, SharedString)>,
    /// Settings → Argo CD: the API servers, as last read (the argocd CLI's and those set here).
    argo_api: crate::k8s::argo::api::Endpoints,
    /// Each server's connection test, by address: running (None), or what it found.
    argo_api_tests: std::collections::HashMap<String, Option<Result<String, String>>>,
    _argo_api_test_tasks: std::collections::HashMap<String, Task<()>>,
    /// Reads the servers off the UI thread.
    argo_api_read: crate::latest::Latest,
    /// A server to add: its address, and a token (never shown back).
    argo_api_url: Entity<InputState>,
    argo_api_token: Entity<InputState>,
    /// Why the server typed could not be added.
    argo_api_error: Option<SharedString>,
    /// The label naming an Application's service on the Argo CD page.
    argo_group_label: Entity<InputState>,
    /// A kind to give its own delete confirmation (Enter adds it).
    delete_kind: Entity<InputState>,
    /// The connected clusters' kinds, to check a kind typed in `delete_kind`.
    kinds: Vec<crate::k8s::Kind>,
    hub: std::sync::Arc<crate::mcp::Hub>,
    _subs: Vec<Subscription>,
    /// Per field being typed in, its save once typing pauses (see `save_typed`).
    _saves: std::collections::HashMap<EntityId, Task<()>>,
    /// Redraws API requests every 2 s while it is shown.
    _ticker: Task<()>,
    /// What About's Check for updates found, and its request (sent only on a click).
    update_check: Option<UpdateCheck>,
    update_task: crate::latest::Latest,
}

/// The Argo CD token field, empty: one is saved, or the argocd CLI's login is used.
const TOKEN_SAVED: &str = "token saved (type to replace)";
const TOKEN_NONE: &str = "token: none, the argocd CLI's login";

/// What Check for updates found.
enum UpdateCheck {
    Checking,
    /// The version of the latest release.
    Found(String),
    Failed(String),
}

impl SettingsView {
    pub fn new(hub: std::sync::Arc<crate::mcp::Hub>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let s = settings::get();
        let field = Self::field;
        let metrics = field(s.metrics_refresh.to_string(), "15", window, cx);
        let detail = field(s.detail_refresh.to_string(), "5", window, cx);
        let overview = field(s.overview_refresh.to_string(), "30", window, cx);
        let tail = field(s.log_tail_lines.to_string(), "1000", window, cx);
        let timeline_kinds = field(s.timeline.kinds.join(", "), "configmaps, secrets, deployments.apps", window, cx);
        let timeline_mb = field(s.timeline.max_mb().to_string(), "16", window, cx);
        let timeline_days = field(s.timeline.days().to_string(), "14", window, cx);
        let mcp_port = field(hub.port().to_string(), "47070", window, cx);
        let kubeconfig = cx.new(|cx| crate::ui::kubeconfig_sources::KubeconfigSources::new(window, cx));
        let textarea = |value: String, placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut st = TextareaState::new(window, cx).auto_grow(4, 16).placeholder(placeholder);
                st.set_value(value, window, cx);
                st
            })
        };
        let notes = textarea(
            s.assistant_notes.clone(),
            "e.g. Grafana: https://grafana.example.com · shop-* namespaces: github.com/acme/shop · on-call runbook: …",
            window,
            cx,
        );
        let repos = textarea(s.assistant_repos.join("\n"), "~/code/shop", window, cx);
        let argo_api_url = field(String::new(), "https://argocd.example.com", window, cx);
        let argo_api_token =
            cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("token (argocd account generate-token)"));
        let argo_group_label = field(s.argocd_group_label, "none: the name without its environment", window, cx);

        let mut subs = Vec::new();
        let number = |input: &Entity<InputState>, apply: fn(&mut settings::Settings, u64), cx: &mut Context<Self>| {
            Self::save_typed(input, input_text, cx, move |v| {
                if let Ok(n) = v.trim().parse::<u64>()
                    && n > 0
                {
                    settings::update(|s| apply(s, n));
                }
            })
        };
        subs.push(number(&metrics, |s, n| s.metrics_refresh = n, cx));
        subs.push(number(&detail, |s, n| s.detail_refresh = n, cx));
        subs.push(number(&overview, |s, n| s.overview_refresh = n, cx));
        subs.push(number(&tail, |s, n| s.log_tail_lines = n as i64, cx));
        subs.push(number(&timeline_mb, |s, n| s.timeline.max_mb = n, cx));
        subs.push(number(&timeline_days, |s, n| s.timeline.days = n, cx));
        subs.push(Self::save_typed(&timeline_kinds, input_text, cx, |v| {
            settings::update(|s| s.timeline.kinds = kind_list(&v));
        }));
        let hub2 = hub.clone();
        subs.push(cx.subscribe(&mcp_port, move |this, state, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                this.mcp_port_error = port_error(&state.read(cx).value(), |p| hub2.set_port(p));
                cx.notify();
            }
        }));
        subs.push(Self::save_typed(&notes, textarea_text, cx, |v| settings::update(|s| s.assistant_notes = v)));
        subs.push(Self::save_typed(&argo_group_label, input_text, cx, |v| {
            settings::update(|s| s.argocd_group_label = v.trim().to_string());
        }));
        subs.push(cx.subscribe_in(&argo_api_token, window, |this, _, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.add_argo_api(window, cx);
            }
        }));
        subs.push(Self::save_typed(&repos, textarea_text, cx, |v| {
            settings::update(|s| s.assistant_repos = repo_lines(&v));
        }));
        let delete_kind = cx.new(|cx| InputState::new(window, cx).placeholder("Add a kind, e.g. Service"));
        subs.push(cx.subscribe_in(&delete_kind, window, |this, state, ev: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                let typed = state.read(cx).value().trim().to_string();
                if typed.is_empty() {
                    return;
                }
                // Saved as deletes look it up: `deploy` or `deployments` is `Deployment`, and a
                // custom resource has its group.
                let Some(kind) = crate::k8s::find_kind(&this.kinds, &typed)
                    .map(|k| settings::delete_kind_key(&k.ar.group, &k.ar.kind))
                else {
                    use gpui_kit::component::{WindowExt as _, notification::Notification};
                    let msg = format!("No kind \"{typed}\" on the connected clusters");
                    window.push_notification(Notification::error(msg), cx);
                    return;
                };
                settings::update(|s| {
                    s.delete_confirm_kinds.entry(kind).or_insert(DeleteConfirm::Type);
                });
                state.update(cx, |s, cx| s.set_value("", window, cx));
                cx.notify();
            }
        }));
        let section = match crate::app::dev_var("RECOUVRANCE_SECTION").as_deref() {
            Some("contexts") => Section::Contexts,
            Some("clusters") => Section::Clusters,
            Some("argocd") => Section::ArgoCd,
            Some("kubeconfig") => Section::Kubeconfig,
            Some("plugins") => Section::Plugins,
            Some("appearance") => Section::Appearance,
            Some("general") => Section::General,
            Some("assistant") => Section::Assistant,
            Some("about") => Section::About,
            _ => Section::General,
        };
        let mut this = Self {
            section,
            metrics,
            detail,
            overview,
            tail,
            timeline_kinds,
            timeline_mb,
            timeline_days,
            kubeconfig,
            mcp_port,
            mcp_port_error: None,
            notes,
            repos,
            prometheus: Vec::new(),
            cluster_inputs: Vec::new(),
            run_as_inputs: Vec::new(),
            argo_inputs: Vec::new(),
            argo_tokens: Default::default(),
            argo_token_error: None,
            argo_api: Default::default(),
            argo_api_tests: Default::default(),
            _argo_api_test_tasks: Default::default(),
            argo_api_read: Default::default(),
            argo_api_url,
            argo_api_token,
            argo_api_error: None,
            argo_group_label,
            delete_kind,
            kinds: Vec::new(),
            hub,
            _subs: subs,
            _saves: Default::default(),
            _ticker: cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(std::time::Duration::from_secs(2)).await;
                    let shown = this.update(cx, |this, cx| {
                        if this.section == Section::Requests {
                            cx.notify();
                        }
                    });
                    if shown.is_err() {
                        break;
                    }
                }
            }),
            update_check: None,
            update_task: Default::default(),
        };
        // Another window may have changed a setting meanwhile.
        this._subs.push(cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.reload_fields(window, cx);
            }
        }));
        this.sync_clusters(window, cx);
        if this.section == Section::ArgoCd {
            this.reload_argo_api(cx);
        }
        this
    }

    /// Read the Argo CD API servers again (the argocd CLI's config, the tokens file), off the
    /// UI thread; a read again replaces the one running.
    fn reload_argo_api(&mut self, cx: &mut Context<Self>) {
        let read = cx.background_executor().spawn(async { crate::k8s::argo::api::endpoints() });
        self.argo_api_read.spawn(cx, read, |this, found, cx| {
            this.argo_api = found;
            cx.notify();
        });
    }

    /// The servers changed: the Argo CD page reads them anew, and this list too.
    fn argo_api_changed(&mut self, cx: &mut Context<Self>) {
        crate::k8s::argo::api::reload();
        self.reload_argo_api(cx);
        cx.notify();
    }

    /// Add the server typed, with its token (or set the token of one listed): the address
    /// goes in settings.json, the token in a file only the user can read.
    fn add_argo_api(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::k8s::argo::{api, server::Server};
        let typed = self.argo_api_url.read(cx).value().trim().to_string();
        let token = self.argo_api_token.read(cx).value().trim().to_string();
        let server = match Server::parse(&typed) {
            Ok(s) => s,
            Err(e) => {
                self.argo_api_error = Some(e.into());
                cx.notify();
                return;
            }
        };
        if !token.is_empty()
            && let Err(e) = api::save_token(&server.base, &token)
        {
            self.argo_api_error = Some(format!("Could not save the token: {e}").into());
            cx.notify();
            return;
        }
        let base = server.base;
        settings::update(|s| {
            let listed = s.argocd_api_servers.iter().any(|t| Server::parse(t).is_ok_and(|x| x.base == base));
            if !listed {
                s.argocd_api_servers.push(base.clone());
            }
            s.argocd_api_off.retain(|b| *b != base);
        });
        self.argo_api_error = None;
        self.argo_api_url.update(cx, |s, cx| s.set_value("", window, cx));
        self.argo_api_token.update(cx, |s, cx| s.set_value("", window, cx));
        self.argo_api_changed(cx);
    }

    /// Remove a server set here, and forget its token. One the argocd CLI is logged in to
    /// stays (turned off, it is not read).
    fn remove_argo_api(&mut self, base: &str, cx: &mut Context<Self>) {
        use crate::k8s::argo::{api, server::Server};
        if let Err(e) = api::save_token(base, "") {
            self.argo_api_error = Some(format!("Could not forget the token: {e}").into());
        }
        settings::update(|s| s.argocd_api_servers.retain(|t| !Server::parse(t).is_ok_and(|x| x.base == base)));
        self.argo_api_changed(cx);
    }

    /// Read a server on the Argo CD page, or not.
    fn set_argo_api_on(&mut self, base: &str, on: bool, cx: &mut Context<Self>) {
        settings::update(|s| {
            s.argocd_api_off.retain(|b| b != base);
            if !on {
                s.argocd_api_off.push(base.to_string());
            }
        });
        self.argo_api_changed(cx);
    }

    /// Check a server's login: who it logs in as, and what it may read. Read-only.
    fn test_argo_api(&mut self, endpoint: crate::k8s::argo::api::Endpoint, cx: &mut Context<Self>) {
        let base = endpoint.server.base.clone();
        self.argo_api_tests.insert(base.clone(), None);
        let done = base.clone();
        let task = cx.spawn(async move |this, cx| {
            let found = crate::k8s::argo::api::test(endpoint).await;
            let _ = this.update(cx, |this, cx| {
                this.argo_api_tests.insert(done, Some(found));
                cx.notify();
            });
        });
        // A test again replaces (and stops) the one running.
        self._argo_api_test_tasks.insert(base, task);
        cx.notify();
    }

    /// Save what is typed in `input` once typing pauses, or when the field loses focus:
    /// not on every key, which rewrote settings.json each time and applied half-typed
    /// values (a refresh of 3 s on the way to 30, part of a path).
    fn save_typed<T: EventEmitter<InputEvent>>(
        input: &Entity<T>,
        text: fn(&T) -> String,
        cx: &mut Context<Self>,
        save: impl Fn(String) + 'static,
    ) -> Subscription {
        let save = std::rc::Rc::new(save);
        cx.subscribe(input, move |this, state, ev: &InputEvent, cx| {
            let id = state.entity_id();
            match ev {
                // Each key replaces, and so cancels, the save not run yet.
                InputEvent::Change => {
                    let (v, save) = (text(state.read(cx)), save.clone());
                    this._saves.insert(
                        id,
                        cx.spawn(async move |_, cx| {
                            cx.background_executor().timer(std::time::Duration::from_millis(600)).await;
                            save(v);
                        }),
                    );
                }
                // Leaving a field that changed saves it now.
                InputEvent::Blur if this._saves.remove(&id).is_some() => save(text(state.read(cx))),
                _ => {}
            }
        })
    }

    fn field(
        value: String,
        placeholder: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        cx.new(|cx| {
            let mut st = InputState::new(window, cx).placeholder(placeholder);
            st.set_value(value, window, cx);
            st
        })
    }

    /// Per cluster rows for the kubeconfig's current contexts (contexts come and go;
    /// checked when the section opens and when the kubeconfig reloads).
    pub fn sync_clusters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let names = crate::k8s::load_contexts().map_or_default(|c| c.names);
        self.cluster_inputs.retain(|(ctx, ..)| names.contains(ctx));
        self.prometheus.retain(|(ctx, _)| names.contains(ctx));
        self.run_as_inputs.retain(|(ctx, ..)| names.contains(ctx));
        self.argo_inputs.retain(|(ctx, ..)| names.contains(ctx));
        self.argo_tokens = crate::k8s::argo::server::token_contexts();
        for ctx in &names {
            if !self.cluster_inputs.iter().any(|(c, ..)| c == ctx) {
                self.add_cluster(ctx.clone(), window, cx);
            }
        }
        // New contexts take their place in the kubeconfig's order.
        self.cluster_inputs.sort_by_key(|(ctx, ..)| names.iter().position(|n| n == ctx));
        cx.notify();
    }

    fn add_cluster(&mut self, ctx: String, window: &mut Window, cx: &mut Context<Self>) {
        let field = Self::field;
        let s = settings::get();
        let input =
            field(s.prometheus.get(&ctx).cloned().unwrap_or_default(), "Prometheus: found automatically", window, cx);
        let c = ctx.clone();
        self._subs.push(Self::save_typed(&input, input_text, cx, move |v| {
            let v = v.trim().to_string();
            settings::update(|s| {
                if v.is_empty() {
                    s.prometheus.remove(&c);
                } else {
                    s.prometheus.insert(c.clone(), v);
                }
            });
        }));
        self.prometheus.push((ctx.clone(), input));
        let cs = settings::cluster(&ctx);
        let image = field(cs.node_shell_image.clone().unwrap_or_default(), "busybox:1.37", window, cx);
        let namespaces =
            field(cs.namespaces.join(", "), "all (or: team-a, team-b when you may not list namespaces)", window, cx);
        let c = ctx.clone();
        self._subs.push(Self::save_typed(&image, input_text, cx, move |v| {
            let v = v.trim().to_string();
            settings::update(|s| {
                s.clusters.entry(c.clone()).or_default().node_shell_image = (!v.is_empty()).then_some(v);
            });
        }));
        let c = ctx.clone();
        self._subs.push(Self::save_typed(&namespaces, input_text, cx, move |v| {
            let list: Vec<String> = v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect();
            settings::update(|s| s.clusters.entry(c.clone()).or_default().namespaces = list);
        }));
        self.cluster_inputs.push((ctx.clone(), image, namespaces));
        let saved = cs.run_as.unwrap_or_default();
        let user = field(saved.user.clone(), "none (a user, e.g. super-admin)", window, cx);
        let groups = field(saved.groups.join(", "), "groups (optional, comma separated)", window, cx);
        // What is typed in both, saved together: groups count only with a user.
        let typed = std::rc::Rc::new(std::cell::RefCell::new(saved));
        let (c, t) = (ctx.clone(), typed.clone());
        self._subs.push(Self::save_typed(&user, input_text, cx, move |v| {
            t.borrow_mut().user = v;
            settings::set_run_as(&c, Some(t.borrow().clone()));
        }));
        let c = ctx.clone();
        self._subs.push(Self::save_typed(&groups, input_text, cx, move |v| {
            typed.borrow_mut().groups =
                v.split(',').map(str::trim).filter(|g| !g.is_empty()).map(String::from).collect();
            settings::set_run_as(&c, Some(typed.borrow().clone()));
        }));
        self.run_as_inputs.push((ctx.clone(), user, groups));
        self.add_argo_server(ctx, window, cx);
    }

    /// The Argo CD server of `ctx` (saved as typed), and its token: saved on Enter or when
    /// the field is left, then cleared, so it is never shown again.
    fn add_argo_server(&mut self, ctx: String, window: &mut Window, cx: &mut Context<Self>) {
        let saved = settings::with(|s| s.argocd_servers.get(&ctx).cloned()).unwrap_or_default();
        let server = Self::field(saved, "Argo CD server: none, its diff is off", window, cx);
        let c = ctx.clone();
        self._subs.push(Self::save_typed(&server, input_text, cx, move |v| {
            let v = v.trim().to_string();
            settings::update(|s| {
                if v.is_empty() {
                    s.argocd_servers.remove(&c);
                } else {
                    s.argocd_servers.insert(c.clone(), v);
                }
            });
        }));
        let placeholder = if self.argo_tokens.contains(&ctx) { TOKEN_SAVED } else { TOKEN_NONE };
        let token = cx.new(|cx| InputState::new(window, cx).masked(true).placeholder(placeholder));
        let c = ctx.clone();
        self._subs.push(cx.subscribe_in(&token, window, move |this, state, ev: &InputEvent, window, cx| {
            if !matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                return;
            }
            let typed = state.read(cx).value().trim().to_string();
            if typed.is_empty() {
                return;
            }
            match crate::k8s::argo::server::save_token(&c, &typed) {
                Ok(()) => {
                    this.argo_tokens.insert(c.clone());
                    this.argo_token_error = None;
                    state.update(cx, |s, cx| {
                        s.set_value("", window, cx);
                        s.set_placeholder(TOKEN_SAVED, window, cx);
                    });
                }
                Err(e) => this.argo_token_error = Some((c.clone(), format!("Could not save the token: {e}").into())),
            }
            cx.notify();
        }));
        self.argo_inputs.push((ctx, server, token));
    }

    /// Forget the Argo CD token saved for `ctx`: the argocd CLI's login is used again.
    fn forget_argo_token(&mut self, ctx: &str, window: &mut Window, cx: &mut Context<Self>) {
        match crate::k8s::argo::server::save_token(ctx, "") {
            Ok(()) => {
                self.argo_tokens.remove(ctx);
                self.argo_token_error = None;
                if let Some((_, _, token)) = self.argo_inputs.iter().find(|(c, ..)| c == ctx) {
                    token.update(cx, |s, cx| s.set_placeholder(TOKEN_NONE, window, cx));
                }
            }
            Err(e) => {
                self.argo_token_error = Some((ctx.to_string(), format!("Could not forget the token: {e}").into()));
            }
        }
        cx.notify();
    }

    /// Show the saved values in the fields not being edited: each window has its own fields,
    /// and another one may have changed a setting since they were filled.
    fn reload_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let s = settings::get();
        let mut fields = vec![
            (self.metrics.clone(), s.metrics_refresh.to_string()),
            (self.detail.clone(), s.detail_refresh.to_string()),
            (self.overview.clone(), s.overview_refresh.to_string()),
            (self.tail.clone(), s.log_tail_lines.to_string()),
            (self.timeline_kinds.clone(), s.timeline.kinds.join(", ")),
            (self.timeline_mb.clone(), s.timeline.max_mb().to_string()),
            (self.timeline_days.clone(), s.timeline.days().to_string()),
            (self.mcp_port.clone(), self.hub.port().to_string()),
        ];
        for (ctx, input) in &self.prometheus {
            fields.push((input.clone(), s.prometheus.get(ctx).cloned().unwrap_or_default()));
        }
        for (ctx, image, namespaces) in &self.cluster_inputs {
            let cs = s.clusters.get(ctx).cloned().unwrap_or_default();
            fields.push((image.clone(), cs.node_shell_image.unwrap_or_default()));
            fields.push((namespaces.clone(), cs.namespaces.join(", ")));
        }
        for (ctx, server, _) in &self.argo_inputs {
            fields.push((server.clone(), s.argocd_servers.get(ctx).cloned().unwrap_or_default()));
        }
        fields.push((self.argo_group_label.clone(), s.argocd_group_label.clone()));
        for (ctx, user, groups) in &self.run_as_inputs {
            let saved = s.clusters.get(ctx).and_then(|c| c.run_as.clone()).unwrap_or_default();
            fields.push((user.clone(), saved.user));
            fields.push((groups.clone(), saved.groups.join(", ")));
        }
        for (input, saved) in fields {
            let state = input.read(cx);
            if !state.focus_handle(cx).is_focused(window) && state.value().trim() != saved {
                input.update(cx, |state, cx| state.set_value(saved, window, cx));
            }
        }
        for (input, saved) in
            [(self.notes.clone(), s.assistant_notes.clone()), (self.repos.clone(), s.assistant_repos.join("\n"))]
        {
            let state = input.read(cx);
            let current = state.value().to_string();
            let same = if input == self.repos { repo_lines(&current) == s.assistant_repos } else { current == saved };
            if !state.focus_handle(cx).is_focused(window) && !same {
                input.update(cx, |state, cx| state.set_value(saved, window, cx));
            }
        }
    }

    pub fn set_kinds(&mut self, kinds: Vec<crate::k8s::Kind>) {
        self.kinds = kinds;
    }

    /// Every section: its name and icon, in the page's order.
    pub fn sections() -> &'static [(Section, &'static str, Lucide)] {
        &SECTIONS
    }

    /// Show `section`, with its fields read again.
    pub fn show(&mut self, section: Section, window: &mut Window, cx: &mut Context<Self>) {
        self.section = section;
        if matches!(section, Section::Clusters | Section::Contexts) {
            self.sync_clusters(window, cx);
        }
        if section == Section::ArgoCd {
            self.reload_argo_api(cx);
        }
        self.reload_fields(window, cx);
        cx.notify();
    }

    /// Open on the General section (where deletes are configured).
    pub fn show_general(&mut self, cx: &mut Context<Self>) {
        self.section = Section::General;
        cx.notify();
    }

    /// Open on the About section.
    pub fn show_about(&mut self, cx: &mut Context<Self>) {
        self.section = Section::About;
        cx.notify();
    }

    /// Open on the Kubeconfig section.
    pub fn show_kubeconfig(&mut self, cx: &mut Context<Self>) {
        self.section = Section::Kubeconfig;
        cx.notify();
    }

    #[cfg(feature = "dev")]
    pub fn demo_paste_kubeconfig(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.section = Section::Kubeconfig;
        self.kubeconfig.update(cx, |k, cx| k.demo_paste(text, window, cx));
    }

    fn row(label: &str, hint: &str, control: impl IntoElement, cx: &App) -> Div {
        h_flex()
            .gap_6()
            .py_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(label.to_string()))
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(hint.to_string())),
            )
            .child(div().flex_shrink_0().child(control))
    }

    /// Kinds with their own delete confirmation, and a field to add one.
    fn render_delete_kinds(&self, s: &settings::Settings, cx: &mut Context<Self>) -> Div {
        let levels = [
            (DeleteConfirm::Off, "Delete right away"),
            (DeleteConfirm::Ask, "Ask"),
            (DeleteConfirm::Type, "Type the name"),
        ];
        v_flex()
            .gap_1()
            .py_2()
            .pl_4()
            .border_b_1()
            .border_color(cx.theme().border)
            .children(s.delete_confirm_kinds.iter().enumerate().map(|(i, (kind, level))| {
                let level = *level;
                h_flex()
                    .gap_3()
                    .child(div().w(px(180.)).text_sm().child(kind.clone()))
                    .child(h_flex().gap_1().children(levels.iter().enumerate().map(|(j, (v, label))| {
                        let (v, kind) = (*v, kind.clone());
                        Button::new(SharedString::from(format!("del-{i}-{j}")))
                            .small()
                            .label(*label)
                            .when(v == level, ButtonVariants::primary)
                            .when(v != level, Button::outline)
                            .on_click(move |_, _, cx| {
                                settings::update(|s| {
                                    s.delete_confirm_kinds.insert(kind.clone(), v);
                                });
                                cx.refresh_windows();
                            })
                    })))
                    .child({
                        let kind = kind.clone();
                        Button::new(SharedString::from(format!("del-rm-{i}")))
                            .small()
                            .ghost()
                            .icon(Lucide::X)
                            .accessibility_label("Use the default for this kind")
                            .tooltip("Use the default for this kind")
                            .on_click(move |_, _, cx| {
                                settings::update(|s| {
                                    s.delete_confirm_kinds.remove(&kind);
                                });
                                cx.refresh_windows();
                            })
                    })
            }))
            .child(
                h_flex().gap_3().child(div().w(px(180.)).child(Input::new(&self.delete_kind).small())).child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child("The kind as Kubernetes names it: Pod, Deployment, PersistentVolumeClaim…"),
                ),
            )
    }

    fn choice<T: Copy + PartialEq + 'static>(
        id: &'static str,
        current: T,
        options: &[(T, &'static str)],
        on: fn(T, &mut Window, &mut App),
    ) -> Div {
        h_flex().gap_1().children(options.iter().enumerate().map(|(i, (value, label))| {
            let v = *value;
            Button::new((id, i))
                .small()
                .label(*label)
                .selected(v == current)
                .when(v == current, ButtonVariants::primary)
                .when(v != current, Button::outline)
                .on_click(move |_, window, cx| on(v, window, cx))
        }))
    }

    fn render_section(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        match self.section {
            Section::General => self.general_section(cx),
            Section::Contexts => self.contexts_section(cx),
            Section::Clusters => self.clusters_section(cx),
            Section::ArgoCd => self.argo_section(cx),
            Section::Appearance => self.appearance_section(cx),
            Section::Refresh => self.refresh_section(cx),
            Section::Logs => self.logs_section(cx),
            Section::Timeline => self.timeline_section(cx),
            Section::Kubeconfig => self.kubeconfig_section(),
            Section::Agents => self.agents_section(cx),
            Section::Assistant => self.assistant_section(cx),
            Section::Plugins => self.plugins_section(cx),
            Section::Shortcuts => self.shortcuts_section(cx),
            Section::Requests => super::api_requests::render(cx),
            Section::About => self.about_section(window, cx),
        }
    }

    fn general_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = settings::get();
        v_flex()
        .child(Self::row(
            "Metrics",
            "Read CPU/memory from metrics-server and history from Prometheus. Turn off on clusters where that is slow or not allowed.",
            Switch::new("metrics-on").checked(s.metrics_enabled).label(if s.metrics_enabled { "On" } else { "Off" }).on_click(|v: &bool, _, cx| {
                let v = *v;
                settings::update(|s| s.metrics_enabled = v);
                cx.refresh_windows();
            }),
            cx,
        ))
        .child(Self::row(
            "Open the browser on port-forward",
            "After a one-click forward on a web port (HTTP, dashboards, UIs). Databases and gRPC never open it.",
            Switch::new("pf-browser")
                .checked(s.open_browser_on_forward)
                .label(if s.open_browser_on_forward { "On" } else { "Off" })
                .on_click(|v: &bool, _, cx| {
                    let v = *v;
                    settings::update(|s| s.open_browser_on_forward = v);
                    cx.refresh_windows();
                }),
            cx,
        ))
        .child(Self::row(
            "Startup",
            "What Recouvrance shows when it starts.",
            Self::choice("startup", s.startup, &[(Startup::Reconnect, "Reconnect previous clusters"), (Startup::Picker, "Always ask")], |v, _, cx| {
                settings::update(|s| s.startup = v);
                cx.refresh_windows();
            }),
            cx,
        ))
        .child(Self::row(
            "Initial view",
            "What to show after connecting.",
            Self::choice(
                "initial",
                s.initial_view,
                &[(InitialView::Pods, "Pods"), (InitialView::Workloads, "Workloads overview"), (InitialView::Cluster, "Cluster overview")],
                |v, _, cx| {
                    settings::update(|s| s.initial_view = v);
                    cx.refresh_windows();
                },
            ),
            cx,
        ))
        .child(Self::row(
            "Write actions",
            "Off by default. When on, detail pages can scale, restart, run, drain, delete and edit. Each asks first, except deletes set to Delete right away below.",
            {
                let on = crate::details::writes_enabled(cx);
                Checkbox::new("writes").label(if on { "Enabled" } else { "Disabled" }).checked(on).on_click(|v: &bool, _, cx| {
                    crate::details::set_writes(*v, cx);
                    cx.refresh_windows();
                })
            },
            cx,
        ))
        .child(Self::row(
            "Deleting",
            "What a delete asks first, for kinds not listed below. A protected context always asks (see Confirming).",
            Self::choice(
                "del-default",
                s.delete_confirm,
                &[(DeleteConfirm::Off, "Delete right away"), (DeleteConfirm::Ask, "Ask"), (DeleteConfirm::Type, "Type the name")],
                |v, _, cx| {
                    settings::update(|s| s.delete_confirm = v);
                    cx.refresh_windows();
                },
            ),
            cx,
        ))
        .child(self.render_delete_kinds(&s, cx))
        .child(Self::row(
            "Confirming",
            "How deletes set to Type the name, and every write on a protected context, are confirmed: \
             the names typed, or the button disabled for a few seconds, showing the seconds left.",
            Self::choice(
                "confirm-style",
                s.confirm_style,
                &[(ConfirmStyle::Wait, "Wait"), (ConfirmStyle::Type, "Type the names")],
                |v, _, cx| {
                    settings::update(|s| s.confirm_style = v);
                    cx.refresh_windows();
                },
            ),
            cx,
        ))
        .when(s.confirm_style == ConfirmStyle::Wait, |d| {
            d.child(Self::row(
                "Wait",
                "Seconds before the confirm button works.",
                Self::choice(
                    "confirm-wait",
                    s.confirm_wait_secs,
                    &[(1, "1 s"), (2, "2 s"), (3, "3 s"), (5, "5 s"), (10, "10 s")],
                    |v, _, cx| {
                        settings::update(|s| s.confirm_wait_secs = v);
                        cx.refresh_windows();
                    },
                ),
                cx,
            ))
        })
        .into_any_element()
    }

    fn contexts_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = settings::get();
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let names = crate::k8s::load_contexts().map_or_default(|c| c.names);
        v_flex()
            .child(div().pb_2().text_sm().text_color(muted).child(
                "Color a context to recognise it at a glance. Protected contexts ask you to type their name (or to wait, see General → Confirming) before any write. \
                 Run as: an identity you may impersonate (`kubectl --as`); write confirmations then offer to run that one \
                 action as it, off each time. Never for agents. Argo CD: the contexts Argo CD's Applications are read from; none \
                 ticked, they are found on their own (a context whose login may prompt is only asked once ticked). \
                 Metrics, node shell and namespaces per cluster are under Per cluster.",
            ))
            .when(s.protect_unset, |d| {
                d.child(
                    h_flex()
                        .gap_3()
                        .pb_2()
                        .child(div().flex_1().text_sm().text_color(theme.danger).child(
                            "The protection choices could not be read, so every context not set below is protected.",
                        ))
                        .child(Button::new("guess-protection").small().outline().label("Guess them again").on_click(
                            |_, _, cx| {
                                settings::update(|s| s.protect_unset = false);
                                cx.refresh_windows();
                            },
                        )),
                )
            })
            .children(names.into_iter().enumerate().map(|(i, ctx)| {
                let protected = settings::is_protected(&ctx);
                let current = s.context_colors.get(&ctx).cloned().unwrap_or_default();
                let swatches = h_flex().gap_1().children(settings::COLORS.iter().enumerate().map(|(j, (name, hex))| {
                    let (ctx, hex_s) = (ctx.clone(), hex.to_string());
                    let selected = current == *hex;
                    let color: Hsla = if hex.is_empty() { theme.secondary } else { rgb(u32::from_str_radix(&hex[1..], 16).unwrap_or(0)).into() };
                    div()
                        .id(SharedString::from(format!("sw-{i}-{j}")))
                        .size(px(18.))
                        .rounded_full()
                        .bg(color)
                        .border_2()
                        .border_color(if selected { theme.foreground } else { theme.border })
                        .cursor_pointer()
                        .tooltip({
                            let n = name.to_string();
                            move |w, cx| component::tooltip::Tooltip::new(n.clone()).build(w, cx)
                        })
                        .on_click(move |_, _, cx| {
                            let (ctx, hex) = (ctx.clone(), hex_s.clone());
                            settings::update(|s| {
                                if hex.is_empty() {
                                    s.context_colors.remove(&ctx);
                                } else {
                                    s.context_colors.insert(ctx, hex);
                                }
                            });
                            cx.refresh_windows();
                        })
                }));
                let c2 = ctx.clone();
                let c3 = ctx.clone();
                let argo_hub = s.argocd_contexts.contains(&ctx);
                let run_as = self.run_as_inputs.iter().find(|(c, ..)| *c == ctx).map(|(_, u, g)| (u.clone(), g.clone()));
                v_flex()
                    .gap_1()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .gap_4()
                            .child(div().flex_1().min_w_0().text_sm().child(ctx))
                            .child(swatches)
                            .child(
                                Checkbox::new(("argo-hub", i))
                                    .label("Argo CD")
                                    .checked(argo_hub)
                                    .tooltip("Read Argo CD's Applications from this context")
                                    .on_click(move |v: &bool, _, cx| {
                                        let v = *v;
                                        settings::update(|s| {
                                            s.argocd_contexts.retain(|c| *c != c3);
                                            if v {
                                                s.argocd_contexts.push(c3.clone());
                                            }
                                        });
                                        cx.refresh_windows();
                                    }),
                            )
                            .child(
                                Checkbox::new(("protect", i)).label("Protected").checked(protected).on_click(move |v: &bool, _, cx| {
                                    settings::set_protected(&c2, *v);
                                    cx.refresh_windows();
                                }),
                            ),
                    )
                    .children(run_as.map(|(user, groups)| {
                        h_flex()
                            .gap_2()
                            .child(div().w(px(70.)).flex_shrink_0().text_xs().text_color(muted).child("Run as"))
                            .child(div().flex_1().child(Input::new(&user).small()))
                            .child(div().flex_1().child(Input::new(&groups).small()))
                    }))
            }))
            .into_any_element()
    }

    fn clusters_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let rows: Vec<AnyElement> = self
            .cluster_inputs
            .iter()
            .enumerate()
            .map(|(i, (ctx, image, namespaces))| {
                let prom = self.prometheus.iter().find(|(c, _)| c == ctx).map(|(_, i)| i.clone());
                let argo = self.argo_inputs.iter().find(|(c, ..)| c == ctx).map(|(_, s, t)| (s.clone(), t.clone()));
                let has_token = self.argo_tokens.contains(ctx);
                let token_error = self.argo_token_error.as_ref().filter(|(c, _)| c == ctx).map(|(_, e)| e.clone());
                let forget = ctx.clone();
                let off = settings::cluster(ctx).metrics_disabled;
                let c = ctx.clone();
                let label =
                    |t: &str| div().w(px(170.)).flex_shrink_0().text_sm().text_color(muted).child(t.to_string());
                v_flex()
                    .gap_2()
                    .py_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        h_flex()
                            .gap_3()
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(ctx.clone()))
                            .child(div().flex_1())
                            .child(
                                Checkbox::new(("metrics-off", i))
                                    .label("Metrics off for this cluster")
                                    .checked(off)
                                    .on_click(move |v: &bool, _, cx| {
                                        let v = *v;
                                        settings::update(|s| {
                                            s.clusters.entry(c.clone()).or_default().metrics_disabled = v;
                                        });
                                        cx.refresh_windows();
                                    }),
                            ),
                    )
                    .children(prom.map(|p| {
                        h_flex()
                            .gap_2()
                            .child(label("Prometheus / VictoriaMetrics"))
                            .child(div().flex_1().child(Input::new(&p).small()))
                    }))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(label("Node shell image"))
                            .child(div().flex_1().child(Input::new(image).small())),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(label("Accessible namespaces"))
                            .child(div().flex_1().child(Input::new(namespaces).small())),
                    )
                    .children(argo.map(|(server, token)| {
                        h_flex()
                            .gap_2()
                            .child(label("Argo CD server, token"))
                            .child(div().flex_1().child(Input::new(&server).small()))
                            .child(div().flex_1().child(Input::new(&token).small()))
                            .when(has_token, |d| {
                                d.child(
                                    Button::new(("argo-token-forget", i)).xsmall().ghost().label("Forget").on_click(
                                        cx.listener(move |this, _, window, cx| {
                                            this.forget_argo_token(&forget, window, cx);
                                        }),
                                    ),
                                )
                            })
                    }))
                    .children(token_error.map(|e| div().text_xs().text_color(theme.danger).child(e)))
                    .into_any_element()
            })
            .collect();
        v_flex()
            .child(div().pb_2().text_sm().text_color(muted).child(format!(
                "Metrics endpoint: found automatically; or namespace/service:port, with a path for VictoriaMetrics \
                 (vmselect: monitoring/vmselect:8481/select/0/prometheus; single: monitoring/victoria-metrics:8428), or a full http(s) URL. \
                 Accessible namespaces are used when you may not list namespaces: they fill the picker, and the list shows one of them at a time. \
                 Argo CD server: on a context Argo CD runs in, its API server (https://argocd.example.com), to show Argo CD's own \
                 diff on an Application's page. It logs in with the argocd CLI's login to that server, or with a read-only token \
                 (`applications, get`) kept in {}, readable only by you. Nothing is sent \
                 until you ask for the diff.",
                crate::paths::shown_config("argocd-tokens.json")
            )))
            .children(rows)
            .into_any_element()
    }

    /// The Argo CD API servers: those of the argocd CLI and those set here, each with its
    /// login (never the token), a test of its connection, and on or off; a form to add one;
    /// the label that groups an Application's environments.
    fn argo_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let label = |t: &str| div().w(px(170.)).flex_shrink_0().text_sm().text_color(muted).child(t.to_string());
        let rows: Vec<AnyElement> =
            self.argo_api
                .list
                .iter()
                .enumerate()
                .map(|(i, e)| {
                    let base = e.server.base.clone();
                    let from = match (&e.cli_context, e.in_settings) {
                        (Some(c), true) => format!("argocd CLI context {c}, and set here"),
                        (Some(c), false) => format!("argocd CLI context {c}"),
                        (None, _) => "set here".to_string(),
                    };
                    let login = match &e.login {
                        Ok((_, how)) => (format!("Logs in with {}", how.text()), muted),
                        Err(why) => (why.clone(), theme.danger),
                    };
                    let test = self.argo_api_tests.get(&base).map(|t| match t {
                        None => ("Testing…".to_string(), muted),
                        Some(Ok(found)) => (found.clone(), theme.success),
                        Some(Err(why)) => (why.clone(), theme.danger),
                    });
                    let (b_on, b_test, b_rm) = (base.clone(), e.clone(), base.clone());
                    // A token set here, expired or not, can be forgotten (the CLI's is used then).
                    let settings_token = e.settings_token;
                    let b_forget = base.clone();
                    v_flex()
                        .gap_1()
                        .py_3()
                        .border_b_1()
                        .border_color(theme.border)
                        .child(
                            h_flex()
                                .gap_3()
                                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(e.name()))
                                .child(div().text_xs().text_color(muted).child(from))
                                .when(e.insecure, |d| {
                                    d.child(div().text_xs().text_color(theme.warning).child("certificate not checked"))
                                })
                                .child(div().flex_1())
                                .child(
                                    Button::new(("argo-api-test", i))
                                        .xsmall()
                                        .outline()
                                        .label("Test")
                                        .disabled(e.login.is_err())
                                        .on_click(
                                            cx.listener(move |this, _, _, cx| this.test_argo_api(b_test.clone(), cx)),
                                        ),
                                )
                                .when(settings_token, |d| {
                                    d.child(
                                        Button::new(("argo-api-forget", i))
                                            .xsmall()
                                            .ghost()
                                            .label("Forget token")
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                if let Err(e) = crate::k8s::argo::api::save_token(&b_forget, "") {
                                                    this.argo_api_error =
                                                        Some(format!("Could not forget the token: {e}").into());
                                                }
                                                this.argo_api_changed(cx);
                                            })),
                                    )
                                })
                                .when(e.in_settings, |d| {
                                    d.child(
                                        Button::new(("argo-api-remove", i)).xsmall().ghost().label("Remove").on_click(
                                            cx.listener(move |this, _, _, cx| this.remove_argo_api(&b_rm, cx)),
                                        ),
                                    )
                                })
                                .child(
                                    Switch::new(("argo-api-on", i))
                                        .checked(e.on)
                                        .label(if e.on { "Read" } else { "Off" })
                                        .on_click(cx.listener(move |this, v: &bool, _, cx| {
                                            this.set_argo_api_on(&b_on, *v, cx);
                                        })),
                                ),
                        )
                        .child(div().text_xs().text_color(muted).child(base))
                        .child(div().text_xs().text_color(login.1).child(login.0))
                        .children(test.map(|(text, color)| div().text_xs().text_color(color).child(text)))
                        .into_any_element()
                })
                .collect();
        v_flex()
            .child(div().pb_2().text_sm().text_color(muted).child(format!(
                "Argo CD read by its own API, with no kubeconfig for the cluster it runs in: the servers the argocd CLI \
                 is logged in to (`argocd login`), and those added here. A token added here is kept in \
                 {}, readable only by you, and wins over the CLI's; it is \
                 never shown, and sent to that server only. A read-only one is enough to list (`applications, get` and \
                 `projects, get`); the Argo CD page's actions need what Argo CD's own UI would. Applications are \
                 listed once, then kept live from Argo CD's stream while the Argo CD page is shown.",
                crate::paths::shown_config("argocd-api-tokens.json")
            )))
            .children(self.argo_api.errors.iter().map(|e| div().pb_1().text_xs().text_color(theme.danger).child(e.clone())))
            .when(self.argo_api.list.is_empty(), |d| {
                d.child(div().py_2().text_sm().text_color(muted).child("No Argo CD server: add one below, or run `argocd login`."))
            })
            .children(rows)
            .child(
                h_flex()
                    .gap_2()
                    .pt_3()
                    .child(label("Add a server, or its token"))
                    .child(div().flex_1().child(Input::new(&self.argo_api_url).small()))
                    .child(div().flex_1().child(Input::new(&self.argo_api_token).small()))
                    .child(
                        Button::new("argo-api-add")
                            .small()
                            .outline()
                            .label("Add")
                            .on_click(cx.listener(|this, _, window, cx| this.add_argo_api(window, cx))),
                    ),
            )
            .children(self.argo_api_error.clone().map(|e| div().pt_1().text_xs().text_color(theme.danger).child(e)))
            .child(Self::row(
                "Group environments by label",
                "On the Argo CD page grouped by service: Applications with the same value of this label are one \
                 service (app.kubernetes.io/part-of, say). Without it, or for one without the label, its name \
                 without the environment in it (shop-prod and shop-staging are shop).",
                div().w(px(260.)).child(Input::new(&self.argo_group_label).small()),
                cx,
            ))
            .into_any_element()
    }

    fn appearance_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = settings::get();
        let theme = cx.theme().clone();
        let mode = theme.mode;
        let system = crate::app::follows_system(cx);
        let (light, dark) = (!system && !mode.is_dark(), !system && mode.is_dark());
        v_flex()
            .child(Self::row(
                "Theme",
                "Saved between runs. \"System\" follows your desktop.",
                h_flex()
                    .gap_1()
                    .child(
                        Button::new("th-light")
                            .small()
                            .label("Light")
                            .selected(light)
                            .when(light, ButtonVariants::primary)
                            .when(!light, Button::outline)
                            .on_click(|_, w, cx| crate::app::set_theme(ThemeMode::Light, Some(w), cx)),
                    )
                    .child(
                        Button::new("th-dark")
                            .small()
                            .label("Dark")
                            .selected(dark)
                            .when(dark, ButtonVariants::primary)
                            .when(!dark, Button::outline)
                            .on_click(|_, w, cx| crate::app::set_theme(ThemeMode::Dark, Some(w), cx)),
                    )
                    .child(
                        Button::new("th-system")
                            .small()
                            .label("System")
                            .selected(system)
                            .when(system, ButtonVariants::primary)
                            .when(!system, Button::outline)
                            .on_click(|_, w, cx| {
                                crate::app::forget_theme_choice();
                                let mode = crate::app::initial_theme(cx);
                                crate::app::apply_theme(mode, Some(w), cx);
                            }),
                    ),
                cx,
            ))
            .child(Self::row(
                "Accent color",
                "Links, highlights and selection.",
                h_flex().gap_1().children(settings::ACCENTS.iter().enumerate().map(|(i, (name, dark, light))| {
                    let n = name.to_string();
                    let selected = s.accent == *name;
                    div()
                        .id(("accent", i))
                        .size(px(22.))
                        .rounded_full()
                        .bg(rgb(if mode.is_dark() { *dark } else { *light }))
                        .border_2()
                        .border_color(if selected { theme.foreground } else { theme.border })
                        .cursor_pointer()
                        .tooltip({
                            let n = n.clone();
                            move |w, cx| component::tooltip::Tooltip::new(n.clone()).build(w, cx)
                        })
                        .on_click(move |_, w, cx| {
                            let n = n.clone();
                            settings::update(|s| s.accent = n);
                            let mode = cx.theme().mode;
                            crate::app::apply_theme(mode, Some(w), cx);
                        })
                })),
                cx,
            ))
            .child(Self::row(
                "Zoom",
                &crate::app::with_keys("Also {secondary-=} / {secondary--} / {secondary-0}."),
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("z-out")
                            .small()
                            .outline()
                            .icon(Lucide::ZoomOut)
                            .accessibility_label("Zoom out")
                            .on_click(|_, w, cx| crate::app::set_zoom(crate::app::zoom(cx) - 0.1, w, cx)),
                    )
                    .child(
                        div().w(px(48.)).text_sm().text_center().child(format!("{:.0}%", crate::app::zoom(cx) * 100.)),
                    )
                    .child(
                        Button::new("z-in")
                            .small()
                            .outline()
                            .icon(Lucide::ZoomIn)
                            .accessibility_label("Zoom in")
                            .on_click(|_, w, cx| crate::app::set_zoom(crate::app::zoom(cx) + 0.1, w, cx)),
                    )
                    .child(
                        Button::new("z-reset")
                            .small()
                            .ghost()
                            .label("Reset")
                            .on_click(|_, w, cx| crate::app::set_zoom(1.0, w, cx)),
                    ),
                cx,
            ))
            .into_any_element()
    }

    fn refresh_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let number_input = |e: &Entity<InputState>| div().w(px(110.)).child(Input::new(e).small());
        v_flex()
        .child(div().pb_2().text_sm().text_color(muted).child(
            "Tables and detail pages are live (Kubernetes watches) and never poll their objects. These intervals control everything that is polled.",
        ))
        .child(Self::row("Metrics", "Seconds between CPU/memory updates (metrics-server).", number_input(&self.metrics), cx))
        .child(Self::row(
            "Detail pages",
            "Seconds between reads of an open object your access can't watch (a role that only grants get).",
            number_input(&self.detail),
            cx,
        ))
        .child(Self::row("Overviews", "Minimum seconds between overview refreshes; very large clusters wait longer.", number_input(&self.overview), cx))
        .into_any_element()
    }

    fn logs_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = settings::get();
        let number_input = |e: &Entity<InputState>| div().w(px(110.)).child(Input::new(e).small());
        v_flex()
            .child(Self::row(
                "Initial lines",
                "How many recent lines to fetch when logs open.",
                number_input(&self.tail),
                cx,
            ))
            .child(Self::row(
                "Timestamps",
                "Show the time column.",
                Checkbox::new("log-ts").checked(s.log_timestamps).on_click(|v: &bool, _, cx| {
                    let v = *v;
                    settings::update(|s| s.log_timestamps = v);
                    cx.refresh_windows();
                }),
                cx,
            ))
            .into_any_element()
    }

    fn timeline_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = settings::get();
        let number_input = |e: &Entity<InputState>| div().w(px(110.)).child(Input::new(e).small());
        let on = s.timeline.record;
        v_flex()
            .child(div().pb_2().text_sm().text_color(cx.theme().muted_foreground).child(format!(
                "While a cluster is connected, changes to what the lists and the Problems scan watch are recorded \
                 (pods, events and ReplicaSets excepted), with no request added. Kept on disk per context in \
                 {}/; a Secret's values never, only which keys changed.",
                crate::paths::shown_data("timeline")
            )))
            .child(Self::row(
                "Record changes",
                "Off: nothing more is recorded; what was is still shown.",
                Switch::new("timeline-on").checked(on).label(if on { "On" } else { "Off" }).on_click(
                    |v: &bool, _, cx| {
                        let v = *v;
                        settings::update(|s| s.timeline.record = v);
                        cx.refresh_windows();
                    },
                ),
                cx,
            ))
            .child(Self::row(
                "Kinds watched whole",
                "Comma-separated (configmaps, secrets, deployments.apps). Each gets a watch of its own on every \
                 connected cluster, and its changes a diff of the whole object, not only labels and annotations. \
                 Applies when you connect next.",
                div().w(px(320.)).child(Input::new(&self.timeline_kinds).small()),
                cx,
            ))
            .child(Self::row(
                "Size kept",
                "MB per context (1 to 1024); the oldest records go first.",
                number_input(&self.timeline_mb),
                cx,
            ))
            .child(Self::row(
                "Days kept",
                "Records older than this are dropped (1 to 365).",
                number_input(&self.timeline_days),
                cx,
            ))
            .into_any_element()
    }

    fn kubeconfig_section(&mut self) -> AnyElement {
        self.kubeconfig.clone().into_any_element()
    }

    fn agents_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let number_input = |e: &Entity<InputState>| div().w(px(110.)).child(Input::new(e).small());
        v_flex()
            .child(Self::row(
                "MCP server",
                "Agents connect to http://127.0.0.1:<port>/mcp. Port changes apply after restarting Recouvrance.",
                v_flex()
                    .items_end()
                    .gap_1()
                    .child(number_input(&self.mcp_port))
                    .when_some(self.mcp_port_error.clone(), |d, e| {
                        d.child(div().text_xs().text_color(theme.danger).child(e))
                    }),
                cx,
            ))
            .child(Self::row(
                "Agent access",
                "The kill switch; same as on the Agents page.",
                {
                    let on = self.hub.agents_enabled();
                    let hub = self.hub.clone();
                    Checkbox::new("agents-on").label(if on { "On" } else { "Off" }).checked(on).on_click(
                        move |v: &bool, window, cx| {
                            // The switch still applies to this session; say it won't survive a restart.
                            if let Err(e) = hub.set_agents_enabled(*v) {
                                use gpui_kit::component::{WindowExt as _, notification::Notification};
                                window.push_notification(
                                    Notification::error(format!("Agent access not saved: {e:#}")),
                                    cx,
                                );
                            }
                            cx.refresh_windows();
                        },
                    )
                },
                cx,
            ))
            .into_any_element()
    }

    fn assistant_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let s = settings::get();
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let block = |label: &str, hint: &str, input: &Entity<TextareaState>| {
            v_flex()
                .gap_1()
                .py_3()
                .border_b_1()
                .border_color(theme.border)
                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(label.to_string()))
                .child(div().text_xs().text_color(muted).child(hint.to_string()))
                .child(div().pt_1().child(Textarea::new(input).small()))
        };
        let missing: Vec<String> =
            s.assistant_repos.iter().filter(|r| !crate::paths::expand(r).is_dir()).cloned().collect();
        v_flex()
            .child(div().pb_2().text_sm().text_color(muted).child(
                "The assistant reads the clusters with Recouvrance's tools (objects, events, logs, Prometheus usage) under its agent rules. \
                 Tell it what only your team knows; it reads this with every question, and \"Investigate\" on a broken pod or workload starts from it.",
            ))
            .child(block(
                "Team notes",
                "Where things run, dashboards and log search links, runbooks, which repository holds which namespace, conventions. Plain text.",
                &self.notes,
            ))
            .child(block(
                "Code folders",
                "Local clones of your services and charts, one folder a line. With Claude Code only: it may read and search them (Read, Grep, Glob), nothing outside them, and never writes.",
                &self.repos,
            ))
            .when(!missing.is_empty(), |d| {
                d.child(div().pt_1().text_sm().text_color(theme.warning).child(format!("Not a folder, skipped: {}", missing.join(", "))))
            })
            .into_any_element()
    }

    fn plugins_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let (plugins, error) = cx.try_global::<crate::plugins::Loaded>().map_or_default(|l| (l.0.clone(), l.1.clone()));
        v_flex()
            .gap_3()
            .child(div().text_sm().text_color(muted).child(
                "Your own commands as actions on resources. They appear on detail pages of the kinds they name. \
                 $CONTEXT, $NAMESPACE, $NAME and $KIND are filled in; kubectl already points at the right cluster.",
            ))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .text_sm()
                            .font_family(theme.mono_font_family.clone())
                            .child(crate::paths::tilde(&crate::plugins::path())),
                    )
                    .child(
                        Button::new("plugins-reload").small().ghost().icon(Lucide::RefreshCw).label("Reload").on_click(
                            |_, _, cx| {
                                crate::plugins::reload(cx);
                                cx.refresh_windows();
                            },
                        ),
                    ),
            )
            .when_some(error, |d, e| {
                d.child(div().text_sm().text_color(theme.danger).child(format!("Could not load: {e}")))
            })
            .child(if plugins.is_empty() {
                div().text_sm().text_color(muted).child("No plugins yet.").into_any_element()
            } else {
                v_flex()
                    .children(plugins.iter().map(|p| {
                        h_flex()
                            .gap_3()
                            .py_1p5()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(div().w(px(200.)).text_sm().font_weight(FontWeight::MEDIUM).child(p.name.clone()))
                            .child(div().w(px(220.)).text_xs().text_color(muted).child(if p.kinds.is_empty() {
                                "all kinds".to_string()
                            } else {
                                p.kinds.join(", ")
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_xs()
                                    .font_family(theme.mono_font_family.clone())
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(p.command.clone()),
                            )
                            .child(div().w(px(90.)).text_xs().text_color(muted).child(match p.mode {
                                crate::plugins::Mode::Terminal => "terminal",
                                crate::plugins::Mode::Background => "background",
                            }))
                    }))
                    .into_any_element()
            })
            .child(div().pt_2().text_sm().font_weight(FontWeight::MEDIUM).child("Example"))
            .child(
                div()
                    .p_2()
                    .rounded_md()
                    .bg(theme.secondary)
                    .font_family(theme.mono_font_family.clone())
                    .text_xs()
                    .children(crate::plugins::EXAMPLE.lines().map(|l| div().whitespace_nowrap().child(l.to_string()))),
            )
            .into_any_element()
    }

    /// The shortcut list of the `?` panel, then where the commands without a key are.
    fn shortcuts_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .gap_3()
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(crate::app::with_keys(
                "Every command, with a key or without, is in the command palette ({secondary-k}). Single keys \
                 (?, g, j, k) work when no text field or terminal has focus.",
            )))
            .child(crate::app::shortcut_rows(cx))
            .into_any_element()
    }

    fn about_section(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        use crate::updates::{self, VERSION};
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let _ = window;
        let latest = match &self.update_check {
            Some(UpdateCheck::Found(v)) => Some(v.as_str()),
            _ => None,
        };
        let install = updates::install();
        let advice = install.advice(latest);
        let found = self.update_check.as_ref().map(|check| {
            let line = h_flex().gap_2().text_sm();
            match check {
                UpdateCheck::Checking => line.text_color(muted).child("Asking GitHub for the latest release…"),
                UpdateCheck::Failed(e) => line.text_color(theme.danger).child(format!("Could not check: {e}")),
                UpdateCheck::Found(v) if updates::is_newer(v, VERSION) => {
                    let page = updates::release_page(v);
                    line.child(Icon::new(Lucide::CircleArrowUp).size_4().text_color(theme.success))
                        .child(format!("Recouvrance {v} is out."))
                        .child(
                            Button::new("about-release")
                                .xsmall()
                                .ghost()
                                .icon(Lucide::ExternalLink)
                                .label("Release notes")
                                .on_click(move |_, _, _| crate::forwards::open_url(&page)),
                        )
                }
                UpdateCheck::Found(v) if updates::is_newer(VERSION, v) => {
                    line.text_color(muted).child(format!("This build is newer than the latest release, {v}."))
                }
                UpdateCheck::Found(_) => line
                    .child(Icon::new(Lucide::CircleCheck).size_4().text_color(theme.success))
                    .child("This is the latest version."),
            }
        });
        let command = advice.command.map(|cmd| {
            let copy = cmd.clone();
            h_flex()
                .gap_1()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .bg(theme.secondary)
                        .font_family(theme.mono_font_family.clone())
                        .text_xs()
                        .child(super::selectable::selectable(cmd)),
                )
                .child(
                    Button::new("about-copy")
                        .xsmall()
                        .ghost()
                        .icon(Lucide::Copy)
                        .accessibility_label("Copy the command")
                        .tooltip("Copy the command")
                        .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))),
                )
        });
        let installed = match &install.path {
            Some(path) => format!("{}: {}", advice.installed, path.display()),
            None => format!("{}.", advice.installed),
        };
        v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(super::logo(px(40.)))
                    .child(div().text_2xl().font_weight(FontWeight::BOLD).child("Recouvrance")),
            )
            .child(div().text_sm().text_color(muted).child(format!("Version {VERSION}")))
            .child(
                div()
                    .text_sm()
                    .child("A native Kubernetes client in Rust (gpui), with a local MCP server for AI agents."),
            )
            .child(
                h_flex()
                    .pt_2()
                    .gap_2()
                    .when(updates::own_notes().is_some(), |d| {
                        d.child(
                            Button::new("about-whats-new")
                                .small()
                                .outline()
                                .icon(Lucide::Sparkles)
                                .label(format!("What's new in {VERSION}"))
                                .on_click(|_, window, cx| super::whats_new::open_own(window, cx)),
                        )
                    })
                    .child(
                        Button::new("about-check")
                            .small()
                            .outline()
                            .icon(Lucide::RefreshCw)
                            .label("Check for updates")
                            .loading(matches!(self.update_check, Some(UpdateCheck::Checking)))
                            .on_click(cx.listener(|this, _, _, cx| this.check_updates(cx))),
                    )
                    .child(
                        Button::new("about-releases")
                            .small()
                            .ghost()
                            .icon(Lucide::ExternalLink)
                            .label("Releases")
                            .on_click(|_, _, _| crate::forwards::open_url(updates::RELEASES)),
                    ),
            )
            .children(found)
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("Check for updates asks GitHub for the latest release, only when you click it."),
            )
            .child(
                v_flex()
                    .gap_1()
                    .pt_4()
                    .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Updating"))
                    .child(div().text_sm().text_color(muted).child(installed))
                    .child(div().text_sm().child(advice.how))
                    .children(command)
                    .child(div().text_sm().child("Then restart Recouvrance.")),
            )
            .child(div().pt_3().text_xs().text_color(muted).child(format!(
                "Settings: {}/ · Agent activity: {}",
                crate::paths::tilde(&crate::paths::config_dir()),
                crate::paths::shown_data("audit.jsonl")
            )))
            .into_any_element()
    }

    /// Ask GitHub for the latest release: the one request About sends, and only on a click.
    fn check_updates(&mut self, cx: &mut Context<Self>) {
        self.update_check = Some(UpdateCheck::Checking);
        cx.notify();
        let request = crate::k8s::run(crate::updates::latest_release());
        self.update_task.spawn(cx, request, |this, found, cx| {
            this.update_check = Some(match found {
                Ok(version) => UpdateCheck::Found(version),
                Err(e) => UpdateCheck::Failed(format!("{e:#}")),
            });
            cx.notify();
        });
    }
}

impl Render for SettingsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let body = self.render_section(window, cx);
        h_flex()
            .size_full()
            .items_start()
            .bg(theme.background)
            .child(
                v_flex()
                    .id("settings-sections")
                    .role(Role::TabList)
                    .aria_label("Settings sections")
                    .w(px(220.))
                    .h_full()
                    .p_3()
                    .gap_0p5()
                    .border_r_1()
                    .border_color(theme.border)
                    .child(div().px_2().pb_3().text_lg().font_weight(FontWeight::BOLD).child("Settings"))
                    .children(SECTIONS.iter().enumerate().map(|(i, (section, label, icon))| {
                        let sec = *section;
                        let on = self.section == sec;
                        h_flex()
                            .id(("settings-nav", i))
                            .gap_2()
                            .px_2()
                            .py_1p5()
                            .rounded_md()
                            .text_sm()
                            .cursor_pointer()
                            .when(on, |d| d.bg(theme.list_active))
                            .hover(|s| s.bg(theme.list_hover))
                            .control(Role::Tab, *label, theme.ring)
                            .aria_selected(on)
                            .on_click(cx.listener(move |this, _, window, cx| this.show(sec, window, cx)))
                            .child(Icon::new(*icon).size_4().text_color(theme.muted_foreground))
                            .child(*label)
                    })),
            )
            .child(
                v_flex().id("settings-body").flex_1().min_w_0().h_full().overflow_y_scroll().px_8().py_6().child(
                    div()
                        .max_w(px(820.))
                        .when_some(settings::load_error(), |d, e| {
                            d.child(div().pb_4().text_sm().text_color(theme.danger).child(format!(
                                "{e}. Using the defaults, with every context protected; changes are not saved."
                            )))
                        })
                        .child(body),
                ),
            )
    }
}

/// Kinds typed in a list: `configmaps, secrets deployments.apps`.
fn kind_list(text: &str) -> Vec<String> {
    text.split([',', ' ', '\n']).map(str::trim).filter(|k| !k.is_empty()).map(String::from).collect()
}

fn input_text(state: &InputState) -> String {
    state.value().to_string()
}

fn textarea_text(state: &TextareaState) -> String {
    state.value().to_string()
}

/// The code folders typed one a line.
fn repo_lines(text: &str) -> Vec<String> {
    text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()
}

/// Why a typed MCP port is not saved, or None once `save` saved it.
fn port_error(typed: &str, save: impl FnOnce(u16) -> Result<()>) -> Option<SharedString> {
    match typed.trim().parse::<u16>() {
        Ok(p) if p >= 1024 => save(p).err().map(|e| format!("Not saved: {e:#}").into()),
        Ok(_) => Some("Ports below 1024 need root: use 1024 to 65535".into()),
        Err(_) => Some("Use a port from 1024 to 65535".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::port_error;

    #[test]
    fn a_port_not_saved_says_why() {
        assert_eq!(port_error("48000", |_| Ok(())), None);
        let failed = port_error(" 48000 ", |p| anyhow::bail!("agents.json is read-only ({p})"));
        assert_eq!(failed.as_deref(), Some("Not saved: agents.json is read-only (48000)"));
        let saved = std::cell::Cell::new(false);
        let save = |_| {
            saved.set(true);
            Ok(())
        };
        assert!(port_error("80", save).is_some_and(|e| e.contains("1024")));
        assert!(port_error("70000", save).is_some());
        assert!(!saved.get(), "a refused port is not saved");
    }
}
