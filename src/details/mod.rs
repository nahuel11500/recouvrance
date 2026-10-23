//! Detail page for one object: overview, containers, revisions, logs, YAML, events, shell.

use futures::StreamExt as _;
use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Editor, EditorState, Input, InputEvent, InputState};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{ActiveTheme as _, IndexPath, Sizable as _, h_flex, v_flex};
use gpui_kit::component::{Icon, IconName, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use kube::Client;

use crate::favorites::{FavObject, Favorites};
use crate::forwards::Forwards;
use crate::k8s::{self, EventLine, ObjectRef, TaskGuard};
use crate::overview::{Ctx, Related};
use crate::ui::terminal::TerminalView;

mod access_actions;
#[allow(unused_imports)]
pub(crate) use access_actions::*;
mod files;
#[allow(unused_imports)]
pub(crate) use files::*;
mod forward_options;
#[allow(unused_imports)]
pub(crate) use forward_options::*;
mod action_bar;
#[allow(unused_imports)]
pub(crate) use action_bar::*;
mod actions;
mod compare;
#[cfg(feature = "dev")]
mod demo;
mod logs;
mod related;
mod tabs;
#[allow(unused_imports)]
pub(crate) use actions::*;
#[allow(unused_imports)]
pub(crate) use compare::*;
#[cfg(feature = "dev")]
#[allow(unused_imports)]
pub(crate) use demo::*;
#[allow(unused_imports)]
pub(crate) use logs::*;
#[allow(unused_imports)]
pub(crate) use related::*;
#[allow(unused_imports)]
pub(crate) use tabs::*;

#[derive(Clone, Copy, PartialEq)]
enum DetailTab {
    Overview,
    /// A pod's container, by index in spec.containers.
    Container(usize),
    Revisions,
    Map,
    Compare,
    Logs,
    Yaml,
    Events,
    Shell,
}

/// Whether the user turned on write actions (off by default).
pub struct WriteMode(pub bool);

impl Global for WriteMode {}

pub fn writes_enabled(cx: &App) -> bool {
    cx.try_global::<WriteMode>().is_some_and(|w| w.0)
}

/// True when writes are on; otherwise offers to enable them and returns false.
pub fn ensure_writes(window: &mut Window, cx: &mut App) -> bool {
    if writes_enabled(cx) {
        return true;
    }
    window.open_alert_dialog(cx, |alert, _, _| {
        alert
            .title("Enable write actions?")
            .description(
                "Recouvrance is read-only by default. Enabling writes lets you scale, restart, run, suspend, drain, \
                 delete and edit, open shells in containers and run plugins, using your own kubeconfig credentials. \
                 Every action still asks for confirmation.",
            )
            .show_cancel(true)
            .ok_text("Enable writes")
            .on_ok(|_, _, cx| {
                cx.set_global(WriteMode(true));
                cx.refresh_windows();
                true
            })
    });
    false
}

/// Ask the app to open another object (links in the overview).
#[derive(Clone)]
pub struct OpenObject {
    pub kind: String,
    pub name: String,
    pub namespace: Option<String>,
    /// Context to open in; None means the sender's own cluster.
    pub context: Option<String>,
}

impl EventEmitter<OpenObject> for DetailPanel {}

impl EventEmitter<crate::plugins::RunPlugin> for DetailPanel {}

pub struct DetailPanel {
    client: Client,
    context: String,
    obj: ObjectRef,
    tab: DetailTab,
    yaml: Entity<EditorState>,
    yaml_error: Option<SharedString>,
    object: Option<serde_json::Value>,
    /// YAML last put in the editor, to tell user edits from stale content.
    yaml_text: String,
    related: Related,
    pub(crate) compare_rev: Option<String>,
    reveal_secrets: bool,
    /// Secret keys revealed one at a time on the overview.
    revealed_keys: std::collections::HashSet<String>,
    /// Range of the usage charts, in minutes.
    history_minutes: i64,
    /// Node shell pod started from this page; deleted when the page goes away.
    node_shell: Option<String>,
    env_expanded: bool,
    /// The overview of a page with a pods table, drawn as a list: a node can hold a
    /// thousand pods, and laying them all out on every frame made scrolling stutter.
    overview_list: ListState,
    /// A CRD's spec fields, worked out when it is loaded rather than on every frame.
    schema_rows: Vec<crate::overview::SchemaRow>,
    /// Inline editor of a ConfigMap's or Secret's values.
    data_editor: Option<Entity<crate::ui::data_edit::DataEditor>>,
    /// Show metadata.managedFields in the YAML (hidden by default).
    show_managed: bool,
    /// Field docs next to the YAML editor.
    field_docs: Option<Entity<crate::ui::field_docs::FieldDocs>>,
    /// Write actions the API server says this user may not do ("patch", "delete", …).
    denied: std::collections::HashSet<&'static str>,
    events: Option<Result<Vec<EventLine>, SharedString>>,
    logs: Option<Entity<LogsView>>,
    terminal: Option<Entity<TerminalView>>,
    forwards: Entity<Forwards>,
    favorites: Entity<Favorites>,
    /// Every connected cluster (for Compare).
    pub(crate) clusters: Vec<(String, Client)>,
    compare: CompareState,
    /// The live refresh and related polling loops run (started once).
    loops_started: bool,
    /// A row-menu action waiting for the object's first load (its dialog reads it).
    pending_action: Option<String>,
    /// The object is gone from the cluster (a read answered 404): polling stopped.
    deleted: bool,
    _tasks: Vec<Task<()>>,
    _subs: Vec<Subscription>,
}

#[derive(Default)]
struct CompareState {
    /// "context" for the same object elsewhere, or an object name here.
    target_ctx: Option<String>,
    target_name: Option<String>,
    other: Option<Result<serde_json::Value, String>>,
    candidates: Vec<String>,
    /// Why the candidates could not be listed.
    candidates_error: Option<String>,
    /// Candidates were listed (or are being listed): never list them again.
    candidates_loaded: bool,
    _candidates: Option<Task<()>>,
    /// The comparison being fetched; replacing it cancels the previous one.
    _compare: Option<Task<()>>,
}

impl DetailPanel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: Client,
        context: String,
        risky: bool,
        forwards: Entity<Forwards>,
        favorites: Entity<Favorites>,
        obj: ObjectRef,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let yaml = cx.new(|cx| EditorState::new(window, cx).language("yaml").line_number(true).searchable(true));
        let is_pod = obj.kind.is("", "Pod");
        let ns = obj.namespace.clone().unwrap_or_default();
        let logs = is_pod.then(|| {
            let (client, ns, name) = (client.clone(), ns.clone(), obj.name.clone());
            cx.new(|cx| LogsView::new(client, ns, vec![name], window, cx))
        });
        let terminal = is_pod.then(|| {
            let (client, ns, name, ctx) = (client.clone(), ns.clone(), obj.name.clone(), context.clone());
            // Exec and attach run commands in the cluster: write mode and the protection guard apply.
            cx.new(|cx| TerminalView::new(client, ns, name, None, risky, cx).guarded(ctx))
        });
        let mut this = Self {
            client,
            context,
            obj,
            tab: DetailTab::Overview,
            yaml,
            yaml_error: None,
            object: None,
            yaml_text: String::new(),
            related: Related::default(),
            compare_rev: None,
            reveal_secrets: false,
            revealed_keys: Default::default(),
            history_minutes: crate::settings::get().usage_range,
            denied: Default::default(),
            node_shell: None,
            env_expanded: false,
            overview_list: ListState::new(0, ListAlignment::Top, px(600.)),
            schema_rows: Vec::new(),
            data_editor: None,
            show_managed: false,
            field_docs: None,
            events: None,
            logs,
            terminal,
            forwards,
            favorites,
            clusters: Vec::new(),
            compare: CompareState::default(),
            loops_started: false,
            pending_action: None,
            deleted: false,
            _tasks: Vec::new(),
            _subs: Vec::new(),
        };
        this.load_object(window, cx);
        this.load_events(cx);
        this
    }

    /// Row-menu shortcuts into the same confirmed actions as the header buttons.
    pub fn quick_action(&mut self, action: &str, window: &mut Window, cx: &mut Context<Self>) {
        // A page just opened from the row menu has not read its object yet, and the
        // dialogs show (and prefill) what it holds: wait for the first load.
        if self.object.is_none() {
            self.pending_action = Some(action.to_string());
            return;
        }
        match action {
            "run" => {
                let title = format!("Run CronJob {} now?", self.obj.name);
                self.confirm_action(
                    title,
                    "Creates a Job from its template, like `kubectl create job --from=cronjob/…`.",
                    "Run",
                    |p| {
                        Box::pin(k8s::trigger_cronjob(
                            p.client.clone(),
                            p.obj.namespace.clone().unwrap_or_default(),
                            p.obj.name.clone(),
                        ))
                    },
                    window,
                    cx,
                )
            }
            "restart" => self.confirm_restart(window, cx),
            "scale" => self.confirm_scale(window, cx),
            "delete" => self.confirm_delete(window, cx),
            "debug" => self.confirm_debug(window, cx),
            "nodeshell" => self.confirm_node_shell(window, cx),
            _ => {}
        }
    }

    fn run_plugin(&mut self, plugin: crate::plugins::Plugin, window: &mut Window, cx: &mut Context<Self>) {
        let command = plugin.script();
        let env = crate::plugins::env(
            &self.context,
            self.obj.namespace.as_deref(),
            &self.obj.name,
            &self.obj.kind.kubectl_name(),
        );
        let ev = crate::plugins::RunPlugin {
            plugin: plugin.clone(),
            command: command.clone(),
            env,
            context: self.context.clone(),
            namespace: self.obj.namespace.clone(),
        };
        // A plugin may change the cluster unless it says it only reads: write mode and
        // the protection guard apply to it like to any write.
        let writes = !plugin.read_only;
        if writes && !self.ensure_writes(window, cx) {
            return;
        }
        let guard = if writes { self.protection(window, cx) } else { None };
        if !plugin.confirm && guard.is_none() {
            cx.emit(ev);
            return;
        }
        let panel = cx.entity().downgrade();
        let ev = std::rc::Rc::new(std::cell::RefCell::new(Some(ev)));
        window.open_alert_dialog(cx, move |alert, _, cx| {
            let (panel, ev, guard) = (panel.clone(), ev.clone(), guard.clone());
            alert
                .title(format!("Run {}?", plugin.name))
                .child(confirm_extras(&command, guard.as_ref(), cx))
                .show_cancel(true)
                .ok_text("Run")
                .on_ok(move |_, _, cx| {
                    if !guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    if let Some(ev) = ev.borrow_mut().take() {
                        let _ = panel.update(cx, |_, cx| cx.emit(ev));
                    }
                    true
                })
        });
    }

    /// Ask the API server which write actions this user may do on the object.
    fn check_access(&mut self, cx: &mut Context<Self>) {
        use k8s_openapi::api::{batch::v1::Job, core::v1::Pod, rbac::v1::RoleBinding};
        let (client, kind, ns, name) =
            (self.client.clone(), self.obj.kind.clone(), self.obj.namespace.clone(), self.obj.name.clone());
        let task = cx.spawn(async move |this, cx| {
            let job = k8s::Kind { ar: k8s::ar_of::<Job>(), namespaced: true, section: "", label: "Job".into() };
            let pod = k8s::Kind { ar: k8s::ar_of::<Pod>(), namespaced: true, section: "", label: "Pod".into() };
            let mut checks = vec![
                ("patch", k8s::can_i(client.clone(), kind.clone(), "patch", ns.clone(), Some(name.clone()), None)),
                ("delete", k8s::can_i(client.clone(), kind.clone(), "delete", ns.clone(), Some(name.clone()), None)),
                ("run", k8s::can_i(client.clone(), job, "create", ns.clone(), None, None)),
                ("drain", k8s::can_i(client.clone(), pod.clone(), "create", None, None, Some("eviction"))),
                (
                    "debug",
                    k8s::can_i(
                        client.clone(),
                        pod.clone(),
                        "patch",
                        ns.clone(),
                        Some(name.clone()),
                        Some("ephemeralcontainers"),
                    ),
                ),
                ("nodeshell", k8s::can_i(client.clone(), pod, "create", Some(k8s::NODE_SHELL_NS.into()), None, None)),
            ];
            // A ServiceAccount's kubeconfig asks for a token (`create serviceaccounts/token`).
            if kind.is("", "ServiceAccount") {
                let token =
                    k8s::can_i(client.clone(), kind.clone(), "create", ns.clone(), Some(name.clone()), Some("token"));
                checks.push(("token", token));
            }
            // Binding a Role creates a RoleBinding in its namespace. A ClusterRole can be bound
            // in any namespace or cluster-wide, so it is not gated: the server decides.
            if kind.is("rbac.authorization.k8s.io", "Role") {
                let rb = k8s::Kind {
                    ar: k8s::ar_of::<RoleBinding>(),
                    namespaced: true,
                    section: "",
                    label: "RoleBinding".into(),
                };
                checks.push(("create", k8s::can_i(client.clone(), rb, "create", ns.clone(), None, None)));
            }
            let mut denied = std::collections::HashSet::new();
            for (verb, fut) in checks {
                if !fut.await {
                    denied.insert(verb);
                }
            }
            let _ = this.update(cx, |this, cx| {
                this.denied = denied;
                cx.notify();
            });
        });
        self.keep_task(task);
    }

    /// Change the usage charts' range and reload them now.
    pub fn set_history_minutes(&mut self, minutes: i64, cx: &mut Context<Self>) {
        self.history_minutes = minutes;
        crate::settings::update(|s| s.usage_range = minutes);
        let (Some(obj), client) = (self.object.clone(), self.client.clone()) else { return };
        let (context, kind, ns, pods) =
            (self.context.clone(), self.obj.kind.clone(), self.obj.namespace.clone(), self.related.pods.clone());
        let task = cx.spawn(async move |this, cx| {
            let (h, g, c) = futures::join!(
                history_for(client.clone(), &context, &kind, ns.as_deref(), &obj, &pods, minutes),
                gpu_history_for(client.clone(), &context, &kind, ns.as_deref(), &obj, &pods, minutes),
                container_history_for(client, &context, &kind, ns.as_deref(), &obj, minutes)
            );
            let _ = this.update(cx, |this, cx| {
                if this.history_minutes == minutes {
                    this.related.history = h;
                    this.related.gpu = g;
                    this.related.container_history = c;
                    cx.notify();
                }
            });
        });
        self.keep_task(task);
        cx.notify();
    }

    /// Logs stream only while their tab has been opened on a visible page.
    fn resume_logs_if_shown(&mut self, cx: &mut Context<Self>) {
        if self.tab == DetailTab::Logs
            && let Some(l) = &self.logs
        {
            l.update(cx, |l, cx| l.resume(cx));
        }
    }

    pub fn select_tab_named(&mut self, name: &str, cx: &mut Context<Self>) {
        self.tab = match name {
            "yaml" => DetailTab::Yaml,
            "events" => DetailTab::Events,
            "logs" => DetailTab::Logs,
            "revisions" => DetailTab::Revisions,
            "map" => DetailTab::Map,
            "compare" => DetailTab::Compare,
            "container" => DetailTab::Container(0),
            "shell" => DetailTab::Shell,
            _ => DetailTab::Overview,
        };
        self.resume_logs_if_shown(cx);
        cx.notify();
    }

    /// Read the object once; the first success also starts the page's polling loops.
    fn load_object(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_object(false, window, cx);
    }

    /// One-shot re-read of the object (after a write, say). `reset_yaml` replaces the
    /// editor's text even when it differs from what was last loaded (after saving it).
    pub(super) fn refresh_object(&mut self, reset_yaml: bool, window: &mut Window, cx: &mut Context<Self>) {
        let fut = k8s::get_object_opts(self.client.clone(), self.obj.clone(), true);
        self.keep_task(cx.spawn_in(window, async move |this, cx| {
            let result = fut.await;
            let _ = this.update_in(cx, |this, window, cx| {
                match result {
                    Ok(value) => {
                        this.set_object(value);
                        if reset_yaml || this.yaml.read(cx).value().as_ref() == this.yaml_text {
                            this.refresh_yaml(window, cx);
                        }
                        if !this.loops_started {
                            this.loops_started = true;
                            this.load_related(window, cx);
                            this.check_access(cx);
                            this.start_live(window, cx);
                        }
                        if let Some(action) = this.pending_action.take() {
                            this.quick_action(&action, window, cx);
                        }
                    }
                    Err(e) => {
                        if k8s::is_not_found(&e) {
                            this.deleted = true;
                        }
                        if let Some(action) = this.pending_action.take() {
                            window.push_notification(
                                Notification::error(format!(
                                    "Could not read {} {}, so nothing was done ({action}): {}",
                                    this.obj.kind.ar.kind,
                                    this.obj.name,
                                    k8s::short_error(&e)
                                )),
                                cx,
                            );
                        }
                        // Keep showing the last state when a later read fails.
                        if this.object.is_none() {
                            this.yaml_error = Some(k8s::short_error(&e).into());
                        }
                    }
                }
                cx.notify();
            });
        }));
    }

    /// Keep a task alive with the page; finished ones are dropped on the way.
    pub(super) fn keep_task(&mut self, task: Task<()>) {
        self._tasks.retain(|t| !t.is_ready());
        self._tasks.push(task);
    }

    /// Re-read the object every 5s so status, conditions and YAML stay current.
    fn start_live(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (client, obj) = (self.client.clone(), self.obj.clone());
        let task = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(crate::settings::secs(crate::settings::get().detail_refresh)).await;
                // Hidden (in history): stop log streams, then wait to be shown again.
                let hidden = this.update(cx, |this, cx| {
                    let hidden = !crate::app::is_visible(cx.entity_id(), cx);
                    if hidden && let Some(l) = &this.logs {
                        l.update(cx, |l, _| l.pause());
                    }
                    // A node shell pod nobody is connected to is not left running.
                    if hidden
                        && this.node_shell.is_some()
                        && !this.terminal.as_ref().is_some_and(|t| t.read(cx).connected())
                    {
                        this.end_node_shell(cx);
                    }
                    hidden
                });
                match hidden {
                    Err(_) => break,
                    Ok(true) => {
                        let exec = cx.background_executor().clone();
                        if !crate::app::wait_visible(&this, cx, &exec).await {
                            break;
                        }
                        let _ = this.update(cx, |this, cx| {
                            if this.tab == DetailTab::Logs
                                && let Some(l) = &this.logs
                            {
                                l.update(cx, |l, cx| l.resume(cx));
                            }
                        });
                    }
                    Ok(false) => {}
                }
                let value = match k8s::get_object_opts(client.clone(), obj.clone(), true).await {
                    Ok(v) => v,
                    // Deleted: say so and stop asking.
                    Err(e) if k8s::is_not_found(&e) => {
                        let _ = this.update(cx, |this, cx| {
                            this.deleted = true;
                            cx.notify();
                        });
                        break;
                    }
                    Err(_) => continue,
                };
                let alive = this.update_in(cx, |this, window, cx| {
                    this.deleted = false;
                    let changed = this.object.as_ref() != Some(&value);
                    if !changed {
                        return;
                    }
                    this.set_object(value);
                    // Don't overwrite what the user is editing.
                    if this.yaml.read(cx).value().as_ref() == this.yaml_text {
                        this.refresh_yaml(window, cx);
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        self.keep_task(task);
    }

    /// Fetch what the overview links to, then refresh usage every 15s.
    fn load_related(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.object.is_none() {
            return;
        }
        let client = self.client.clone();
        let kind = self.obj.kind.clone();
        let ns = self.obj.namespace.clone();
        let context = self.context.clone();
        let wants_history =
            kind.is("", "Pod") || kind.ar.group == "apps" || kind.is("batch", "Job") || kind.is("", "Node");
        // Workload logs follow its pods (one namespace); a pod page has its own.
        let pods_logs = !kind.is("", "Pod") && ns.is_some();
        let task = cx.spawn_in(window, async move |this, cx| {
            loop {
                let exec = cx.background_executor().clone();
                if !crate::app::wait_visible(&this, cx, &exec).await {
                    break;
                }
                // The object as the live refresh last saw it (status, node, crashes move on).
                // Nothing more to follow once the object is deleted.
                let Ok(Some(obj)) = this.update(cx, |this, _| this.object.clone().filter(|_| !this.deleted)) else {
                    break;
                };
                let related = fetch_related(client.clone(), &kind, ns.clone(), &obj).await;
                let indexed = obj.clone();
                let mut related = exec
                    .spawn(async move {
                        let mut related = related;
                        related.index(&indexed);
                        related
                    })
                    .await;
                // Prometheus can be slow: show the rest first, keeping the previous charts.
                let pods_for_history = if wants_history { related.pods.clone() } else { Vec::new() };
                related.history = this.update(cx, |this, _| this.related.history.take()).ok().flatten();
                related.gpu = this.update(cx, |this, _| this.related.gpu.take()).ok().flatten();
                related.container_history =
                    this.update(cx, |this, _| std::mem::take(&mut this.related.container_history)).unwrap_or_default();
                let ok = this.update_in(cx, |this, window, cx| {
                    // Workload logs follow all of its pods, as they come and go.
                    let names: Vec<String> = related
                        .pods
                        .iter()
                        .filter_map(|p| p.pointer("/metadata/name").and_then(|v| v.as_str()).map(String::from))
                        .collect();
                    if pods_logs && !names.is_empty() {
                        match &this.logs {
                            None => {
                                let (client, ns) =
                                    (this.client.clone(), this.obj.namespace.clone().unwrap_or_default());
                                this.logs = Some(cx.new(|cx| LogsView::new(client, ns, names, window, cx)));
                                this.resume_logs_if_shown(cx);
                            }
                            Some(logs) => logs.update(cx, |l, cx| l.set_pods(names, cx)),
                        }
                    }
                    // A thousand pods take a while to free: not on the UI thread.
                    let old = std::mem::replace(&mut this.related, related);
                    cx.background_executor().spawn(async move { drop(old) }).detach();
                    #[cfg(feature = "dev")]
                    if std::env::var("RECOUVRANCE_COMPARE").is_ok() && this.compare_rev.is_none() {
                        let mut revs: Vec<(i64, String)> = this
                            .related
                            .revisions
                            .iter()
                            .map(|r| {
                                let n = r
                                    .pointer("/metadata/annotations/deployment.kubernetes.io~1revision")
                                    .and_then(|v| v.as_str())
                                    .and_then(|v| v.parse().ok())
                                    .unwrap_or(0);
                                (
                                    n,
                                    r.pointer("/metadata/name")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or_default()
                                        .to_string(),
                                )
                            })
                            .collect();
                        revs.sort();
                        if revs.len() > 1 {
                            this.compare_rev = Some(revs[revs.len() - 2].1.clone());
                        }
                    }
                    cx.notify();
                });
                if ok.is_err() {
                    break;
                }
                if wants_history {
                    let minutes = this.update(cx, |this, _| this.history_minutes).unwrap_or(60);
                    let (history, containers, gpu) = futures::join!(
                        history_for(client.clone(), &context, &kind, ns.as_deref(), &obj, &pods_for_history, minutes),
                        container_history_for(client.clone(), &context, &kind, ns.as_deref(), &obj, minutes),
                        gpu_history_for(
                            client.clone(),
                            &context,
                            &kind,
                            ns.as_deref(),
                            &obj,
                            &pods_for_history,
                            minutes
                        )
                    );
                    if this
                        .update(cx, |this, cx| {
                            this.related.history = history;
                            this.related.gpu = gpu;
                            this.related.container_history = containers;
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                cx.background_executor().timer(crate::settings::secs(crate::settings::get().metrics_refresh)).await;
            }
        });
        self.keep_task(task);
    }

    fn refresh_yaml(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(value) = &self.object else { return };
        let mut value = value.clone();
        if !self.show_managed
            && let Some(m) = value.get_mut("metadata").and_then(|m| m.as_object_mut())
        {
            m.remove("managedFields");
        }
        match k8s::to_yaml(&value, self.obj.kind.is("", "Secret"), self.reveal_secrets) {
            Ok(text) => {
                self.yaml_error = None;
                self.yaml_text = text.clone();
                self.yaml.update(cx, |s, cx| s.set_value(text, window, cx));
            }
            Err(e) => self.yaml_error = Some(format!("{e:#}").into()),
        }
    }

    // MARK: Write actions (only when write mode is on; each one confirmed)

    pub fn toggle_field_docs(&mut self, on: bool, cx: &mut Context<Self>) {
        self.field_docs = on.then(|| {
            let (ctx, client, editor) = (self.context.clone(), self.client.clone(), self.yaml.clone());
            cx.new(|cx| crate::ui::field_docs::FieldDocs::new(ctx, client, editor, cx))
        });
        cx.notify();
    }

    /// "pod web-1" for tab titles.
    pub fn title(&self) -> String {
        format!("{} {}", self.obj.kind.ar.kind, self.obj.name)
    }

    pub fn toggle_env(&mut self, cx: &mut Context<Self>) {
        self.env_expanded = !self.env_expanded;
        cx.notify();
    }

    // MARK: Compare

    fn set_object(&mut self, value: serde_json::Value) {
        if self.obj.kind.is("apiextensions.k8s.io", "CustomResourceDefinition") {
            self.schema_rows = crate::overview::crd_rows(&value);
        }
        self.object = Some(value);
    }

    fn ctx<'a>(&'a self, value: &'a serde_json::Value, cx: &Context<Self>) -> Ctx<'a> {
        self.ctx_for(value, cx.entity().downgrade())
    }

    fn ctx_for<'a>(&'a self, value: &'a serde_json::Value, panel: WeakEntity<Self>) -> Ctx<'a> {
        Ctx {
            compare: self.compare_rev.as_deref(),
            obj: value,
            kind: &self.obj.kind,
            related: &self.related,
            panel,
            reveal: self.reveal_secrets,
            revealed_keys: &self.revealed_keys,
            history_minutes: self.history_minutes,
            env_expanded: self.env_expanded,
            schema_rows: &self.schema_rows,
        }
    }

    /// Show or hide one Secret value on the overview.
    pub fn toggle_secret_key(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.revealed_keys.remove(key) {
            self.revealed_keys.insert(key.to_string());
        }
        cx.notify();
    }

    fn load_events(&mut self, cx: &mut Context<Self>) {
        let fut = k8s::events_for(self.client.clone(), self.obj.clone());
        self.keep_task(cx.spawn(async move |this, cx| {
            let result = fut.await.map_err(|e| SharedString::from(k8s::short_error(&e)));
            let _ = this.update(cx, |this, cx| {
                this.events = Some(result);
                cx.notify();
            });
        }));
    }
}

impl Drop for DetailPanel {
    fn drop(&mut self) {
        if let Some(pod) = self.node_shell.take() {
            k8s::delete_node_shell(self.client.clone(), pod);
        }
    }
}
