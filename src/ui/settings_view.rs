//! Settings page: startup, appearance, refresh rates, logs, kubeconfig,
//! agents and shortcuts.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, Icon, Selectable as _, Sizable as _, ThemeMode, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::settings::{self, InitialView, Startup};

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
    Plugins,
    Shortcuts,
    About,
}

const SECTIONS: [(Section, &str, Lucide); 11] = [
    (Section::General, "General", Lucide::Settings),
    (Section::Contexts, "Contexts", Lucide::Ship),
    (Section::Clusters, "Per cluster", Lucide::Server),
    (Section::Appearance, "Appearance", Lucide::Palette),
    (Section::Refresh, "Refresh rates", Lucide::RefreshCw),
    (Section::Logs, "Log viewer", Lucide::ScrollText),
    (Section::Kubeconfig, "Kubeconfig", Lucide::FileText),
    (Section::Agents, "Agents (MCP)", Lucide::Bot),
    (Section::Plugins, "Plugins", Lucide::Puzzle),
    (Section::Shortcuts, "Shortcuts", Lucide::Keyboard),
    (Section::About, "About", Lucide::Info),
];

pub struct SettingsView {
    section: Section,
    metrics: Entity<InputState>,
    detail: Entity<InputState>,
    overview: Entity<InputState>,
    tail: Entity<InputState>,
    kubeconfigs: Entity<InputState>,
    kubeconfig_folders: Entity<InputState>,
    mcp_port: Entity<InputState>,
    /// Prometheus override per context (`namespace/service:port`).
    prometheus: Vec<(String, Entity<InputState>)>,
    /// Per context: node shell image and accessible namespaces.
    cluster_inputs: Vec<(String, Entity<InputState>, Entity<InputState>)>,
    hub: std::sync::Arc<crate::mcp::Hub>,
    _subs: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(hub: std::sync::Arc<crate::mcp::Hub>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let s = settings::get();
        let field = |value: String, placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut st = InputState::new(window, cx).placeholder(placeholder);
                st.set_value(value, window, cx);
                st
            })
        };
        let metrics = field(s.metrics_refresh.to_string(), "15", window, cx);
        let detail = field(s.detail_refresh.to_string(), "5", window, cx);
        let overview = field(s.overview_refresh.to_string(), "30", window, cx);
        let tail = field(s.log_tail_lines.to_string(), "1000", window, cx);
        let kubeconfigs = field(s.kubeconfigs.join(", "), "~/.kube/other-config, ~/clusters/team.yaml", window, cx);
        let mcp_port = field(hub.port().to_string(), "47070", window, cx);
        let kubeconfig_folders = field(s.kubeconfig_folders.join(", "), "~/.kube/configs, ~/clusters", window, cx);

        let mut subs = Vec::new();
        let number = |input: &Entity<InputState>,
                      apply: fn(&mut settings::Settings, u64),
                      subs: &mut Vec<Subscription>,
                      cx: &mut Context<Self>| {
            subs.push(cx.subscribe(input, move |_, state, ev: &InputEvent, cx| {
                if let InputEvent::Change = ev
                    && let Ok(n) = state.read(cx).value().trim().parse::<u64>()
                    && n > 0
                {
                    settings::update(|s| apply(s, n));
                }
            }));
        };
        number(&metrics, |s, n| s.metrics_refresh = n, &mut subs, cx);
        number(&detail, |s, n| s.detail_refresh = n, &mut subs, cx);
        number(&overview, |s, n| s.overview_refresh = n, &mut subs, cx);
        number(&tail, |s, n| s.log_tail_lines = n as i64, &mut subs, cx);
        subs.push(cx.subscribe(&kubeconfigs, |_, state, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                let list: Vec<String> = state
                    .read(cx)
                    .value()
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                settings::update(|s| s.kubeconfigs = list);
                cx.notify();
            }
        }));
        subs.push(cx.subscribe(&kubeconfig_folders, |_, state, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                let list: Vec<String> = state
                    .read(cx)
                    .value()
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
                settings::update(|s| s.kubeconfig_folders = list);
                cx.notify();
            }
        }));
        let hub2 = hub.clone();
        subs.push(cx.subscribe(&mcp_port, move |_, state, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev
                && let Ok(p) = state.read(cx).value().trim().parse::<u16>()
                && p >= 1024
            {
                let _ = hub2.set_port(p);
            }
        }));
        let mut prometheus = Vec::new();
        for ctx in crate::k8s::load_contexts().map(|c| c.names).unwrap_or_default() {
            let input = field(
                s.prometheus.get(&ctx).cloned().unwrap_or_default(),
                "Prometheus: found automatically",
                window,
                cx,
            );
            let c = ctx.clone();
            subs.push(cx.subscribe(&input, move |_, state, ev: &InputEvent, cx| {
                if let InputEvent::Change = ev {
                    let v = state.read(cx).value().trim().to_string();
                    settings::update(|s| {
                        if v.is_empty() {
                            s.prometheus.remove(&c);
                        } else {
                            s.prometheus.insert(c.clone(), v);
                        }
                    });
                }
            }));
            prometheus.push((ctx, input));
        }
        let mut cluster_inputs = Vec::new();
        for ctx in crate::k8s::load_contexts().map(|c| c.names).unwrap_or_default() {
            let cs = settings::cluster(&ctx);
            let image = field(cs.node_shell_image.clone().unwrap_or_default(), "busybox:1.37", window, cx);
            let namespaces = field(
                cs.namespaces.join(", "),
                "all (or: team-a, team-b when you may not list namespaces)",
                window,
                cx,
            );
            let c = ctx.clone();
            subs.push(cx.subscribe(&image, move |_, state, ev: &InputEvent, cx| {
                if let InputEvent::Change = ev {
                    let v = state.read(cx).value().trim().to_string();
                    settings::update(|s| {
                        s.clusters.entry(c.clone()).or_default().node_shell_image = (!v.is_empty()).then_some(v)
                    });
                }
            }));
            let c = ctx.clone();
            subs.push(cx.subscribe(&namespaces, move |_, state, ev: &InputEvent, cx| {
                if let InputEvent::Change = ev {
                    let list: Vec<String> = state
                        .read(cx)
                        .value()
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect();
                    settings::update(|s| s.clusters.entry(c.clone()).or_default().namespaces = list);
                }
            }));
            cluster_inputs.push((ctx, image, namespaces));
        }
        let section = match crate::app::dev_var("RECOUVRANCE_SECTION").as_deref() {
            Some("contexts") => Section::Contexts,
            Some("clusters") => Section::Clusters,
            Some("kubeconfig") => Section::Kubeconfig,
            Some("plugins") => Section::Plugins,
            Some("appearance") => Section::Appearance,
            Some("general") => Section::General,
            _ => Section::General,
        };
        Self {
            section,
            metrics,
            detail,
            overview,
            tail,
            kubeconfigs,
            kubeconfig_folders,
            mcp_port,
            prometheus,
            cluster_inputs,
            hub,
            _subs: subs,
        }
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
                .when(v == current, |b| b.primary())
                .when(v != current, |b| b.outline())
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
                    "Off by default. When on, detail pages can scale, restart, run, drain, delete and edit (each asks for confirmation).",
                    {
                        let on = crate::details::writes_enabled(cx);
                        Checkbox::new("writes").label(if on { "Enabled" } else { "Disabled" }).checked(on).on_click(|v: &bool, _, cx| {
                            cx.set_global(crate::details::WriteMode(*v));
                            cx.refresh_windows();
                        })
                    },
                    cx,
                ))
                .into_any_element(),
            Section::Contexts => {
                let names = crate::k8s::load_contexts().map(|c| c.names).unwrap_or_default();
                v_flex()
                    .child(div().pb_2().text_sm().text_color(muted).child(
                        "Color a context to recognise it at a glance. Protected contexts ask you to type their name before any write. \
                         Metrics, node shell and namespaces per cluster are under Per cluster.",
                    ))
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
                            .child(div().flex_1().min_w_0().text_sm().child(ctx.clone()))
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
                         Accessible namespaces are used when you may not list namespaces: they fill the picker and \"all namespaces\" means them.",
                    ))
                    .children(rows)
                    .into_any_element()
            }
            Section::Appearance => {
                let mode = theme.mode;
                v_flex()
                    .child(Self::row(
                        "Theme",
                        "Saved between runs. \"System\" follows your desktop.",
                        h_flex()
                            .gap_1()
                            .child(Button::new("th-light").small().label("Light").selected(!mode.is_dark()).when(!mode.is_dark(), |b| b.primary()).when(mode.is_dark(), |b| b.outline()).on_click(|_, w, cx| crate::app::set_theme(ThemeMode::Light, Some(w), cx)))
                            .child(Button::new("th-dark").small().label("Dark").selected(mode.is_dark()).when(mode.is_dark(), |b| b.primary()).when(!mode.is_dark(), |b| b.outline()).on_click(|_, w, cx| crate::app::set_theme(ThemeMode::Dark, Some(w), cx)))
                            .child(Button::new("th-system").small().outline().label("System").on_click(|_, w, cx| {
                                crate::app::forget_theme_choice();
                                let mode = crate::app::initial_theme(cx);
                                crate::app::set_theme(mode, Some(w), cx);
                                crate::app::forget_theme_choice();
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
                                    crate::app::set_theme(mode, Some(w), cx);
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
            Section::Kubeconfig => {
                let default = std::env::var("KUBECONFIG").unwrap_or_else(|_| "~/.kube/config".into());
                let found: Vec<(String, bool)> = s.kubeconfigs.iter().map(|p| (p.clone(), settings::expand(p).is_file())).collect();
                v_flex()
                    .child(Self::row("Default", "From KUBECONFIG, or ~/.kube/config.", div().text_sm().font_family(theme.mono_font_family.clone()).child(default), cx))
                    .child(
                        v_flex()
                            .gap_1()
                            .py_3()
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Extra kubeconfig files"))
                            .child(div().text_xs().text_color(muted).child("Comma-separated; merged with the default. Changes to any kubeconfig file are picked up within seconds."))
                            .child(Input::new(&self.kubeconfigs).small())
                            .children(found.into_iter().map(|(p, ok)| {
                                h_flex()
                                    .gap_1()
                                    .text_xs()
                                    .child(Icon::new(if ok { Lucide::Check } else { Lucide::X }).size_3().text_color(if ok { theme.success } else { theme.danger }))
                                    .child(div().text_color(if ok { muted } else { theme.danger }).child(if ok { p } else { format!("{p} (not found)") }))
                            })),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .py_3()
                            .child(div().text_sm().font_weight(FontWeight::MEDIUM).child("Watched folders"))
                            .child(div().text_xs().text_color(muted).child(
                                "Every file in these folders is merged (one kubeconfig per cluster works well). Contexts with the same name in two files \
                                 are kept apart as name@file.",
                            ))
                            .child(Input::new(&self.kubeconfig_folders).small())
                            .child(div().text_xs().text_color(muted).child(format!("{} kubeconfig files in use", crate::k8s::kubeconfig_files().len()))),
                    )
                    .into_any_element()
            }
            Section::Agents => v_flex()
                .child(Self::row(
                    "MCP server",
                    "Agents connect to http://127.0.0.1:<port>/mcp. Port changes apply after restarting Recouvrance.",
                    number_input(&self.mcp_port),
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
            Section::About => {
                let _ = window;
                v_flex()
                    .gap_2()
                    .child(h_flex().gap_2().child(Icon::new(Lucide::Ship).size_8().text_color(theme.blue)).child(div().text_2xl().font_weight(FontWeight::BOLD).child("Recouvrance")))
                    .child(div().text_sm().text_color(muted).child(format!("Version {}", env!("CARGO_PKG_VERSION"))))
                    .child(div().text_sm().child("A native Kubernetes client in Rust (gpui), with a local MCP server for AI agents."))
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
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.section = sec;
                                cx.notify();
                            }))
                            .child(Icon::new(*icon).size_4().text_color(theme.muted_foreground))
                            .child(*label)
                    })),
            )
            .child(
                v_flex()
                    .id("settings-body")
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_y_scroll()
                    .px_8()
                    .py_6()
                    .child(div().max_w(px(820.)).child(body)),
            )
    }
}
