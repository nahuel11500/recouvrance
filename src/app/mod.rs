//! Main window: context and namespace pickers, kind sidebar, live table, details.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use futures::StreamExt as _;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::table::{TableEvent, TableState};
use gpui_kit::component::{TitleBar, WindowExt as _};
use gpui_kit::*;
use kube::Client;

use crate::agents::AgentsView;
use crate::details::DetailPanel;
use crate::favorites::{ColumnPrefs, Favorites};
use crate::forwards::Forwards;
use crate::k8s::{self, Kind, TaskGuard};
use crate::mcp::{self, Hub, HubEvent};
use crate::table::ResourceTable;
use crate::ui::cluster::ClusterOverview;
use crate::ui::ctx_picker::{ContextEvent, ContextPicker};
use crate::ui::dashboard::Dashboard;
use crate::ui::helm::{HelmList, HelmRelease};
use crate::ui::ns_picker::{NamespacePicked, NamespacePicker, NamespacesChanged};
use crate::ui::palette::Palette;
use crate::ui::settings_view::SettingsView;
use banners::ApprovalStrip;

mod banners;
mod chrome;
pub(crate) mod commands;
pub(crate) mod conn;
mod connect_view;
mod contexts;
#[cfg(feature = "dev")]
mod demo;
mod kept_lists;
mod kind_filters;
mod live_list;
mod menus;
mod navigation;
mod overviews;
mod pages;
mod pinned;
mod problem_badge;
mod reach;
mod render;
mod search_index;
mod session;
mod shortcuts;
mod sidebar;
mod table_page;
mod theme;
mod view_cache;
mod watch;

pub use menus::init as init_menus;
pub(crate) use shortcuts::{key_cap, shortcut_rows, with_keys};
use sidebar::section_icon;
pub(crate) use theme::*;

/// macOS takes ⌘ for the app's shortcuts where Linux takes Ctrl, and has a menu bar.
pub(crate) const MAC: bool = cfg!(target_os = "macos");

/// Where the main area is; kept in a history for back/forward.
#[derive(Clone)]
enum Route {
    Resources(Option<Kind>),
    Detail(Entity<DetailPanel>),
    Workloads(Entity<Dashboard>),
    Cluster(Entity<ClusterOverview>),
    Problems(Entity<crate::ui::problems_view::ProblemsView>),
    Timeline(Entity<crate::ui::timeline_view::TimelineView>),
    Access(Entity<crate::ui::access_view::AccessView>),
    HelmInstall(Entity<crate::ui::helm_install::HelmInstall>),
    Assistant(Entity<crate::ui::assistant_view::AssistantView>),
    Helm(Entity<HelmList>),
    HelmRelease(Entity<HelmRelease>),
    Argo(Entity<crate::ui::argo::ArgoView>),
    Contexts,
    Settings,
    Terminal(Entity<crate::ui::terminal::TerminalView>),
    Apply(Entity<crate::ui::apply_view::ApplyView>),
    /// The logs of the checked rows.
    Logs(Entity<crate::details::LogsPage>),
    Agents,
}

impl Route {
    fn is_agents(&self) -> bool {
        matches!(self, Self::Agents)
    }

    fn is_resources(&self) -> bool {
        matches!(self, Self::Resources(_) | Self::Detail(_))
    }

    /// The page shown, for the routes that are one.
    fn page(&self) -> Option<EntityId> {
        match self {
            Self::Detail(e) => Some(e.entity_id()),
            Self::Workloads(e) => Some(e.entity_id()),
            Self::Cluster(e) => Some(e.entity_id()),
            Self::Problems(e) => Some(e.entity_id()),
            Self::Timeline(e) => Some(e.entity_id()),
            Self::Access(e) => Some(e.entity_id()),
            Self::HelmInstall(e) => Some(e.entity_id()),
            Self::Assistant(e) => Some(e.entity_id()),
            Self::Helm(e) => Some(e.entity_id()),
            Self::HelmRelease(e) => Some(e.entity_id()),
            Self::Argo(e) => Some(e.entity_id()),
            Self::Terminal(e) => Some(e.entity_id()),
            Self::Apply(e) => Some(e.entity_id()),
            Self::Logs(e) => Some(e.entity_id()),
            Self::Resources(_) | Self::Contexts | Self::Settings | Self::Agents => None,
        }
    }
}

/// A tab: its own back/forward history, filter and namespaces. The active tab's
/// state lives in the `AppView` fields; the others are kept here.
#[derive(Clone, Default)]
struct TabState {
    history: Vec<Route>,
    pos: usize,
    filter: String,
    namespaces: Vec<String>,
    /// The resource type its list shows.
    kind: Option<Kind>,
    /// The overview it opens when first shown: a tab reopened on one (see `session`) builds
    /// its page only then.
    open: Option<overviews::Overview>,
    /// Reopened on a type of an API group that didn't answer: saved as it was until changed.
    kept: Option<session::KeptTab>,
}

/// The page each window shows, and the windows hidden as the desktop reports it (see
/// `AppView::window_hidden`). Pages hidden in the back/forward history, and every page of a
/// hidden window, stop polling until shown again.
#[derive(Default)]
struct VisiblePages {
    /// Each window's page, by the window's `AppView`.
    pages: HashMap<EntityId, EntityId>,
    /// The windows hidden, by their `AppView`.
    hidden: HashSet<EntityId>,
}

impl Global for VisiblePages {}

impl VisiblePages {
    /// Whether `page` is on screen: its window shows it, and is not hidden.
    fn on_screen(&self, page: EntityId) -> bool {
        self.pages.iter().any(|(window, p)| *p == page && !self.hidden.contains(window))
    }

    /// Whether `page` is the one its window shows, hidden or not.
    fn current(&self, page: EntityId) -> bool {
        self.pages.values().any(|p| *p == page)
    }
}

/// Whether `page` is on screen: the page its window shows, in a window not hidden.
pub fn is_visible(page: EntityId, cx: &App) -> bool {
    cx.try_global::<VisiblePages>().is_some_and(|v| v.on_screen(page))
}

/// Whether `page` is the page its window shows, even while that window is hidden (not one
/// in the back/forward history).
pub fn is_current(page: EntityId, cx: &App) -> bool {
    cx.try_global::<VisiblePages>().is_some_and(|v| v.current(page))
}

/// Put `page` on screen, or take it off: for view tests of pages that wait to be shown.
#[cfg(all(test, feature = "dev"))]
pub fn show_for_test(cx: &mut App, page: EntityId, shown: bool) {
    let window: EntityId = 999_999u64.into();
    let visible = cx.default_global::<VisiblePages>();
    if shown {
        visible.pages.insert(window, page);
    } else {
        visible.pages.remove(&window);
    }
}

/// Wait until this page is on screen; false once it is gone.
pub async fn wait_visible<T: 'static, C: AppContext>(
    this: &WeakEntity<T>,
    cx: &mut C,
    exec: &BackgroundExecutor,
) -> bool {
    wait_visible_or_away(this, cx, exec, |_, _| {}).await.is_some()
}

/// Wait until this page is on screen, calling `away` on each turn it spends in the
/// back/forward history (not its window's page). Checked on every turn: a page waiting while
/// its window is hidden can go into history before the window is shown again. Whether it
/// waited, once it is on screen; None once it is gone.
pub async fn wait_visible_or_away<T: 'static, C: AppContext>(
    this: &WeakEntity<T>,
    cx: &mut C,
    exec: &BackgroundExecutor,
    mut away: impl FnMut(&mut T, &mut Context<T>),
) -> Option<bool> {
    let mut waited = false;
    loop {
        let shown = this.update(cx, |this, cx| {
            let page = cx.entity_id();
            if !is_current(page, cx) {
                away(this, cx);
            }
            is_visible(page, cx)
        });
        match shown {
            Err(_) => return None,
            Ok(true) => return Some(waited),
            Ok(false) => {
                waited = true;
                exec.timer(std::time::Duration::from_millis(500)).await;
            }
        }
    }
}

/// The agent hub and its MCP server, shared by every window.
struct SharedHub(Arc<Hub>, #[allow(dead_code)] Arc<TaskGuard>);

impl Global for SharedHub {}

/// Open a main window: at startup, with Ctrl+Shift+N (⇧⌘N), and on macOS from the Dock or the
/// menu bar when none is open.
pub fn open_main_window(cx: &mut App) -> Option<Entity<AppView>> {
    use crate::settings::{WINDOW_MIN, WINDOW_SIZE};
    // The first window opens where the app was left; the others at the default size, centered.
    let bounds = if cx.has_global::<session::FirstWindowOpened>() {
        WindowBounds::Windowed(Bounds::centered(None, size(px(WINDOW_SIZE.0), px(WINDOW_SIZE.1)), cx))
    } else {
        session::first_window_bounds(cx)
    };
    // The app draws its own title bar (GNOME on Wayland has no server-side
    // decorations): dragging, window controls and resize borders.
    let options = WindowOptions {
        window_bounds: Some(bounds),
        window_decorations: Some(WindowDecorations::Client),
        window_min_size: Some(size(px(WINDOW_MIN.0), px(WINDOW_MIN.1))),
        app_id: Some("recouvrance".into()),
        ..TitleBar::window_options()
    };
    match open_window(options, cx, |window, cx| cx.new(|cx| AppView::new(window, cx))) {
        Ok((window, view)) => {
            // After an update, its release notes, once: the next window finds them seen.
            crate::ui::whats_new::show_once(window, cx);
            Some(view)
        }
        Err(e) => {
            eprintln!("could not open a window: {e:#}");
            None
        }
    }
}

/// The Dock icon was clicked with no window shown. macOS keeps the app running when its last
/// window closes: show a minimized window again, or open one.
pub fn reopen(cx: &mut App) {
    if let Some(&handle) = cx.windows().first() {
        let _ = handle.update(cx, |_, window, _| window.activate_window());
    } else {
        open_main_window(cx);
    }
}

gpui_kit::actions!(
    Recouvrance,
    [
        NewWindow,
        NewTab,
        CloseTab,
        CloseWindow,
        MinimizeWindow,
        ZoomWindow,
        Quit,
        HideApp,
        HideOtherApps,
        ShowAllApps,
        ShowAbout,
        OpenHelp,
        ReportIssue,
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
        RowDelete,
        ShowShortcuts,
        GoProblems,
        GoCluster,
        GoWorkloads,
        GoHelm,
        GoAssistant,
        GoSettings,
        ToggleAssistant,
        OpenArgo,
        OpenTimeline,
        OpenAccess,
        OpenUpgradeCheck,
        OpenAgents,
        OpenAgentActivity,
        OpenContexts,
        OpenTerminal,
        OpenApply,
        OpenHelmInstall,
        OpenApiRequests,
        ToggleWriteMode,
        ToggleTheme,
        ToggleFavorite
    ]
);

pub fn bind_keys(cx: &mut App) {
    cx.bind_keys(key_bindings_on(MAC));
}

/// The app's key bindings on macOS (`mac`) or Linux. A shortcut that takes Ctrl on Linux takes ⌘
/// on macOS, where Ctrl keys then reach the shell in a terminal; Ctrl+Tab and Ctrl+Page Up/Down
/// switch tabs on both, as in browsers. macOS also gets the keys every Mac app has (⌘Q, ⌘,, ⌘M…)
/// and ⌘[ / ⌘] for back and forward instead of Alt+arrows, which move by word in its shells.
fn key_bindings_on(mac: bool) -> Vec<KeyBinding> {
    // gpui's `secondary-` (⌘ on macOS, Ctrl elsewhere), spelled out for the platform asked rather
    // than the one gpui is built for: the macOS keymap can be checked on Linux too.
    let key = |keys: &str| keys.replace("secondary-", if mac { "cmd-" } else { "ctrl-" });
    let mut bindings = Vec::new();
    if mac {
        // First: the menu bar shows an action's first binding (⌘, for Settings, ⌘/ for the list).
        bindings.extend([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("cmd-,", GoSettings, None),
            KeyBinding::new("cmd-shift-w", CloseWindow, None),
            KeyBinding::new("cmd-m", MinimizeWindow, None),
            KeyBinding::new("cmd-h", HideApp, None),
            KeyBinding::new("cmd-alt-h", HideOtherApps, None),
            KeyBinding::new("cmd-/", ShowShortcuts, None),
            KeyBinding::new("cmd-[", GoBack, None),
            KeyBinding::new("cmd-]", GoForward, None),
        ]);
    } else {
        bindings.extend([KeyBinding::new("alt-left", GoBack, None), KeyBinding::new("alt-right", GoForward, None)]);
    }
    bindings.extend([
        KeyBinding::new(&key("secondary-k"), OpenPalette, None),
        KeyBinding::new(&key("secondary-shift-n"), NewWindow, None),
        KeyBinding::new(&key("secondary-t"), NewTab, None),
        KeyBinding::new(&key("secondary-w"), CloseTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-pagedown", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevTab, None),
        KeyBinding::new("ctrl-pageup", PrevTab, None),
        KeyBinding::new(&key("secondary-b"), ToggleSidebar, None),
        KeyBinding::new(&key("secondary-j"), ToggleAssistant, None),
        // Before F5, so the macOS menu bar shows ⌘R for Refresh, as Mac apps do.
        KeyBinding::new(&key("secondary-r"), RefreshPage, None),
        KeyBinding::new("f5", RefreshPage, None),
        KeyBinding::new(&key("secondary-="), ZoomIn, None),
        KeyBinding::new(&key("secondary-+"), ZoomIn, None),
        KeyBinding::new(&key("secondary-shift-="), ZoomIn, None),
        KeyBinding::new(&key("secondary--"), ZoomOut, None),
        KeyBinding::new(&key("secondary-0"), ZoomReset, None),
        KeyBinding::new(&key("secondary-f"), FocusFilter, Some("App")),
        // Inputs and dialogs handle Escape themselves (deeper contexts win).
        KeyBinding::new("escape", EscapeBack, Some("App")),
        // Only while the table has focus, so typing "/" elsewhere is untouched.
        KeyBinding::new("/", FocusFilter, Some("DataTable")),
        KeyBinding::new("enter", OpenSelected, Some("DataTable")),
        // F5 goes to the program running in a terminal, like the other function keys.
        KeyBinding::new("f5", NoAction, Some("Terminal")),
        // Single keys: typed as text in an input or a terminal (see `shortcuts::OUTSIDE_TEXT`).
        KeyBinding::new("?", ShowShortcuts, Some(shortcuts::OUTSIDE_TEXT)),
        KeyBinding::new("g p", GoProblems, Some(shortcuts::OUTSIDE_TEXT)),
        KeyBinding::new("g o", GoCluster, Some(shortcuts::OUTSIDE_TEXT)),
        KeyBinding::new("g w", GoWorkloads, Some(shortcuts::OUTSIDE_TEXT)),
        KeyBinding::new("g h", GoHelm, Some(shortcuts::OUTSIDE_TEXT)),
        KeyBinding::new("g i", GoAssistant, Some(shortcuts::OUTSIDE_TEXT)),
        KeyBinding::new("g s", GoSettings, Some(shortcuts::OUTSIDE_TEXT)),
        KeyBinding::new("*", ToggleFavorite, Some(shortcuts::OUTSIDE_TEXT)),
        KeyBinding::new("j", base::actions::SelectDown, Some(shortcuts::TABLE_OUTSIDE_TEXT)),
        KeyBinding::new("k", base::actions::SelectUp, Some(shortcuts::TABLE_OUTSIDE_TEXT)),
    ]);
    bindings
}

pub struct AppView {
    /// Object names across the clusters, for the palette.
    search_index: search_index::SearchIndex,
    focus: FocusHandle,
    palette: Option<Entity<Palette>>,
    /// The open palette's subscription, replaced on each open.
    _palette_sub: Option<Subscription>,
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
    /// The tab strip's sideways scroll.
    tab_scroll: ScrollHandle,
    agents: Entity<AgentsView>,
    /// The approval banners: those waiting, and those that just went, kept a moment.
    approvals: Entity<ApprovalStrip>,
    /// Redraws the banner's countdown each second while a request waits.
    countdown: Option<Task<()>>,
    /// How many requests the window title last said were waiting.
    title_waiting: Option<usize>,
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
    /// The sidebar's problem count, refreshed in the background.
    problems: problem_badge::ProblemBadge,
    /// Sidebar hidden (Ctrl+B).
    sidebar_hidden: bool,
    /// The window's assistant chat, the same on its page and beside the others; made when
    /// first opened.
    assistant: Option<Entity<crate::ui::assistant_view::AssistantView>>,
    /// The assistant shown beside the page (Ctrl+J).
    assistant_docked: bool,
    /// Selection events the app caused itself (keeping the selection on its row
    /// after an update); they must not open the row like a click does.
    skip_select: Option<usize>,
    /// The contexts and namespace each Workloads or Problems page was built for.
    page_scopes: HashMap<EntityId, (Vec<String>, Vec<String>)>,
    /// Opening an object on a context that isn't connected, or whose kind is looked up there
    /// (where an Argo CD Application deploys, and what it deploys there).
    context_open: crate::latest::Latest,
    /// A favorite to open once the connect under way is done (it is on a context that wasn't
    /// connected); any other connect drops it.
    after_connect: Option<crate::favorites::Favorite>,
    /// Selected namespaces; empty means all.
    namespaces: Vec<String>,
    filter: Entity<InputState>,
    /// The filter typed for each resource type.
    kind_filters: kind_filters::KindFilters,
    kind_filter: Entity<InputState>,
    /// Primary context (sidebar kinds, overview, Helm); first of `clusters`.
    context: Option<String>,
    client: Option<Client>,
    /// Every connected context, primary first.
    clusters: Vec<(String, Client)>,
    /// Which clusters serve each kind (by `Kind::key`).
    kind_clusters: HashMap<String, Vec<usize>>,
    /// The kind on screen, listed across the clusters.
    list: live_list::LiveList,
    /// Set while the list isn't on screen (see `list_on_screen`): its watches hold their
    /// rows back, its metrics stop polling.
    list_pause: k8s::Pause,
    /// The desktop reports the window hidden: minimized or on another workspace, and on
    /// macOS also fully covered by other windows. Wayland compositors without the xdg-shell
    /// `suspended` state never report it. Its pages stop polling, the list pauses and the
    /// problem badge waits.
    window_hidden: bool,
    /// Lists shown recently, shown again at once when you come back to them.
    views: view_cache::ViewCache,
    /// What the list on screen is (clusters, kind, namespace, custom columns), to keep it
    /// in `views` when another list replaces it.
    view_key: Option<String>,
    /// Lists left recently with their watches still going, by view key (see `kept_lists`).
    kept: kept_lists::KeptLists<watch::KeptList>,
    /// Drops the kept lists left too long ago, while `kept_sweeping`.
    kept_sweep: Option<Task<()>>,
    kept_sweeping: bool,
    /// The list whose watches may touch the table: the one on screen. A kept list's tasks
    /// carry the id they started with, and wait while it isn't this one.
    watch_id: u64,
    /// The last id given to a list.
    watch_seq: u64,
    kinds: Vec<Kind>,
    kind: Option<Kind>,
    namespace: Option<String>,
    expanded_groups: HashSet<String>,
    /// The port forwards at the bottom of the sidebar are listed (folded into one row else).
    forwards_open: bool,
    table: Entity<TableState<ResourceTable>>,
    /// Contexts that failed to connect, until the next connect.
    connect_errors: Option<SharedString>,
    /// Each context of the set that can't show rows: connecting, or why it failed.
    conn: conn::ConnStates,
    /// What the bar above the list says of the connect besides each context's state (a
    /// namespace list that failed, the kubeconfig that can't be read).
    /// Kept with full names: the status line names each context as the screen does.
    connect_notes: Vec<String>,
    /// The add-on API groups a connected cluster couldn't read (`k8s::SkippedGroup`), until
    /// the next connect: by cluster, for the status line, and their names.
    skipped: Vec<(String, Vec<k8s::SkippedGroup>)>,
    skipped_groups: HashSet<String>,
    /// What the session saved for those groups' types, kept to be saved again.
    kept_session: session::Kept,
    watch: Option<watch::ListWatch>,
    metrics: Option<Vec<(TaskGuard, Task<()>)>>,
    /// The timeline's own watches (`timeline.kinds`), while connected.
    recording: Vec<TaskGuard>,
    /// Each connected cluster's namespace watch, shared with the other windows: the picker
    /// follows namespaces created and deleted after the connect (`namespaces_moved`).
    namespace_feeds: Vec<k8s::namespace_feed::NamespaceHold>,
    _namespaces_follow: Option<Task<()>>,
    /// A connect is in flight (`_connect` stays set once it is done).
    connecting: bool,
    _connect: Option<Task<()>>,
    /// The Pods lists the connect started before discovery answered, until the list's watch
    /// takes them over (see `reach::EarlyList`).
    early: Option<reach::EarlyLists>,
    /// A cluster joined after the first rows (see `reach`): the session's tabs were reopened
    /// without what only it has, so the session is not saved over until every cluster of the
    /// set is in (`joined_whole`) or the next connect.
    joined_late: bool,
    /// The tabs as a connect that left clusters connecting ended with: once they have all
    /// joined, the session is reopened whole only if the user didn't change them since.
    partial_tabs: Option<(Vec<crate::settings::SavedTab>, usize)>,
    /// The set of contexts picked (`settings::session_set`), whose session this window saves;
    /// empty when none is saved (not connected yet, or contexts given by a developer hook).
    session_key: Vec<String>,
    /// Whether a connect reopens where its set of contexts was left: the first window's first
    /// set, until a connect to it reaches every cluster (see `session::Restore`).
    restore: session::Restore,
    /// This view, by which `session::SessionOwners` knows the window that saves a set.
    view_id: EntityId,
    /// Where the window was last, not maximized: what is saved while it is maximized.
    windowed: Option<crate::settings::WindowPlace>,
    /// The save waiting for changes to stop (see `session_changed`), and the window's.
    _save_session: Option<Task<()>>,
    _save_window: Option<Task<()>>,
    _kubeconfig_watch: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

impl AppView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (contexts, current, status) = match k8s::load_contexts() {
            Ok(c) => (c.names, dev_var("RECOUVRANCE_CONTEXT").or(c.current), None),
            Err(e) => (Vec::new(), None, Some(SharedString::from(format!("{e:#}")))),
        };
        // One store for every window: a window never saves over the stars of another.
        let favorites = crate::favorites::shared(cx);
        let ns_picker = cx.new(|cx| NamespacePicker::new(favorites.clone(), window, cx));
        let ctx_picker = cx.new(|cx| ContextPicker::new(contexts.clone(), favorites.clone(), window, cx));
        let col_header = cx.new(|cx| InputState::new(window, cx).placeholder("Header, e.g. Image"));
        let col_path = cx.new(|cx| InputState::new(window, cx).placeholder(".spec.containers[*].image"));
        let filter = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Filter: text · !text · app=web · ns:team-* · status:Running · restarts>3 · /regex/")
        });
        let kind_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Find a page or resource type…"));
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
                    this.labels_for_filter(cx);
                    this.follow_selection(cx);
                    this.session_changed(cx);
                }
                // A filter that was used (Enter, or leaving the field) goes into the history.
                InputEvent::PressEnter { .. } | InputEvent::Blur => {
                    crate::settings::remember_filter(state.read(cx).value().as_ref());
                }
                _ => {}
            }),
            cx.subscribe_in(&kind_filter, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
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
                let Some(kind) = this.kind.as_ref().map(Kind::kubectl_name) else { return };
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
            // Only changes count: a window starts shown, and one whose platform never says it
            // is hidden keeps working as before.
            cx.observe_window_visibility(window, |this, visibility, _, cx| {
                this.set_window_hidden(!visibility.is_visible(), cx);
            }),
            cx.observe_window_bounds(window, |this, window, cx| this.window_moved(window, cx)),
            // Where the window is left is saved as it closes, and as the app quits.
            cx.on_release(Self::leave_session),
            cx.on_app_quit(|this, cx| {
                this.save_session_now(cx);
                async {}
            }),
        ];
        let first_window = !cx.has_global::<session::FirstWindowOpened>();
        cx.set_global(session::FirstWindowOpened);

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
        let approvals = cx.new(|_| ApprovalStrip::new(hub.clone()));
        let hub_events = cx.spawn_in(window, async move |this, cx| {
            while let Some(ev) = hub_rx.next().await {
                let alive = this.update_in(cx, |this, window, cx| this.on_hub_event(&ev, window, cx));
                if alive.is_err() {
                    break;
                }
            }
        });

        let forwards = cx.new(|_| Forwards::default());
        let mut subs = subs;
        subs.push(cx.observe(&forwards, |_, _, cx| cx.notify()));

        subs.push(cx.observe(&favorites, |_, _, cx| cx.notify()));
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        table.update(cx, |t, _| t.delegate_mut().app_focus = Some(focus.clone()));
        let settings_view = cx.new(|cx| SettingsView::new(hub.clone(), window, cx));
        k8s::short_names::set_contexts(&contexts);
        let mut this = Self {
            settings_view,
            terminals: HashMap::new(),
            search_index: search_index::SearchIndex::default(),
            focus,
            palette: None,
            _palette_sub: None,
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
            approvals,
            countdown: None,
            title_waiting: None,
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
            context_open: Default::default(),
            after_connect: None,
            sidebar_hidden: false,
            assistant: None,
            assistant_docked: false,
            problems: problem_badge::ProblemBadge::default(),
            namespaces: Vec::new(),
            filter,
            kind_filters: Default::default(),
            kind_filter,
            context: None,
            client: None,
            clusters: Vec::new(),
            kind_clusters: HashMap::new(),
            recording: Vec::new(),
            namespace_feeds: Vec::new(),
            _namespaces_follow: None,
            views: Default::default(),
            list: live_list::LiveList::default(),
            list_pause: k8s::Pause::default(),
            window_hidden: false,
            view_key: None,
            kept: Default::default(),
            kept_sweep: None,
            kept_sweeping: false,
            watch_id: 0,
            watch_seq: 0,
            kinds: Vec::new(),
            kind: None,
            namespace: None,
            expanded_groups: HashSet::new(),
            forwards_open: false,
            table,
            // A kubeconfig that can't be read shows where connect errors do.
            connect_notes: status.iter().map(SharedString::to_string).collect(),
            connect_errors: status,
            conn: conn::ConnStates::default(),
            skipped_groups: HashSet::new(),
            skipped: Vec::new(),
            kept_session: session::Kept::default(),
            watch: None,
            metrics: None,
            connecting: false,
            _connect: None,
            early: None,
            joined_late: false,
            partial_tabs: None,
            session_key: Vec::new(),
            restore: if first_window { session::Restore::FirstConnect } else { session::Restore::Off },
            view_id: cx.entity_id(),
            windowed: None,
            _save_session: None,
            _save_window: None,
            _kubeconfig_watch: None,
            tabs: vec![TabState::default()],
            active_tab: 0,
            tab_scroll: ScrollHandle::new(),
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
        // The window has a name in task switchers and for screen readers.
        this.show_waiting(window, cx);
        this
    }

    fn on_hub_event(&mut self, ev: &HubEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            HubEvent::ApprovalRequested(a) => {
                let request = a.clone();
                self.approvals.update(cx, |s, cx| s.requested(request, cx));
                // Every window flashes in the taskbar or bounces the Dock (nothing happens to
                // the one in front): an approval nobody sees is refused after 2 minutes.
                window.request_attention();
                self.start_countdown(cx);
                // Each window hears of the request; one notification is enough.
                if first_to_notify(cx, a.id) {
                    // The request, then what it will change (the notification keeps the first lines).
                    let body = match &a.preview {
                        Some(p) => format!("{}\n{}", a.summary, p.text()),
                        None => a.summary.clone(),
                    };
                    notify_desktop(&format!("{} asks to run {}", a.agent, a.tool), &body);
                }
            }
            HubEvent::ApprovalResolved(id) => self.approvals.update(cx, |s, cx| s.resolved(*id, cx)),
            HubEvent::ServerStatus(status) => {
                let status = status.clone();
                self.agents.update(cx, |v, cx| {
                    v.set_server_status(status, cx);
                    cx.notify();
                });
            }
            HubEvent::Audit(_) | HubEvent::AgentSeen | HubEvent::Forwards => {}
        }
        self.show_waiting(window, cx);
        // The agents screen shows live activity and connection state.
        self.agents.update(cx, |_, cx| cx.notify());
        cx.notify();
    }

    /// The window title says how many requests wait, so a window behind others or in a
    /// task switcher shows it. Set only when the count changes.
    fn show_waiting(&mut self, window: &mut Window, cx: &App) {
        let waiting = self.approvals.read(cx).waiting_count();
        if self.title_waiting != Some(waiting) {
            self.title_waiting = Some(waiting);
            window.set_window_title(&window_title(waiting));
        }
    }

    /// Redraw each second while a request waits, for the banner's "refused in" time; it
    /// stops with the last request. A hidden window isn't drawn, so it skips the redraw.
    fn start_countdown(&mut self, cx: &mut Context<Self>) {
        if self.countdown.is_some() {
            return;
        }
        self.countdown = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                let waiting = this.update(cx, |this, cx| {
                    if this.approvals.read(cx).waiting_count() == 0 {
                        this.countdown = None;
                        return false;
                    }
                    if !this.window_hidden {
                        this.approvals.update(cx, |_, cx| cx.notify());
                    }
                    true
                });
                if !matches!(waiting, Ok(true)) {
                    break;
                }
            }
        }));
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

    /// The desktop hid the window or shows it again. Meanwhile its pages stop polling, the
    /// list holds its rows back and the problem badge waits; watches, log streams and
    /// shells keep going. A window on another monitor, shown but not focused, is not hidden.
    fn set_window_hidden(&mut self, hidden: bool, cx: &mut Context<Self>) {
        self.window_hidden = hidden;
        self.mark_visible(cx);
        cx.notify();
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

/// The window title: the app's name, and how many approvals wait while any do.
fn window_title(waiting: usize) -> String {
    match waiting {
        0 => "Recouvrance".into(),
        1 => "1 approval waiting — Recouvrance".into(),
        n => format!("{n} approvals waiting — Recouvrance"),
    }
}

/// The highest approval id a notification was sent for, across the app's windows.
struct NotifiedApproval(u64);

impl Global for NotifiedApproval {}

/// Whether this window is the first to hear of approval `id`, and so sends its notification.
/// Ids only grow, so a later window (or event) for the same request sends nothing.
fn first_to_notify(cx: &mut App, id: u64) -> bool {
    if cx.try_global::<NotifiedApproval>().is_some_and(|n| n.0 >= id) {
        return false;
    }
    cx.set_global(NotifiedApproval(id));
    true
}

/// Desktop notification (best effort), so approvals are seen while Recouvrance is in the background.
fn notify_desktop(title: &str, body: &str) {
    // Approval texts can hold a whole manifest; the notification only needs the first lines.
    let mut body: String = body.lines().take(3).collect::<Vec<_>>().join("\n");
    if body.chars().count() > 200 {
        body = body.chars().take(200).collect::<String>() + "…";
    }
    #[cfg(windows)]
    {
        // A native toast. Recouvrance has no registered AppUserModelID (it is not installed
        // through an MSIX package), so the toast is shown under PowerShell's, which Windows
        // always knows; PowerShell itself is not started. The text is set through the XML DOM,
        // so an agent's text cannot change the toast's markup. Showing it is a blocking WinRT
        // call: it runs off the UI thread.
        let title = title.to_string();
        std::thread::spawn(move || {
            use tauri_winrt_notification::{Duration, Toast};
            if let Err(e) =
                Toast::new(Toast::POWERSHELL_APP_ID).title(&title).text1(&body).duration(Duration::Long).show()
            {
                eprintln!("recouvrance: desktop notification failed: {e}");
            }
        });
    }
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut cmd = crate::process::command("osascript");
        cmd.args(osascript_args(title, &body));
        cmd
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let mut cmd = {
        let mut cmd = crate::process::command("notify-send");
        // `--`: a title or body starting with '-' (an agent's text) is never read as an option.
        // The body may be read as markup: the agent's text is escaped (no links, no formatting).
        cmd.args([
            "--app-name=recouvrance",
            "--urgency=critical",
            "--icon=dialog-question",
            "--",
            title,
            markup_escaped(&body).as_str(),
        ]);
        cmd
    };
    #[cfg(not(windows))]
    crate::forwards::spawn_reaped(cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()));
}

/// Text as plain text in a notification's body, which notification servers may read as markup
/// (the Desktop Notifications spec allows `<b>`, `<i>`, `<a href>` and `<img>`): `&`, `<` and
/// `>` are escaped, so an agent's text shows as written and can't add a link or an image.
#[cfg_attr(any(target_os = "macos", windows), allow(dead_code))]
fn markup_escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// osascript's arguments for a notification on macOS (Notification Center). The agent's text
/// is never part of the script, where it could run commands (`do shell script`): the script
/// reads it from its arguments. The fixed first argument ends osascript's options, so a text
/// starting with '-' is not read as one.
#[cfg(any(target_os = "macos", test))]
fn osascript_args<'a>(title: &'a str, body: &'a str) -> [&'a str; 9] {
    [
        "-e",
        "on run argv",
        "-e",
        "display notification (item 3 of argv) with title (item 2 of argv)",
        "-e",
        "end run",
        "recouvrance",
        title,
        body,
    ]
}

/// Close the open dialog and show Settings → General, where what deleting asks is set.
pub fn open_delete_settings(window: &mut Window, cx: &mut App) {
    window.close_dialog(cx);
    let Some(Some(root)) = window.root::<base::Root>() else { return };
    let Ok(app) = root.read(cx).view().clone().downcast::<AppView>() else { return };
    app.update(cx, |this, cx| {
        this.settings_view.update(cx, SettingsView::show_general);
        this.navigate(Route::Settings, cx);
    });
}

#[cfg(test)]
mod tests {
    use gpui_kit::{EntityId, KeyContext, Keymap, Keystroke};

    use super::VisiblePages;

    #[test]
    fn a_hidden_window_shows_no_page() {
        let (window, other, page, behind) = (1u64.into(), 2u64.into(), 3u64.into(), 4u64.into());
        let mut visible = VisiblePages::default();
        visible.pages.insert(window, page);
        visible.pages.insert(other, behind);
        assert!(visible.on_screen(page) && visible.current(page));
        visible.hidden.insert(window);
        assert!(!visible.on_screen(page), "its polling pauses");
        assert!(visible.current(page), "still its window's page: its log streams keep going");
        assert!(visible.on_screen(behind), "the other window is shown");
        // A page in the history is neither.
        let gone: EntityId = 5u64.into();
        assert!(!visible.on_screen(gone) && !visible.current(gone));
    }

    #[test]
    fn the_title_says_how_many_approvals_wait() {
        assert_eq!(super::window_title(0), "Recouvrance");
        assert_eq!(super::window_title(1), "1 approval waiting — Recouvrance");
        assert_eq!(super::window_title(3), "3 approvals waiting — Recouvrance");
    }

    /// The agent's text reaches osascript only as arguments after a fixed one: it is never
    /// script source (where `do shell script` would run commands), nor read as an option.
    #[test]
    fn a_macos_notification_never_runs_the_agents_text() {
        let (title, body) = ("-e evil", "\" & do shell script \"touch /tmp/x\" & \"");
        let args = super::osascript_args(title, body);
        let (script, rest) = args.split_at(args.len() - 3);
        assert_eq!(rest, ["recouvrance", title, body]);
        assert!(script.iter().all(|a| !a.contains("shell") && !a.contains("evil")));
        // Each statement follows its own -e.
        let (pairs, odd) = script.as_chunks::<2>();
        assert!(odd.is_empty());
        assert!(pairs.iter().all(|[flag, statement]| *flag == "-e" && *statement != "-e"));
        assert!(script.iter().any(|a| a.contains("item 2 of argv") && a.contains("item 3 of argv")));
    }

    /// The agent's text is never markup in a notify-send body: no tag and no entity survive,
    /// and unescaping gives the text back.
    #[test]
    fn a_notification_body_is_never_markup() {
        let text = r#"<a href="https://evil.example">approve</a> & <b>x</b> &lt; <img src="/x">"#;
        let escaped = super::markup_escaped(text);
        assert!(!escaped.contains('<') && !escaped.contains('>'), "{escaped}");
        assert!(
            escaped.split('&').skip(1).all(|e| e.starts_with("amp;") || e.starts_with("lt;") || e.starts_with("gt;"))
        );
        let back = escaped.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
        assert_eq!(back, text);
        assert_eq!(super::markup_escaped("plain -- text\nsecond"), "plain -- text\nsecond");
    }

    proptest::proptest! {
        /// Whatever the text, its escaped form has no tag delimiters and gives the text back.
        #[test]
        fn any_notification_body_comes_back_as_written(text in "\\PC{0,40}") {
            let escaped = super::markup_escaped(&text);
            proptest::prop_assert!(!escaped.contains('<') && !escaped.contains('>'));
            let back = escaped.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
            proptest::prop_assert_eq!(back, text);
        }
    }

    #[test]
    fn f5_reaches_the_terminal() {
        for mac in [false, true] {
            let keymap = Keymap::new(super::key_bindings_on(mac));
            let f5 = [Keystroke::parse("f5").unwrap()];
            let app = KeyContext::parse("App").unwrap();
            let (bindings, _) = keymap.bindings_for_input(&f5, std::slice::from_ref(&app));
            assert_eq!(bindings.len(), 1);
            let (bindings, _) = keymap.bindings_for_input(&f5, &[app, KeyContext::parse("Terminal").unwrap()]);
            assert!(bindings.is_empty());
        }
    }
}

#[cfg(all(test, feature = "dev"))]
mod wait_tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::time::Duration;

    use gpui_kit::{AppContext as _, Entity, EntityId, TestAppContext};

    use super::{VisiblePages, first_to_notify, wait_visible_or_away};

    /// Every window hears of each approval; only the first to hear sends the notification,
    /// whichever order the windows hear of the requests in.
    #[gpui_kit::test]
    fn one_notification_per_approval_for_all_windows(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(first_to_notify(cx, 1), "the first window");
            assert!(!first_to_notify(cx, 1), "a second window, same request");
            assert!(first_to_notify(cx, 2), "the next request");
            assert!(!first_to_notify(cx, 1), "a window late with an earlier request");
            assert!(!first_to_notify(cx, 2));
        });
    }

    /// A page that counts the turns it spent in the back/forward history.
    #[derive(Default)]
    struct Page {
        away: usize,
    }

    /// Put `page` in its window (`current`) or in the history, the window `hidden` or not.
    fn put(cx: &mut TestAppContext, page: &Entity<Page>, current: bool, hidden: bool) {
        let (window, id): (EntityId, EntityId) = (1000u64.into(), page.entity_id());
        cx.update(|cx| {
            let visible = cx.default_global::<VisiblePages>();
            if current {
                visible.pages.insert(window, id);
            } else {
                visible.pages.remove(&window);
            }
            if hidden {
                visible.hidden.insert(window);
            } else {
                visible.hidden.remove(&window);
            }
        });
    }

    /// A page waits while its window is hidden; the window is shown and Back pressed at once,
    /// between two turns. The next turn finds it in history: it is away (a detail page stops
    /// its log streams then), though it never was while its window was hidden.
    #[gpui_kit::test]
    fn a_page_that_goes_into_history_as_it_waits_is_away(cx: &mut TestAppContext) {
        let page = cx.new(|_| Page::default());
        put(cx, &page, true, true);
        let (weak, shown) = (page.downgrade(), Rc::new(Cell::new(None)));
        let out = shown.clone();
        cx.spawn(|mut cx| async move {
            let exec = cx.background_executor().clone();
            out.set(Some(wait_visible_or_away(&weak, &mut cx, &exec, |page: &mut Page, _| page.away += 1).await));
        })
        .detach();
        cx.run_until_parked();
        assert_eq!(page.read_with(cx, |p, _| p.away), 0, "its window is hidden: still its window's page");
        put(cx, &page, false, false);
        cx.executor().advance_clock(Duration::from_millis(600));
        cx.run_until_parked();
        assert_eq!(page.read_with(cx, |p, _| p.away), 1, "in history");
        assert_eq!(shown.get(), None, "still waiting");
        // Forward: on screen again, after a wait.
        put(cx, &page, true, false);
        cx.executor().advance_clock(Duration::from_millis(600));
        cx.run_until_parked();
        assert_eq!(shown.get(), Some(Some(true)));
        assert_eq!(page.read_with(cx, |p, _| p.away), 1);
    }
}
