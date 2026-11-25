//! Detail page for one object: overview, containers, revisions, logs, YAML, history, events, files, shell.

use std::sync::Arc;

use futures::StreamExt as _;
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
mod browser;
mod compare;
#[cfg(feature = "dev")]
mod demo;
mod files;
mod forward_options;
mod guard;
mod history;
mod logs;
mod logs_page;
mod map;
mod reachability;
mod related;
mod tabs;
mod yaml_edit;

pub(crate) use actions::*;
pub(crate) use argo::{ArgoAction, OpenOn, OpenOnContext, confirm_api_action};
use browser::FileBrowser;
pub(crate) use compare::{diff_block, preview_block, preview_scroll};
pub(crate) use guard::*;
use history::HistoryState;
pub(crate) use logs::*;
pub(crate) use logs_page::{LogTarget, LogsPage};
use related::{PodsOf, container_history_for, container_names, fetch_related, gpu_history_for, history_for, pods_of};
use tabs::error_box;
pub(crate) use tabs::{heading_name, heading_path};

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
    /// A pod's files, read through exec.
    Files,
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

/// True when writes are on; otherwise offers to turn them on for an action by Argo CD's API,
/// saying what it is (`what`: `sync 17 Applications`) and on which Argo CD server: it goes
/// with the user's Argo CD login, not their kubeconfig. Returns false then.
pub fn ensure_argo_writes(what: &str, server: &str, window: &mut Window, cx: &mut App) -> bool {
    if writes_enabled(cx) {
        return true;
    }
    let text = argo_writes_text(what, server);
    window.open_alert_dialog(cx, move |alert, _, _| {
        alert
            .title("Turn writes on for Argo CD?")
            .description(text.clone())
            .show_cancel(true)
            .ok_text("Turn writes on")
            .on_ok(|_, _, cx| {
                set_writes(true, cx);
                cx.refresh_windows();
                true
            })
    });
    false
}

/// What the prompt of [`ensure_argo_writes`] says.
fn argo_writes_text(what: &str, server: &str) -> String {
    format!(
        "Recouvrance is read-only. Turning writes on lets you {what} on {server}, through Argo CD's API with your \
         Argo CD login: Argo CD applies it with its own access, not your kubeconfig. Write mode is the app's: until \
         you turn it off, the clusters' write actions (scale, delete, edit, shells, plugins) are on too. Every \
         action still asks for confirmation."
    )
}

/// Whether the window may write to `context`: not when it is read-only (Settings →
/// Contexts), whatever write mode says. Says so when it is. The gate refuses such writes
/// anyway when they are sent; this stops them before a dialog asks for nothing.
pub fn not_read_only(context: &str, window: &mut Window, cx: &mut App) -> bool {
    if !crate::settings::is_read_only(context) {
        return true;
    }
    window.push_notification(Notification::error(k8s::gate::read_only_refusal(context)), cx);
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
    /// The object that text was made from: Save sends the edit as of its version, and a
    /// newer live object means it changed on the cluster while it was edited.
    yaml_base: Option<serde_json::Value>,
    /// What changed on the cluster since the edit started, shown under that warning once
    /// asked for (worked out again when the object changes, not on every frame).
    yaml_compare: Option<crate::mcp::preview::Section>,
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
    /// What the Map tab read when it was last opened.
    map: map::MapState,
    /// The Argo CD Applications that deploy the object or into the namespace, when the read
    /// shown started, and a read made at once (after a Sync).
    argo: k8s::argo::Lookup,
    argo_started: Option<std::time::Instant>,
    argo_load: crate::latest::Latest,
    /// A CRD's spec fields, worked out when it is loaded rather than on every frame.
    schema_rows: Vec<crate::overview::SchemaRow>,
    /// An Application's resources table, worked out when it is read rather than on every frame.
    argo_resources: crate::overview::ArgoResources,
    /// An Application's diff as Argo CD renders it from git (read on a click), and its read.
    argo_git: crate::overview::ArgoGitDiff,
    argo_git_load: crate::latest::Latest,
    /// The alert whose rule is shown on the overview (read on a click), and its read.
    alert_rule: crate::overview::AlertRule,
    alert_rule_load: crate::latest::Latest,
    /// Inline editor of a ConfigMap's or Secret's values.
    data_editor: Option<Entity<crate::ui::data_edit::DataEditor>>,
    /// Show metadata.managedFields in the YAML (hidden by default).
    show_managed: bool,
    /// Field docs next to the YAML editor.
    field_docs: Option<Entity<crate::ui::field_docs::FieldDocs>>,
    /// Write actions the API server says this user may not do ("patch", "delete", …).
    denied: std::collections::HashSet<&'static str>,
    events: Option<Result<Vec<EventLine>, SharedString>>,
    /// The Events tab shows the warnings alone.
    warnings_only: bool,
    logs: Option<Entity<LogsView>>,
    terminal: Option<Entity<TerminalView>>,
    /// A pod's Files tab.
    files: Option<Entity<FileBrowser>>,
    /// The shell in the last debug container started (the pod's own one stays).
    debug_terminal: Option<Entity<TerminalView>>,
    forwards: Entity<Forwards>,
    favorites: Entity<Favorites>,
    /// Every connected cluster (for Compare).
    pub(crate) clusters: Vec<(String, Client)>,
    /// The kinds this page's context serves (the Map reads an operator's kinds only when served).
    pub(crate) kinds: Vec<k8s::Kind>,
    compare: CompareState,
    history: HistoryState,
    /// The page's pods, followed by a watch (`follow_pods`).
    pods_watch: PodsFollow,
    /// Works out the pods table's rows off the UI thread: the latest wins.
    pods_index: crate::latest::Latest,
    /// The related reads and the RBAC check run (started on the first read that worked).
    loops_started: bool,
    /// The object's watch runs (started once; it also retries a first read that failed).
    live_started: bool,
    /// A row-menu action waiting for the object's first load (its dialog reads it).
    pending_action: Option<String>,
    /// The object is gone from the cluster (its watch saw it deleted, or a read answered 404).
    deleted: bool,
    _tasks: Vec<Task<()>>,
    /// The node shell's end and the data editor's save: replaced when another opens.
    _shell_sub: Option<Subscription>,
    _data_editor_sub: Option<Subscription>,
}

/// The page's pods as their watch follows them (`follow_pods`).
#[derive(Default)]
struct PodsFollow {
    /// What picks them: None before the object is read, `Some(None)` for a page without pods.
    of: Option<Option<PodsOf>>,
    /// Their first list came (or failed): the related reads, which read their usage, may start.
    known: bool,
    /// Counts the sets of pods shown: the related reads read what the page links to again when
    /// it moves.
    seen: u64,
    /// The watch and what shows its pods; dropping it stops both.
    _watch: Option<Task<()>>,
}

/// The longest the related reads wait for the pods' first list, whose usage they read.
const PODS_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// What the page links to (Services, revisions, events…) is read again after this many turns
/// of the related reads (`metrics_refresh` each) when nothing changed.
const LINKS_EVERY: u32 = 4;

/// How often a page's watches look where it is (shown, its window hidden, in history).
const PLACE_EVERY: std::time::Duration = std::time::Duration::from_secs(1);

/// Where a page is, for the watches that follow what it shows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Place {
    /// On screen.
    Shown,
    /// Its window's page, the window hidden: the watches go on, holding back what they send.
    Hidden,
    /// In the back/forward history: the watches stop, and start again when it is shown.
    Away,
}

/// Where the page is now; None once it is gone.
fn place<T: 'static, C: AppContext>(this: &WeakEntity<T>, cx: &mut C) -> Option<Place> {
    this.update(cx, |_, cx| {
        let page = cx.entity_id();
        if crate::app::is_visible(page, cx) {
            Place::Shown
        } else if crate::app::is_current(page, cx) {
            Place::Hidden
        } else {
            Place::Away
        }
    })
    .ok()
}

/// What a page's watch has next for it.
enum Next<M> {
    Msg(M),
    /// The watch ended: it was refused.
    Ended,
    /// The page went into the back/forward history: its watch stops.
    Away,
    /// The page is gone.
    Gone,
}

/// The next message of a page's watch, while the page is its window's page: the watch is
/// paused while that window is hidden. Where the page is, is looked at every `PLACE_EVERY` and
/// before each message.
async fn next_while_current<T: 'static, C: AppContext, M>(
    this: &WeakEntity<T>,
    cx: &mut C,
    exec: &BackgroundExecutor,
    rx: &mut futures::channel::mpsc::UnboundedReceiver<M>,
    pause: &k8s::Pause,
) -> Next<M> {
    loop {
        match place(this, cx) {
            None => return Next::Gone,
            Some(Place::Away) => return Next::Away,
            Some(place) => pause.set(place == Place::Hidden),
        }
        match futures::future::select(rx.next(), exec.timer(PLACE_EVERY)).await {
            futures::future::Either::Left((Some(msg), _)) => return Next::Msg(msg),
            futures::future::Either::Left((None, _)) => return Next::Ended,
            futures::future::Either::Right(_) => {}
        }
    }
}

/// Read the object every `detail_refresh` while the page is shown, as before it was watched:
/// where it can't be. Stops once it is deleted, or the page is gone.
async fn poll_object<C: AppContext>(
    this: &WeakEntity<DetailPanel>,
    cx: &mut C,
    exec: &BackgroundExecutor,
    client: Client,
    obj: ObjectRef,
) {
    loop {
        exec.timer(crate::settings::secs(crate::settings::get().detail_refresh)).await;
        match crate::app::wait_visible_or_away(this, cx, exec, DetailPanel::put_away).await {
            None => return,
            Some(true) => {
                let _ = this.update(cx, DetailPanel::resume_logs_if_shown);
            }
            Some(false) => {}
        }
        let started = std::time::Instant::now();
        let shown = match k8s::get_object_opts(client.clone(), obj.clone(), true).await {
            Ok(value) => this.update_in(cx, |this, window, cx| this.object_seen(value, started, window, cx)),
            // Deleted: say so and stop asking.
            Err(e) if k8s::is_not_found(&e) => {
                let _ = this.update(cx, DetailPanel::object_gone);
                return;
            }
            Err(_) => Ok(()),
        };
        if shown.is_err() {
            return;
        }
    }
}

/// What the objects a page links to were last read for: the object's version and its pods'
/// (`PodsFollow::seen`), and when.
struct LinksRead {
    version: String,
    pods: u64,
    at: std::time::Instant,
}

/// The object's version, by which the related reads see it changed.
fn version_of(obj: &serde_json::Value) -> String {
    obj.pointer("/metadata/resourceVersion").and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

/// Whether to read what a page links to again: never read, the object or its pods changed
/// since, or the last read is `every` old. What changes alone (a new Service, an edited
/// ConfigMap) shows in that long at most.
fn links_due(last: Option<&LinksRead>, now: &LinksRead, every: std::time::Duration) -> bool {
    last.is_none_or(|last| {
        last.version != now.version || last.pods != now.pods || now.at.duration_since(last.at) >= every
    })
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
    /// The comparison being fetched: a new one replaces (and cancels) it.
    fetch: crate::latest::Latest,
}

impl DetailPanel {
    /// The object shown, with its context.
    pub fn subject(&self) -> (String, ObjectRef) {
        (self.context.clone(), self.obj.clone())
    }

    /// The object shown as a favorite (an Argo CD Application by its instance).
    pub fn favorite(&self) -> crate::favorites::Favorite {
        let o = &self.obj;
        crate::favorites::Favorite::for_object(&self.context, &o.kind, o.namespace.clone(), o.name.clone())
    }

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
        let files = is_pod.then(|| {
            let (client, ns, name, ctx) = (client.clone(), ns.clone(), obj.name.clone(), context.clone());
            cx.new(|cx| FileBrowser::new(client, ctx, ns, name, window, cx))
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
            yaml_base: None,
            yaml_compare: None,
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
            map: Default::default(),
            argo: Default::default(),
            argo_started: None,
            argo_load: Default::default(),
            schema_rows: Vec::new(),
            argo_resources: Default::default(),
            argo_git: Default::default(),
            argo_git_load: Default::default(),
            alert_rule: Default::default(),
            alert_rule_load: Default::default(),
            data_editor: None,
            show_managed: false,
            field_docs: None,
            events: None,
            warnings_only: false,
            logs,
            terminal,
            files,
            debug_terminal: None,
            forwards,
            favorites,
            clusters: Vec::new(),
            kinds: Vec::new(),
            compare: CompareState::default(),
            history: HistoryState::default(),
            pods_watch: PodsFollow::default(),
            pods_index: Default::default(),
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
            // The Files tab runs its commands through exec.
            if kind.is("", "Pod") {
                let exec =
                    k8s::can_i(client.clone(), kind.clone(), "create", ns.clone(), Some(name.clone()), Some("exec"));
                checks.push(("exec", exec));
                let evict = k8s::can_i(
                    client.clone(),
                    kind.clone(),
                    "create",
                    ns.clone(),
                    Some(name.clone()),
                    Some("eviction"),
                );
                checks.push(("evict", evict));
                // A container that isn't running is read from a copy of the pod, deleted after.
                checks.push(("copy", k8s::can_i(client.clone(), kind.clone(), "create", ns.clone(), None, None)));
                checks
                    .push(("copy-delete", k8s::can_i(client.clone(), kind.clone(), "delete", ns.clone(), None, None)));
            }
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
                if let Some(files) = &this.files {
                    files.update(cx, |f, cx| f.set_exec_denied(denied.contains("exec"), cx));
                    files.update(cx, |f, cx| f.set_debug_denied(denied.contains("debug"), cx));
                    files.update(cx, |f, cx| {
                        f.set_copy_denied(denied.contains("copy"), denied.contains("copy-delete"), cx);
                    });
                }
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

    /// Show the rule of the alert with these labels and a chart of it, or hide it when it is
    /// the one shown. Read on this click only (the rules are kept a few minutes); a click on
    /// another alert replaces the read running.
    pub fn toggle_alert_rule(&mut self, labels: std::collections::BTreeMap<String, String>, cx: &mut Context<Self>) {
        if self.alert_rule.open.as_ref() == Some(&labels) {
            self.alert_rule = Default::default();
            self.alert_rule_load.cancel();
            cx.notify();
            return;
        }
        let Some(alert) = self.related.alerts.iter().find(|a| a.labels == labels).cloned() else { return };
        self.alert_rule = crate::overview::AlertRule { open: Some(labels), read: None };
        let read = crate::alerts::detail(self.context.clone(), self.client.clone(), alert);
        self.alert_rule_load.spawn(cx, read, |this, read, cx| {
            this.alert_rule.read = Some(read);
            cx.notify();
        });
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
        self.tab_opened(cx);
        cx.notify();
    }

    /// A tab that reads when it is opened reads again: the cluster moves on.
    fn tab_opened(&mut self, cx: &mut Context<Self>) {
        match self.tab {
            DetailTab::Reachability => self.check_reachability(cx),
            DetailTab::Map => self.load_map(cx),
            _ => {}
        }
    }

    /// The Reachability or Map tab shown before the object was read is read once it is.
    fn check_tab_if_waiting(&mut self, cx: &mut Context<Self>) {
        if self.tab == DetailTab::Reachability && self.reach.unchecked() {
            self.check_reachability(cx);
        }
        if self.tab == DetailTab::Map && self.map.unread() {
            self.load_map(cx);
        }
    }

    /// Read the object once; the first success also starts the page's watches and reads, a
    /// failure the object's watch (which retries).
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
                    // A selector saved from the YAML picks other pods at once.
                    this.follow_pods(window, cx);
                    this.check_tab_if_waiting(cx);
                    if let Some(action) = this.pending_action.take() {
                        this.quick_action(&action, window, cx);
                    }
                }
                Err(e) => {
                    if k8s::is_not_found(&e) {
                        this.deleted = true;
                    }
                    // Gone, a timeout or a 5xx: the object's watch follows its name (and tries
                    // again), and the page starts once it reads one, one created again included.
                    this.start_live(window, cx);
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

    /// Once the object has been read: its pods' watch, the related reads, the RBAC check, and
    /// the watch that follows the object.
    fn start_loops(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.loops_started {
            self.loops_started = true;
            self.follow_pods(window, cx);
            self.load_related(window, cx);
            self.check_access(cx);
            self.start_argo(cx);
        }
        self.start_live(window, cx);
    }

    /// Follow the object with a watch on it alone (once): status, conditions and YAML change
    /// about a second after the cluster does, and nothing is asked while nothing changes. The
    /// watch runs while the page is its window's page, holding back what it sends while that
    /// window is hidden; it stops when the page goes into history, and starts again (with a
    /// list) when it is shown. Where the object can't be watched (a role that only grants
    /// `get`), it is read every `detail_refresh` instead.
    fn start_live(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.live_started {
            return;
        }
        self.live_started = true;
        let (client, obj) = (self.client.clone(), self.obj.clone());
        let poll = crate::settings::secs(crate::settings::get().detail_refresh);
        let follow = k8s::Follow::object(obj.kind.ar.clone(), obj.namespace.clone(), &obj.name, poll);
        let task = cx.spawn_in(window, async move |this, cx| {
            let exec = cx.background_executor().clone();
            loop {
                // In history: no watch, and on each turn there, no log streams. A hidden window
                // keeps them, like its shells, until Back puts the page in history.
                match crate::app::wait_visible_or_away(&this, cx, &exec, Self::put_away).await {
                    None => return,
                    Some(true) => {
                        let _ = this.update(cx, Self::resume_logs_if_shown);
                    }
                    Some(false) => {}
                }
                let pause = k8s::Pause::default();
                let (watch, mut rx) = k8s::follow(client.clone(), follow.clone(), pause.clone());
                let refused = loop {
                    let read = match next_while_current(&this, cx, &exec, &mut rx, &pause).await {
                        Next::Msg(k8s::Followed::Objects(objects)) => {
                            this.update_in(cx, |this, window, cx| match objects.first() {
                                Some(value) => this.object_seen(value.clone(), std::time::Instant::now(), window, cx),
                                None => this.object_gone(cx),
                            })
                        }
                        // Tried again: the last state stays shown meanwhile.
                        Next::Msg(k8s::Followed::Error(_)) => Ok(()),
                        Next::Msg(k8s::Followed::Refused(_)) | Next::Ended => break true,
                        Next::Away => break false,
                        Next::Gone => return,
                    };
                    if read.is_err() {
                        return;
                    }
                };
                drop(watch);
                if refused {
                    poll_object(&this, cx, &exec, client.clone(), obj.clone()).await;
                    return;
                }
            }
        });
        self.keep_task(task);
    }

    /// A read of the object, from its watch (or its timer): shown unless older than the one
    /// shown. The first that works starts the page.
    fn object_seen(
        &mut self,
        value: serde_json::Value,
        started: std::time::Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_deleted = std::mem::replace(&mut self.deleted, false);
        let changed = self.object.as_ref() != Some(&value);
        if !changed || !self.set_object(value, started, cx) {
            if was_deleted {
                cx.notify();
            }
            return;
        }
        self.pod_containers_read(window, cx);
        // Don't overwrite what the user is editing.
        if self.yaml.read(cx).value().as_ref() == self.yaml_text {
            self.refresh_yaml(window, cx);
        }
        // The first read failed: this one starts the page.
        self.start_loops(window, cx);
        self.check_tab_if_waiting(cx);
        // A Service's or a PDB's selector can change, and its pods with it.
        self.follow_pods(window, cx);
        cx.notify();
    }

    /// The object is gone from the cluster: its watch saw it deleted, or none has its name. The
    /// watch goes on: one created again under its name is shown.
    fn object_gone(&mut self, cx: &mut Context<Self>) {
        if !std::mem::replace(&mut self.deleted, true) {
            cx.notify();
        }
    }

    /// On each turn the page spends in the back/forward history: its log streams stop, and a
    /// node shell pod nobody is connected to is not left running.
    fn put_away(&mut self, cx: &mut Context<Self>) {
        if let Some(l) = &self.logs {
            l.update(cx, |l, _| l.pause());
        }
        if self.node_shell.is_some() && !self.terminal.as_ref().is_some_and(|t| t.read(cx).connected()) {
            self.end_node_shell(cx);
        }
    }

    /// Follow the page's pods with a watch on what picks them (`related::pods_of`), started
    /// again when that changes (a Service's selector edited): they come and go on the page about
    /// a second after the cluster, instead of being listed again at each turn. Like the
    /// object's, the watch stops while the page is in history, and starts again when it is shown.
    fn follow_pods(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(obj) = &self.object else { return };
        let of = pods_of(&self.obj.kind, self.obj.namespace.as_deref(), obj);
        if self.pods_watch.of.as_ref() == Some(&of) {
            return;
        }
        let first = self.pods_watch.of.is_none();
        self.pods_watch.of = Some(of.clone());
        // The pods of what picked them before stop coming.
        self.pods_watch._watch = None;
        let Some(of) = of else {
            // Nothing picks pods (a selector emptied): none to show.
            self.pods_watch.known = true;
            if !first {
                self.related.errors.remove("pods");
                self.set_pods(Arc::default(), window, cx);
            }
            return;
        };
        self.pods_watch.known = false;
        let (client, follow) = (self.client.clone(), of.follow());
        let task = cx.spawn_in(window, async move |this, cx| {
            let exec = cx.background_executor().clone();
            loop {
                if !crate::app::wait_visible(&this, cx, &exec).await {
                    return;
                }
                let pause = k8s::Pause::default();
                let (_watch, mut rx) = k8s::follow(client.clone(), follow.clone(), pause.clone());
                loop {
                    let msg = match next_while_current(&this, cx, &exec, &mut rx, &pause).await {
                        Next::Msg(msg) => msg,
                        // Refused (said on the page), or the page is gone: nothing more to follow.
                        Next::Ended | Next::Gone => return,
                        Next::Away => break,
                    };
                    if this.update_in(cx, |this, window, cx| this.pods_followed(msg, window, cx)).is_err() {
                        return;
                    }
                }
            }
        });
        self.pods_watch._watch = Some(task);
    }

    /// What the pods' watch says: the pods now, or why they could not be read, said next to
    /// their heading (the last pods stay meanwhile).
    fn pods_followed(&mut self, msg: k8s::Followed, window: &mut Window, cx: &mut Context<Self>) {
        match msg {
            k8s::Followed::Objects(pods) => {
                self.related.errors.remove("pods");
                self.set_pods(pods, window, cx);
            }
            k8s::Followed::Error(why) => {
                self.related.errors.insert("pods", why);
            }
            k8s::Followed::Refused(why) => {
                self.related.errors.insert("pods", why);
                self.set_pods(Arc::default(), window, cx);
            }
        }
        self.pods_watch.known = true;
        cx.notify();
    }

    /// Show these pods: workload logs follow them, and their rows are worked out again.
    fn set_pods(&mut self, pods: Arc<Vec<serde_json::Value>>, window: &mut Window, cx: &mut Context<Self>) {
        self.pods_watch.seen += 1;
        let old = std::mem::replace(&mut self.related.pods, pods);
        // A thousand pods take a while to free: not on the UI thread.
        cx.background_executor().spawn(async move { drop(old) }).detach();
        self.follow_logs(window, cx);
        self.reindex_pods(cx);
        // The Map lists the pods its page holds.
        self.refresh_map_known(cx);
    }

    /// Workload logs follow its pods (up to a limit), as they come and go; a pod page has its own.
    fn follow_logs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.obj.kind.is("", "Pod") || self.obj.namespace.is_none() {
            return;
        }
        let followed = followed_pods(&self.related.pods);
        let left_out = self.related.pods.len() - followed.len();
        let names: Vec<(String, Vec<String>)> = followed
            .into_iter()
            .filter_map(|p| {
                let name = p.pointer("/metadata/name").and_then(|v| v.as_str())?;
                Some((name.to_string(), k8s::pod_containers(p)))
            })
            .collect();
        if names.is_empty() {
            return;
        }
        match &self.logs {
            None => {
                let (client, ns) = (self.client.clone(), self.obj.namespace.clone().unwrap_or_default());
                self.logs = Some(cx.new(|cx| {
                    let mut logs = LogsView::new(client, ns, names, window, cx);
                    logs.left_out = left_out;
                    logs
                }));
                self.resume_logs_if_shown(cx);
            }
            Some(logs) => logs.update(cx, |l, cx| {
                l.left_out = left_out;
                l.set_pods(names, window, cx);
                cx.notify();
            }),
        }
    }

    /// Work out the pods table's rows and a node's requests and limits again, off the UI
    /// thread, from the pods and their usage now: the latest wins.
    fn reindex_pods(&mut self, cx: &mut Context<Self>) {
        if self.related.pods.is_empty() {
            // Nothing to work out (a page without a pods table, or none left), and no copy of
            // the usage to make: a pod's page reads its whole namespace's.
            self.pods_index.cancel();
            self.related.allocated.clear();
            if !self.related.pod_rows.is_empty() {
                let old = std::mem::take(&mut self.related.pod_rows);
                cx.background_executor().spawn(async move { drop(old) }).detach();
            }
            return;
        }
        let (pods, usage, stats) =
            (self.related.pods.clone(), self.related.usage.clone(), self.related.node_stats.clone());
        let allocatable =
            self.object.as_ref().and_then(|o| o.pointer("/status/allocatable")).and_then(|a| a.as_object()).cloned();
        let work = cx
            .background_executor()
            .spawn(async move { crate::overview::pods_index(&pods, &usage, stats.as_deref(), allocatable.as_ref()) });
        self.pods_index.spawn(cx, work, |this, (rows, allocated), cx| {
            let old = std::mem::replace(&mut this.related.pod_rows, rows);
            cx.background_executor().spawn(async move { drop(old) }).detach();
            crate::overview::sort_pod_rows(&mut this.related.pod_rows, this.pod_order);
            this.related.allocated = allocated;
            cx.notify();
        });
    }

    /// Read what the overview shows next to the object, then again every `metrics_refresh`
    /// while the page is shown: the usage (metrics-server, the kubelet) and the charts and
    /// alerts (Prometheus) each time, and what it links to (Services, revisions, events…) when
    /// the object or its pods changed since, or every `LINKS_EVERY` turns. Its pods come from
    /// their watch (`follow_pods`).
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
        let task = cx.spawn_in(window, async move |this, cx| {
            // How often metrics-server measures, learnt over the reads, for the usage's age.
            let mut cadence = k8s::Cadence::default();
            // What the objects the page links to were last read for.
            let mut links: Option<LinksRead> = None;
            loop {
                let exec = cx.background_executor().clone();
                if !crate::app::wait_visible(&this, cx, &exec).await {
                    break;
                }
                // The usage is read for the pods the page lists: wait for their first list (a
                // little).
                let waiting = std::time::Instant::now();
                while this.update(cx, |this, _| !this.pods_watch.known).unwrap_or(false)
                    && waiting.elapsed() < PODS_WAIT
                {
                    exec.timer(std::time::Duration::from_millis(100)).await;
                }
                let every = crate::settings::secs(crate::settings::get().metrics_refresh);
                // The object as its watch last saw it, and the pods as theirs did. Nothing to
                // read while it is deleted: it may come back under its name.
                let Ok((obj, pods, seen, clusters)) = this.update(cx, |this, _| {
                    let obj = this.object.clone().filter(|_| !this.deleted);
                    (obj, this.related.pods.clone(), this.pods_watch.seen, this.clusters.clone())
                }) else {
                    break;
                };
                let Some(obj) = obj else {
                    exec.timer(every).await;
                    continue;
                };
                let read = LinksRead { version: version_of(&obj), pods: seen, at: std::time::Instant::now() };
                let again = links_due(links.as_ref(), &read, every * LINKS_EVERY);
                let mut related =
                    fetch_related(client.clone(), &context, &kind, ns.clone(), &obj, clusters, &pods, again).await;
                if again {
                    links = Some(read);
                }
                // A kubelet too slow this time keeps what it said last.
                if related.node_stats.is_none() {
                    related.node_stats = this.update(cx, |this, _| this.related.node_stats.clone()).ok().flatten();
                }
                if let Some(read) = related.usage_read {
                    // The page reads more than usage: it keeps its own pace, and only learns
                    // how often metrics-server measures.
                    cadence.next(&read, every);
                    related.usage_fresh = k8s::Freshness::new(&read, cadence.every());
                }
                // Prometheus can be slow: show the rest first, keeping the previous charts.
                let pods_for_history = if wants_history { pods.clone() } else { Arc::default() };
                let focus = crate::alerts::focus(&kind, &obj, &pods);
                let ok = this.update(cx, |this, cx| {
                    let old = this.related.take_reads(related, again);
                    // What it replaced can be big (every Role of the cluster): not freed on the UI thread.
                    cx.background_executor().spawn(async move { drop(old) }).detach();
                    // The pods' rows, with their new usage.
                    this.reindex_pods(cx);
                    if again {
                        // What the Map draws from the page changed with what it links to.
                        this.map_page_read(cx);
                    }
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
        let base = value.clone();
        let mut value = value.clone();
        if !self.show_managed
            && let Some(m) = value.get_mut("metadata").and_then(|m| m.as_object_mut())
        {
            m.remove("managedFields");
        }
        match k8s::to_yaml(&value, self.obj.kind.is("", "Secret"), self.reveal_secrets) {
            Ok(text) => {
                self.yaml_error = None;
                self.yaml_text.clone_from(&text);
                self.yaml_base = Some(base);
                self.yaml_compare = None;
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
        if self.obj.kind.is("argoproj.io", "Application") {
            self.argo_resources.set(&value);
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
        if self.yaml_compare.is_some() {
            self.yaml_compare = self.meanwhile_section();
        }
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
            let (names, default) = (k8s::pod_containers(value), k8s::default_container(value));
            terminal.update(cx, |t, cx| t.set_containers(names, default, window, cx));
        }
        if let Some(files) = &self.files {
            let (names, default) = (k8s::pod_containers(value), k8s::default_container(value));
            files.update(cx, |f, cx| f.set_containers(names, default, window, cx));
            files.update(cx, |f, cx| f.set_pod(value, cx));
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
            argo_resources: &self.argo_resources,
            argo_git: &self.argo_git,
            alert_rule: &self.alert_rule,
            context: &self.context,
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
        "files" => DetailTab::Files,
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
    use super::{DetailTab, LinksRead, links_due, new_uid, own_pod_logs, stale_read, tab_named, version_of};
    use serde_json::json;

    /// The prompt for an Argo CD action says what it is, on which server, and that it goes with
    /// the Argo CD login, not the kubeconfig; and that write mode is the whole app's.
    #[test]
    fn the_argo_cd_prompt_says_what_it_allows() {
        let t = super::argo_writes_text("sync 17 Applications", "argocd.example.com");
        assert!(t.contains("sync 17 Applications on argocd.example.com"), "{t}");
        assert!(t.contains("Argo CD login") && t.contains("not your kubeconfig"), "{t}");
        assert!(t.contains("Write mode is the app's"), "{t}");
    }

    /// What a page links to is read again when the object or its pods changed, and otherwise
    /// once in a while (a new Service changes neither): never at every turn for nothing.
    #[test]
    fn links_are_read_again_when_something_changed_or_after_a_while() {
        let start = std::time::Instant::now();
        let read = |version: &str, pods: u64, secs: u64| LinksRead {
            version: version.to_string(),
            pods,
            at: start + std::time::Duration::from_secs(secs),
        };
        let every = std::time::Duration::from_secs(60);
        assert!(links_due(None, &read("5", 0, 0), every));
        let last = read("5", 1, 0);
        assert!(!links_due(Some(&last), &read("5", 1, 15), every));
        assert!(!links_due(Some(&last), &read("5", 1, 59), every));
        // The object changed, its pods did, or a minute went by.
        assert!(links_due(Some(&last), &read("6", 1, 15), every));
        assert!(links_due(Some(&last), &read("5", 2, 15), every));
        assert!(links_due(Some(&last), &read("5", 1, 60), every));
        assert_eq!(version_of(&json!({"metadata": {"resourceVersion": "42"}})), "42");
    }

    #[test]
    fn a_debug_shell_has_its_own_tab() {
        assert!(tab_named("debug") == DetailTab::Debug);
        assert!(tab_named("shell") == DetailTab::Shell);
        assert!(tab_named("files") == DetailTab::Files);
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
