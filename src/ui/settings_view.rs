//! Settings page: startup, appearance, refresh rates, logs, kubeconfig,
//! agents and shortcuts.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Icon, Selectable as _, Sizable as _, ThemeMode, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::settings::{self, DeleteConfirm, InitialView, Startup};

#[derive(Clone, Copy, PartialEq)]
enum Section {
    General,
    Contexts,
    Clusters,
    Appearance,
    Refresh,
    Logs,
    Kubeconfig,
    Agents,
    Assistant,
    Plugins,
    Shortcuts,
    Requests,
    About,
}

const SECTIONS: [(Section, &str, Lucide); 13] = [
    (Section::General, "General", Lucide::Settings),
    (Section::Contexts, "Contexts", Lucide::Ship),
    (Section::Clusters, "Per cluster", Lucide::Server),
    (Section::Appearance, "Appearance", Lucide::Palette),
    (Section::Refresh, "Refresh rates", Lucide::RefreshCw),
    (Section::Logs, "Log viewer", Lucide::ScrollText),
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
}

impl SettingsView {
    pub fn new(hub: std::sync::Arc<crate::mcp::Hub>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let s = settings::get();
        let field = Self::field;
        let metrics = field(s.metrics_refresh.to_string(), "15", window, cx);
        let detail = field(s.detail_refresh.to_string(), "5", window, cx);
        let overview = field(s.overview_refresh.to_string(), "30", window, cx);
        let tail = field(s.log_tail_lines.to_string(), "1000", window, cx);
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
        let hub2 = hub.clone();
        subs.push(cx.subscribe(&mcp_port, move |this, state, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                this.mcp_port_error = port_error(&state.read(cx).value(), |p| hub2.set_port(p));
                cx.notify();
            }
        }));
        subs.push(Self::save_typed(&notes, textarea_text, cx, |v| settings::update(|s| s.assistant_notes = v)));
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
            Some("kubeconfig") => Section::Kubeconfig,
            Some("plugins") => Section::Plugins,
            Some("appearance") => Section::Appearance,
            Some("general") => Section::General,
            Some("assistant") => Section::Assistant,
            _ => Section::General,
        };
        let mut this = Self {
            section,
            metrics,
            detail,
            overview,
            tail,
            kubeconfig,
            mcp_port,
            mcp_port_error: None,
            notes,
            repos,
            prometheus: Vec::new(),
            cluster_inputs: Vec::new(),
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
        };
        // Another window may have changed a setting meanwhile.
        this._subs.push(cx.observe_window_activation(window, |this, window, cx| {
            if window.is_window_active() {
                this.reload_fields(window, cx);
            }
        }));
        this.sync_clusters(window, cx);
        this
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
        let names = crate::k8s::load_contexts().map(|c| c.names).unwrap_or_default();
        self.cluster_inputs.retain(|(ctx, ..)| names.contains(ctx));
        self.prometheus.retain(|(ctx, _)| names.contains(ctx));
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
        self.cluster_inputs.push((ctx, image, namespaces));
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
        let s = settings::get();
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let number_input = |e: &Entity<InputState>| div().w(px(110.)).child(Input::new(e).small());
        match self.section {
            Section::General => v_flex()
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
                    "What a delete asks first, for kinds not listed below. A protected context always asks for its name.",
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
                .into_any_element(),
            Section::Contexts => {
                let names = crate::k8s::load_contexts().map(|c| c.names).unwrap_or_default();
                v_flex()
                    .child(div().pb_2().text_sm().text_color(muted).child(
                        "Color a context to recognise it at a glance. Protected contexts ask you to type their name before any write. \
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
                        h_flex()
                            .gap_4()
                            .py_2()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(div().flex_1().min_w_0().text_sm().child(ctx))
                            .child(swatches)
                            .child(
                                Checkbox::new(("protect", i)).label("Protected").checked(protected).on_click(move |v: &bool, _, cx| {
                                    settings::set_protected(&c2, *v);
                                    cx.refresh_windows();
                                }),
                            )
                    }))
                    .into_any_element()
            }
            Section::Clusters => {
                let rows: Vec<AnyElement> = self
                    .cluster_inputs
                    .iter()
                    .enumerate()
                    .map(|(i, (ctx, image, namespaces))| {
                        let prom = self.prometheus.iter().find(|(c, _)| c == ctx).map(|(_, i)| i.clone());
                        let off = settings::cluster(ctx).metrics_disabled;
                        let c = ctx.clone();
                        let label = |t: &str| div().w(px(170.)).flex_shrink_0().text_sm().text_color(muted).child(t.to_string());
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
                                    .child(Checkbox::new(("metrics-off", i)).label("Metrics off for this cluster").checked(off).on_click(move |v: &bool, _, cx| {
                                        let v = *v;
                                        settings::update(|s| s.clusters.entry(c.clone()).or_default().metrics_disabled = v);
                                        cx.refresh_windows();
                                    })),
                            )
                            .children(prom.map(|p| h_flex().gap_2().child(label("Prometheus / VictoriaMetrics")).child(div().flex_1().child(Input::new(&p).small()))))
                            .child(h_flex().gap_2().child(label("Node shell image")).child(div().flex_1().child(Input::new(image).small())))
                            .child(h_flex().gap_2().child(label("Accessible namespaces")).child(div().flex_1().child(Input::new(namespaces).small())))
                            .into_any_element()
                    })
                    .collect();
                v_flex()
                    .child(div().pb_2().text_sm().text_color(muted).child(
                        "Metrics endpoint: found automatically; or namespace/service:port, with a path for VictoriaMetrics \
                         (vmselect: monitoring/vmselect:8481/select/0/prometheus; single: monitoring/victoria-metrics:8428), or a full http(s) URL. \
                         Accessible namespaces are used when you may not list namespaces: they fill the picker, and the list shows one of them at a time.",
                    ))
                    .children(rows)
                    .into_any_element()
            }
            Section::Appearance => {
                let mode = theme.mode;
                let system = crate::app::follows_system(cx);
                let (light, dark) = (!system && !mode.is_dark(), !system && mode.is_dark());
                v_flex()
                    .child(Self::row(
                        "Theme",
                        "Saved between runs. \"System\" follows your desktop.",
                        h_flex()
                            .gap_1()
                            .child(Button::new("th-light").small().label("Light").selected(light).when(light, ButtonVariants::primary).when(!light, Button::outline).on_click(|_, w, cx| crate::app::set_theme(ThemeMode::Light, Some(w), cx)))
                            .child(Button::new("th-dark").small().label("Dark").selected(dark).when(dark, ButtonVariants::primary).when(!dark, Button::outline).on_click(|_, w, cx| crate::app::set_theme(ThemeMode::Dark, Some(w), cx)))
                            .child(Button::new("th-system").small().label("System").selected(system).when(system, ButtonVariants::primary).when(!system, Button::outline).on_click(|_, w, cx| {
                                crate::app::forget_theme_choice();
                                let mode = crate::app::initial_theme(cx);
                                crate::app::apply_theme(mode, Some(w), cx);
                            })),
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
                        "Also Ctrl + / Ctrl - / Ctrl 0.",
                        h_flex()
                            .gap_2()
                            .child(Button::new("z-out").small().outline().icon(Lucide::ZoomOut).on_click(|_, w, cx| crate::app::set_zoom(crate::app::zoom(cx) - 0.1, w, cx)))
                            .child(div().w(px(48.)).text_sm().text_center().child(format!("{:.0}%", crate::app::zoom(cx) * 100.)))
                            .child(Button::new("z-in").small().outline().icon(Lucide::ZoomIn).on_click(|_, w, cx| crate::app::set_zoom(crate::app::zoom(cx) + 0.1, w, cx)))
                            .child(Button::new("z-reset").small().ghost().label("Reset").on_click(|_, w, cx| crate::app::set_zoom(1.0, w, cx))),
                        cx,
                    ))
                    .into_any_element()
            }
            Section::Refresh => v_flex()
                .child(div().pb_2().text_sm().text_color(muted).child(
                    "Tables are live (Kubernetes watches) and never poll. These intervals control everything that is polled.",
                ))
                .child(Self::row("Metrics", "Seconds between CPU/memory updates (metrics-server).", number_input(&self.metrics), cx))
                .child(Self::row("Detail pages", "Seconds between refreshes of an open object.", number_input(&self.detail), cx))
                .child(Self::row("Overviews", "Minimum seconds between overview refreshes; very large clusters wait longer.", number_input(&self.overview), cx))
                .into_any_element(),
            Section::Logs => v_flex()
                .child(Self::row("Initial lines", "How many recent lines to fetch when logs open.", number_input(&self.tail), cx))
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
                .into_any_element(),
            Section::Kubeconfig => self.kubeconfig.clone().into_any_element(),
            Section::Agents => v_flex()
                .child(Self::row(
                    "MCP server",
                    "Agents connect to http://127.0.0.1:<port>/mcp. Port changes apply after restarting Recouvrance.",
                    v_flex().items_end().gap_1().child(number_input(&self.mcp_port)).when_some(
                        self.mcp_port_error.clone(),
                        |d, e| d.child(div().text_xs().text_color(theme.danger).child(e)),
                    ),
                    cx,
                ))
                .child(Self::row(
                    "Agent access",
                    "The kill switch; same as on the Agents page.",
                    {
                        let on = self.hub.agents_enabled();
                        let hub = self.hub.clone();
                        Checkbox::new("agents-on").label(if on { "On" } else { "Off" }).checked(on).on_click(move |v: &bool, window, cx| {
                            // The switch still applies to this session; say it won't survive a restart.
                            if let Err(e) = hub.set_agents_enabled(*v) {
                                use gpui_kit::component::{WindowExt as _, notification::Notification};
                                window.push_notification(Notification::error(format!("Agent access not saved: {e:#}")), cx);
                            }
                            cx.refresh_windows();
                        })
                    },
                    cx,
                ))
                .into_any_element(),
            Section::Assistant => {
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
                let missing: Vec<String> = s
                    .assistant_repos
                    .iter()
                    .filter(|r| !crate::paths::expand(r).is_dir())
                    .cloned()
                    .collect();
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
            Section::Plugins => {
                let (plugins, error) = cx.try_global::<crate::plugins::Loaded>().map(|l| (l.0.clone(), l.1.clone())).unwrap_or_default();
                v_flex()
                    .gap_3()
                    .child(div().text_sm().text_color(muted).child(
                        "Your own commands as actions on resources. They appear on detail pages of the kinds they name. \
                         $CONTEXT, $NAMESPACE, $NAME and $KIND are filled in; kubectl already points at the right cluster.",
                    ))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().text_sm().font_family(theme.mono_font_family.clone()).child(crate::plugins::path().display().to_string()))
                            .child(Button::new("plugins-reload").small().ghost().icon(Lucide::RefreshCw).label("Reload").on_click(|_, _, cx| {
                                crate::plugins::reload(cx);
                                cx.refresh_windows();
                            })),
                    )
                    .when_some(error, |d, e| d.child(div().text_sm().text_color(theme.danger).child(format!("Could not load: {e}"))))
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
                                    .child(div().w(px(220.)).text_xs().text_color(muted).child(if p.kinds.is_empty() { "all kinds".to_string() } else { p.kinds.join(", ") }))
                                    .child(div().flex_1().min_w_0().text_xs().font_family(theme.mono_font_family.clone()).overflow_hidden().text_ellipsis().whitespace_nowrap().child(p.command.clone()))
                                    .child(div().w(px(90.)).text_xs().text_color(muted).child(match p.mode {
                                        crate::plugins::Mode::Terminal => "terminal",
                                        crate::plugins::Mode::Background => "background",
                                    }))
                            }))
                            .into_any_element()
                    })
                    .child(div().pt_2().text_sm().font_weight(FontWeight::MEDIUM).child("Example"))
                    .child(div().p_2().rounded_md().bg(theme.secondary).font_family(theme.mono_font_family.clone()).text_xs().children(crate::plugins::EXAMPLE.lines().map(|l| div().whitespace_nowrap().child(l.to_string()))))
                    .into_any_element()
            }
            Section::Shortcuts => {
                let keys = [
                    ("Ctrl K", "Command palette: resources, pages, objects across clusters"),
                    ("Ctrl F  or  /", "Filter the current table"),
                    ("Esc", "Back from a detail page"),
                    ("↑ ↓  then  Enter", "Move in a table, open the row"),
                    ("Ctrl Shift N", "New window (shares agents and settings)"),
                    ("Ctrl T / Ctrl W", "New tab / close tab (right-click a row: open in new tab)"),
                    ("Ctrl Tab / Ctrl Shift Tab", "Next / previous tab"),
                    ("Ctrl B", "Hide or show the sidebar"),
                    ("F5  or  Ctrl R", "Refresh the page"),
                    ("Alt ← / Alt →", "Back / forward"),
                    ("Ctrl + / Ctrl - / Ctrl 0", "Zoom in / out / reset"),
                    ("Right-click a row", "Open, logs, shell, YAML, events, run, scale, restart, delete"),
                    ("Ctrl Shift V", "Paste in a shell"),
                    ("Double-click a context", "Connect to just that context"),
                ];
                v_flex()
                    .children(keys.into_iter().map(|(k, what)| {
                        h_flex()
                            .gap_4()
                            .py_2()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(div().w(px(220.)).child(div().px_2().py_0p5().rounded_md().bg(theme.secondary).text_xs().font_family(theme.mono_font_family.clone()).child(k)))
                            .child(div().text_sm().child(what))
                    }))
                    .into_any_element()
            }
            Section::Requests => {
                use crate::k8s::meter;
                let ms = |d: std::time::Duration| format!("{} ms", d.as_millis());
                let rows = meter::snapshot();
                let cell = |w: f32, text: String| div().w(px(w)).flex_shrink_0().child(text);
                let header = h_flex()
                    .gap_3()
                    .py_1()
                    .text_xs()
                    .text_color(muted)
                    .border_b_1()
                    .border_color(theme.border)
                    .child(cell(170., "Cluster".into()))
                    .child(cell(160., "Requests to".into()))
                    .child(cell(80., "Count".into()))
                    .child(cell(70., "Failed".into()))
                    .child(cell(90., "Average".into()))
                    .child(cell(90., "Slowest".into()))
                    .child(cell(90., "Last".into()));
                v_flex()
                    .child(div().pb_2().text_sm().text_color(muted).child(
                        "How long the API servers take to start answering, since Recouvrance started (watches and log streams count until they open). Failed: an error status, a timeout or no connection.",
                    ))
                    .child(header)
                    .children(rows.iter().map(|(ctx, class, s)| {
                        h_flex()
                            .gap_3()
                            .py_1()
                            .text_sm()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(cell(170., ctx.clone()).overflow_hidden().text_ellipsis().whitespace_nowrap())
                            .child(cell(160., class.label().into()))
                            .child(cell(80., s.count.to_string()))
                            .child(cell(70., s.errors.to_string()).when(s.errors > 0, |d| d.text_color(theme.warning)))
                            .child(cell(90., ms(s.average())))
                            .child(cell(90., ms(s.slowest)))
                            .child(cell(90., ms(s.last)))
                    }))
                    .when(rows.is_empty(), |d| d.child(div().py_2().text_sm().text_color(muted).child("No requests yet.")))
                    .child(h_flex().pt_3().child(Button::new("meter-reset").small().outline().label("Reset").on_click(
                        cx.listener(|_, _, _, cx| {
                            meter::reset();
                            cx.notify();
                        }),
                    )))
                    .into_any_element()
            }
            Section::About => {
                let _ = window;
                v_flex()
                    .gap_2()
                    .child(h_flex().gap_2().child(super::logo(px(40.))).child(div().text_2xl().font_weight(FontWeight::BOLD).child("Recouvrance")))
                    .child(div().text_sm().text_color(muted).child(format!("Version {}", env!("CARGO_PKG_VERSION"))))
                    .child(div().text_sm().child("A native Kubernetes client in Rust (gpui), with a local MCP server for AI agents."))
                    .child(div().text_sm().text_color(muted).child(
                        "Installed with the install script: update with recouvrance-update. Releases: github.com/nahuel11500/recouvrance/releases",
                    ))
                    .child(div().pt_3().text_xs().text_color(muted).child("Settings: ~/.config/recouvrance/ · Agent activity: ~/.local/share/recouvrance/audit.jsonl"))
                    .into_any_element()
            }
        }
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
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.section = sec;
                                if sec == Section::Clusters {
                                    this.sync_clusters(window, cx);
                                }
                                this.reload_fields(window, cx);
                                cx.notify();
                            }))
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
