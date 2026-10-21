//! Helm releases, read from Helm's own storage (Secrets labelled owner=helm).

use std::io::Read as _;

use anyhow::{Context as _, Result};
use base64::Engine as _;
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use k8s_openapi::api::core::v1::Secret;
use kube::Client;
use kube::api::{Api, ListParams};
use serde_json::Value;

use crate::k8s;

#[derive(Clone)]
pub struct Release {
    pub context: String,
    pub name: String,
    pub namespace: String,
    pub revision: i64,
    pub status: String,
    pub chart: String,
    pub chart_version: String,
    pub app_version: String,
    pub updated: Option<jiff::Timestamp>,
    pub description: String,
    pub notes: String,
    pub values: Value,
    pub manifest: String,
}

/// Helm keeps a release revision in one Secret (at most ~1 MiB); more is not Helm's.
const MAX_RELEASE_DATA: usize = 4 << 20;
/// Decompressed release JSON: generous for huge charts, small enough that a gzip bomb fails.
const MAX_RELEASE_JSON: u64 = 32 << 20;

/// Whether `name` can be a Helm release name (also a valid label value): lowercase
/// letters, digits, '-' and '.', at most 53 characters.
fn valid_release_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 53
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
}

/// Secret data -> base64 (Helm's own) -> gzip -> JSON.
///
/// The release's identity comes from the Secret itself (its namespace, its `name` label and
/// Helm's own Secret name), never from the payload: anyone who can create a Secret in their
/// namespace could otherwise label it `owner=helm` and claim to be `kube-system/cilium`.
fn decode(secret: &Secret) -> Result<Release> {
    let meta = &secret.metadata;
    let namespace = meta.namespace.clone().context("no namespace")?;
    let label = |k: &str| meta.labels.as_ref().and_then(|l| l.get(k)).cloned();
    let name = label("name").filter(|n| valid_release_name(n)).context("no valid release name label")?;
    let raw = secret.data.as_ref().and_then(|d| d.get("release")).context("no release data")?;
    anyhow::ensure!(raw.0.len() <= MAX_RELEASE_DATA, "release data too large");
    let inner = base64::engine::general_purpose::STANDARD.decode(&raw.0)?;
    let mut json = Vec::new();
    if inner.starts_with(&[0x1f, 0x8b]) {
        // One byte past the cap tells "too large" from "exactly the cap".
        flate2::read::GzDecoder::new(&inner[..]).take(MAX_RELEASE_JSON + 1).read_to_end(&mut json)?;
        anyhow::ensure!(json.len() as u64 <= MAX_RELEASE_JSON, "release data decompresses to more than 32 MiB");
    } else {
        json = inner;
    }
    let r: Value = serde_json::from_slice(&json)?;
    let s = |p: &str| r.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string();
    let revision = r["version"].as_i64().unwrap_or(0);
    // A payload naming another release, or a Secret not named the way Helm names it, is not Helm's.
    anyhow::ensure!(s("/name") == name && s("/namespace") == namespace, "release payload names another release");
    anyhow::ensure!(
        meta.name.as_deref() == Some(format!("sh.helm.release.v1.{name}.v{revision}").as_str()),
        "not a Helm release Secret name"
    );
    if let Some(v) = label("version") {
        anyhow::ensure!(v == revision.to_string(), "revision label and payload disagree");
    }
    Ok(Release {
        context: String::new(),
        name,
        namespace,
        revision,
        status: s("/info/status"),
        chart: s("/chart/metadata/name"),
        chart_version: s("/chart/metadata/version"),
        app_version: s("/chart/metadata/appVersion"),
        updated: r.pointer("/info/last_deployed").and_then(Value::as_str).and_then(|t| t.parse().ok()),
        description: s("/info/description"),
        notes: s("/info/notes"),
        values: r.get("config").cloned().unwrap_or(Value::Null),
        manifest: s("/manifest"),
    })
}

async fn secrets(client: Client, namespace: Option<String>, labels: String) -> Result<Vec<Secret>> {
    k8s::run(async move {
        let api: Api<Secret> = match &namespace {
            Some(ns) => Api::namespaced(client, ns),
            None => Api::all(client),
        };
        Ok(api.list(&ListParams::default().labels(&labels)).await?.items)
    })
    .await
}

/// The current revision of every release (one Secret each, not the whole history).
pub async fn list_releases(client: Client, namespace: Option<String>) -> Result<Vec<Release>> {
    let items = secrets(client, namespace, "owner=helm,status!=superseded".into()).await?;
    Ok(latest_revisions(items.iter().filter_map(|s| decode(s).ok()).collect()))
}

/// One release per (namespace, name): the newest revision, when an upgrade is in flight.
fn latest_revisions(mut releases: Vec<Release>) -> Vec<Release> {
    releases.sort_by(|a, b| {
        (&a.namespace, &a.name, std::cmp::Reverse(a.revision)).cmp(&(
            &b.namespace,
            &b.name,
            std::cmp::Reverse(b.revision),
        ))
    });
    releases.dedup_by(|a, b| a.namespace == b.namespace && a.name == b.name);
    releases
}

pub async fn history(client: Client, namespace: String, name: String) -> Result<Vec<Release>> {
    // The name went through `decode`, but this builds a selector: check it again.
    anyhow::ensure!(valid_release_name(&name), "invalid release name {name:?}");
    let items = secrets(client, Some(namespace.clone()), format!("owner=helm,name={name}")).await?;
    let mut revs: Vec<Release> =
        items.iter().filter_map(|s| decode(s).ok()).filter(|r| r.name == name && r.namespace == namespace).collect();
    revs.sort_by_key(|r| std::cmp::Reverse(r.revision));
    Ok(revs)
}

/// Values as YAML; secret-looking keys are masked unless revealed.
fn values_yaml(values: &Value, reveal: bool) -> String {
    if values.is_null() {
        return "# no user-supplied values\n".into();
    }
    let mut v = values.clone();
    if !reveal {
        mask_secrets(&mut v);
    }
    crate::yaml::to_string(&v).unwrap_or_default()
}

fn mask_secrets(v: &mut Value) {
    const HINTS: [&str; 9] =
        ["password", "passwd", "secret", "token", "apikey", "api_key", "credential", "privatekey", "private_key"];
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                let key = k.to_lowercase().replace('-', "_");
                let secretish =
                    HINTS.iter().any(|h| key.contains(h)) || key.ends_with("key") && !key.ends_with("publickey");
                if secretish && (val.is_string() || val.is_number()) {
                    *val = Value::String("••••••".into());
                } else {
                    mask_secrets(val);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(mask_secrets),
        _ => {}
    }
}

/// The rendered manifest with the values of its Secrets masked (keys stay visible); other
/// documents, and the `# Source:` comments, are kept as they are.
fn mask_manifest(manifest: &str) -> String {
    let mut out = String::with_capacity(manifest.len());
    let mut doc = String::new();
    let flush = |doc: &mut String, out: &mut String| {
        out.push_str(&mask_manifest_doc(doc));
        doc.clear();
    };
    for line in manifest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            flush(&mut doc, &mut out);
            out.push_str(line);
        } else {
            doc.push_str(line);
        }
    }
    flush(&mut doc, &mut out);
    out
}

fn mask_manifest_doc(doc: &str) -> String {
    if !doc.contains("Secret") {
        return doc.to_string();
    }
    let comments: String =
        doc.lines().take_while(|l| l.trim_start().starts_with('#')).map(|l| format!("{l}\n")).collect();
    match crate::yaml::from_str::<Value>(doc) {
        Ok(mut v) if v["kind"] == "Secret" => {
            k8s::mask_secret_values(&mut v);
            comments + &crate::yaml::to_string(&v).unwrap_or_default()
        }
        Ok(_) => doc.to_string(),
        // Unreadable, but it may be a Secret: hide it rather than show its values.
        Err(_) if doc.lines().any(|l| l.trim() == "kind: Secret") => {
            comments + "# A Secret (its values are hidden; reveal secrets to see it)\n"
        }
        Err(_) => doc.to_string(),
    }
}

fn status_color(status: &str, cx: &App) -> Hsla {
    let t = cx.theme();
    match status {
        "deployed" => t.success,
        "failed" => t.danger,
        "superseded" | "uninstalled" => t.muted_foreground,
        _ => t.warning,
    }
}

// MARK: List

pub struct OpenRelease(pub Release);

/// The "Install a chart" button was clicked.
pub struct OpenInstall;

pub struct HelmList {
    multi: bool,
    releases: Option<Result<Vec<Release>, SharedString>>,
    filter: Entity<InputState>,
    _task: Task<()>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenRelease> for HelmList {}
impl EventEmitter<OpenInstall> for HelmList {}

impl HelmList {
    pub fn new(
        clusters: Vec<(String, Client)>,
        namespace: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multi = clusters.len() > 1;
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter releases…"));
        let subs = vec![cx.subscribe(&filter, |_, _, ev: &InputEvent, cx| {
            if let InputEvent::Change = ev {
                cx.notify();
            }
        })];
        let task = cx.spawn(async move |this, cx| {
            let loads = clusters.iter().map(|(ctx, client)| {
                let (ctx, client, ns) = (ctx.clone(), client.clone(), namespace.clone());
                async move {
                    let mut rels = list_releases(client, ns).await?;
                    rels.iter_mut().for_each(|r| r.context = ctx.clone());
                    anyhow::Ok(rels)
                }
            });
            let result = futures::future::join_all(loads)
                .await
                .into_iter()
                .collect::<anyhow::Result<Vec<_>>>()
                .map(|v| v.into_iter().flatten().collect())
                .map_err(|e| SharedString::from(format!("{e:#}")));
            let _ = this.update(cx, |this, cx| {
                this.releases = Some(result);
                cx.notify();
            });
        });
        Self { multi, releases: None, filter, _task: task, _subs: subs }
    }
}

impl Render for HelmList {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let query = self.filter.read(cx).value().to_lowercase();
        let body: AnyElement = match &self.releases {
            None => div().p_4().text_color(theme.muted_foreground).child("Loading releases…").into_any_element(),
            Some(Err(e)) => div().p_4().text_color(theme.danger).child(e.clone()).into_any_element(),
            Some(Ok(releases)) => {
                let rows: Vec<Release> = releases
                    .iter()
                    .filter(|r| {
                        query.is_empty()
                            || r.name.to_lowercase().contains(&query)
                            || r.namespace.contains(&query)
                            || r.chart.to_lowercase().contains(&query)
                    })
                    .cloned()
                    .collect();
                let header = h_flex()
                    .gap_3()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .border_b_1()
                    .border_color(theme.border)
                    .when(self.multi, |d| d.child(div().w(px(160.)).child("Context")))
                    .child(div().w(px(200.)).child("Namespace"))
                    .child(div().w(px(280.)).child("Name"))
                    .child(div().w(px(70.)).child("Revision"))
                    .child(div().w(px(220.)).child("Chart"))
                    .child(div().w(px(140.)).child("App version"))
                    .child(div().w(px(100.)).child("Status"))
                    .child(div().w(px(80.)).child("Updated"));
                let count = rows.len();
                let multi = self.multi;
                let list = rows.into_iter().enumerate().map(|(i, r)| {
                    let open = r.clone();
                    h_flex()
                        .id(("release", i))
                        .gap_3()
                        .px_2()
                        .py_1()
                        .text_sm()
                        .border_b_1()
                        .border_color(theme.border)
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.list_hover))
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(OpenRelease(open.clone()))))
                        .when(multi, |d| {
                            d.child(
                                div()
                                    .w(px(160.))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .whitespace_nowrap()
                                    .child(r.context.clone()),
                            )
                        })
                        .child(
                            div()
                                .w(px(200.))
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(r.namespace.clone()),
                        )
                        .child(
                            div()
                                .w(px(280.))
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_color(theme.blue)
                                .child(r.name.clone()),
                        )
                        .child(div().w(px(70.)).child(r.revision.to_string()))
                        .child(
                            div()
                                .w(px(220.))
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(format!("{}-{}", r.chart, r.chart_version)),
                        )
                        .child(
                            div()
                                .w(px(140.))
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .child(r.app_version.clone()),
                        )
                        .child(div().w(px(100.)).text_color(status_color(&r.status, cx)).child(r.status.clone()))
                        .child(
                            div()
                                .w(px(80.))
                                .text_color(theme.muted_foreground)
                                .child(r.updated.map(k8s::age).unwrap_or_default()),
                        )
                });
                v_flex()
                    .size_full()
                    .child(
                        h_flex()
                            .gap_2()
                            .px_2()
                            .pb_2()
                            .text_sm()
                            .child(div().size_2().rounded_full().bg(theme.success))
                            .child(format!("{count} Helm releases")),
                    )
                    .child(header)
                    .child(v_flex().id("releases").flex_1().min_h_0().overflow_y_scroll().children(list))
                    .into_any_element()
            }
        };
        v_flex()
            .size_full()
            .p_2()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(320.)).child(Input::new(&self.filter).small().cleanable(true)))
                    .child(div().flex_1())
                    .child(
                        Button::new("helm-install-open")
                            .small()
                            .primary()
                            .icon(Lucide::Download)
                            .label("Install a chart")
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(OpenInstall))),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
    }
}

// MARK: Release page

#[derive(Clone, Copy, PartialEq)]
enum ReleaseTab {
    Overview,
    Values,
    Manifest,
    History,
}

pub struct HelmRelease {
    client: Client,
    context: String,
    release: Release,
    tab: ReleaseTab,
    reveal: bool,
    values: Entity<EditorState>,
    manifest: Entity<EditorState>,
    history: Option<Result<Vec<Release>, SharedString>>,
    /// Several repositories have this chart: the user picks one (for a dry run or not).
    chart_choice: Option<(bool, Vec<String>)>,
    /// The values text captured when the upgrade started (revealed, never the masked view).
    upgrade_text: Option<String>,
    _task: Task<()>,
}

/// The edited values to upgrade with, or None while secrets are masked: the masked view
/// holds `••••••` placeholders, which Helm would store as the real values.
fn upgrade_text(reveal: bool, edited: &str, values: &Value) -> Option<String> {
    let mask = "••••••";
    let placeholder = edited.contains(mask) && !values_yaml(values, true).contains(mask);
    (reveal && !placeholder).then(|| edited.to_string())
}

/// The chart references (`repo/chart`) from `helm search repo -o json` naming exactly `chart`.
fn chart_refs(search_json: &str, chart: &str) -> Vec<String> {
    let suffix = format!("/{chart}");
    let mut refs: Vec<String> = serde_json::from_str::<Vec<Value>>(search_json)
        .unwrap_or_default()
        .iter()
        .filter_map(|c| c["name"].as_str())
        .filter(|n| n.ends_with(&suffix))
        .map(String::from)
        .collect();
    refs.sort();
    refs.dedup();
    refs
}

impl HelmRelease {
    pub fn new(client: Client, context: String, release: Release, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = |text: String, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut s = EditorState::new(window, cx).language("yaml").line_number(true).searchable(true);
                s.set_value(text, window, cx);
                s
            })
        };
        let values = editor(values_yaml(&release.values, false), window, cx);
        let manifest = editor(mask_manifest(&release.manifest), window, cx);
        let (ns, name, c2) = (release.namespace.clone(), release.name.clone(), client.clone());
        let task = cx.spawn(async move |this, cx| {
            let result = history(c2, ns, name).await;
            let _ = this.update(cx, |this, cx| {
                this.history = Some(result.map_err(|e| format!("{e:#}").into()));
                cx.notify();
            });
        });
        Self {
            client,
            context,
            release,
            tab: ReleaseTab::Overview,
            reveal: false,
            values,
            manifest,
            history: None,
            chart_choice: None,
            upgrade_text: None,
            _task: task,
        }
    }

    fn reload_history(&mut self, cx: &mut Context<Self>) {
        let (client, ns, name) = (self.client.clone(), self.release.namespace.clone(), self.release.name.clone());
        self._task = cx.spawn(async move |this, cx| {
            let result = history(client, ns, name).await;
            let _ = this.update(cx, |this, cx| {
                this.history = Some(result.map_err(|e| format!("{e:#}").into()));
                cx.notify();
            });
        });
    }

    /// Show or mask secret values, in the values and in the rendered Secrets of the manifest.
    fn set_reveal(&mut self, reveal: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.reveal = reveal;
        let values = values_yaml(&self.release.values, reveal);
        self.values.update(cx, |s, cx| s.set_value(values, window, cx));
        let manifest = if reveal { self.release.manifest.clone() } else { mask_manifest(&self.release.manifest) };
        self.manifest.update(cx, |s, cx| s.set_value(manifest, window, cx));
        cx.notify();
    }

    fn reveal_button(&self, id: &'static str, cx: &mut Context<Self>) -> Button {
        Button::new(id)
            .xsmall()
            .ghost()
            .icon(if self.reveal { Lucide::EyeOff } else { Lucide::Eye })
            .label(if self.reveal { "Mask secrets" } else { "Reveal secrets" })
            .on_click(cx.listener(|this, _, window, cx| this.set_reveal(!this.reveal, window, cx)))
    }

    /// Upgrade (or dry run) this release with the edited values, same chart and version:
    /// find the chart in your helm repositories first. When several repositories have it,
    /// the user picks one; a real upgrade is confirmed showing the chart reference used.
    fn start_upgrade(&mut self, dry_run: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !dry_run && !crate::details::ensure_writes(window, cx) {
            return;
        }
        // The values are taken now: masking them again while the chart is looked up must
        // not send the `••••••` placeholders to the cluster.
        let edited = self.values.read(cx).value().to_string();
        let Some(text) = upgrade_text(self.reveal, &edited, &self.release.values) else {
            window.push_notification(Notification::error("Reveal the secrets before upgrading"), cx);
            return;
        };
        self.upgrade_text = Some(text);
        self.chart_choice = None;
        let (context, chart, version) =
            (self.context.clone(), self.release.chart.clone(), self.release.chart_version.clone());
        cx.spawn_in(window, async move |this, cx| {
            // Plain search, then an exact name match (helm's -r regex also sees descriptions).
            let found = k8s::helm(
                context,
                // `--` everywhere: names from the cluster are never read as flags.
                vec![
                    "search".into(),
                    "repo".into(),
                    "--version".into(),
                    version.clone(),
                    "-o".into(),
                    "json".into(),
                    "--".into(),
                    chart.clone(),
                ],
            )
            .await;
            let refs = found.map(|o| chart_refs(&o, &chart));
            let _ = this.update_in(cx, |this, window, cx| match refs {
                Err(e) => window.push_notification(Notification::error(format!("helm search failed: {e:#}")), cx),
                Ok(refs) => match refs.as_slice() {
                    [] => window.push_notification(
                        Notification::error(format!(
                            "chart {chart} {version} is not in your helm repositories (add it with `helm repo add`)"
                        )),
                        cx,
                    ),
                    [one] => this.chosen_chart(one.clone(), dry_run, window, cx),
                    _ => {
                        this.chart_choice = Some((dry_run, refs));
                        cx.notify();
                    }
                },
            });
        })
        .detach();
    }

    fn chosen_chart(&mut self, chart_ref: String, dry_run: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.chart_choice = None;
        cx.notify();
        if dry_run {
            self.upgrade(chart_ref, true, window, cx);
        } else {
            self.confirm_upgrade(chart_ref, window, cx);
        }
    }

    fn confirm_upgrade(&mut self, chart_ref: String, window: &mut Window, cx: &mut Context<Self>) {
        let (name, ns) = (self.release.name.clone(), self.release.namespace.clone());
        let view = cx.entity().downgrade();
        let context = self.context.clone();
        let protected = crate::settings::is_protected(&context);
        let guard =
            protected.then(|| (cx.new(|cx| InputState::new(window, cx).placeholder(context.clone())), context.clone()));
        let cmd = format!(
            "helm --kube-context {context} upgrade --version {} -n {ns} -f values.yaml -- {name} {chart_ref}",
            self.release.chart_version
        );
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (view, guard, chart_ref) = (view.clone(), guard.clone(), chart_ref.clone());
            alert
                .title(format!("Upgrade {name} with the edited values?"))
                .description(
                    "Same chart and version; Helm applies the new values as a new revision (you can roll back). \
                     If CI or GitOps (helmfile, Flux, Argo CD) deploys this release, its next run will overwrite this change.",
                )
                .child(crate::details::confirm_extras(&cmd, guard.as_ref(), cx))
                .show_cancel(true)
                .ok_text("Upgrade")
                .on_ok(move |_, window, cx| {
                    if !crate::details::guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    let chart_ref = chart_ref.clone();
                    let _ = view.update(cx, |this, cx| this.upgrade(chart_ref, false, window, cx));
                    true
                })
        });
    }

    /// `helm upgrade` this release with the edited values from `chart_ref`, same version.
    fn upgrade(&mut self, chart_ref: String, dry_run: bool, window: &mut Window, cx: &mut Context<Self>) {
        // Captured by `start_upgrade`, so what was revealed then is what is sent.
        let Some(values) = self.upgrade_text.clone() else {
            window.push_notification(Notification::error("Reveal the secrets before upgrading"), cx);
            return;
        };
        let (context, name, ns, version) = (
            self.context.clone(),
            self.release.name.clone(),
            self.release.namespace.clone(),
            self.release.chart_version.clone(),
        );
        // A fresh private directory: the values may hold secrets.
        let written =
            crate::private_files::PrivateDir::create().and_then(|d| Ok((d.write("values.yaml", &values)?, d)));
        let (file, dir) = match written {
            Ok(w) => w,
            Err(e) => {
                window.push_notification(Notification::error(format!("Could not write values: {e}")), cx);
                return;
            }
        };
        window.push_notification(Notification::info(if dry_run { "Dry run upgrade…" } else { "Upgrading…" }), cx);
        cx.spawn_in(window, async move |this, cx| {
            let mut args = vec![
                "upgrade".into(),
                "--version".into(),
                version,
                "-n".into(),
                ns,
                "-f".into(),
                file.display().to_string(),
            ];
            if dry_run {
                args.push("--dry-run=server".into());
            }
            args.extend(["--".into(), name, chart_ref]);
            let result = k8s::helm(context, args).await;
            drop(dir);
            let _ = this.update_in(cx, |this, window, cx| match result {
                Ok(out) if dry_run => {
                    let status = out
                        .lines()
                        .find(|l| l.starts_with("STATUS:") || l.starts_with("DESCRIPTION:"))
                        .unwrap_or("ok")
                        .to_string();
                    window.push_notification(Notification::success(format!("Dry run upgrade: {status}")), cx)
                }
                Ok(_) => {
                    window.push_notification(Notification::success("Upgraded"), cx);
                    this.reload_history(cx);
                }
                Err(e) => window.push_notification(Notification::error(format!("helm upgrade failed: {e:#}")), cx),
            });
        })
        .detach();
    }

    /// Confirm, then run `helm <args>` against this release's context.
    fn confirm_helm(
        &mut self,
        title: String,
        description: &'static str,
        ok: &'static str,
        args: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !crate::details::ensure_writes(window, cx) {
            return;
        }
        let context = self.context.clone();
        let protected = crate::settings::is_protected(&context);
        let guard =
            protected.then(|| (cx.new(|cx| InputState::new(window, cx).placeholder(context.clone())), context.clone()));
        let cmd = format!("helm --kube-context {context} {}", args.join(" "));
        let view = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (guard, view, args, context) = (guard.clone(), view.clone(), args.clone(), context.clone());
            alert
                .title(title.clone())
                .description(description)
                .child(crate::details::confirm_extras(&cmd, guard.as_ref(), cx))
                .show_cancel(true)
                .ok_text(ok)
                .when(ok == "Uninstall", |a| a.ok_variant(component::button::ButtonVariant::Danger))
                .on_ok(move |_, window, cx| {
                    if !crate::details::guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    let (args, context) = (args.clone(), context.clone());
                    let _ = view.update(cx, |_, cx| {
                        cx.spawn_in(window, async move |this, cx| {
                            let result = k8s::helm(context, args).await;
                            let _ = this.update_in(cx, |this, window, cx| {
                                match result {
                                    Ok(out) => window.push_notification(
                                        Notification::success(out.lines().last().unwrap_or("done").to_string()),
                                        cx,
                                    ),
                                    Err(e) => {
                                        window.push_notification(Notification::error(format!("helm failed: {e:#}")), cx)
                                    }
                                }
                                this.reload_history(cx);
                            });
                        })
                        .detach();
                    });
                    true
                })
        });
    }

    fn field(label: &str, value: impl IntoElement, cx: &App) -> Div {
        v_flex()
            .min_w(px(160.))
            .gap_0p5()
            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(label.to_string()))
            .child(div().text_sm().child(value))
    }
}

impl Render for HelmRelease {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let current = self
            .history
            .as_ref()
            .and_then(|h| h.as_ref().ok())
            .and_then(|h| h.iter().map(|r| r.revision).max())
            .unwrap_or(self.release.revision);
        let r = &self.release;
        let tabs = [ReleaseTab::Overview, ReleaseTab::Values, ReleaseTab::Manifest, ReleaseTab::History];
        let selected = tabs.iter().position(|t| *t == self.tab).unwrap_or(0);
        let body: AnyElement = match self.tab {
            ReleaseTab::Overview => v_flex()
                .id("helm-overview")
                .size_full()
                .overflow_y_scroll()
                .p_2()
                .gap_4()
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_x_8()
                        .gap_y_4()
                        .child(Self::field("Status", div().text_color(status_color(&r.status, cx)).child(r.status.clone()), cx))
                        .child(Self::field("Revision", r.revision.to_string(), cx))
                        .child(Self::field("Chart", format!("{} {}", r.chart, r.chart_version), cx))
                        .child(Self::field("App version", r.app_version.clone(), cx))
                        .child(Self::field("Updated", r.updated.map(|t| format!("{} ago", k8s::age(t))).unwrap_or_default(), cx)),
                )
                .child(Self::field("Description", r.description.clone(), cx))
                .when(!r.notes.is_empty(), |d| {
                    d.child(Self::field(
                        "Notes",
                        div()
                            .p_2()
                            .rounded_md()
                            .bg(theme.secondary)
                            .font_family(theme.mono_font_family.clone())
                            .text_xs()
                            .children(r.notes.lines().map(|l| div().child(l.to_string()))),
                        cx,
                    ))
                })
                .into_any_element(),
            ReleaseTab::Values => v_flex()
                .size_full()
                .gap_2()
                .child(
                    h_flex()
                        .gap_2()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(self.reveal_button("reveal-values", cx))
                        .when(!self.reveal, |d| d.child("Values of keys that look secret (password, token, key…) are masked. Reveal them to edit and upgrade."))
                        .when(self.reveal, |d| {
                            d.child(div().flex_1())
                                .child(Button::new("upgrade-dry").xsmall().ghost().label("Dry run upgrade").on_click(cx.listener(|this, _, window, cx| this.start_upgrade(true, window, cx))))
                                .child(Button::new("upgrade").xsmall().primary().icon(Lucide::CircleArrowUp).label("Upgrade with these values").on_click(cx.listener(|this, _, window, cx| this.start_upgrade(false, window, cx))))
                        }),
                )
                .when_some(self.chart_choice.clone(), |d, (dry_run, refs)| {
                    d.child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .text_xs()
                            .child(format!(
                                "Chart {} {} is in several repositories; upgrade from:",
                                self.release.chart, self.release.chart_version
                            ))
                            .children(refs.into_iter().enumerate().map(|(i, r)| {
                                Button::new(("chart-ref", i)).xsmall().outline().label(r.clone()).on_click(cx.listener(
                                    move |this, _, window, cx| this.chosen_chart(r.clone(), dry_run, window, cx),
                                ))
                            }))
                            .child(Button::new("chart-ref-cancel").xsmall().ghost().label("Cancel").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.chart_choice = None;
                                    cx.notify();
                                }),
                            )),
                    )
                })
                .child(div().flex_1().min_h_0().child(Editor::new(&self.values).readonly(!self.reveal).h_full()))
                .into_any_element(),
            ReleaseTab::Manifest => v_flex()
                .size_full()
                .gap_2()
                .when(self.release.manifest.contains("Secret"), |d| {
                    d.child(
                        h_flex()
                            .gap_2()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(self.reveal_button("reveal-manifest", cx))
                            .when(!self.reveal, |d| d.child("The values of the rendered Secrets are masked.")),
                    )
                })
                .child(div().flex_1().min_h_0().child(Editor::new(&self.manifest).readonly(true).h_full()))
                .into_any_element(),
            ReleaseTab::History => match &self.history {
                None => div().p_2().text_color(theme.muted_foreground).child("Loading history…").into_any_element(),
                Some(Err(e)) => div().p_2().text_color(theme.danger).child(e.clone()).into_any_element(),
                Some(Ok(revs)) => v_flex()
                    .p_2()
                    .child(
                        h_flex()
                            .gap_3()
                            .py_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .border_b_1()
                            .border_color(theme.border)
                            .child(div().w(px(70.)).child("Revision"))
                            .child(div().w(px(110.)).child("Status"))
                            .child(div().w(px(240.)).child("Chart"))
                            .child(div().w(px(140.)).child("App version"))
                            .child(div().w(px(80.)).child("Updated"))
                            .child(div().flex_1().child("Description")),
                    )
                    .children(revs.iter().map(|h| {
                        h_flex()
                            .gap_3()
                            .py_1()
                            .text_sm()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(div().w(px(70.)).child(h.revision.to_string()))
                            .child(div().w(px(110.)).text_color(status_color(&h.status, cx)).child(h.status.clone()))
                            .child(div().w(px(240.)).child(format!("{}-{}", h.chart, h.chart_version)))
                            .child(div().w(px(140.)).child(h.app_version.clone()))
                            .child(div().w(px(80.)).text_color(theme.muted_foreground).child(h.updated.map(k8s::age).unwrap_or_default()))
                            .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().child(h.description.clone()))
                            .child(div().w(px(110.)).when(h.revision != current, |d| {
                                let rev = h.revision;
                                d.child(Button::new(("rollback", rev as usize)).xsmall().ghost().icon(Lucide::Undo2).label("Roll back").on_click(cx.listener(move |this, _, window, cx| {
                                    let (name, ns) = (this.release.name.clone(), this.release.namespace.clone());
                                    this.confirm_helm(
                                        format!("Roll back {name} to revision {rev}?"),
                                        "Helm re-applies that revision's manifest and values as a new revision.",
                                        "Roll back",
                                        vec!["rollback".into(), "-n".into(), ns, "--wait=false".into(), "--".into(), name, rev.to_string()],
                                        window,
                                        cx,
                                    );
                                })))
                            }))
                    }))
                    .into_any_element(),
            },
        };
        let view = cx.entity().downgrade();
        v_flex()
            .size_full()
            .bg(theme.background)
            .child(
                v_flex()
                    .px_4()
                    .pt_3()
                    .pb_1()
                    .gap_0p5()
                    .child(
                        h_flex()
                            .gap_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(self.context.clone())
                            .child("·")
                            .child(div().text_color(theme.blue).child("Helm Release")),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .text_xl()
                            .child(Icon::new(Lucide::Ship).size_5())
                            .child(div().text_color(theme.muted_foreground).child(format!("{} / ", r.namespace)))
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(r.name.clone()))
                            .child(div().flex_1())
                            .child(
                                Button::new("helm-uninstall")
                                    .small()
                                    .ghost()
                                    .icon(Icon::new(Lucide::Trash).text_color(theme.danger))
                                    .label("Uninstall")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        let (name, ns) = (this.release.name.clone(), this.release.namespace.clone());
                                        this.confirm_helm(
                                            format!("Uninstall {name}?"),
                                            "Deletes every object of the release and its history, like `helm uninstall`.",
                                            "Uninstall",
                                            vec!["uninstall".into(), "-n".into(), ns, "--wait=false".into(), "--".into(), name],
                                            window,
                                            cx,
                                        );
                                    })),
                            ),
                    ),
            )
            .child(
                TabBar::new("helm-tabs")
                    .underline()
                    .px_2()
                    .selected_index(selected)
                    .children(tabs.iter().map(|t| {
                        Tab::new().label(match t {
                            ReleaseTab::Overview => "Overview",
                            ReleaseTab::Values => "Values",
                            ReleaseTab::Manifest => "Manifest",
                            ReleaseTab::History => "History",
                        })
                    }))
                    .on_click(move |ix, _, cx| {
                        let tab = tabs[*ix];
                        let _ = view.update(cx, |this, cx| {
                            this.tab = tab;
                            cx.notify();
                        });
                    }),
            )
            .child(div().flex_1().min_h_0().p_2().child(body))
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's own `test` attribute would shadow the standard one.
    use super::{
        Secret, Value, chart_refs, decode, latest_revisions, mask_manifest, mask_secrets, upgrade_text, values_yaml,
    };
    use base64::Engine as _;
    use std::io::Write as _;

    /// A Helm release Secret: `meta` is (Secret name, namespace, `name` label), `payload` the JSON.
    fn helm_secret(meta: (&str, &str, &str), payload: &Value, gzip: bool) -> Secret {
        let mut json = serde_json::to_vec(payload).unwrap();
        if gzip {
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            gz.write_all(&json).unwrap();
            json = gz.finish().unwrap();
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(json);
        let mut s = Secret::default();
        s.metadata.name = Some(meta.0.into());
        s.metadata.namespace = Some(meta.1.into());
        s.metadata.labels = Some([("owner".into(), "helm".into()), ("name".into(), meta.2.into())].into());
        s.data = Some([("release".into(), k8s_openapi::ByteString(b64.into_bytes()))].into());
        s
    }

    fn payload(name: &str, ns: &str, version: i64) -> Value {
        serde_json::json!({"name": name, "namespace": ns, "version": version, "info": {"status": "deployed"},
            "chart": {"metadata": {"name": "c", "version": "1.0.0"}}, "manifest": ""})
    }

    #[test]
    fn decodes_a_release_from_its_secret() {
        let s = helm_secret(
            ("sh.helm.release.v1.cilium.v3", "kube-system", "cilium"),
            &payload("cilium", "kube-system", 3),
            true,
        );
        let r = decode(&s).unwrap();
        assert_eq!(
            (r.name.as_str(), r.namespace.as_str(), r.revision, r.chart.as_str()),
            ("cilium", "kube-system", 3, "c")
        );
    }

    #[test]
    fn spoofed_releases_are_dropped() {
        // A tenant's Secret claiming to be kube-system/cilium at revision 99999.
        let spoof = payload("cilium", "kube-system", 99999);
        assert!(decode(&helm_secret(("sh.helm.release.v1.cilium.v99999", "tenant", "cilium"), &spoof, true)).is_err());
        // The payload's name disagrees with the label.
        let other = payload("cilium", "tenant", 2);
        assert!(decode(&helm_secret(("sh.helm.release.v1.app.v2", "tenant", "app"), &other, true)).is_err());
        // Not Helm's Secret name for that revision.
        assert!(
            decode(&helm_secret(("cilium-fake", "tenant", "cilium"), &payload("cilium", "tenant", 2), false)).is_err()
        );
        // A label that is not a release name (it would land in a label selector).
        assert!(
            decode(&helm_secret(("sh.helm.release.v1.a,b.v1", "t", "a,b"), &payload("a,b", "t", 1), false)).is_err()
        );

        // A same-named release elsewhere never hides the real one.
        let real = decode(&helm_secret(
            ("sh.helm.release.v1.cilium.v3", "kube-system", "cilium"),
            &payload("cilium", "kube-system", 3),
            true,
        ))
        .unwrap();
        let theirs = decode(&helm_secret(
            ("sh.helm.release.v1.cilium.v99999", "tenant", "cilium"),
            &payload("cilium", "tenant", 99999),
            true,
        ))
        .unwrap();
        let kept = latest_revisions(vec![theirs, real]);
        assert_eq!(kept.len(), 2);
        let sys = kept.iter().find(|r| r.namespace == "kube-system").unwrap();
        assert_eq!(sys.revision, 3);
    }

    #[test]
    fn keeps_the_newest_revision() {
        let rel = |v| {
            decode(&helm_secret(
                (&format!("sh.helm.release.v1.web.v{v}"), "apps", "web"),
                &payload("web", "apps", v),
                false,
            ))
            .unwrap()
        };
        let kept = latest_revisions(vec![rel(4), rel(5)]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].revision, 5);
    }

    #[test]
    fn gzip_bombs_are_refused() {
        // Highly compressible: a JSON string just over the cap.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(b"{\"name\":\"").unwrap();
        let zeros = vec![b'a'; 1 << 20];
        for _ in 0..33 {
            gz.write_all(&zeros).unwrap();
        }
        gz.write_all(b"\"}").unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(gz.finish().unwrap());
        let mut s = helm_secret(("sh.helm.release.v1.x.v1", "t", "x"), &payload("x", "t", 1), false);
        s.data = Some([("release".into(), k8s_openapi::ByteString(b64.into_bytes()))].into());
        let err = decode(&s).err().expect("refused").to_string();
        assert!(err.contains("32 MiB"), "{err}");
    }

    #[test]
    fn rendered_secrets_are_masked() {
        let manifest = "---\n# Source: c/templates/cm.yaml\napiVersion: v1\nkind: ConfigMap\ndata:\n  a: visible\n---\n# Source: c/templates/secret.yaml\napiVersion: v1\nkind: Secret\nmetadata:\n  name: s\nstringData:\n  config: example-value\ndata:\n  token: ZXhhbXBsZQ==\n";
        let masked = mask_manifest(manifest);
        assert!(!masked.contains("example-value") && !masked.contains("ZXhhbXBsZQ"), "{masked}");
        assert!(masked.contains("config:") && masked.contains("a: visible"));
        assert!(
            masked.contains("# Source: c/templates/secret.yaml") && masked.contains("# Source: c/templates/cm.yaml")
        );
        assert_eq!(mask_manifest("kind: ConfigMap\n"), "kind: ConfigMap\n");
        // Unparsable but a Secret: hidden whole.
        assert!(!mask_manifest("kind: Secret\ndata: [x: {\n  p: example-value\n").contains("example-value"));
    }

    #[test]
    fn never_upgrades_with_masked_values() {
        let values = serde_json::json!({"db": {"password": "example"}});
        let masked = values_yaml(&values, false);
        assert!(upgrade_text(false, &masked, &values).is_none());
        // Revealed, then masked text pasted back: still refused.
        assert!(upgrade_text(true, &masked, &values).is_none());
        let revealed = values_yaml(&values, true);
        assert_eq!(upgrade_text(true, &revealed, &values).as_deref(), Some(revealed.as_str()));
    }

    #[test]
    fn finds_exact_chart_refs() {
        let json = r#"[{"name":"bitnami/redis"},{"name":"other/redis"},{"name":"bitnami/redis-cluster"},{"name":"bitnami/redis"}]"#;
        assert_eq!(chart_refs(json, "redis"), ["bitnami/redis", "other/redis"]);
        assert!(chart_refs("not json", "redis").is_empty());
    }

    #[test]
    fn masks_secret_looking_values() {
        let mut v = serde_json::json!({
            "db": {"password": "example", "host": "pg", "port": 5432},
            "auth": {"apiKey": "abc", "publicKey": "pub"},
            "list": [{"token": "t"}],
        });
        mask_secrets(&mut v);
        assert_eq!(v["db"]["password"], "••••••");
        assert_eq!(v["db"]["host"], "pg");
        assert_eq!(v["db"]["port"], 5432);
        assert_eq!(v["auth"]["apiKey"], "••••••");
        assert_eq!(v["auth"]["publicKey"], "pub");
        assert_eq!(v["list"][0]["token"], "••••••");
    }
}
