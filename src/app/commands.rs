//! Every command of the window in one list: its name, its keys, where they act and the action
//! they run. The palette, the `?` panel, Settings → Shortcuts and the shortcut table in
//! docs/features.md are all built from it (a test keeps the docs equal), so a command found in
//! one is found in the others.

use gpui_kit::Action;
use gpui_kit::assets::IconName as Lucide;

use crate::ui::palette::{Item as PaletteItem, Target};

use super::{
    CloseTab, EscapeBack, FocusFilter, GoAssistant, GoBack, GoCluster, GoForward, GoHelm, GoProblems, GoSettings,
    GoWorkloads, NewTab, NewWindow, NextTab, OpenAccess, OpenAgentActivity, OpenAgents, OpenApiRequests, OpenApply,
    OpenArgo, OpenContexts, OpenHelmInstall, OpenPalette, OpenTerminal, OpenTimeline, OpenUpgradeCheck, PrevTab,
    RefreshPage, ShowShortcuts, ToggleAssistant, ToggleFavorite, ToggleSidebar, ToggleTheme, ToggleWriteMode, ZoomIn,
    ZoomOut, ZoomReset,
};

/// Where a command's keys act.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Scope {
    /// Wherever focus is.
    Anywhere,
    /// Outside text fields and terminals, where the key is typed instead (see
    /// `shortcuts::OUTSIDE_TEXT`).
    OutsideText,
    /// In a table.
    Table,
    /// On a detail page.
    Detail,
    /// In a shell.
    Shell,
    /// On the Contexts page.
    Contexts,
}

impl Scope {
    /// How the shortcut lists say it.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Anywhere => "Anywhere",
            Self::OutsideText => "Outside text fields",
            Self::Table => "In a table",
            Self::Detail => "On a detail page",
            Self::Shell => "In a shell",
            Self::Contexts => "On the Contexts page",
        }
    }
}

/// One command.
pub(crate) struct Command {
    /// Its name in the palette and the shortcut lists.
    pub name: &'static str,
    /// What it does, after the name; may be empty.
    pub help: &'static str,
    /// Its keys, each in braces as `shortcuts::with_keys` names them (`{g p}` is `g` then `p`);
    /// empty when it has none. Text outside braces is kept (`Right-click a row`).
    pub keys: &'static str,
    /// Where the keys act.
    pub scope: Scope,
    /// More words the palette finds it by (`rbac` for My access).
    pub also: &'static str,
    pub icon: Lucide,
    /// The action its keys are bound to, which the palette runs too. None for what several
    /// actions or a click do (moving in a table, a row's menu).
    pub action: Option<fn() -> Box<dyn Action>>,
    /// Listed in the palette.
    pub palette: bool,
}

/// An action that takes no data, made when the command runs.
fn run<A: Action + Default>() -> Box<dyn Action> {
    Box::new(A::default())
}

impl Command {
    /// A command the palette lists and runs with `action`, with no keys yet.
    const fn new(name: &'static str, help: &'static str, icon: Lucide, action: fn() -> Box<dyn Action>) -> Self {
        Self { name, help, keys: "", scope: Scope::Anywhere, also: "", icon, action: Some(action), palette: true }
    }

    /// Keys or a gesture that act only where they are pressed: not in the palette.
    const fn here(name: &'static str, help: &'static str, keys: &'static str, scope: Scope) -> Self {
        Self { name, help, keys, scope, also: "", icon: Lucide::Keyboard, action: None, palette: false }
    }

    const fn keys(mut self, keys: &'static str, scope: Scope) -> Self {
        self.keys = keys;
        self.scope = scope;
        self
    }

    const fn also(mut self, words: &'static str) -> Self {
        self.also = words;
        self
    }

    /// Bound to `action` but left out of the palette: the palette itself, and keys that act on
    /// what has focus, which the palette takes (a table's filter, Esc on a detail page).
    const fn not_in_palette(mut self) -> Self {
        self.palette = false;
        self
    }

    /// The name, then what it does: a row of the shortcut lists.
    pub(crate) fn described(&self) -> String {
        if self.help.is_empty() { self.name.to_string() } else { format!("{}: {}", self.name, self.help) }
    }
}

/// Every command, in the order the shortcut lists show them. Pages and commands without a key
/// come last: only the palette lists them.
pub(crate) const COMMANDS: &[Command] = &[
    Command::new(
        "Command palette",
        "resources, pages, contexts, objects and every command",
        Lucide::Search,
        run::<OpenPalette>,
    )
    .keys("{secondary-k}", Scope::Anywhere)
    .not_in_palette(),
    Command::new("Keyboard shortcuts", "this list", Lucide::Keyboard, run::<ShowShortcuts>)
        .keys("{?}", Scope::OutsideText)
        .also("help keys bindings hotkeys"),
    Command::new("New window", "", Lucide::AppWindow, run::<NewWindow>).keys("{secondary-shift-n}", Scope::Anywhere),
    Command::new("New tab", "right-click a row to open it in one", Lucide::Plus, run::<NewTab>)
        .keys("{secondary-t}", Scope::Anywhere),
    Command::new("Close tab", "", Lucide::X, run::<CloseTab>).keys("{secondary-w}", Scope::Anywhere),
    Command::new("Next tab", "", Lucide::ChevronRight, run::<NextTab>)
        .keys("{ctrl-tab} or {ctrl-pagedown}", Scope::Anywhere),
    Command::new("Previous tab", "", Lucide::ChevronLeft, run::<PrevTab>)
        .keys("{ctrl-shift-tab} or {ctrl-pageup}", Scope::Anywhere),
    Command::new("Hide or show the sidebar", "", Lucide::PanelLeftClose, run::<ToggleSidebar>)
        .keys("{secondary-b}", Scope::Anywhere),
    Command::new("Refresh", "in a terminal, F5 goes to the program", Lucide::RefreshCw, run::<RefreshPage>)
        .keys("{f5} or {secondary-r}", Scope::Anywhere)
        .also("reload"),
    Command::new("Back", "", Lucide::ArrowLeft, run::<GoBack>).keys("{alt-left|cmd-[}", Scope::Anywhere),
    Command::new("Forward", "", Lucide::ArrowRight, run::<GoForward>).keys("{alt-right|cmd-]}", Scope::Anywhere),
    Command::new("Back to the list", "", Lucide::ArrowLeft, run::<EscapeBack>)
        .keys("{escape}", Scope::Detail)
        .not_in_palette(),
    Command::new("Zoom in", "", Lucide::ZoomIn, run::<ZoomIn>).keys("{secondary-=}", Scope::Anywhere),
    Command::new("Zoom out", "", Lucide::ZoomOut, run::<ZoomOut>).keys("{secondary--}", Scope::Anywhere),
    Command::new("Reset the zoom", "", Lucide::Search, run::<ZoomReset>).keys("{secondary-0}", Scope::Anywhere),
    Command::new(
        "Filter the table",
        "`web` · `!Running` · `app=web` · `tier!=db` · `l:app` · `ns:team-*` · `/^api-\\d+/` · `status:Running` · \
         `restarts>3` · `cpu>200m` (all terms must match)",
        Lucide::Search,
        run::<FocusFilter>,
    )
    .keys("{secondary-f} or {/}", Scope::Table)
    .not_in_palette(),
    Command::here("Move the selection, open the selected row", "", "{j} / {k} / {enter}", Scope::Table),
    Command::here(
        "Row menu",
        "open, logs, shell, YAML, events, copy name, favorite",
        "Right-click a row",
        Scope::Table,
    ),
    Command::new("Problems", "what is broken, across the clusters", Lucide::TriangleAlert, run::<GoProblems>)
        .keys("{g p}", Scope::OutsideText)
        .also("errors crashloop failing broken alerts"),
    Command::new("Cluster overview", "nodes, capacity and versions", Lucide::Server, run::<GoCluster>)
        .keys("{g o}", Scope::OutsideText)
        .also("nodes capacity"),
    Command::new("Workloads overview", "", Lucide::LayoutDashboard, run::<GoWorkloads>)
        .keys("{g w}", Scope::OutsideText)
        .also("deployments dashboard"),
    Command::new("Helm releases", "", Lucide::Ship, run::<GoHelm>).keys("{g h}", Scope::OutsideText).also("charts"),
    Command::new("Assistant", "ask your clusters", Lucide::Sparkles, run::<GoAssistant>)
        .keys("{g i}", Scope::OutsideText)
        .also("chat ai claude"),
    Command::new("Settings", "", Lucide::Settings, run::<GoSettings>)
        .keys("{g s}", Scope::OutsideText)
        .also("preferences options"),
    Command::new(
        "Add to or remove from favorites",
        "the object or Application shown; on a list, the selected row, else the one namespace picked",
        Lucide::Star,
        run::<ToggleFavorite>,
    )
    .keys("{*}", Scope::OutsideText)
    .also("favourite star pin bookmark unstar"),
    Command::new("Assistant beside the page", "it is told what is on screen", Lucide::Sparkles, run::<ToggleAssistant>)
        .keys("{secondary-j}", Scope::Anywhere)
        .also("chat ai dock"),
    Command::here("Paste in a shell", "", "{ctrl-shift-v|cmd-v}", Scope::Shell),
    Command::here("Connect to just that context", "", "Double-click a context", Scope::Contexts),
    // Only in the palette.
    Command::new("Argo CD Applications", "", Lucide::GitFork, run::<OpenArgo>).also("gitops sync argocd"),
    Command::new("Timeline", "what changed, across clusters", Lucide::ClockArrowDown, run::<OpenTimeline>)
        .also("history changes events rollouts"),
    Command::new("My access", "what you may do in this namespace, and who can", Lucide::KeyRound, run::<OpenAccess>)
        .also("rbac can-i permissions who can roles"),
    Command::new(
        "Upgrade check",
        "deprecated APIs and what blocks the next Kubernetes version",
        Lucide::CircleArrowUp,
        run::<OpenUpgradeCheck>,
    )
    .also("deprecated removed apis version"),
    Command::new("Agents", "the MCP server, agents' rules and approvals", Lucide::Bot, run::<OpenAgents>)
        .also("mcp claude rules"),
    Command::new(
        "Agent activity",
        "every agent call, its decision and the rule that decided",
        Lucide::Activity,
        run::<OpenAgentActivity>,
    )
    .also("audit log calls"),
    Command::new("Contexts", "pick the contexts to connect", Lucide::Server, run::<OpenContexts>)
        .also("clusters kubeconfig connect"),
    Command::new("Terminal", "kubectl on this context", Lucide::SquareTerminal, run::<OpenTerminal>)
        .also("shell kubectl"),
    Command::new("Create / apply YAML", "", Lucide::Plus, run::<OpenApply>).also("manifest new"),
    Command::new("Install a Helm chart", "", Lucide::Download, run::<OpenHelmInstall>).also("chart"),
    Command::new(
        "API requests",
        "what the app asked each API server, and how long it took",
        Lucide::Activity,
        run::<OpenApiRequests>,
    )
    .also("requests latency slow calls"),
    Command::new("Write mode on or off", "turning it on asks first", Lucide::Pencil, run::<ToggleWriteMode>)
        .also("read-only writes enable disable"),
    Command::new("Dark or light theme", "", Lucide::Moon, run::<ToggleTheme>).also("appearance"),
];

/// The commands the shortcut lists show: those with keys or a gesture.
pub(crate) fn with_shortcut() -> impl Iterator<Item = &'static Command> {
    COMMANDS.iter().filter(|c| !c.keys.is_empty())
}

/// The palette's entries for the commands: name, what it does, keys and the other words it is
/// found by. Picking one runs the action its keys run.
pub(crate) fn palette_items() -> impl Iterator<Item = PaletteItem> {
    COMMANDS.iter().filter(|c| c.palette).filter_map(|c| {
        let mut item = PaletteItem::new(c.name.into(), c.help.into(), c.icon, Target::Action(c.action?));
        item.keys = c.keys;
        item.also = c.also;
        Some(item)
    })
}
