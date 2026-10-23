//! Main window: context and namespace pickers, kind sidebar, live table, details.

use std::collections::{BTreeMap, HashMap, HashSet};

use std::sync::Arc;

use futures::StreamExt as _;

use gpui_kit::assets::IconName as Lucide;

use gpui_kit::component::button::{Button, ButtonVariants as _};

use gpui_kit::component::checkbox::Checkbox;

use gpui_kit::component::input::{Input, InputEvent, InputState};

use gpui_kit::component::notification::Notification;

use gpui_kit::component::popover::Popover;

use gpui_kit::component::table::{DataTable, TableDelegate as _, TableEvent, TableState};

use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, Theme, ThemeMode, h_flex, h_resizable,
    resizable_panel, v_flex,
};

use gpui_kit::component::{TitleBar, WindowExt as _};

use gpui_kit::prelude::FluentBuilder as _;

use gpui_kit::*;

use kube::Client;

use crate::agents::AgentsView;

use crate::ui::cluster::ClusterOverview;

use crate::ui::ctx_picker::{ContextEvent, ContextPicker};

use crate::ui::dashboard::Dashboard;

use crate::details::{DetailPanel, OpenObject};

use crate::favorites::{ColumnPrefs, FavObject, Favorites, SavedView};

use crate::forwards::Forwards;

use crate::ui::helm::{HelmList, HelmRelease, OpenRelease};

use crate::k8s::{self, Kind, ObjectRef, TableMsg, TaskGuard};

use crate::mcp::{self, Approval, Hub, HubEvent};

use crate::ui::ns_picker::{NamespacePicked, NamespacePicker, NamespacesChanged};

use crate::ui::palette::{Item as PaletteItem, Palette, PaletteEvent, Target};

use crate::ui::settings_view::SettingsView;

use crate::table::ResourceTable;

mod pages;
#[allow(unused_imports)]
pub(crate) use pages::*;
mod watch;
#[allow(unused_imports)]
pub(crate) use watch::*;
mod contexts;
#[allow(unused_imports)]
pub(crate) use contexts::*;
mod chrome;

#[cfg(feature = "dev")]
mod demo;

mod navigation;

mod render;

mod sidebar;

mod table_page;

mod theme;

mod view_cache;

#[allow(unused_imports)]
pub(crate) use chrome::*;

#[cfg(feature = "dev")]
#[allow(unused_imports)]
pub(crate) use demo::*;

#[allow(unused_imports)]
pub(crate) use navigation::*;

#[allow(unused_imports)]
pub(crate) use render::*;

#[allow(unused_imports)]
pub(crate) use sidebar::*;

#[allow(unused_imports)]
pub(crate) use table_page::*;

#[allow(unused_imports)]
pub(crate) use theme::*;

/// Where the main area is; kept in a history for back/forward.
#[derive(Clone)]
enum Route {
    Resources(Option<Kind>),
    Detail(Entity<DetailPanel>),
    Workloads(Entity<Dashboard>),
    Cluster(Entity<ClusterOverview>),
    Problems(Entity<crate::ui::problems_view::ProblemsView>),
    Access(Entity<crate::ui::access_view::AccessView>),
    HelmInstall(Entity<crate::ui::helm_install::HelmInstall>),
    Assistant(Entity<crate::ui::assistant_view::AssistantView>),
    Helm(Entity<HelmList>),
    HelmRelease(Entity<HelmRelease>),
    Contexts,
    Settings,
    Terminal(Entity<crate::ui::terminal::TerminalView>),
    Apply(Entity<crate::ui::apply_view::ApplyView>),
    Agents,
}

impl Route {
    fn is_agents(&self) -> bool {
        matches!(self, Route::Agents)
    }

    fn is_resources(&self) -> bool {
        matches!(self, Route::Resources(_) | Route::Detail(_))
    }
}

/// The page each window shows. Hidden pages (in back/forward history) stop
/// polling and streaming until shown again.
/// A tab: its own back/forward history, filter and namespaces. The active tab's
/// state lives in the AppView fields; the others are kept here.
#[derive(Clone, Default)]
struct TabState {
    history: Vec<Route>,
    pos: usize,
    filter: String,
    namespaces: Vec<String>,
    /// The resource type its list shows.
    kind: Option<Kind>,
}

#[derive(Default)]
struct VisiblePages(HashMap<EntityId, EntityId>);

impl Global for VisiblePages {}

pub fn is_visible(page: EntityId, cx: &App) -> bool {
    cx.try_global::<VisiblePages>().is_some_and(|v| v.0.values().any(|p| *p == page))
}

/// Wait until this page is shown; false once it is gone.
pub async fn wait_visible<T: 'static, C: AppContext>(
    this: &WeakEntity<T>,
    cx: &mut C,
    exec: &BackgroundExecutor,
) -> bool {
    loop {
        match this.update(cx, |_, cx| is_visible(cx.entity_id(), cx)) {
            Err(_) => return false,
            Ok(true) => return true,
            Ok(false) => exec.timer(std::time::Duration::from_millis(500)).await,
        }
    }
}

/// The agent hub and its MCP server, shared by every window.
struct SharedHub(Arc<Hub>, #[allow(dead_code)] Arc<TaskGuard>);

impl Global for SharedHub {}

/// Open a main window (at startup and with Ctrl+Shift+N).
pub fn open_main_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1500.), px(920.)), cx);
    // The app draws its own title bar (GNOME on Wayland has no server-side
    // decorations): dragging, window controls and resize borders.
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_decorations: Some(WindowDecorations::Client),
        window_min_size: Some(size(px(900.), px(560.))),
        app_id: Some("recouvrance".into()),
        ..TitleBar::window_options()
    };
    if let Err(e) = open_window(options, cx, |window, cx| cx.new(|cx| AppView::new(window, cx))) {
        eprintln!("could not open a window: {e:#}");
    }
}

gpui_kit::actions!(
    Recouvrance,
    [
        NewWindow,
        NewTab,
        CloseTab,
        NextTab,
        PrevTab,
        RowOpenTab,
        EscapeBack,
        OpenSelected,
        ToggleSidebar,
        RefreshPage,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        OpenPalette,
        GoBack,
        GoForward,
        FocusFilter,
        RowOpen,
        RowLogs,
        RowShell,
        RowYaml,
        RowEvents,
        RowCopyName,
        RowFavorite,
        RowRun,
        RowScale,
        RowRestart,
        RowDelete
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(key_bindings());
}

fn key_bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("ctrl-k", OpenPalette, None),
        KeyBinding::new("ctrl-shift-n", NewWindow, None),
        KeyBinding::new("ctrl-t", NewTab, None),
        KeyBinding::new("ctrl-w", CloseTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-pagedown", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevTab, None),
        KeyBinding::new("ctrl-pageup", PrevTab, None),
        KeyBinding::new("ctrl-b", ToggleSidebar, None),
        KeyBinding::new("f5", RefreshPage, None),
        KeyBinding::new("ctrl-r", RefreshPage, None),
        KeyBinding::new("ctrl-=", ZoomIn, None),
        KeyBinding::new("ctrl-+", ZoomIn, None),
        KeyBinding::new("ctrl-shift-=", ZoomIn, None),
        KeyBinding::new("ctrl--", ZoomOut, None),
        KeyBinding::new("ctrl-0", ZoomReset, None),
        KeyBinding::new("alt-left", GoBack, None),
        KeyBinding::new("alt-right", GoForward, None),
        KeyBinding::new("ctrl-f", FocusFilter, Some("App")),
        // Inputs and dialogs handle Escape themselves (deeper contexts win).
        KeyBinding::new("escape", EscapeBack, Some("App")),
        // Only while the table has focus, so typing "/" elsewhere is untouched.
        KeyBinding::new("/", FocusFilter, Some("DataTable")),
        KeyBinding::new("enter", OpenSelected, Some("DataTable")),
        // F5 goes to the program running in a terminal, like the other function keys.
        KeyBinding::new("f5", NoAction, Some("Terminal")),
    ]
}

/// Names of common objects across connected clusters, for the palette.
#[derive(Clone)]
struct IndexEntry {
    ctx: usize,
    kind: String,
    label: String,
    icon: Lucide,
    namespace: Option<String>,
    name: String,
}

/// Kinds the palette searches across whole clusters.
const SEARCH_KINDS: [&str; 12] = [
    "deployments",
    "statefulsets",
    "daemonsets",
    "cronjobs",
    "services",
    "ingresses",
    "configmaps",
    "secrets",
    "persistentvolumeclaims",
    "namespaces",
    "nodes",
    "pods",
];

pub struct AppView {
    search_index: Vec<IndexEntry>,
    index_built: Option<std::time::Instant>,
    _index_tasks: Vec<Task<()>>,
    focus: FocusHandle,
    palette: Option<Entity<Palette>>,
    favorites: Entity<Favorites>,
    column_prefs: ColumnPrefs,
    all_contexts: Vec<String>,
    hub: Arc<Hub>,
    settings_view: Entity<SettingsView>,
    /// Local shells, one per context, kept while the app runs.
    terminals: HashMap<String, Entity<crate::ui::terminal::TerminalView>>,
    history: Vec<Route>,
    pos: usize,
    /// Every tab; the slot at `active_tab` is a placeholder for the live state.
    tabs: Vec<TabState>,
    active_tab: usize,
    agents: Entity<AgentsView>,
    approvals: Vec<Arc<Approval>>,
    forwards: Entity<Forwards>,
    _hub_events: Task<()>,
    /// Contexts ticked on the picker page.
    picked: std::collections::BTreeSet<String>,
    ns_picker: Entity<NamespacePicker>,
    ctx_picker: Entity<ContextPicker>,
    /// "Add column" form: header and JSONPath.
    col_header: Entity<InputState>,
    col_path: Entity<InputState>,
    /// Top bar popovers (controlled, so a pick closes them).
    ctx_open: bool,
    ns_open: bool,
    /// (critical, warnings) across the connected clusters, refreshed in the background.
    problem_counts: Option<(usize, usize)>,
    /// Kinds the problem scan could not read (refused or failed): the counts are partial.
    problem_gaps: Option<SharedString>,
    _problem_watch: Option<Task<()>>,
    /// Sidebar hidden (Ctrl+B).
    sidebar_hidden: bool,
    /// Selection events the app caused itself (keeping the selection on its row
    /// after an update); they must not open the row like a click does.
    skip_select: Option<usize>,
    /// The contexts and namespace each Workloads or Problems page was built for.
    page_scopes: HashMap<EntityId, (Vec<String>, Option<String>)>,
    /// Selected namespaces; empty means all.
    namespaces: Vec<String>,
    filter: Entity<InputState>,
    kind_filter: Entity<InputState>,
    /// Primary context (sidebar kinds, overview, Helm); first of `clusters`.
    context: Option<String>,
    client: Option<Client>,
    /// Every connected context, primary first.
    clusters: Vec<(String, Client)>,
    /// Which clusters serve each kind (by `Kind::key`).
    kind_clusters: HashMap<String, Vec<usize>>,
    /// Clusters whose initial list is still arriving.
    listing: HashSet<usize>,
    /// Clusters that refused to list the kind (403).
    refused: HashSet<usize>,
    /// Latest rows per cluster, merged into the table.
    rows_by_ctx: Vec<Vec<Arc<k8s::Row>>>,
    /// Lists shown recently, shown again at once when you come back to them.
    views: view_cache::ViewCache,
    /// What the list on screen is (clusters, kind, namespace, custom columns), to keep it
    /// in `views` when another list replaces it.
    view_key: Option<String>,
    kinds: Vec<Kind>,
    kind: Option<Kind>,
    namespace: Option<String>,
    expanded_groups: HashSet<String>,
    table: Entity<TableState<ResourceTable>>,
    status: Option<SharedString>,
    /// Contexts that failed to connect, until the next connect.
    connect_errors: Option<SharedString>,
    /// Each cluster's last watch error, until that cluster sends rows again.
    watch_errors: BTreeMap<usize, SharedString>,
    watch: Option<Vec<(TaskGuard, Task<()>)>>,
    metrics: Option<Vec<(TaskGuard, Task<()>)>>,
    _connect: Option<Task<()>>,
    _kubeconfig_watch: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl AppView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (contexts, current, status) = match k8s::load_contexts() {
            Ok(c) => (c.names, dev_var("RECOUVRANCE_CONTEXT").or(c.current), None),
            Err(e) => (Vec::new(), None, Some(format!("{e:#}").into())),
        };
        let ns_picker = cx.new(|cx| NamespacePicker::new(window, cx));
        let ctx_picker = cx.new(|cx| ContextPicker::new(contexts.clone(), window, cx));
        let col_header = cx.new(|cx| InputState::new(window, cx).placeholder("Header, e.g. Image"));
        let col_path = cx.new(|cx| InputState::new(window, cx).placeholder(".spec.containers[*].image"));
        let filter = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Filter: text · !text · app=web · ns:team-* · status:Running · restarts>3 · /regex/")
        });
        let kind_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter resources…"));
        // A header click sorts (see `render_th`); it no longer highlights the column too.
        let table = cx.new(|cx| TableState::new(ResourceTable::new(), window, cx).col_selectable(false));

        let subs = vec![
            cx.subscribe(&ns_picker, |this, _, ev: &NamespacesChanged, cx| {
                this.set_selected_namespaces(ev.0.clone(), cx);
            }),
            cx.subscribe(&ns_picker, |this, _, _: &NamespacePicked, cx| {
                this.ns_open = false;
                cx.notify();
            }),
            cx.subscribe_in(&ctx_picker, window, |this, _, ev: &ContextEvent, window, cx| {
                this.ctx_open = false;
                match ev {
                    ContextEvent::Connect(list) => {
                        this.picked = list.iter().cloned().collect();
                        this.connect_many(list.clone(), window, cx);
                    }
                    ContextEvent::Manage => this.navigate(Route::Contexts, cx),
                }
                cx.notify();
            }),
            cx.subscribe_in(&filter, window, |this, state, ev: &InputEvent, _, cx| match ev {
                InputEvent::Change => {
                    let text = state.read(cx).value().to_string();
                    this.table.update(cx, |t, cx| {
                        t.delegate_mut().set_filter(text);
                        cx.notify();
                    });
                    this.follow_selection(cx);
                }
                // A filter that was used (Enter, or leaving the field) goes into the history.
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    crate::settings::remember_filter(state.read(cx).value().as_ref())
                }
                _ => {}
            }),
            cx.subscribe_in(&kind_filter, window, |_, _, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    cx.notify();
                }
            }),
            // Checked rows change the selection bar; a clicked namespace becomes the filter.
            cx.observe_in(&table, window, |this, table, window, cx| {
                if let Some((ctx, kind, name, ns)) = table.update(cx, |t, _| t.delegate_mut().pending_open.take()) {
                    this.open_object_in(ctx, &kind, name, ns, window, cx);
                }
                if let Some(ns) = table.update(cx, |t, _| t.delegate_mut().pending_namespace.take()) {
                    this.ns_picker.update(cx, |p, cx| {
                        crate::settings::visit_namespaces(&p.context, std::slice::from_ref(&ns));
                        p.selected = [ns.clone()].into();
                        cx.notify();
                    });
                    this.set_selected_namespaces(vec![ns], cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(&table, window, |this, table, ev: &TableEvent, _, cx| {
                let Some(kind) = this.kind.as_ref().map(|k| k.kubectl_name()) else { return };
                match ev {
                    TableEvent::ColumnWidthsChanged(widths) => {
                        let z = zoom(cx);
                        let headers = table.read(cx).delegate().headers();
                        let w: HashMap<String, f32> = headers
                            .into_iter()
                            .zip(widths)
                            .filter(|(h, _)| !h.is_empty())
                            .map(|(h, w)| (h, f32::from(*w) / z))
                            .collect();
                        // The table keeps them too, so a column rebuild does not undo the resize.
                        table.update(cx, |t, _| t.delegate_mut().widths.extend(w.clone()));
                        this.column_prefs.set_widths(kind, w);
                    }
                    TableEvent::MoveColumn(..) => {
                        let order = table.read(cx).delegate().movable_order();
                        table.update(cx, |t, _| t.delegate_mut().remember_order(order.clone()));
                        this.column_prefs.set(kind, Some(order));
                    }
                    _ => {}
                }
            }),
            // A click on a row opens it; arrow keys only move the selection (Enter opens).
            cx.subscribe_in(&table, window, |this, table, ev: &TableEvent, window, cx| match ev {
                TableEvent::SelectRow(ix) => {
                    // The selection following its row after an update is not a click.
                    if this.skip_select.take() == Some(*ix) {
                        return;
                    }
                    // Arrow keys move the selection too: updates keep it on that row.
                    let pressed = table.update(cx, |t, _| {
                        let d = t.delegate_mut();
                        let picked = d.row_context(*ix).zip(d.row(*ix).map(|r| r.uid.clone()));
                        d.selected = picked.or(d.selected.take());
                        d.row_pressed.take()
                    });
                    if pressed.is_some_and(|t| t.elapsed() < std::time::Duration::from_millis(800)) {
                        this.open_detail(*ix, window, cx);
                    }
                }
                // Remember the row itself: row menu actions run later, after updates may have moved it.
                TableEvent::RightClickedRow(Some(ix)) => {
                    let target = {
                        let d = table.read(cx).delegate();
                        d.row_context(*ix).zip(d.row(*ix).map(|r| r.uid.clone()))
                    };
                    table.update(cx, |t, _| t.delegate_mut().menu_row = target);
                }
                _ => {}
            }),
            // "System" follows the desktop switching between light and dark while the app runs.
            cx.observe_window_appearance(window, |_, window, cx| {
                let mode = system_theme(window.appearance());
                if follows_system(cx) && mode.is_dark() != cx.theme().mode.is_dark() {
                    apply_theme(mode, Some(window), cx);
                }
            }),
        ];

        // One agent hub and MCP server for the whole app, shared by its windows.
        if !cx.has_global::<SharedHub>() {
            let hub = Hub::load();
            hub.claim_config();
            let _ = hub.apply_rules_file();
            hub.watch_rules_file();
            let server = mcp::server::start(hub.clone());
            cx.set_global(SharedHub(hub, Arc::new(server)));
        }
        let hub = cx.global::<SharedHub>().0.clone();
        // Subscribe first: the agents view reads the last status, later ones arrive here.
        let mut hub_rx = hub.subscribe();
        let agents = cx.new(|cx| AgentsView::new(hub.clone(), window, cx));
        let hub_events = cx.spawn(async move |this, cx| {
            while let Some(ev) = hub_rx.next().await {
                let alive = this.update(cx, |this, cx| this.on_hub_event(&ev, cx));
                if alive.is_err() {
                    break;
                }
            }
        });

        let forwards = cx.new(|_| Forwards::default());
        let mut subs = subs;
        subs.push(cx.observe(&forwards, |_, _, cx| cx.notify()));

        let favorites = cx.new(|_| Favorites::load());
        subs.push(cx.observe(&favorites, |_, _, cx| cx.notify()));
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        table.update(cx, |t, _| t.delegate_mut().app_focus = Some(focus.clone()));
        let settings_view = cx.new(|cx| SettingsView::new(hub.clone(), window, cx));
        let mut this = Self {
            settings_view,
            terminals: HashMap::new(),
            search_index: Vec::new(),
            index_built: None,
            _index_tasks: Vec::new(),
            focus,
            palette: None,
            column_prefs: ColumnPrefs::load(),
            favorites,
            all_contexts: contexts.clone(),
            forwards,
            hub,
            history: vec![if dev_var("RECOUVRANCE_PAGE").as_deref() == Some("agents") {
                Route::Agents
            } else {
                Route::Resources(None)
            }],
            pos: 0,
            agents,
            approvals: Vec::new(),
            _hub_events: hub_events,
            picked: current.iter().cloned().collect(),
            ns_picker,
            ctx_picker,
            col_header,
            col_path,
            ctx_open: false,
            ns_open: false,
            skip_select: None,
            page_scopes: HashMap::new(),
            sidebar_hidden: false,
            problem_counts: None,
            problem_gaps: None,
            _problem_watch: None,
            namespaces: Vec::new(),
            filter,
            kind_filter,
            context: None,
            client: None,
            clusters: Vec::new(),
            kind_clusters: HashMap::new(),
            rows_by_ctx: Vec::new(),
            views: Default::default(),
            view_key: None,
            listing: HashSet::new(),
            refused: HashSet::new(),
            kinds: Vec::new(),
            kind: None,
            namespace: None,
            expanded_groups: HashSet::new(),
            table,
            status,
            connect_errors: None,
            watch_errors: BTreeMap::new(),
            watch: None,
            metrics: None,
            _connect: None,
            _kubeconfig_watch: None,
            tabs: vec![TabState::default()],
            active_tab: 0,
            _subs: subs,
        };
        // Start on the context picker, unless a context is given.
        let saved = crate::settings::get();
        match (dev_var("RECOUVRANCE_CONTEXT"), current) {
            (Some(ctx), _) => this.connect_many(ctx.split(',').map(String::from).collect(), window, cx),
            _ if saved.startup == crate::settings::Startup::Reconnect && !saved.last_contexts.is_empty() => {
                let known: Vec<String> =
                    saved.last_contexts.into_iter().filter(|c| this.all_contexts.contains(c)).collect();
                if known.is_empty() {
                    this.history = vec![Route::Contexts];
                } else {
                    this.connect_many(known, window, cx);
                }
            }
            _ if this.history[0].is_agents() => {}
            _ => this.history = vec![Route::Contexts],
        }
        this.watch_kubeconfigs(window, cx);
        this
    }

    fn on_hub_event(&mut self, ev: &HubEvent, cx: &mut Context<Self>) {
        match ev {
            HubEvent::ApprovalRequested(a) => {
                self.approvals.push(a.clone());
                notify_desktop(&format!("{} asks to run {}", a.agent, a.tool), &a.summary);
            }
            HubEvent::ApprovalResolved(id) => self.approvals.retain(|a| a.id != *id),
            HubEvent::ServerStatus(status) => {
                let status = status.clone();
                self.agents.update(cx, |v, cx| {
                    v.server_status = status;
                    cx.notify();
                });
            }
            HubEvent::Audit(_) | HubEvent::AgentSeen => {}
        }
        // The agents screen shows live activity and connection state.
        self.agents.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    fn route(&self) -> &Route {
        &self.history[self.pos]
    }

    fn detail(&self) -> Option<&Entity<DetailPanel>> {
        match self.route() {
            Route::Detail(d) => Some(d),
            _ => None,
        }
    }

    // MARK: Tabs

    /// Reload what is shown now (F5): the list, or a fresh overview page.
    fn refresh_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let clusters = self.clusters.clone();
        let route = match self.route().clone() {
            Route::Resources(_) => {
                self.restart_watch(cx);
                return;
            }
            Route::Cluster(_) => Route::Cluster(cx.new(|cx| ClusterOverview::new(clusters, cx))),
            Route::Workloads(_) => Route::Workloads(cx.new(|cx| Dashboard::new(clusters, self.namespace.clone(), cx))),
            Route::Problems(_) => {
                let (kinds, ns) = (self.kinds.clone(), self.namespace.clone());
                Route::Problems(cx.new(|cx| crate::ui::problems_view::ProblemsView::new(clusters, kinds, ns, cx)))
            }
            Route::Helm(_) => Route::Helm(cx.new(|cx| HelmList::new(clusters, self.namespace.clone(), window, cx))),
            _ => return,
        };
        match &route {
            Route::Workloads(p) => self.remember_scope(p.entity_id()),
            Route::Problems(p) => self.remember_scope(p.entity_id()),
            _ => {}
        }
        // Same place in history, fresh data (links keep working: subscribe like the openers do).
        match &route {
            Route::Cluster(p) => cx
                .subscribe_in(p, window, |this, _, ev: &OpenObject, window, cx| {
                    let ctx =
                        ev.context.as_ref().and_then(|c| this.clusters.iter().position(|(n, _)| n == c)).unwrap_or(0);
                    this.open_object_in(ctx, &ev.kind, ev.name.clone(), ev.namespace.clone(), window, cx);
                })
                .detach(),
            Route::Workloads(p) => cx
                .subscribe_in(p, window, |this, _, ev: &OpenObject, window, cx| {
                    let ctx =
                        ev.context.as_ref().and_then(|c| this.clusters.iter().position(|(n, _)| n == c)).unwrap_or(0);
                    this.open_object_in(ctx, &ev.kind, ev.name.clone(), ev.namespace.clone(), window, cx);
                })
                .detach(),
            Route::Problems(p) => cx
                .subscribe_in(p, window, |this, _, ev: &OpenObject, window, cx| {
                    let ctx =
                        ev.context.as_ref().and_then(|c| this.clusters.iter().position(|(n, _)| n == c)).unwrap_or(0);
                    this.open_object_in(ctx, &ev.kind, ev.name.clone(), ev.namespace.clone(), window, cx);
                })
                .detach(),
            Route::Helm(p) => self.subscribe_helm(p, window, cx),
            _ => return,
        }
        if let Some(slot) = self.history.get_mut(self.pos) {
            *slot = route;
        }
        self.apply_route(cx);
        window.push_notification(Notification::info("Refreshed"), cx);
    }
}

/// A developer hook from the environment (`RECOUVRANCE_CONTEXT`, `_PAGE`…), read only in
/// `--features dev` builds: a stray variable never changes what a release build opens.
pub(crate) fn dev_var(name: &str) -> Option<String> {
    #[cfg(feature = "dev")]
    return std::env::var(name).ok();
    #[cfg(not(feature = "dev"))]
    {
        let _ = name;
        None
    }
}

/// Desktop notification (best effort), so approvals are seen while Recouvrance is in the background.
fn notify_desktop(title: &str, body: &str) {
    // Approval texts can hold a whole manifest; the notification only needs the first lines.
    let mut body: String = body.lines().take(3).collect::<Vec<_>>().join("\n");
    if body.chars().count() > 200 {
        body = body.chars().take(200).collect::<String>() + "…";
    }
    let body = body.as_str();
    // `--`: a title or body starting with '-' (an agent's text) is never read as an option.
    crate::forwards::spawn_reaped(
        std::process::Command::new("notify-send")
            .args(["--app-name=recouvrance", "--urgency=critical", "--icon=dialog-question", "--", title, body])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    );
}

/// Close the open dialog and show Settings → General, where what deleting asks is set.
pub fn open_delete_settings(window: &mut Window, cx: &mut App) {
    window.close_dialog(cx);
    let Some(Some(root)) = window.root::<base::Root>() else { return };
    let Ok(app) = root.read(cx).view().clone().downcast::<AppView>() else { return };
    app.update(cx, |this, cx| {
        this.settings_view.update(cx, |s, cx| s.show_general(cx));
        this.navigate(Route::Settings, cx);
    });
}

#[cfg(test)]
mod tests {
    use gpui_kit::{KeyContext, Keymap, Keystroke};

    #[test]
    fn f5_reaches_the_terminal() {
        let keymap = Keymap::new(super::key_bindings());
        let f5 = [Keystroke::parse("f5").unwrap()];
        let app = KeyContext::parse("App").unwrap();
        let (bindings, _) = keymap.bindings_for_input(&f5, std::slice::from_ref(&app));
        assert_eq!(bindings.len(), 1);
        let (bindings, _) = keymap.bindings_for_input(&f5, &[app, KeyContext::parse("Terminal").unwrap()]);
        assert!(bindings.is_empty());
    }
}
