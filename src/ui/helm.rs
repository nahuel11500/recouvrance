//! The Helm pages: the releases of the connected clusters, and one release's details,
//! history, values, manifest and actions (see `crate::helm` for reading them).

use anyhow::Result;
use futures::StreamExt as _;
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;
use serde_json::Value;

use crate::helm::{
    Release, fill_releases, history, list_releases, mask_manifest, merge_clusters, newest_revision,
    unreadable_revision, values_yaml,
};
use crate::k8s;

/// Why an upgrade built from revision `shown` must not run, given the release's newest
/// revision now: it would send that revision's chart version and values over newer ones.
/// `unreadable` is why the newest one can't be read, if it can't.
fn stale_upgrade(name: &str, shown: i64, newest: Option<i64>, unreadable: Option<&str>) -> Option<String> {
    match (newest, unreadable) {
        (Some(n), _) if n == shown => None,
        (Some(n), Some(e)) => {
            Some(format!("{name}'s newest revision ({n}) can't be read here ({e}): upgrade it with the helm CLI"))
        }
        (Some(n), None) => Some(format!(
            "{name} changed since this page was read (revision {shown}, now {n}): open it again to upgrade from there"
        )),
        (None, _) => Some(format!("{name} has no revision any more: it may have been uninstalled")),
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

fn scope_key(scope: &HelmScope) -> Vec<(String, Vec<Option<String>>)> {
    scope.iter().map(|(ctx, _, ns)| (ctx.clone(), ns.clone())).collect()
}

pub struct OpenRelease(pub Release);

/// The "Install a chart" button was clicked.
pub struct OpenInstall;

/// What the release list reads: each cluster with the namespaces to list there (`None`:
/// all of them; none: the cluster is left out).
pub type HelmScope = Vec<(String, Client, Vec<Option<String>>)>;

pub struct HelmList {
    multi: bool,
    /// Contexts and namespaces of the list on screen, to reload only when they change.
    scope: Vec<(String, Vec<Option<String>>)>,
    releases: Option<Result<Vec<Release>, SharedString>>,
    /// Indices in `releases` the filter keeps, worked out on each render.
    shown: Vec<usize>,
    /// Clusters that could not be read while others could: "context: error".
    errors: Vec<SharedString>,
    filter: Entity<InputState>,
    _task: Task<()>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<OpenRelease> for HelmList {}
impl EventEmitter<OpenInstall> for HelmList {}

impl HelmList {
    pub fn new(scope: HelmScope, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter releases…"));
        let subs = vec![cx.subscribe(&filter, |_, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        })];
        let mut list = Self {
            multi: false,
            scope: Vec::new(),
            releases: None,
            shown: Vec::new(),
            errors: Vec::new(),
            filter,
            _task: Task::ready(()),
            _subs: subs,
        };
        list.load(scope, cx);
        list
    }

    /// List the releases of another selection of clusters or namespaces; the filter stays.
    pub fn set_scope(&mut self, scope: HelmScope, cx: &mut Context<Self>) {
        if scope_key(&scope) != self.scope {
            self.load(scope, cx);
        }
    }

    fn load(&mut self, scope: HelmScope, cx: &mut Context<Self>) {
        self.multi = scope.len() > 1;
        self.scope = scope_key(&scope);
        self.releases = None;
        self.errors.clear();
        cx.notify();
        let multi = self.multi;
        // One list per cluster and namespace chosen, named by what an error should say.
        let lists: Vec<(String, String, Client, Option<String>)> = scope
            .into_iter()
            .flat_map(|(ctx, client, namespaces)| {
                namespaces.into_iter().map(move |ns| {
                    let label = match &ns {
                        Some(ns) if multi => format!("{ctx}/{ns}"),
                        Some(ns) => ns.clone(),
                        None => ctx.clone(),
                    };
                    (label, ctx.clone(), client.clone(), ns)
                })
            })
            .collect();
        // Replacing the task drops the previous load: its results never land over these.
        self._task = cx.spawn(async move |this, cx| {
            // The releases from their Secrets' labels first: shown at once.
            let loads = lists.iter().map(|(label, ctx, client, ns)| {
                let (label, ctx, client, ns) = (label.clone(), ctx.clone(), client.clone(), ns.clone());
                async move {
                    let rels = list_releases(client, ns).await.map(|mut rels| {
                        for r in &mut rels {
                            r.context = ctx.clone();
                        }
                        rels
                    });
                    (label, rels)
                }
            });
            let (result, errors) = merge_clusters(futures::future::join_all(loads).await);
            let listed = result.is_ok();
            let _ = this.update(cx, |this, cx| {
                this.releases = Some(result.map_err(SharedString::from));
                this.errors = errors.into_iter().map(SharedString::from).collect();
                cx.notify();
            });
            if !listed {
                return;
            }
            // Then their charts and app versions, as each page of payloads is read.
            let fills = lists.into_iter().map(|(_, ctx, client, ns)| {
                let (this, mut cx) = (this.clone(), cx.clone());
                async move {
                    let (tx, mut rx) = futures::channel::mpsc::unbounded();
                    let apply = async {
                        while let Some(page) = rx.next().await {
                            let _ = this.update(&mut cx, |this, cx| {
                                this.fill(&ctx, page);
                                cx.notify();
                            });
                        }
                    };
                    // Without them the list still works: chart columns stay empty.
                    let _ = futures::join!(fill_releases(client, ns, tx), apply);
                }
            });
            futures::future::join_all(fills).await;
        });
    }

    /// Put read releases in place of their label-only rows.
    fn fill(&mut self, context: &str, page: Vec<Release>) {
        let Some(Ok(list)) = &mut self.releases else { return };
        for mut r in page {
            r.context = context.to_string();
            let same = |l: &&mut Release| l.context == r.context && l.namespace == r.namespace && l.name == r.name;
            if let Some(slot) = list.iter_mut().find(same)
                && slot.revision <= r.revision
            {
                *slot = r;
            }
        }
    }

    fn row(&self, i: usize, r: &Release, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let open = r.clone();
        h_flex()
            .id(("release", i))
            .w_full()
            .gap_3()
            .px_2()
            .py_1()
            .text_sm()
            .border_b_1()
            .border_color(theme.border)
            .cursor_pointer()
            .hover(|s| s.bg(theme.list_hover))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(OpenRelease(open.clone()))))
            .when(self.multi, |d| {
                d.child(
                    div().w(px(160.)).overflow_hidden().text_ellipsis().whitespace_nowrap().child(r.context.clone()),
                )
            })
            .child(div().w(px(200.)).overflow_hidden().text_ellipsis().whitespace_nowrap().child(r.namespace.clone()))
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
                    .when(!r.chart.is_empty(), |d| d.child(format!("{}-{}", r.chart, r.chart_version))),
            )
            .child(div().w(px(140.)).overflow_hidden().text_ellipsis().whitespace_nowrap().child(r.app_version.clone()))
            .child(div().w(px(100.)).text_color(status_color(&r.status, cx)).child(r.status.clone()))
            .child(
                div()
                    .w(px(80.))
                    .text_color(cx.theme().muted_foreground)
                    .child(r.updated.map(k8s::age).unwrap_or_default()),
            )
            .into_any_element()
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
                self.shown = (0..releases.len())
                    .filter(|&i| {
                        let r = &releases[i];
                        query.is_empty()
                            || r.name.to_lowercase().contains(&query)
                            || r.namespace.contains(&query)
                            || r.chart.to_lowercase().contains(&query)
                    })
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
                let count = self.shown.len();
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
                    .when(!self.errors.is_empty(), |d| {
                        d.child(
                            v_flex()
                                .px_2()
                                .pb_2()
                                .text_xs()
                                .text_color(theme.danger)
                                .children(self.errors.iter().map(|e| div().child(e.clone()))),
                        )
                    })
                    .child(header)
                    .child(
                        // Only the rows on screen are laid out: clusters hold thousands of releases.
                        uniform_list(
                            "releases",
                            count,
                            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                                let Some(Ok(releases)) = &this.releases else { return Vec::new() };
                                range
                                    .filter_map(|i| this.shown.get(i).map(|&ix| this.row(i, &releases[ix], cx)))
                                    .collect()
                            }),
                        )
                        .flex_1()
                        .min_h_0(),
                    )
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
    /// The first read of the release and its history (fills in what the list left out).
    /// Its own slot: a later reload must not cancel it.
    _first_load: Task<()>,
    /// The history again after a write.
    history_load: crate::latest::Latest,
    /// The chart lookup of the upgrade started last (a dry run, then a real one, or back).
    chart_lookup: crate::latest::Latest,
}

/// The edited values to upgrade with, or None while secrets are masked: the masked view
/// holds `••••••` placeholders, which Helm would store as the real values.
fn upgrade_text(reveal: bool, edited: &str, values: &Value) -> Option<String> {
    let mask = "••••••";
    let placeholder = edited.contains(mask) && !values_yaml(values, true).contains(mask);
    (reveal && !placeholder).then(|| edited.to_string())
}

/// The newest revision in `history`, when it is newer than `current`.
fn newer_revision(current: i64, history: &[Release]) -> Option<&Release> {
    history.iter().max_by_key(|r| r.revision).filter(|r| r.revision > current)
}

/// The revision to open from the list: the newest in `history`, when it is `listed` or
/// newer (the list can be out of date).
fn opened_revision(listed: i64, history: &[Release]) -> Option<&Release> {
    history.iter().max_by_key(|r| r.revision).filter(|r| r.revision >= listed)
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
        let reading = || "# reading the release…\n".to_string();
        let values = editor(if release.loaded { values_yaml(&release.values, false) } else { reading() }, window, cx);
        let manifest = editor(if release.loaded { mask_manifest(&release.manifest) } else { reading() }, window, cx);
        let (ns, name, c2) = (release.namespace.clone(), release.name.clone(), client.clone());
        let task = cx.spawn_in(window, async move |this, cx| {
            let result = history(c2, ns, name).await;
            let _ = this.update_in(cx, |this, window, cx| {
                // Opened from the list: notes, values and manifest come with the history.
                if !this.release.loaded
                    && let Ok(revs) = &result
                    && let Some(full) = opened_revision(this.release.revision, revs)
                {
                    this.release = Release { context: this.release.context.clone(), ..full.clone() };
                    this.set_reveal(this.reveal, window, cx);
                }
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
            _first_load: task,
            history_load: Default::default(),
            chart_lookup: Default::default(),
        }
    }

    /// After a write: the history again, and the page on the newest revision (an upgrade
    /// or rollback makes one), so its values are what the next upgrade starts from.
    fn reload_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (client, ns, name) = (self.client.clone(), self.release.namespace.clone(), self.release.name.clone());
        self.history_load.spawn_in(window, cx, history(client, ns, name), |this, result, window, cx| {
            if let Some(newest) = result.as_ref().ok().and_then(|h| newer_revision(this.release.revision, h)) {
                this.release = Release { context: this.release.context.clone(), ..newest.clone() };
                this.set_reveal(this.reveal, window, cx);
            }
            this.history = Some(result.map_err(|e| format!("{e:#}").into()));
            cx.notify();
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
        if !self.release.loaded {
            window.push_notification(Notification::error("The release is still being read"), cx);
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
        // Plain search, then an exact name match (helm's -r regex also sees descriptions).
        let lookup = {
            let (chart, version) = (chart.clone(), version.clone());
            async move {
                // `--` everywhere: names from the cluster are never read as flags.
                let args = ["search", "repo", "--version", &version, "-o", "json", "--", &chart];
                k8s::helm(context, args.iter().map(ToString::to_string).collect()).await.map(|o| chart_refs(&o, &chart))
            }
        };
        // A newer upgrade (a dry run then the real one, or back) replaces this lookup.
        self.chart_lookup.spawn_in(window, cx, lookup, move |this, refs, window, cx| match refs {
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
        // The chart lookup (or the choice between repositories) came after write mode was checked.
        if !crate::details::writes_still_on(window, cx) {
            return;
        }
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
                    if !crate::details::writes_still_on(window, cx) {
                        return true;
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
        let (client, shown) = (self.client.clone(), self.release.revision);
        // A fresh private directory: the values may hold secrets.
        let written = crate::private_files::PrivateDir::create()
            .and_then(|d| Ok((d.write("values.yaml", values.as_bytes())?, d)));
        let (file, dir) = match written {
            Ok(w) => w,
            Err(e) => {
                window.push_notification(Notification::error(format!("Could not write values: {e}")), cx);
                return;
            }
        };
        window.push_notification(Notification::info(if dry_run { "Dry run upgrade…" } else { "Upgrading…" }), cx);
        cx.spawn_in(window, async move |this, cx| {
            // The page may be out of date (CI upgraded meanwhile): its chart version and values
            // would undo the newer revision. Refused when that can't be checked either.
            let stale = match newest_revision(client.clone(), ns.clone(), name.clone()).await {
                Ok(Some(n)) if n != shown => {
                    let unreadable = unreadable_revision(client, ns.clone(), name.clone(), n).await;
                    stale_upgrade(&name, shown, Some(n), unreadable.as_deref())
                }
                Ok(newest) => stale_upgrade(&name, shown, newest, None),
                Err(e) => Some(format!("could not read {name}'s newest revision: {e:#}")),
            };
            if let Some(why) = stale {
                drop(dir);
                let _ = this.update_in(cx, |_, window, cx| {
                    window.push_notification(Notification::error(format!("helm upgrade refused: {why}")), cx);
                });
                return;
            }
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
                    window.push_notification(Notification::success(format!("Dry run upgrade: {status}")), cx);
                }
                Ok(_) => {
                    window.push_notification(Notification::success("Upgraded"), cx);
                    this.reload_history(window, cx);
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
                    if !crate::details::writes_still_on(window, cx) {
                        return true;
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
                                        window
                                            .push_notification(Notification::error(format!("helm failed: {e:#}")), cx);
                                    }
                                }
                                this.reload_history(window, cx);
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
    use super::{chart_refs, newer_revision, opened_revision, stale_upgrade, upgrade_text, values_yaml};
    use crate::helm::decode;
    use crate::helm::tests::{helm_secret, payload};

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
    fn upgrades_only_from_the_newest_revision() {
        assert!(stale_upgrade("web", 5, Some(5), None).is_none());
        // CI upgraded to 6 after the page read 5: 5's chart version and values would undo it.
        assert!(stale_upgrade("web", 5, Some(6), None).is_some_and(|e| e.contains("now 6")));
        assert!(stale_upgrade("web", 5, None, None).is_some());
        // 6 is there but can't be decoded: reopening the page won't reach it, so say why.
        let e = stale_upgrade("web", 5, Some(6), Some("too large")).unwrap_or_default();
        assert!(e.contains("(6)") && e.contains("too large") && !e.contains("open it again"));
    }

    #[test]
    fn moves_to_the_newest_revision() {
        let rev = |v| {
            decode(&helm_secret(
                (&format!("sh.helm.release.v1.web.v{v}"), "ns", "web"),
                &payload("web", "ns", v),
                false,
            ))
            .unwrap()
        };
        // Rolled back from 5 to 3: Helm writes revision 6.
        let history = vec![rev(3), rev(6), rev(5)];
        assert_eq!(newer_revision(5, &history).map(|r| r.revision), Some(6));
        assert!(newer_revision(6, &history).is_none());
        assert!(newer_revision(5, &[]).is_none());
        // Opened from a list showing 5 while 6 was written: the page shows 6.
        assert_eq!(opened_revision(5, &history).map(|r| r.revision), Some(6));
        assert_eq!(opened_revision(6, &history).map(|r| r.revision), Some(6));
        assert!(opened_revision(7, &history).is_none());
    }

    #[test]
    fn finds_exact_chart_refs() {
        let json = r#"[{"name":"bitnami/redis"},{"name":"other/redis"},{"name":"bitnami/redis-cluster"},{"name":"bitnami/redis"}]"#;
        assert_eq!(chart_refs(json, "redis"), ["bitnami/redis", "other/redis"]);
        assert!(chart_refs("not json", "redis").is_empty());
    }
}
