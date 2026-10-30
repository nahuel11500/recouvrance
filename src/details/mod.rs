//! Detail page for one object: overview, containers, revisions, logs, YAML, history, events, shell.

use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::EditorState;
use gpui_kit::component::notification::Notification;
use gpui_kit::*;
use kube::Client;

use crate::favorites::Favorites;
use crate::forwards::Forwards;
use crate::k8s::{self, EventLine, ObjectRef};
use crate::overview::{Ctx, Related};
use crate::ui::terminal::TerminalView;

mod access_actions;
mod action_bar;
mod actions;
mod argo;
mod compare;
#[cfg(feature = "dev")]
mod demo;
mod files;
mod forward_options;
mod guard;
mod history;
mod logs;
mod reachability;
mod related;
mod tabs;

pub(crate) use actions::*;
pub(crate) use argo::OpenOnContext;
pub(crate) use compare::diff_block;
pub(crate) use guard::*;
use history::HistoryState;
pub(crate) use logs::*;
use related::{container_history_for, container_names, fetch_related, gpu_history_for, history_for};
use tabs::error_box;

#[derive(Clone, Copy, PartialEq)]
enum DetailTab {
    Overview,
    /// A pod's container, by index in spec.containers.
    Container(usize),
    Revisions,
    /// The path a request takes to a Service, an Ingress or an HTTPRoute.
    Reachability,
    Map,
    Compare,
    /// What changed: revisions, Helm and Argo CD history, field managers, events.
    History,
    Logs,
    Yaml,
    Events,
    Shell,
    /// The shell in a debug container, next to the pod's own.
    Debug,
}

/// Whether the user turned on write actions (off by default).
pub struct WriteMode(pub bool);

impl Global for WriteMode {}

/// Turn write mode on or off, for the UI and for its clients' requests, which check it again
/// when they are sent.
pub fn set_writes(on: bool, cx: &mut App) {
    k8s::gate::set_ui_writes(on);
    cx.set_global(WriteMode(on));
}

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
                set_writes(true, cx);
                cx.refresh_windows();
                true
            })
    });
    false
}

/// Write mode is still on. Checked again after a dry run or a lookup, and when a write is
/// confirmed: it may have been turned off since the write was asked for. Says so when off.
pub fn writes_still_on(window: &mut Window, cx: &mut App) -> bool {
    if writes_enabled(cx) {
        return true;
    }
    window.push_notification(Notification::error("Write mode is off: nothing was changed."), cx);
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

/// Ask the assistant why this object is broken (the Diagnosis card).
#[derive(Clone)]
pub struct Investigate {
    pub kind: String,
    pub name: String,
    pub namespace: Option<String>,
    /// What the diagnosis found, a finding a line.
    pub findings: String,
}

impl EventEmitter<Investigate> for DetailPanel {}

impl EventEmitter<crate::plugins::RunPlugin> for DetailPanel {}

pub struct DetailPanel {
    client: Client,
    context: String,
    obj: ObjectRef,
    tab: DetailTab,
    yaml: Entity<EditorState>,
    yaml_error: Option<SharedString>,
    object: Option<serde_json::Value>,
    /// When the read of the object shown started.
    object_started: Option<std::time::Instant>,
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
    /// How the overview's pods table is sorted (kept across reloads).
    pod_order: crate::overview::PodOrder,
    /// One-shot reads of the object, and of its events: the latest wins.
    object_read: crate::latest::Latest,
    events_load: crate::latest::Latest,
    /// The Reachability tab's check, and its test from this machine.
    reach: reachability::Reach,
    /// The Argo CD Applications that deploy the object or into the namespace, when the read
    /// shown started, and a read made at once (after a Sync).
    argo: k8s::argo::Lookup,
    argo_started: Option<std::time::Instant>,
    argo_load: crate::latest::Latest,
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
    /// The shell in the last debug container started (the pod's own one stays).
    debug_terminal: Option<Entity<TerminalView>>,
    forwards: Entity<Forwards>,
    favorites: Entity<Favorites>,
    /// Every connected cluster (for Compare).
    pub(crate) clusters: Vec<(String, Client)>,
    compare: CompareState,
    history: HistoryState,
    /// The related polling and RBAC check run (started on the first read that worked).
    loops_started: bool,
    /// The live refresh runs (started once; it also retries a first read that failed).
    live_started: bool,
    /// A row-menu action waiting for the object's first load (its dialog reads it).
    pending_action: Option<String>,
    /// The object is gone from the cluster (a read answered 404): polling stopped.
    deleted: bool,
    _tasks: Vec<Task<()>>,
    /// The node shell's end and the data editor's save: replaced when another opens.
    _shell_sub: Option<Subscription>,
    _data_editor_sub: Option<Subscription>,
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
            cx.new(|cx| LogsView::new(client, ns, vec![(name, Vec::new())], window, cx))
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
            object_started: None,
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
            pod_order: None,
            object_read: Default::default(),
            events_load: Default::default(),
            reach: Default::default(),
            argo: Default::default(),
            argo_started: None,
            argo_load: Default::default(),
            schema_rows: Vec::new(),
            data_editor: None,
            show_managed: false,
            field_docs: None,
            events: None,
            logs,
            terminal,
            debug_terminal: None,
            forwards,
            favorites,
            clusters: Vec::new(),
            compare: CompareState::default(),
            history: HistoryState::default(),
            loops_started: false,
            live_started: false,
            pending_action: None,
            deleted: false,
            _tasks: Vec::new(),
            _shell_sub: None,
            _data_editor_sub: None,
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
                    |p, client| {
                        Box::pin(k8s::trigger_cronjob(
                            client,
                            p.obj.namespace.clone().unwrap_or_default(),
                            p.obj.name.clone(),
                        ))
                    },
                    window,
                    cx,
                );
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
                .confirm_buttons("Run", false, guard.as_ref())
                .on_ok(move |_, window, cx| {
                    if !guard_ok(guard.as_ref(), cx) {
                        return false;
                    }
                    if writes && !writes_still_on(window, cx) {
                        return true;
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
            l.update(cx, LogsView::resume);
        }
    }

    pub fn select_tab_named(&mut self, name: &str, cx: &mut Context<Self>) {
        self.tab = tab_named(name);
        self.resume_logs_if_shown(cx);
        if self.tab == DetailTab::Reachability {
            self.check_reachability(cx);
        }
        cx.notify();
    }

    /// The Reachability tab shown before the object was read is checked once it is.
    fn check_reachability_if_waiting(&mut self, cx: &mut Context<Self>) {
        if self.tab == DetailTab::Reachability && self.reach.unchecked() {
            self.check_reachability(cx);
        }
    }

    /// Read the object once; the first success also starts the page's polling loops, a
    /// failure the live refresh (which retries).
    fn load_object(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.refresh_object(false, window, cx);
    }

    /// One-shot re-read of the object (after a write, say). `reset_yaml` replaces the
    /// editor's text even when it differs from what was last loaded (after saving it).
    pub(super) fn refresh_object(&mut self, reset_yaml: bool, window: &mut Window, cx: &mut Context<Self>) {
        let fut = k8s::get_object_opts(self.client.clone(), self.obj.clone(), true);
        let started = std::time::Instant::now();
        // A newer one-shot read replaces this one.
        self.object_read.spawn_in(window, cx, fut, move |this, result, window, cx| {
            match result {
                Ok(value) => {
                    let taken = this.set_object(value, started, cx);
                    if taken {
                        this.pod_containers_read(window, cx);
                    }
                    if taken && reset_yaml || this.yaml.read(cx).value().as_ref() == this.yaml_text {
                        this.refresh_yaml(window, cx);
                    }
                    this.start_loops(window, cx);
                    this.check_reachability_if_waiting(cx);
                    if let Some(action) = this.pending_action.take() {
                        this.quick_action(&action, window, cx);
                    }
                }
                Err(e) => {
                    if k8s::is_not_found(&e) {
                        this.deleted = true;
                    } else {
                        // A timeout or a 5xx: keep trying, the page starts when a read works.
                        this.start_live(window, cx);
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
    }

    /// Keep a task alive with the page; finished ones are dropped on the way.
    pub(super) fn keep_task(&mut self, task: Task<()>) {
        self._tasks.retain(|t| !t.is_ready());
        self._tasks.push(task);
    }

    /// Once the object has been read: the related polling, the RBAC check and the live refresh.
    fn start_loops(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.loops_started {
            self.loops_started = true;
            self.load_related(window, cx);
            self.check_access(cx);
            self.start_argo(cx);
        }
        self.start_live(window, cx);
    }

    /// Re-read the object every 5s so status, conditions and YAML stay current (once).
    fn start_live(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.live_started {
            return;
        }
        self.live_started = true;
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
                                l.update(cx, LogsView::resume);
                            }
                        });
                    }
                    Ok(false) => {}
                }
                let started = std::time::Instant::now();
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
                    if !changed || !this.set_object(value, started, cx) {
                        return;
                    }
                    this.pod_containers_read(window, cx);
                    // Don't overwrite what the user is editing.
                    if this.yaml.read(cx).value().as_ref() == this.yaml_text {
                        this.refresh_yaml(window, cx);
                    }
                    // The first read failed: this one starts the page.
                    this.start_loops(window, cx);
                    this.check_reachability_if_waiting(cx);
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
                let related = fetch_related(client.clone(), &context, &kind, ns.clone(), &obj).await;
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
                let focus = crate::alerts::focus(&kind, &obj, &related.pods);
                related.alerts =
                    this.update(cx, |this, _| std::mem::take(&mut this.related.alerts)).unwrap_or_default();
                related.alerts_note = this.update(cx, |this, _| this.related.alerts_note.take()).ok().flatten();
                related.history = this.update(cx, |this, _| this.related.history.take()).ok().flatten();
                related.gpu = this.update(cx, |this, _| this.related.gpu.take()).ok().flatten();
                related.container_history =
                    this.update(cx, |this, _| std::mem::take(&mut this.related.container_history)).unwrap_or_default();
                let ok = this.update_in(cx, |this, window, cx| {
                    // Workload logs follow its pods (up to a limit), as they come and go.
                    let followed = followed_pods(&related.pods);
                    let left_out = related.pods.len() - followed.len();
                    let names: Vec<(String, Vec<String>)> = followed
                        .into_iter()
                        .filter_map(|p| {
                            let name = p.pointer("/metadata/name").and_then(|v| v.as_str())?;
                            Some((name.to_string(), k8s::pod_containers(p)))
                        })
                        .collect();
                    if pods_logs && !names.is_empty() {
                        match &this.logs {
                            None => {
                                let (client, ns) =
                                    (this.client.clone(), this.obj.namespace.clone().unwrap_or_default());
                                this.logs = Some(cx.new(|cx| {
                                    let mut logs = LogsView::new(client, ns, names, window, cx);
                                    logs.left_out = left_out;
                                    logs
                                }));
                                this.resume_logs_if_shown(cx);
                            }
                            Some(logs) => logs.update(cx, |l, cx| {
                                l.left_out = left_out;
                                l.set_pods(names, window, cx);
                                cx.notify();
                            }),
                        }
                    }
                    crate::overview::sort_pod_rows(&mut related.pod_rows, this.pod_order);
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
                let minutes = this.update(cx, |this, _| this.history_minutes).unwrap_or(60);
                let charts = async {
                    if !wants_history {
                        return None;
                    }
                    Some(futures::join!(
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
                    ))
                };
                // Alerts about this object, from the same Prometheus (shared by every page).
                let alerts = async {
                    let Some(focus) = &focus else { return None };
                    let alerts = crate::alerts::active(&context, client.clone()).await;
                    Some((crate::alerts::about(&alerts.list, focus), alerts.silences_note()))
                };
                let (charts, alerts) = futures::join!(charts, alerts);
                if this
                    .update(cx, |this, cx| {
                        if let Some((alerts, note)) = alerts {
                            this.related.alerts = alerts;
                            this.related.alerts_note = note;
                        }
                        // The range changed meanwhile: its own reload fills the charts.
                        if let Some((history, containers, gpu)) = charts
                            && this.history_minutes == minutes
                        {
                            this.related.history = history;
                            this.related.gpu = gpu;
                            this.related.container_history = containers;
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
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

    /// A click on a pods table's header: sort on that column, then the other way, then
    /// back to the order the pods were loaded in.
    pub fn sort_pods(&mut self, col: crate::overview::PodSort, cx: &mut Context<Self>) {
        self.pod_order = crate::overview::next_pod_order(self.pod_order, col);
        crate::overview::sort_pod_rows(&mut self.related.pod_rows, self.pod_order);
        cx.notify();
    }

    pub fn toggle_env(&mut self, cx: &mut Context<Self>) {
        self.env_expanded = !self.env_expanded;
        cx.notify();
    }

    // MARK: Compare

    /// Show this read of the object, unless it is older than the one shown: a periodic read
    /// that started before a write can arrive after the read made once it was done.
    /// Returns whether it was taken.
    fn set_object(&mut self, value: serde_json::Value, started: std::time::Instant, cx: &mut Context<Self>) -> bool {
        if let (Some(shown), Some(shown_started)) = (&self.object, self.object_started)
            && stale_read(&value, started, shown, shown_started)
        {
            return false;
        }
        self.object_started = Some(self.object_started.map_or(started, |s| s.max(started)));
        if self.obj.kind.is("apiextensions.k8s.io", "CustomResourceDefinition") {
            self.schema_rows = crate::overview::crd_rows(&value);
        }
        // Re-created under the same name (a StatefulSet's pod, say), or opened by name:
        // the events and logs shown are this object's. Logs follow the pod by name, so they
        // restart only for a re-created one, not on a page's first read.
        if let Some(uid) = new_uid(&self.obj.uid, &value) {
            let recreated = !self.obj.uid.is_empty() && !self.obj.uid.contains('/');
            self.obj.uid = uid;
            if recreated {
                self.reset_history();
            }
            self.load_events(cx);
            if let Some(logs) = self.logs.as_ref().filter(|_| recreated) {
                logs.update(cx, |l, cx| {
                    if !l.paused {
                        l.restart(cx);
                    }
                });
            }
        }
        self.object = Some(value);
        // Its field managers, an Application's deployments: kept current in the History tab.
        self.merge_history();
        true
    }

    /// A pod page's logs take the containers from the pod as read, so they know which one
    /// to read without waiting for their own listing (or when it fails). Its shell takes them
    /// too, so it execs in the default one (kubectl's choice) and offers the others.
    fn pod_containers_read(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.obj.kind.is("", "Pod") {
            return;
        }
        let Some(value) = &self.object else { return };
        if let Some(terminal) = &self.terminal {
            let (names, default) = (k8s::shell_containers(value), k8s::default_container(value));
            terminal.update(cx, |t, cx| t.set_containers(names, default, window, cx));
        }
        let Some(logs) = &self.logs else { return };
        let pods = own_pod_logs(&self.obj.name, value);
        logs.update(cx, |l, cx| l.set_pods(pods, window, cx));
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
            pod_order: self.pod_order,
            argo: &self.argo,
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
        // After several writes in a row, only the last list of events is shown.
        self.events_load.spawn(cx, fut, |this, result, cx| {
            this.events = Some(result.map_err(|e| SharedString::from(k8s::short_error(&e))));
            this.merge_history();
            cx.notify();
        });
    }
}

fn tab_named(name: &str) -> DetailTab {
    match name {
        "yaml" => DetailTab::Yaml,
        "events" => DetailTab::Events,
        "logs" => DetailTab::Logs,
        "revisions" => DetailTab::Revisions,
        "reachability" => DetailTab::Reachability,
        "map" => DetailTab::Map,
        "compare" => DetailTab::Compare,
        "history" => DetailTab::History,
        "container" => DetailTab::Container(0),
        "shell" => DetailTab::Shell,
        "debug" => DetailTab::Debug,
        _ => DetailTab::Overview,
    }
}

/// What a pod page's logs follow: the pod, with its containers (default first).
fn own_pod_logs(name: &str, pod: &serde_json::Value) -> Vec<(String, Vec<String>)> {
    vec![(name.to_string(), k8s::pod_containers(pod))]
}

/// Whether a read of the object is older than the one shown. The same object is compared
/// by version; a different one (deleted and re-created under its name) by when each read
/// started, so a slow read of the deleted object doesn't bring it back.
fn stale_read(
    read: &serde_json::Value,
    started: std::time::Instant,
    shown: &serde_json::Value,
    shown_started: std::time::Instant,
) -> bool {
    let other = read.pointer("/metadata/uid") != shown.pointer("/metadata/uid");
    k8s::older_version(read, shown) || (other && started < shown_started)
}

/// The uid of a read of the object when it isn't the one the page knows (which is empty
/// for a page opened by name).
fn new_uid(known: &str, read: &serde_json::Value) -> Option<String> {
    let uid = read.pointer("/metadata/uid")?.as_str()?;
    (!uid.is_empty() && uid != known).then(|| uid.to_string())
}

impl Drop for DetailPanel {
    fn drop(&mut self) {
        if let Some(pod) = self.node_shell.take() {
            k8s::delete_node_shell(self.client.clone(), pod);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DetailTab, new_uid, own_pod_logs, stale_read, tab_named};
    use serde_json::json;

    #[test]
    fn a_debug_shell_has_its_own_tab() {
        assert!(tab_named("debug") == DetailTab::Debug);
        assert!(tab_named("shell") == DetailTab::Shell);
        assert!(tab_named("debug") != tab_named("shell"));
        assert!(tab_named("reachability") == DetailTab::Reachability);
    }

    #[test]
    fn a_pod_page_logs_know_the_pods_containers() {
        let pod = json!({"spec": {"containers": [{"name": "app"}, {"name": "proxy"}]}});
        let names = |v: &[&str]| v.iter().map(ToString::to_string).collect::<Vec<_>>();
        assert_eq!(own_pod_logs("web-1", &pod), vec![("web-1".to_string(), names(&["app", "proxy"]))]);
    }

    #[test]
    fn a_recreated_object_has_a_new_uid() {
        let read = json!({"metadata": {"name": "db-0", "uid": "b"}});
        assert_eq!(new_uid("a", &read).as_deref(), Some("b"));
        assert_eq!(new_uid("", &read).as_deref(), Some("b"));
        assert_eq!(new_uid("b", &read), None);
        assert_eq!(new_uid("a", &json!({"metadata": {"name": "db-0"}})), None);
    }

    #[test]
    fn a_slow_read_of_a_deleted_object_does_not_come_back() {
        let early = std::time::Instant::now();
        let late = early + std::time::Duration::from_secs(1);
        let old = json!({"metadata": {"uid": "a", "resourceVersion": "5"}});
        let new = json!({"metadata": {"uid": "b", "resourceVersion": "9"}});
        // The re-created object is shown; the read of the old one started before it.
        assert!(stale_read(&old, early, &new, late));
        // A read that started later is the newer word on which object is there.
        assert!(!stale_read(&old, late, &new, early));
        assert!(!stale_read(&new, late, &old, early));
        // The same object goes by version, whenever its read started.
        let newer = json!({"metadata": {"uid": "a", "resourceVersion": "7"}});
        assert!(!stale_read(&newer, early, &old, late));
        assert!(stale_read(&old, late, &newer, early));
    }
}
