//! Install a Helm chart from the repositories configured in your `helm` CLI:
//! search, pick a version, edit values, server dry run, install.

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use serde::Deserialize;

use crate::k8s;

/// The values editor's text while the picked chart's values load.
const LOADING_VALUES: &str = "# loading chart values…";

#[derive(Deserialize, Clone)]
struct Chart {
    name: String,
    version: String,
    #[serde(default)]
    app_version: String,
    #[serde(default)]
    description: String,
}

pub struct HelmInstall {
    context: String,
    search: Entity<InputState>,
    results: Option<Result<Vec<Chart>, String>>,
    chart: Option<Chart>,
    versions: Vec<String>,
    version: Entity<InputState>,
    release: Entity<InputState>,
    namespace: Entity<InputState>,
    create_ns: bool,
    values: Entity<EditorState>,
    output: Option<(bool, String)>,
    busy: bool,
    /// Counts runs and picks: a run shows its result only if neither came after it.
    run_id: u64,
    /// The chart search running; a new search cancels it.
    _search: Option<Task<()>>,
    /// Loading the picked chart's versions and values; a new pick cancels it.
    _pick: Option<Task<()>>,
    _tasks: Vec<Task<()>>,
    _subs: Vec<Subscription>,
}

impl HelmInstall {
    pub fn new(context: String, namespace: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &'static str, value: String, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut s = InputState::new(window, cx).placeholder(placeholder);
                s.set_value(value, window, cx);
                s
            })
        };
        let search = input("Search charts in your helm repositories (e.g. redis)", String::new(), window, cx);
        let version = input("version", String::new(), window, cx);
        let release = input("release name", String::new(), window, cx);
        let namespace = input("namespace", namespace.unwrap_or_else(|| "default".into()), window, cx);
        let values = cx.new(|cx| EditorState::new(window, cx).language("yaml").line_number(true).searchable(true));
        let subs = vec![cx.subscribe_in(&search, window, |this, _, ev: &InputEvent, _, cx| {
            if let InputEvent::PressEnter { .. } = ev {
                this.run_search(cx);
            }
        })];
        Self {
            context,
            search,
            results: None,
            chart: None,
            versions: Vec::new(),
            version,
            release,
            namespace,
            create_ns: false,
            values,
            output: None,
            busy: false,
            run_id: 0,
            _search: None,
            _pick: None,
            _tasks: Vec::new(),
            _subs: subs,
        }
    }

    fn run_search(&mut self, cx: &mut Context<Self>) {
        let q = self.search.read(cx).value().trim().to_string();
        if q.is_empty() {
            return;
        }
        self.results = None;
        let context = self.context.clone();
        let task = cx.spawn(async move |this, cx| {
            let r =
                k8s::helm(context, vec!["search".into(), "repo".into(), "-o".into(), "json".into(), "--".into(), q])
                    .await;
            let parsed = r
                .map_err(|e| format!("{e:#}"))
                .and_then(|out| serde_json::from_str::<Vec<Chart>>(&out).map_err(|e| e.to_string()));
            let _ = this.update(cx, |this, cx| {
                this.results = Some(parsed);
                cx.notify();
            });
        });
        self._search = Some(task);
        cx.notify();
    }

    fn pick(&mut self, chart: Chart, window: &mut Window, cx: &mut Context<Self>) {
        let base = chart.name.rsplit('/').next().unwrap_or(&chart.name).to_string();
        self.version.update(cx, |s, cx| s.set_value(chart.version.clone(), window, cx));
        self.release.update(cx, |s, cx| s.set_value(base, window, cx));
        self.values.update(cx, |s, cx| s.set_value(LOADING_VALUES, window, cx));
        self.output = None;
        // A run still going is for the chart picked before.
        self.busy = false;
        self.run_id += 1;
        self.versions.clear();
        self.chart = Some(chart.clone());
        let context = self.context.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let versions = k8s::helm(
                context.clone(),
                vec![
                    "search".into(),
                    "repo".into(),
                    "--versions".into(),
                    "-o".into(),
                    "json".into(),
                    "--".into(),
                    chart.name.clone(),
                ],
            )
            .await;
            let values = k8s::helm(
                context,
                vec![
                    "show".into(),
                    "values".into(),
                    "--version".into(),
                    chart.version.clone(),
                    "--".into(),
                    chart.name.clone(),
                ],
            )
            .await;
            let _ = this.update_in(cx, |this, window, cx| {
                // Only for the chart and version still picked.
                if this.chart.as_ref().is_none_or(|c| c.name != chart.name || c.version != chart.version) {
                    return;
                }
                this.versions = versions
                    .ok()
                    .and_then(|o| serde_json::from_str::<Vec<Chart>>(&o).ok())
                    .map(|v| v.into_iter().filter(|c| c.name == chart.name).map(|c| c.version).take(12).collect())
                    .unwrap_or_default();
                // Values typed while loading are kept.
                if this.values.read(cx).value() == LOADING_VALUES {
                    let text = values.unwrap_or_else(|e| format!("# could not read the chart's values: {e}"));
                    this.values.update(cx, |s, cx| s.set_value(text, window, cx));
                } else if let Err(e) = values {
                    window
                        .push_notification(Notification::error(format!("Could not read the chart's values: {e}")), cx);
                }
                cx.notify();
            });
        });
        self._pick = Some(task);
        cx.notify();
    }

    /// `helm install` arguments, reading the values from `values_file`.
    fn args(&self, cx: &App, dry_run: bool, values_file: &str) -> Option<Vec<String>> {
        let chart = self.chart.as_ref()?;
        let name = self.release.read(cx).value().trim().to_string();
        let ns = self.namespace.read(cx).value().trim().to_string();
        let version = self.version.read(cx).value().trim().to_string();
        if name.is_empty() || ns.is_empty() {
            return None;
        }
        let mut args = vec!["install".into(), "-n".into(), ns, "-f".into(), values_file.to_string()];
        if !version.is_empty() {
            args.extend(["--version".into(), version]);
        }
        if self.create_ns {
            args.push("--create-namespace".into());
        }
        if dry_run {
            args.push("--dry-run=server".into());
        }
        // `--`: a release or chart name starting with '-' is never read as a flag.
        args.extend(["--".into(), name, chart.name.clone()]);
        Some(args)
    }

    fn run(&mut self, dry_run: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.args(cx, dry_run, "").is_none() {
            window.push_notification(Notification::error("Pick a chart, a release name and a namespace"), cx);
            return;
        }
        // A fresh private directory, removed once helm is done: the values may hold secrets.
        let values = self.values.read(cx).value().to_string();
        let (file, dir) =
            match crate::private_files::PrivateDir::create().and_then(|d| Ok((d.write("values.yaml", &values)?, d))) {
                Ok(written) => written,
                Err(e) => {
                    window.push_notification(Notification::error(format!("Could not write values: {e}")), cx);
                    return;
                }
            };
        let Some(args) = self.args(cx, dry_run, &file.display().to_string()) else { return };
        self.busy = true;
        self.output = None;
        self.run_id += 1;
        let id = self.run_id;
        let release = self.release.read(cx).value().trim().to_string();
        let context = self.context.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let r = k8s::helm(context, args).await;
            drop(dir);
            let _ = this.update_in(cx, |this, window, cx| {
                if this.run_id != id {
                    // Another chart is picked: only an install is worth telling about.
                    if !dry_run {
                        let note = match r {
                            Ok(_) => Notification::success(format!("Release {release} installed")),
                            Err(e) => Notification::error(format!("Release {release} not installed: {e:#}")),
                        };
                        window.push_notification(note, cx);
                    }
                    return;
                }
                this.busy = false;
                match r {
                    Ok(out) => {
                        // Dry runs print every manifest; keep the summary.
                        let summary: String =
                            if dry_run { out.split("\nMANIFEST:").next().unwrap_or(&out).to_string() } else { out };
                        if !dry_run {
                            window.push_notification(Notification::success("Chart installed"), cx);
                        }
                        this.output = Some((true, summary));
                    }
                    Err(e) => this.output = Some((false, format!("{e:#}"))),
                }
                cx.notify();
            });
        });
        self._tasks.push(task);
        cx.notify();
    }

    fn confirm_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !crate::details::ensure_writes(window, cx) {
            return;
        }
        let Some(args) = self.args(cx, false, "values.yaml") else { return };
        let context = self.context.clone();
        let protected = crate::settings::is_protected(&context);
        let guard =
            protected.then(|| (cx.new(|cx| InputState::new(window, cx).placeholder(context.clone())), context.clone()));
        let cmd = format!("helm --kube-context {context} {}", args.join(" "));
        let view = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (guard, view) = (guard.clone(), view.clone());
            alert
                .title("Install this chart?")
                .description("Helm creates every object of the chart in the cluster.")
                .child(crate::details::confirm_extras(&cmd, guard.as_ref(), cx))
                .show_cancel(true)
                .ok_text("Install")
                .on_ok(move |_, window, cx| {
                    if !crate::details::guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    let _ = view.update(cx, |this, cx| this.run(false, window, cx));
                    true
                })
        });
    }
}

#[cfg(feature = "dev")]
impl HelmInstall {
    /// Search, pick the first result, and optionally dry-run it (never installs).
    pub fn demo(&mut self, query: &str, dry_run: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.search.update(cx, |s, cx| s.set_value(query.to_string(), window, cx));
        self.run_search(cx);
        let task = cx.spawn_in(window, async move |this, cx| {
            for _ in 0..100 {
                cx.background_executor().timer(std::time::Duration::from_millis(200)).await;
                let first = this
                    .update(cx, |this, _| {
                        this.results.as_ref().and_then(|r| r.as_ref().ok()).and_then(|c| c.first().cloned())
                    })
                    .ok()
                    .flatten();
                if let Some(chart) = first {
                    let _ = this.update_in(cx, |this, window, cx| this.pick(chart, window, cx));
                    if dry_run {
                        cx.background_executor().timer(std::time::Duration::from_secs(4)).await;
                        let _ = this.update_in(cx, |this, window, cx| this.run(true, window, cx));
                    }
                    break;
                }
            }
        });
        self._tasks.push(task);
    }
}

impl Render for HelmInstall {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let list: AnyElement = match &self.results {
            None if self.search.read(cx).value().is_empty() => div()
                .p_2()
                .text_sm()
                .text_color(muted)
                .child("Type a name and press Enter. Charts come from `helm repo list`.")
                .into_any_element(),
            None => div().p_2().text_sm().text_color(muted).child("Searching…").into_any_element(),
            Some(Err(e)) => div().p_2().text_sm().text_color(theme.danger).child(e.clone()).into_any_element(),
            Some(Ok(charts)) if charts.is_empty() => {
                div().p_2().text_sm().text_color(muted).child("No chart found").into_any_element()
            }
            Some(Ok(charts)) => v_flex()
                .id("charts")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .children(charts.iter().enumerate().map(|(i, c)| {
                    let chart = c.clone();
                    let selected = self.chart.as_ref().is_some_and(|x| x.name == c.name);
                    v_flex()
                        .id(("chart", i))
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .cursor_pointer()
                        .when(selected, |d| d.bg(theme.blue.opacity(0.08)))
                        .hover(|s| s.bg(theme.list_hover))
                        .on_click(cx.listener(move |this, _, window, cx| this.pick(chart.clone(), window, cx)))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().text_sm().font_weight(FontWeight::MEDIUM).child(c.name.clone()))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(muted)
                                        .child(format!("{} · app {}", c.version, c.app_version)),
                                ),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(c.description.clone()),
                        )
                }))
                .into_any_element(),
        };
        let form: AnyElement = match &self.chart {
            None => div().p_6().text_color(muted).child("Pick a chart on the left.").into_any_element(),
            Some(chart) => v_flex()
                .size_full()
                .gap_2()
                .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(chart.name.clone()))
                .child(
                    h_flex()
                        .gap_2()
                        .child(
                            v_flex()
                                .w(px(220.))
                                .gap_0p5()
                                .child(div().text_xs().text_color(muted).child("Release name"))
                                .child(Input::new(&self.release).small()),
                        )
                        .child(
                            v_flex()
                                .w(px(200.))
                                .gap_0p5()
                                .child(div().text_xs().text_color(muted).child("Namespace"))
                                .child(Input::new(&self.namespace).small()),
                        )
                        .child(
                            v_flex()
                                .w(px(140.))
                                .gap_0p5()
                                .child(div().text_xs().text_color(muted).child("Version"))
                                .child(Input::new(&self.version).small()),
                        )
                        .child(div().pt_4().child(
                            Checkbox::new("create-ns").label("Create namespace").checked(self.create_ns).on_click(
                                cx.listener(|this, v: &bool, _, cx| {
                                    this.create_ns = *v;
                                    cx.notify();
                                }),
                            ),
                        )),
                )
                .when(!self.versions.is_empty(), |d| {
                    d.child(div().text_xs().text_color(muted).child(format!("Versions: {}", self.versions.join(", "))))
                })
                .child(div().text_xs().text_color(muted).child("Values (edit before installing)"))
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .border_1()
                        .border_color(theme.border)
                        .rounded_md()
                        .child(Editor::new(&self.values).h_full()),
                )
                .when_some(self.output.clone(), |d, (ok, text)| {
                    d.child(
                        div()
                            .id("helm-output")
                            .max_h(px(180.))
                            .overflow_y_scroll()
                            .p_2()
                            .rounded_md()
                            .bg(if ok { theme.success.opacity(0.08) } else { theme.danger.opacity(0.08) })
                            .font_family(theme.mono_font_family.clone())
                            .text_xs()
                            .child(text),
                    )
                })
                .child(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1())
                        .child(
                            Button::new("helm-dry")
                                .label(if self.busy { "Running…" } else { "Dry run" })
                                .disabled(self.busy)
                                .on_click(cx.listener(|this, _, window, cx| this.run(true, window, cx))),
                        )
                        .child(
                            Button::new("helm-install")
                                .primary()
                                .icon(Lucide::Download)
                                .label("Install")
                                .disabled(self.busy)
                                .on_click(cx.listener(|this, _, window, cx| this.confirm_install(window, cx))),
                        ),
                )
                .into_any_element(),
        };
        v_flex()
            .size_full()
            .p_3()
            .gap_3()
            .child(
                h_flex()
                    .gap_3()
                    .child(Icon::new(Lucide::Ship).size_5().text_color(theme.blue))
                    .child(div().text_xl().font_weight(FontWeight::MEDIUM).child("Install a chart"))
                    .child(div().text_sm().text_color(muted).child(format!("on {} · via your helm CLI", self.context))),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .items_start()
                    .child(
                        v_flex()
                            .w(px(380.))
                            .h_full()
                            .gap_2()
                            .child(Input::new(&self.search).cleanable(true))
                            .child(list),
                    )
                    .child(div().flex_1().h_full().child(form)),
            )
    }
}
