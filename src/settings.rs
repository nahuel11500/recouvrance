//! User settings, stored in ~/.config/recouvrance/settings.json and readable from
//! any thread (the Kubernetes side reads refresh rates and kubeconfig paths).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock, RwLock};

use serde::{Deserialize, Deserializer, Serialize};

/// `protection` (and `read_only`) entry by entry: a value that isn't `true` or `false` (a hand
/// edit) protects its context (makes it read-only), and never costs the other entries theirs.
fn protection_entries<'de, D: Deserializer<'de>>(d: D) -> Result<std::collections::BTreeMap<String, bool>, D::Error> {
    let raw = std::collections::BTreeMap::<String, serde_json::Value>::deserialize(d)?;
    Ok(raw.into_iter().map(|(k, v)| (k, v.as_bool().unwrap_or(true))).collect())
}

/// A flag where anything but `false` (a hand edit) counts as `true`.
fn false_or_true<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(serde_json::Value::deserialize(d)?.as_bool().unwrap_or(true))
}

/// A map of strings entry by entry: an entry that isn't a string is dropped alone.
fn string_entries<'de, D: Deserializer<'de>>(d: D) -> Result<std::collections::BTreeMap<String, String>, D::Error> {
    let raw = std::collections::BTreeMap::<String, serde_json::Value>::deserialize(d)?;
    Ok(raw.into_iter().filter_map(|(k, v)| Some((k, v.as_str()?.to_string()))).collect())
}

/// `delete_confirm_kinds` entry by entry: a value that doesn't read (a hand edit, or one
/// from a newer version) asks for the name, and never costs the other kinds theirs.
fn delete_confirm_entries<'de, D: Deserializer<'de>>(
    d: D,
) -> Result<std::collections::BTreeMap<String, DeleteConfirm>, D::Error> {
    let raw = std::collections::BTreeMap::<String, serde_json::Value>::deserialize(d)?;
    Ok(raw.into_iter().map(|(k, v)| (k, serde_json::from_value(v).unwrap_or(DeleteConfirm::Type))).collect())
}

/// `confirm_style`: a value that doesn't read (a hand edit, or one from a newer version)
/// asks for names typed, the stricter of the two.
fn confirm_style_or_type<'de, D: Deserializer<'de>>(d: D) -> Result<ConfirmStyle, D::Error> {
    Ok(serde_json::from_value(serde_json::Value::deserialize(d)?).unwrap_or(ConfirmStyle::Type))
}

/// `confirm_wait_secs`: a whole number kept within `CONFIRM_WAIT_RANGE`; anything else
/// waits the longest.
fn wait_secs_or_longest<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    let raw = serde_json::Value::deserialize(d)?;
    Ok(raw.as_u64().map_or(*CONFIRM_WAIT_RANGE.end(), clamp_wait))
}

/// Any value: the strings it lists, in order; nothing when it isn't a list.
fn string_list<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    let raw = serde_json::Value::deserialize(d)?;
    let Some(list) = raw.as_array() else { return Ok(Vec::new()) };
    Ok(list.iter().filter_map(|v| Some(v.as_str()?.to_string())).collect())
}

/// Any value: its string entries; nothing when it isn't an object.
fn string_map<'de, D: Deserializer<'de>>(d: D) -> Result<std::collections::BTreeMap<String, String>, D::Error> {
    let raw = serde_json::Value::deserialize(d)?;
    let Some(fields) = raw.as_object() else { return Ok(std::collections::BTreeMap::new()) };
    Ok(fields.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
}

/// Any value: what `T` reads from it, else `T`'s default.
fn or_default<'de, D: Deserializer<'de>, T: serde::de::DeserializeOwned + Default>(d: D) -> Result<T, D::Error> {
    Ok(serde_json::from_value(serde_json::Value::deserialize(d)?).unwrap_or_default())
}

/// `sessions` entry by entry: an entry that isn't an object, or names no context, is
/// dropped; in the others each value that doesn't read takes its default. Never fails, so a
/// hand edit here costs no other setting.
fn session_entries<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Session>, D::Error> {
    let raw = serde_json::Value::deserialize(d)?;
    Ok(sessions_from(&raw))
}

fn sessions_from(raw: &serde_json::Value) -> Vec<Session> {
    let entries = raw.as_array().map_or(&[][..], Vec::as_slice);
    let mut sessions: Vec<Session> = Vec::new();
    for entry in entries.iter().filter(|e| e.is_object()) {
        // Every field of a session reads whatever it holds (see `Session`).
        let Ok(mut s) = serde_json::from_value::<Session>(entry.clone()) else { continue };
        s.contexts = session_set(&s.contexts);
        if !s.contexts.is_empty() && !sessions.iter().any(|x| x.contexts == s.contexts) {
            sessions.push(s);
        }
    }
    sessions.truncate(SESSIONS_KEPT);
    sessions
}

/// `context_sets` entry by entry: an entry that isn't an object, has no name or no context
/// is dropped alone; a name already taken keeps its first set. Never fails, so a hand edit
/// here costs no other setting.
fn context_set_entries<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<ContextSet>, D::Error> {
    let raw = serde_json::Value::deserialize(d)?;
    Ok(context_sets_from(&raw))
}

fn context_sets_from(raw: &serde_json::Value) -> Vec<ContextSet> {
    let entries = raw.as_array().map_or(&[][..], Vec::as_slice);
    let mut sets: Vec<ContextSet> = Vec::new();
    for entry in entries.iter().filter(|e| e.is_object()) {
        let Ok(set) = serde_json::from_value::<ContextSet>(entry.clone()) else { continue };
        let Some(set) = ContextSet::new(&set.name, &set.contexts) else { continue };
        if !sets.iter().any(|x| x.name == set.name) {
            sets.push(set);
        }
    }
    sets
}

/// `window`: none when it doesn't read or can't be a window (the default place then).
fn window_or_none<'de, D: Deserializer<'de>>(d: D) -> Result<Option<WindowPlace>, D::Error> {
    let raw = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value::<WindowPlace>(raw).ok().and_then(WindowPlace::usable))
}

/// The seconds a confirmation may wait: at least one, never long enough to be in the way.
pub const CONFIRM_WAIT_RANGE: std::ops::RangeInclusive<u64> = 1..=30;

/// The sidebar's width to start with, and how narrow or wide it can be dragged (pixels).
pub const SIDEBAR_WIDTH: f32 = 260.;
pub const SIDEBAR_WIDTH_RANGE: std::ops::RangeInclusive<f32> = 180.0..=520.0;

/// The sidebar's width: the one saved, brought within range.
pub fn sidebar_width_in(s: &Settings) -> f32 {
    if s.sidebar_width.is_finite() {
        s.sidebar_width.clamp(*SIDEBAR_WIDTH_RANGE.start(), *SIDEBAR_WIDTH_RANGE.end())
    } else {
        SIDEBAR_WIDTH
    }
}

fn clamp_wait(secs: u64) -> u64 {
    secs.clamp(*CONFIRM_WAIT_RANGE.start(), *CONFIRM_WAIT_RANGE.end())
}

/// How a confirmation that must not be clicked through is made: a delete set to "Type the
/// name", and every write on a protected context.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum ConfirmStyle {
    /// The object's name (or the count) and the protected context's name typed.
    Type,
    /// The button stays disabled for `confirm_wait_secs`, showing the seconds left.
    Wait,
}

/// The identity to run as, or none when it does not read (a hand edit) or names no user:
/// elevation is then not offered, and never costs the cluster's other settings.
fn identity_or_none<'de, D: Deserializer<'de>>(d: D) -> Result<Option<crate::k8s::Identity>, D::Error> {
    let raw = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value::<crate::k8s::Identity>(raw).ok().and_then(crate::k8s::Identity::usable))
}

/// Which backend the assistant uses.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "kebab-case")]
pub enum AssistantBackend {
    /// Claude Code when it is installed, else the API key.
    #[default]
    Auto,
    ClaudeCode,
    Api,
}

/// What deleting asks before it runs (per kind, see `delete_confirm`), from least to most.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum DeleteConfirm {
    /// Delete right away (a protected context still asks for its name).
    Off,
    /// A dialog with the `kubectl` equivalent and a Delete button.
    Ask,
    /// The same dialog, where the name (or the count, for several) must be typed.
    Type,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum Startup {
    /// Always show the context picker ("ask").
    #[serde(rename = "ask")]
    Picker,
    /// Reconnect to the contexts used last time (the default; older files said "picker").
    #[serde(alias = "picker")]
    Reconnect,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum InitialView {
    Pods,
    Workloads,
    Cluster,
}

/// Sets of contexts that remember where they were left (the newest kept).
pub const SESSIONS_KEPT: usize = 20;

/// The window's size to start with, and the smallest it can be (logical pixels).
pub const WINDOW_SIZE: (f32, f32) = (1500., 920.);
pub const WINDOW_MIN: (f32, f32) = (900., 560.);

/// A named group of contexts, connected together in one click from the context picker or
/// the palette. Each field reads whatever the file holds, so a set never fails to load.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(default)]
pub struct ContextSet {
    /// Trimmed, never empty, one set per name.
    #[serde(deserialize_with = "or_default")]
    pub name: String,
    /// In the order they connect (the first is the primary), without repeats or empty names.
    #[serde(deserialize_with = "string_list")]
    pub contexts: Vec<String>,
}

impl ContextSet {
    /// A set named `name` of `contexts`, in their order, without repeats or empty names. None
    /// when the name is blank or no context is left.
    pub fn new(name: &str, contexts: &[String]) -> Option<Self> {
        let name = name.trim();
        let mut kept: Vec<String> = Vec::new();
        for c in contexts.iter().filter(|c| !c.is_empty()) {
            if !kept.contains(c) {
                kept.push(c.clone());
            }
        }
        (!name.is_empty() && !kept.is_empty()).then(|| Self { name: name.to_string(), contexts: kept })
    }

    /// Its contexts the kubeconfig still has, in order: connecting a context that is gone
    /// would only fail.
    pub fn available(&self, all: &[String]) -> Vec<String> {
        self.contexts.iter().filter(|c| all.contains(c)).cloned().collect()
    }

    /// Whether `connected` is this set, in any order.
    pub fn is(&self, connected: &[String]) -> bool {
        session_set(&self.contexts) == session_set(connected)
    }
}

/// Where a window was left on a set of contexts: the first window that connects to that set
/// reopens there. Each field reads whatever the file holds (a value that doesn't read takes
/// its default), so a session never fails to load and never costs the other settings.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(default)]
pub struct Session {
    /// The contexts, sorted and without repeats (`session_set`): the set this is for.
    #[serde(deserialize_with = "string_list")]
    pub contexts: Vec<String>,
    /// Each tab, in order.
    #[serde(deserialize_with = "tab_entries")]
    pub tabs: Vec<SavedTab>,
    /// The tab shown (the last one when there are fewer).
    #[serde(deserialize_with = "or_default")]
    pub active_tab: usize,
    /// The filter typed for each resource type, by kubectl name (`deployments.apps`).
    #[serde(deserialize_with = "string_map")]
    pub filters: std::collections::BTreeMap<String, String>,
}

/// A tab as it was left: its page, its list's resource type, namespaces and filter.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
#[serde(default)]
pub struct SavedTab {
    #[serde(deserialize_with = "or_default")]
    pub page: SavedPage,
    /// The resource type its list shows, by kubectl name (`pods`, `certificates.cert-manager.io`):
    /// the version may change between launches, the name does not.
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "or_default")]
    pub kind: Option<String>,
    /// Empty: all.
    #[serde(deserialize_with = "string_list")]
    pub namespaces: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty", deserialize_with = "or_default")]
    pub filter: String,
}

/// The page a tab reopens on: its list, or an overview built from the clusters. Detail pages
/// and the others (Settings, a shell...) reopen on the list or overview they were opened from.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "kebab-case")]
pub enum SavedPage {
    #[default]
    List,
    Workloads,
    Cluster,
    Problems,
    Timeline,
    Helm,
    Argo,
}

/// `tabs` entry by entry: one that isn't an object is dropped, the others read field by field.
fn tab_entries<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<SavedTab>, D::Error> {
    let raw = serde_json::Value::deserialize(d)?;
    let entries = raw.as_array().map_or(&[][..], Vec::as_slice);
    Ok(entries.iter().filter(|e| e.is_object()).filter_map(|e| serde_json::from_value(e.clone()).ok()).collect())
}

/// The window's place and size when it was last moved, and whether it was maximized.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq)]
pub struct WindowPlace {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub maximized: bool,
}

/// Further than any desktop reaches: a value beyond is a hand edit, not a place.
const WINDOW_FAR: f32 = 100_000.;

impl WindowPlace {
    /// The place, at least the window's smallest size; none when a number isn't finite or is
    /// beyond any desktop.
    pub fn usable(self) -> Option<Self> {
        let numbers = [self.x, self.y, self.width, self.height];
        if numbers.iter().any(|n| !n.is_finite() || n.abs() > WINDOW_FAR) {
            return None;
        }
        Some(Self { width: self.width.max(WINDOW_MIN.0), height: self.height.max(WINDOW_MIN.1), ..self })
    }
}

/// A port-forward to bring back next time (pods are found again by service or name).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SavedForward {
    pub context: String,
    pub namespace: String,
    /// Forward through this Service's pods when set.
    pub service: Option<String>,
    pub pod: String,
    pub remote: u16,
    pub local: Option<u16>,
    /// Listen address (default 127.0.0.1; 0.0.0.0 shares it on the network).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    /// Open as https:// in the browser.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub https: bool,
    /// Open the browser once it listens.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub open_browser: bool,
}

/// How the Argo CD page was last left, restored when it opens: its filters and search, how
/// it groups, the headings folded and the environments unfolded (by their root: Argo CD,
/// namespace, name). One that does not read is the default page.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ArgoPage {
    pub filter: crate::ui::argo::envs::Filter,
    pub group_by: crate::ui::argo::envs::GroupBy,
    pub folded: Vec<String>,
    pub expanded: Vec<(String, String, String)>,
    /// The list is shown, not the overview.
    pub list: bool,
}

impl ArgoPage {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Settings {
    pub startup: Startup,
    pub initial_view: InitialView,
    /// Contexts connected last time (for `Startup::Reconnect`).
    pub last_contexts: Vec<String>,
    /// Seconds between metrics-server polls (tables, usage bars).
    pub metrics_refresh: u64,
    /// Seconds between refreshes of an open detail page.
    pub detail_refresh: u64,
    /// Seconds between overview page refreshes (minimum; big clusters wait longer).
    pub overview_refresh: u64,
    /// Lines fetched when opening logs.
    pub log_tail_lines: i64,
    /// Show the time column in logs.
    pub log_timestamps: bool,
    /// Extra kubeconfig files merged with the default one (like KUBECONFIG).
    pub kubeconfigs: Vec<String>,
    /// Accent color per context (hex like "#ef4444").
    pub context_colors: std::collections::HashMap<String, String>,
    /// Older settings: the full list of protected contexts (it froze the guess).
    /// Read once and turned into `protection`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protected_contexts: Option<Vec<String>>,
    /// Your choices per context (true: protected, false: not). Contexts without one
    /// are guessed from their names, user, cluster and server ("prod", "admin", …).
    #[serde(deserialize_with = "protection_entries")]
    pub protection: std::collections::BTreeMap<String, bool>,
    /// The protection choices could not be read once: contexts without a choice are
    /// protected instead of guessed, until this is turned off in Settings → Contexts.
    #[serde(skip_serializing_if = "std::ops::Not::not", deserialize_with = "false_or_true")]
    pub protect_unset: bool,
    /// Contexts the window never writes to, whatever write mode says, and agents never
    /// write to, whatever their rules say (true), or set back (false). Not set: not
    /// read-only.
    #[serde(deserialize_with = "protection_entries")]
    pub read_only: std::collections::BTreeMap<String, bool>,
    /// The read-only choices could not be read once: every context without a choice is
    /// read-only, until this is turned off in Settings → Contexts.
    #[serde(skip_serializing_if = "std::ops::Not::not", deserialize_with = "false_or_true")]
    pub read_only_unset: bool,
    /// Prometheus per context as `namespace/service:port` (found automatically when unset).
    pub prometheus: std::collections::HashMap<String, String>,
    /// Range of usage charts, in minutes (last choice).
    pub usage_range: i64,
    /// Contexts connected recently, newest first.
    pub recent_contexts: Vec<String>,
    /// Older settings: namespaces chosen recently per context, newest first.
    /// Read once and turned into `namespace_visits`.
    #[serde(skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub recent_namespaces: std::collections::HashMap<String, Vec<String>>,
    /// Namespaces visited per context, to offer the frequent ones first.
    pub namespace_visits: std::collections::HashMap<String, Vec<NamespaceVisit>>,
    /// Port-forwards that were running, to restore.
    pub saved_forwards: Vec<SavedForward>,
    /// Model used by the in-app assistant.
    pub assistant_model: String,
    /// auto (Claude Code if installed, else API key), claude-code or api.
    pub assistant_backend: AssistantBackend,
    /// The team's notes given to the assistant: where things run, dashboards, runbooks, repos.
    pub assistant_notes: String,
    /// Folders of the team's code the assistant may read (Claude Code only).
    pub assistant_repos: Vec<String>,
    /// Read metrics-server and Prometheus (off: no usage columns or charts).
    pub metrics_enabled: bool,
    /// Accent color name (see `ACCENTS`).
    pub accent: String,
    /// Table filters used recently, newest first.
    pub filter_history: Vec<String>,
    /// Filters kept by the user (any resource type).
    pub saved_searches: Vec<String>,
    /// Namespaces added by hand per context (when you may not list namespaces).
    pub extra_namespaces: std::collections::HashMap<String, Vec<String>>,
    /// Per-cluster settings, by context name.
    pub clusters: std::collections::HashMap<String, ClusterSettings>,
    /// Folders whose kubeconfig files are all merged (watched for changes).
    pub kubeconfig_folders: Vec<String>,
    /// Contexts Argo CD's Applications are read from. Empty: found on their own, among the
    /// connected clusters first, then the kubeconfig's other contexts.
    pub argocd_contexts: Vec<String>,
    /// The Argo CD API server of a hub context (`https://argocd.example.com`), to read Argo
    /// CD's own diff from. None set: that diff is off, nothing is sent to Argo CD.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty", deserialize_with = "string_entries")]
    pub argocd_servers: std::collections::BTreeMap<String, String>,
    /// Argo CD API servers read on the Argo CD page with no kubeconfig for their cluster,
    /// added in Settings → Argo CD (`https://argocd.example.com`); their tokens are kept in
    /// `argocd-api-tokens.json`. The argocd CLI's servers are read too.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub argocd_api_servers: Vec<String>,
    /// Argo CD API servers not read (their address), whether set here or the argocd CLI's.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub argocd_api_off: Vec<String>,
    /// The Argo CD page's environment types, in the order shown: `type = pattern` lines (see
    /// `k8s::argo::environments`). Empty: the defaults.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub argocd_env_types: Vec<String>,
    /// How the Argo CD page was last left: its filters, search, grouping and folds.
    #[serde(skip_serializing_if = "ArgoPage::is_default")]
    pub argocd_page: ArgoPage,
    /// Which kubeconfig file keeps a context name that several files have: the first one
    /// seen. The others' contexts are `name@file`, whatever order the files are read in.
    #[serde(deserialize_with = "string_entries")]
    pub context_files: std::collections::BTreeMap<String, String>,
    /// Open the browser after a one-click port-forward on a web port.
    pub open_browser_on_forward: bool,
    /// Port-forwards started recently, newest first (offered again in the sidebar).
    pub recent_forwards: Vec<SavedForward>,
    /// What deleting asks for kinds not in `delete_confirm_kinds`.
    pub delete_confirm: DeleteConfirm,
    /// What deleting asks, by kind: `Node`, `StatefulSet`… for built-in kinds, with the group
    /// for custom resources (`Cluster.postgresql.cnpg.io`), see `delete_kind_key`.
    #[serde(deserialize_with = "delete_confirm_entries")]
    pub delete_confirm_kinds: std::collections::BTreeMap<String, DeleteConfirm>,
    /// How deletes set to `type` and writes on protected contexts are confirmed.
    #[serde(deserialize_with = "confirm_style_or_type")]
    pub confirm_style: ConfirmStyle,
    /// Seconds the confirm button waits with `ConfirmStyle::Wait` (see `CONFIRM_WAIT_RANGE`).
    #[serde(deserialize_with = "wait_secs_or_longest")]
    pub confirm_wait_secs: u64,
    /// What the timeline records, and how much of it is kept.
    pub timeline: TimelineSettings,
    /// The sidebar's width in pixels, as last dragged (read with `sidebar_width_in`).
    pub sidebar_width: f32,
    /// The sidebar's sections folded (by name: "Workloads", "Custom resources"...).
    pub sidebar_folded: Vec<String>,
    /// The newest version whose release notes were shown (What's new, see `crate::updates`).
    /// None in a file from before What's new; a new install starts with the version running
    /// (see `load_from`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_version: Option<String>,
    /// Where each set of contexts was left, the newest first (see `Session`).
    #[serde(deserialize_with = "session_entries")]
    pub sessions: Vec<Session>,
    /// Where the window was left (read with `window_place`).
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "window_or_none")]
    pub window: Option<WindowPlace>,
    /// Named groups of contexts, in the order they were first saved (see `ContextSet`).
    #[serde(skip_serializing_if = "Vec::is_empty", deserialize_with = "context_set_entries")]
    pub context_sets: Vec<ContextSet>,
}

/// The timeline: changes recorded while the app watches (see `crate::timeline`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub struct TimelineSettings {
    /// Record what the lists and the Problems scan see change.
    pub record: bool,
    /// Kinds watched whole while connected, for diffs of the whole object (kubectl names:
    /// `configmaps`, `deployments.apps`); each is a watch more on every cluster.
    pub kinds: Vec<String>,
    /// MB kept per context (see `max_mb`).
    pub max_mb: u64,
    /// Days kept (see `days`).
    pub days: u64,
}

impl Default for TimelineSettings {
    fn default() -> Self {
        Self { record: true, kinds: Vec::new(), max_mb: 16, days: 14 }
    }
}

impl TimelineSettings {
    /// The size bound per context, in MB: 1 to 1024.
    pub fn max_mb(&self) -> u64 {
        self.max_mb.clamp(1, 1024)
    }

    /// The age bound, in days: 1 to 365.
    pub fn days(&self) -> u64 {
        self.days.clamp(1, 365)
    }
}

/// Kinds that come back on their own are deleted right away; kinds whose loss takes data,
/// a machine or a whole namespace with it need the name typed. Others (custom resources
/// too, which can stand for a database) ask.
fn default_delete_confirm() -> std::collections::BTreeMap<String, DeleteConfirm> {
    use DeleteConfirm::{Off, Type};
    let off = ["Pod", "Job", "ReplicaSet", "Event", "EndpointSlice", "Endpoints", "Lease"];
    let typed = [
        "Node",
        "Namespace",
        "StatefulSet",
        "Deployment",
        "DaemonSet",
        "PersistentVolume",
        "PersistentVolumeClaim",
        "CustomResourceDefinition",
    ];
    off.iter().map(|k| (k.to_string(), Off)).chain(typed.iter().map(|k| (k.to_string(), Type))).collect()
}

/// The key a kind's delete choice is saved under: the kind alone when built in (`Job`), with
/// its group for a custom resource (`Job.batch.volcano.sh`), as several groups can name one.
pub fn delete_kind_key(group: &str, kind: &str) -> String {
    if crate::k8s::is_builtin_group(group) { kind.to_string() } else { format!("{kind}.{group}") }
}

/// What deleting an object of this kind (`apps`, `Deployment`) asks first.
pub fn delete_confirm(group: &str, kind: &str) -> DeleteConfirm {
    with(|s| delete_confirm_in(s, group, kind))
}

/// The kind's own choice, else the default. Case is ignored: older versions saved kinds as typed.
fn delete_confirm_in(s: &Settings, group: &str, kind: &str) -> DeleteConfirm {
    let choice = |key: &str| {
        let level = s
            .delete_confirm_kinds
            .get(key)
            .or_else(|| s.delete_confirm_kinds.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, l)| l));
        level.copied()
    };
    let key = delete_kind_key(group, kind);
    if let Some(own) = choice(&key) {
        return own;
    }
    // A custom resource without a choice of its own. The kind alone names a built-in kind (the
    // defaults say `Job`), or was saved by an older version without the group: it can only make
    // deleting ask more than the default, never less.
    match choice(kind) {
        Some(bare) if key != kind => bare.max(s.delete_confirm),
        _ => s.delete_confirm,
    }
}

/// A namespace chosen on a context: how often, and when last (unix seconds).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct NamespaceVisit {
    pub name: String,
    pub count: u32,
    pub last: i64,
}

/// Settings for one cluster (context).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub struct ClusterSettings {
    /// Image for node shells (default busybox).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_shell_image: Option<String>,
    /// Only these namespaces (for users who may not list namespaces); empty = all.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub namespaces: Vec<String>,
    /// Metrics off for this cluster only.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub metrics_disabled: bool,
    /// Who a write may run as when asked in its confirmation (`kubectl --as`); none by default.
    #[serde(skip_serializing_if = "Option::is_none", deserialize_with = "identity_or_none")]
    pub run_as: Option<crate::k8s::Identity>,
}

/// Remember a filter at the top of the history (deduplicated, 20 kept).
pub fn remember_filter(text: &str) {
    let t = text.trim();
    if t.is_empty() {
        return;
    }
    update(|s| {
        s.filter_history.retain(|f| f != t);
        s.filter_history.insert(0, t.to_string());
        s.filter_history.truncate(20);
    });
}

/// Metrics on globally and not turned off for this cluster.
pub fn metrics_on(context: &str) -> bool {
    with(|s| s.metrics_enabled && !s.clusters.get(context).is_some_and(|c| c.metrics_disabled))
}

pub fn cluster(context: &str) -> ClusterSettings {
    with(|s| s.clusters.get(context).cloned().unwrap_or_default())
}

/// The identity writes on this context may run as, when one is set and reads. None when the
/// settings file could not be read: no elevation is offered then.
pub fn run_as(context: &str) -> Option<crate::k8s::Identity> {
    if load_error().is_some() {
        return None;
    }
    with(|s| s.clusters.get(context)?.run_as.clone()).and_then(crate::k8s::Identity::usable)
}

/// Set (or clear, with no user) the identity writes on this context may run as.
pub fn set_run_as(context: &str, identity: Option<crate::k8s::Identity>) {
    let identity = identity.and_then(crate::k8s::Identity::usable);
    update(|s| s.clusters.entry(context.to_string()).or_default().run_as = identity);
}

/// Accent colors: (name, dark theme, light theme). Brass is the house color.
pub const ACCENTS: [(&str, u32, u32); 6] = [
    ("brass", 0xd9a54c, 0xc8963e),
    ("harbour", 0x8db8e8, 0x1f5f99),
    ("sea", 0x5ec2c9, 0x1f7a8c),
    ("kelp", 0x6cc497, 0x2f8a60),
    ("signal", 0xe5736a, 0xc2453d),
    ("violet", 0xa78bfa, 0x7c3aed),
];

impl Default for Settings {
    fn default() -> Self {
        Self {
            startup: Startup::Reconnect,
            initial_view: InitialView::Pods,
            last_contexts: Vec::new(),
            metrics_refresh: 15,
            detail_refresh: 5,
            overview_refresh: 30,
            log_tail_lines: 1000,
            log_timestamps: true,
            kubeconfigs: Vec::new(),
            context_colors: Default::default(),
            protected_contexts: None,
            protection: Default::default(),
            protect_unset: false,
            read_only: Default::default(),
            read_only_unset: false,
            prometheus: Default::default(),
            usage_range: 60,
            recent_contexts: Vec::new(),
            recent_namespaces: Default::default(),
            namespace_visits: Default::default(),
            saved_forwards: Vec::new(),
            assistant_model: crate::assistant::DEFAULT_MODEL.into(),
            assistant_backend: AssistantBackend::Auto,
            assistant_notes: String::new(),
            assistant_repos: Vec::new(),
            metrics_enabled: true,
            accent: "brass".into(),
            filter_history: Vec::new(),
            saved_searches: Vec::new(),
            extra_namespaces: Default::default(),
            clusters: Default::default(),
            kubeconfig_folders: Vec::new(),
            argocd_contexts: Vec::new(),
            argocd_servers: Default::default(),
            argocd_api_servers: Vec::new(),
            argocd_api_off: Vec::new(),
            argocd_env_types: Vec::new(),
            argocd_page: ArgoPage::default(),
            context_files: Default::default(),
            open_browser_on_forward: true,
            recent_forwards: Vec::new(),
            delete_confirm: DeleteConfirm::Ask,
            delete_confirm_kinds: default_delete_confirm(),
            confirm_style: ConfirmStyle::Wait,
            confirm_wait_secs: 3,
            timeline: TimelineSettings::default(),
            sidebar_width: SIDEBAR_WIDTH,
            sidebar_folded: Vec::new(),
            last_seen_version: None,
            sessions: Vec::new(),
            window: None,
            context_sets: Vec::new(),
        }
    }
}

impl Settings {
    /// A new install's settings: the defaults, with the version running already seen, since
    /// What's new is for updates.
    fn new_install() -> Self {
        Self { last_seen_version: Some(crate::updates::VERSION.to_string()), ..Self::default() }
    }
}

static SETTINGS: LazyLock<RwLock<Settings>> = LazyLock::new(|| RwLock::new(load()));
/// Numbers each change, in the order they were made.
static CHANGES: AtomicU64 = AtomicU64::new(0);
/// The newest change on disk.
static SAVED: Mutex<u64> = Mutex::new(0);
/// Why the settings file, which exists, could not be read: nothing is saved over it.
static LOAD_ERROR: OnceLock<String> = OnceLock::new();

fn path() -> PathBuf {
    crate::paths::config_dir().join("settings.json")
}

/// The settings file. A field that does not read (a hand edit, or a value from a newer
/// version) gets its default and the others, such as `protection`, are kept; the file
/// is kept aside as `load_json` does.
fn load() -> Settings {
    let (settings, error) = load_from(&path());
    if let Some(e) = error {
        eprintln!("{e}; using defaults, every context not set is protected and read-only, and nothing is saved");
        let _ = LOAD_ERROR.set(e);
    }
    settings
}

/// The settings at `p`, or the defaults and why when the file exists but can't be read
/// (permissions, a folder): then the file is left as it is. No file is a new install.
fn load_from(p: &Path) -> (Settings, Option<String>) {
    let bytes = match std::fs::read(p) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Settings::new_install(), None),
        Err(e) => return (Settings::default(), Some(format!("{}: {e}", p.display()))),
    };
    let (mut settings, error, lost) = from_bytes(&bytes);
    if let Some(e) = error {
        eprintln!("{}: {e}; fields that don't read use their defaults ({})", p.display(), keep_aside(p));
    }
    if lost {
        eprintln!("{}: the protection choices could not be read: every context is protected", p.display());
        // Saved with the settings, so the next launch doesn't guess them again.
        settings.protect_unset = true;
    }
    (settings, None)
}

/// Settings from the file's bytes: what reads, why the rest didn't, and whether the
/// protection choices were lost with it. Read-only choices lost with it set
/// `read_only_unset`, so every context without one is read-only.
fn from_bytes(bytes: &[u8]) -> (Settings, Option<serde_json::Error>, bool) {
    match serde_json::from_slice(bytes) {
        Ok(v) => (v, None, false),
        Err(e) => {
            let fields = match serde_json::from_slice::<serde_json::Value>(bytes) {
                Ok(serde_json::Value::Object(fields)) => Some(fields),
                _ => None,
            };
            let lost = fields.as_ref().is_none_or(|fields| {
                fields.get("protection").is_some_and(|p| !p.is_object())
                    || fields
                        .get("protected_contexts")
                        .is_some_and(|p| serde_json::from_value::<Option<Vec<String>>>(p.clone()).is_err())
            });
            let mut settings: Settings = by_field(bytes).unwrap_or_default();
            if fields.as_ref().is_none_or(|fields| fields.get("read_only").is_some_and(|r| !r.is_object())) {
                settings.read_only_unset = true;
            }
            (settings, Some(e), lost)
        }
    }
}

/// Why the settings file could not be read, if so: then this session saves nothing and
/// every context not set counts as protected.
pub fn load_error() -> Option<&'static str> {
    LOAD_ERROR.get().map(String::as_str)
}

/// `T` from a JSON object, one field at a time: a field that does not read gets its
/// default. None when the text is not a JSON object.
fn by_field<T: Serialize + serde::de::DeserializeOwned + Default>(bytes: &[u8]) -> Option<T> {
    let serde_json::Value::Object(fields) = serde_json::from_slice::<serde_json::Value>(bytes).ok()? else {
        return None;
    };
    let mut merged = serde_json::to_value(T::default()).ok()?;
    for (key, value) in fields {
        let mut candidate = merged.clone();
        candidate.as_object_mut()?.insert(key, value);
        if serde_json::from_value::<T>(candidate.clone()).is_ok() {
            merged = candidate;
        }
    }
    serde_json::from_value(merged).ok()
}

/// Read a JSON file; missing means the defaults. A file that can't be read or does not
/// parse is kept aside as `<name>.bad-<unix time>` (so it is not overwritten by the next
/// save) and the defaults are used.
pub fn load_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return T::default(),
        Err(e) => {
            eprintln!("{}: {e}; using defaults ({})", path.display(), keep_aside(path));
            return T::default();
        }
    };
    match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}: {e}; using defaults ({})", path.display(), keep_aside(path));
            T::default()
        }
    }
}

/// Move a file that does not read to `<name>.bad-<unix time>`, so the next save does
/// not overwrite it. Says where it went, or why it could not be moved. A symlinked file
/// (a dotfiles repo) is moved aside where it is and the link left in place, so the next
/// save writes through the link instead of a regular file over it.
pub fn keep_aside(path: &Path) -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let path = match crate::private_files::through_links(path) {
        Ok(p) => p,
        Err(e) => return format!("could not keep it: {e}"),
    };
    let path = path.as_path();
    let mut bad = path.as_os_str().to_owned();
    bad.push(format!(".bad-{secs}"));
    match std::fs::rename(path, &bad) {
        Ok(()) => format!("kept as {}", Path::new(&bad).display()),
        Err(e) => format!("could not keep it: {e}"),
    }
}

/// A copy of the current settings.
pub fn get() -> Settings {
    SETTINGS.read().unwrap().clone()
}

/// Read settings without copying them (for code that runs on every frame).
pub fn with<T>(f: impl FnOnce(&Settings) -> T) -> T {
    f(&SETTINGS.read().unwrap())
}

/// Change and save settings (only kept for this session when the file could not be read,
/// see `load_error`).
pub fn update(f: impl FnOnce(&mut Settings)) {
    let (change, json) = {
        let mut s = SETTINGS.write().unwrap();
        f(&mut s);
        (CHANGES.fetch_add(1, Ordering::Relaxed) + 1, serde_json::to_vec_pretty(&*s))
    };
    if let Ok(json) = json
        && load_error().is_none()
    {
        save_newest(&SAVED, change, || {
            let p = path();
            crate::private_files::write_atomic(&p, &json).map_err(|e| eprintln!("{}: {e}", p.display())).is_ok()
        });
    }
}

/// Run `write` for `change` unless a newer change is already saved: two saves at once
/// never leave the older one on disk. `write` says whether it saved.
fn save_newest(saved: &Mutex<u64>, change: u64, write: impl FnOnce() -> bool) {
    let mut newest = saved.lock().unwrap();
    if change > *newest && write() {
        *newest = change;
    }
}

pub fn secs(n: u64) -> std::time::Duration {
    std::time::Duration::from_secs(n.max(1))
}

/// How a confirmation that must not be clicked through is made: the names typed, or a wait
/// of this many seconds. Names typed when the settings file could not be read.
pub fn confirm_wait() -> Option<u64> {
    if load_error().is_some() {
        return None;
    }
    with(confirm_wait_in)
}

fn confirm_wait_in(s: &Settings) -> Option<u64> {
    (s.confirm_style == ConfirmStyle::Wait).then(|| clamp_wait(s.confirm_wait_secs))
}

/// Whether writes on this context need an extra confirmation (a name typed, or a wait). A context never set
/// is guessed from the kubeconfig; one the kubeconfig does not have now (or a kubeconfig
/// that can't be read, say while a tool rewrites it) can't be guessed, so it is protected.
/// So is every context not set when the settings file can't be read.
pub fn is_protected(context: &str) -> bool {
    migrate_protection();
    match with(|s| s.protection.get(context).copied()) {
        Some(chosen) => chosen,
        None if protection_lost() => true,
        None => match crate::k8s::load_contexts() {
            Ok(c) => guess_protected(&c.names, &c.risky, context),
            Err(_) => true,
        },
    }
}

/// Whether the protection choices could not be read: every context not set is protected.
pub fn protection_lost() -> bool {
    load_error().is_some() || with(|s| s.protect_unset)
}

/// The contexts of a kubeconfig that are protected (see [`is_protected`]), from one read of it.
pub fn protected_contexts(contexts: &crate::k8s::Contexts) -> Vec<String> {
    migrate_protection();
    if protection_lost() {
        return with(|s| {
            contexts.names.iter().filter(|n| s.protection.get(*n).copied().unwrap_or(true)).cloned().collect()
        });
    }
    with(|s| protected_among(&s.protection, &contexts.names, &contexts.risky))
}

/// The kubeconfig's contexts that are not protected (see [`is_protected`]), for agent rules
/// naming `@protected`. Empty when the kubeconfig can't be read: every context then counts
/// as protected.
pub fn unprotected_contexts() -> Vec<String> {
    let Ok(contexts) = crate::k8s::load_contexts() else { return Vec::new() };
    let protected: std::collections::HashSet<String> = protected_contexts(&contexts).into_iter().collect();
    contexts.names.into_iter().filter(|n| !protected.contains(n)).collect()
}

fn protected_among(
    chosen: &std::collections::BTreeMap<String, bool>,
    names: &[String],
    risky: &[String],
) -> Vec<String> {
    names
        .iter()
        .filter(|n| chosen.get(*n).copied().unwrap_or_else(|| guess_protected(names, risky, n)))
        .cloned()
        .collect()
}

fn guess_protected(names: &[String], risky: &[String], context: &str) -> bool {
    !names.iter().any(|n| n == context) || risky.iter().any(|r| r == context)
}

/// Protect a context or not; contexts never set keep being guessed.
pub fn set_protected(context: &str, on: bool) {
    migrate_protection();
    update(|s| {
        s.protection.insert(context.to_string(), on);
    });
}

/// Whether the window and agents must never write to this context (see
/// `Settings::read_only`). Every context without a choice is read-only when the choices, or
/// the settings file, could not be read.
pub fn is_read_only(context: &str) -> bool {
    with(|s| read_only_in(s, context, load_error().is_some()))
}

/// [`is_read_only`] in `s`; `file_lost`: the settings file could not be read.
fn read_only_in(s: &Settings, context: &str, file_lost: bool) -> bool {
    s.read_only.get(context).copied().unwrap_or(file_lost || s.read_only_unset)
}

/// Make a context read-only or not.
pub fn set_read_only(context: &str, on: bool) {
    update(|s| {
        s.read_only.insert(context.to_string(), on);
    });
}

/// Follow the read-only choices in settings.json, for `recouvrance serve`: it has no window
/// to change them, and the app (or a hand edit) changes the file while it runs. Checked every
/// two seconds, only when the file changed. Nothing is saved.
pub fn watch_read_only() {
    let _ = std::thread::spawn(|| {
        let mut seen = None;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let p = path();
            let modified = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
            if modified.is_none() || modified == seen {
                continue;
            }
            // A file that can't be read now keeps the choices as they are.
            if let Ok(bytes) = std::fs::read(&p) {
                read_only_from(&mut SETTINGS.write().unwrap(), &bytes);
                seen = modified;
            }
        }
    });
}

/// Take the read-only choices from the settings file's bytes. Choices that don't read make
/// every context without one read-only, as when the file is loaded.
fn read_only_from(s: &mut Settings, bytes: &[u8]) {
    let (disk, _, _) = from_bytes(bytes);
    s.read_only = disk.read_only;
    s.read_only_unset = disk.read_only_unset;
}

/// Turn the old fixed list into per-context choices, once.
fn migrate_protection() {
    if with(|s| s.protected_contexts.is_none()) {
        return;
    }
    update(|s| {
        let list = s.protected_contexts.take().unwrap_or_default();
        migrate_list(&mut s.protection, list);
    });
}

/// Listed contexts stay protected. Others are guessed again: the list cannot tell a
/// context unprotected by hand from one created after it was written, and guessing
/// errs on the safe side.
fn migrate_list(protection: &mut std::collections::BTreeMap<String, bool>, list: Vec<String>) {
    for ctx in list {
        protection.entry(ctx).or_insert(true);
    }
}

pub const COLORS: [(&str, &str); 7] = [
    ("none", ""),
    ("red", "#ef4444"),
    ("orange", "#f97316"),
    ("yellow", "#eab308"),
    ("green", "#22c55e"),
    ("blue", "#3b82f6"),
    ("purple", "#a855f7"),
];

pub fn context_color(context: &str) -> Option<u32> {
    with(|s| {
        let hex = s.context_colors.get(context)?.trim_start_matches('#');
        u32::from_str_radix(hex, 16).ok()
    })
}

/// Put `items` at the front of a recent list, keeping at most `max`.
pub fn push_recent(list: &mut Vec<String>, items: &[String], max: usize) {
    for item in items.iter().rev() {
        list.retain(|x| x != item);
        list.insert(0, item.clone());
    }
    list.truncate(max);
}

/// A set of contexts as sessions are kept by: sorted, without repeats or empty names. The
/// order they were picked in only says which one is primary.
pub fn session_set(contexts: &[String]) -> Vec<String> {
    let mut set: Vec<String> = contexts.iter().filter(|c| !c.is_empty()).cloned().collect();
    set.sort();
    set.dedup();
    set
}

/// Where this set of contexts was left, if it was.
pub fn session(contexts: &[String]) -> Option<Session> {
    let set = session_set(contexts);
    if set.is_empty() {
        return None;
    }
    with(|s| s.sessions.iter().find(|x| x.contexts == set).cloned())
}

/// Remember where a set of contexts was left. Nothing is written when it is already so.
pub fn save_session(mut session: Session) {
    session.contexts = session_set(&session.contexts);
    if session.contexts.is_empty() || with(|s| s.sessions.first() == Some(&session)) {
        return;
    }
    update(|s| keep_session(&mut s.sessions, session));
}

/// `session` first, in place of the one for the same set, the newest `SESSIONS_KEPT` kept.
fn keep_session(list: &mut Vec<Session>, session: Session) {
    list.retain(|x| x.contexts != session.contexts);
    list.insert(0, session);
    list.truncate(SESSIONS_KEPT);
}

/// The context sets, in the order they were first saved.
pub fn context_sets() -> Vec<ContextSet> {
    with(|s| s.context_sets.clone())
}

/// Save `set`, in place of the set of the same name.
pub fn save_context_set(set: ContextSet) {
    update(|s| put_context_set(&mut s.context_sets, set));
}

/// Forget the set named `name`.
pub fn delete_context_set(name: &str) {
    update(|s| s.context_sets.retain(|x| x.name != name));
}

/// `set` in place of the one of the same name, else at the end: a set saved again keeps
/// its place in the picker.
fn put_context_set(list: &mut Vec<ContextSet>, set: ContextSet) {
    match list.iter_mut().find(|x| x.name == set.name) {
        Some(old) => *old = set,
        None => list.push(set),
    }
}

/// Where the window was left, when that reads (see `WindowPlace::usable`).
pub fn window_place() -> Option<WindowPlace> {
    with(|s| s.window.and_then(WindowPlace::usable))
}

/// Remember where the window is. Nothing is written when it is already there.
pub fn save_window(place: WindowPlace) {
    let Some(place) = place.usable() else { return };
    if with(|s| s.window == Some(place)) {
        return;
    }
    update(|s| s.window = Some(place));
}

/// A visit counts half as much after a week without one.
const VISIT_HALF_LIFE: f64 = 7.0 * 24.0 * 3600.0;
/// Namespaces remembered per context.
const VISITS_KEPT: usize = 30;

/// Frecency: the number of visits, halved for every week since the last one.
pub fn visit_score(v: &NamespaceVisit, now: i64) -> f64 {
    let age = (now - v.last).max(0) as f64;
    f64::from(v.count) * 0.5f64.powf(age / VISIT_HALF_LIFE)
}

/// Best score first; ties go to the latest visit.
fn rank(list: &mut [NamespaceVisit], now: i64) {
    list.sort_by(|a, b| visit_score(b, now).total_cmp(&visit_score(a, now)).then(b.last.cmp(&a.last)));
}

/// Count one visit to each of `names`, keeping the best `VISITS_KEPT`.
pub fn record_visits(list: &mut Vec<NamespaceVisit>, names: &[String], now: i64) {
    for name in names {
        match list.iter_mut().find(|v| &v.name == name) {
            Some(v) => {
                v.count = v.count.saturating_add(1);
                v.last = now;
            }
            None => list.push(NamespaceVisit { name: name.clone(), count: 1, last: now }),
        }
    }
    rank(list, now);
    list.truncate(VISITS_KEPT);
}

/// Namespaces visited on `context`, most frequent first.
pub fn frequent_namespaces(context: &str) -> Vec<String> {
    migrate_recent_namespaces();
    let now = unix_now();
    let mut list = with(|s| s.namespace_visits.get(context).cloned().unwrap_or_default());
    rank(&mut list, now);
    list.into_iter().map(|v| v.name).collect()
}

/// Remember that `names` were chosen on `context`.
pub fn visit_namespaces(context: &str, names: &[String]) {
    if context.is_empty() || names.is_empty() {
        return;
    }
    migrate_recent_namespaces();
    let now = unix_now();
    update(|s| record_visits(s.namespace_visits.entry(context.to_string()).or_default(), names, now));
}

/// Turn the old recent lists into visits, once.
fn migrate_recent_namespaces() {
    if with(|s| s.recent_namespaces.is_empty()) {
        return;
    }
    let now = unix_now();
    update(|s| {
        for (ctx, recent) in std::mem::take(&mut s.recent_namespaces) {
            let visits = s.namespace_visits.entry(ctx).or_default();
            if visits.is_empty() {
                *visits = recent_as_visits(&recent, now);
            }
        }
    });
}

/// One visit each, a second apart, so the newest still ranks first.
fn recent_as_visits(recent: &[String], now: i64) -> Vec<NamespaceVisit> {
    recent.iter().enumerate().map(|(i, n)| NamespaceVisit { name: n.clone(), count: 1, last: now - i as i64 }).collect()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::{
        ConfirmStyle, DeleteConfirm, NamespaceVisit, Settings, by_field, confirm_wait_in, delete_confirm_in,
        guess_protected, load_from, load_json, migrate_list, protected_among, recent_as_visits, record_visits,
        save_newest, visit_score,
    };

    #[test]
    fn an_identity_to_run_as_that_does_not_read_is_none() {
        let text = br#"{"clusters": {
            "a": {"node_shell_image": "alpine", "run_as": {"user": "super-admin", "groups": "ops"}},
            "b": {"run_as": {"user": " ", "groups": ["ops"]}},
            "c": {"run_as": "super-admin"},
            "d": {"run_as": {"user": "super-admin", "groups": ["ops"]}}
        }}"#;
        let s: Settings = serde_json::from_slice(text).unwrap();
        assert_eq!(s.clusters["a"].node_shell_image.as_deref(), Some("alpine"));
        assert_eq!(s.clusters["a"].run_as, None);
        assert_eq!(s.clusters["b"].run_as, None);
        assert_eq!(s.clusters["c"].run_as, None);
        let d = s.clusters["d"].run_as.clone().unwrap();
        assert_eq!((d.user.as_str(), d.groups), ("super-admin", vec!["ops".to_string()]));
        // None by default, and not written then.
        let json = serde_json::to_string(&Settings::default()).unwrap();
        assert!(!json.contains("run_as"));
        assert!(!serde_json::to_string(&super::ClusterSettings::default()).unwrap().contains("run_as"));
    }

    #[test]
    fn a_bad_field_keeps_the_others() {
        let text =
            br#"{"protection": {"shop": true}, "delete_confirm": "from-a-newer-version", "metrics_refresh": 42}"#;
        assert!(serde_json::from_slice::<Settings>(text).is_err());
        let s: Settings = by_field(text).unwrap();
        assert_eq!(s.protection.get("shop"), Some(&true));
        assert_eq!(s.metrics_refresh, 42);
        assert_eq!(s.delete_confirm, Settings::default().delete_confirm);
        assert!(by_field::<Settings>(b"{ not json").is_none());
    }

    #[test]
    fn confirmations_wait_by_default_and_ask_for_names_when_the_style_does_not_read() {
        // Older files have neither key: a 3 second wait.
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(confirm_wait_in(&s), Some(3));
        let s: Settings = serde_json::from_str(r#"{"confirm_style": "type", "confirm_wait_secs": 5}"#).unwrap();
        assert_eq!(confirm_wait_in(&s), None);
        let s: Settings = serde_json::from_str(r#"{"confirm_style": "wait", "confirm_wait_secs": 5}"#).unwrap();
        assert_eq!(confirm_wait_in(&s), Some(5));
        for bad in [r#""maybe""#, "null", "1", r#"{"wait": 3}"#, r#""Wait""#] {
            let text = format!(r#"{{"confirm_style": {bad}, "metrics_refresh": 42}}"#);
            let s: Settings = serde_json::from_str(&text).unwrap();
            assert_eq!(s.confirm_style, ConfirmStyle::Type, "{bad}");
            assert_eq!(confirm_wait_in(&s), None, "{bad}");
            assert_eq!(s.metrics_refresh, 42);
        }
        // Kept within 1..=30; a value that isn't a whole number waits the longest.
        for (secs, want) in [("0", 1), ("2", 2), ("1000", 30), ("-1", 30), (r#""3""#, 30), ("2.5", 30), ("null", 30)] {
            let text = format!(r#"{{"confirm_style": "wait", "confirm_wait_secs": {secs}}}"#);
            let s: Settings = serde_json::from_str(&text).unwrap();
            assert_eq!(confirm_wait_in(&s), Some(want), "{secs}");
        }
        // A wait set by hand beyond the range is still kept in it when read.
        let s = Settings { confirm_wait_secs: 0, ..Settings::default() };
        assert_eq!(confirm_wait_in(&s), Some(1));
    }

    #[test]
    fn a_bad_delete_kind_asks_for_the_name_and_keeps_the_others() {
        let text = br#"{"delete_confirm_kinds": {"Pod": "off", "Node": "maybe"}, "metrics_refresh": 42}"#;
        let s: Settings = serde_json::from_slice(text).unwrap();
        assert_eq!(s.delete_confirm_kinds.get("Pod"), Some(&DeleteConfirm::Off));
        assert_eq!(s.delete_confirm_kinds.get("Node"), Some(&DeleteConfirm::Type));
        assert_eq!(s.metrics_refresh, 42);
    }

    #[test]
    fn timeline_settings_that_do_not_read_keep_the_others() {
        let (s, error, lost) = super::from_bytes(br#"{"timeline": {"max_mb": "big"}, "metrics_refresh": 7}"#);
        assert!(error.is_some() && !lost);
        assert_eq!((s.timeline, s.metrics_refresh), (super::TimelineSettings::default(), 7));
        let s: Settings =
            serde_json::from_str(r#"{"timeline": {"kinds": ["configmaps"], "max_mb": 0, "days": 9999}}"#).unwrap();
        assert!(s.timeline.record, "on unless turned off");
        assert_eq!(s.timeline.kinds, ["configmaps"]);
        assert_eq!((s.timeline.max_mb(), s.timeline.days()), (1, 365));
    }

    /// The Argo CD page's state reads back as saved; one that doesn't read is the default
    /// page, and the other settings stay.
    #[test]
    fn the_argo_cd_page_state_reads_back_or_is_the_default() {
        use crate::ui::argo::envs::{Filter, GroupBy};
        let page = super::ArgoPage {
            filter: Filter {
                needs_look: true,
                project: Some("shop".into()),
                query: "1.4".into(),
                ..Default::default()
            },
            group_by: GroupBy::Project,
            folded: vec!["shop / prod".into()],
            expanded: vec![("https://argocd.example.com".into(), "argocd".into(), "shop-prod-a".into())],
            list: true,
        };
        let saved = serde_json::to_vec(&Settings { argocd_page: page.clone(), ..Settings::default() }).unwrap();
        let (s, error, _) = super::from_bytes(&saved);
        assert!(error.is_none());
        assert_eq!(s.argocd_page, page);
        let (s, error, lost) =
            super::from_bytes(br#"{"argocd_page": {"group_by": "Sideways", "folded": ["x"]}, "metrics_refresh": 7}"#);
        assert!(error.is_some() && !lost);
        assert_eq!((s.argocd_page, s.metrics_refresh), (super::ArgoPage::default(), 7));
        // A field it doesn't know is left: the rest reads.
        let (s, _, _) = super::from_bytes(br#"{"argocd_page": {"folded": ["x"], "later": 1}}"#);
        assert_eq!(s.argocd_page.folded, ["x"]);
    }

    #[test]
    fn an_old_protected_list_that_does_not_read_protects_everything() {
        let (_, _, lost) = super::from_bytes(br#"{"protected_contexts": "prod"}"#);
        assert!(lost);
        let (_, _, lost) = super::from_bytes(br#"{"protected_contexts": ["prod"], "metrics_refresh": "x"}"#);
        assert!(!lost);
    }

    #[test]
    fn a_context_missing_from_the_kubeconfig_is_protected() {
        let names = ["dev".to_string(), "prod".to_string()];
        let risky = ["prod".to_string()];
        assert!(!guess_protected(&names, &risky, "dev"));
        assert!(guess_protected(&names, &risky, "prod"));
        assert!(guess_protected(&names, &risky, "shop"));
    }

    #[test]
    fn protection_set_by_hand_wins_over_the_guess() {
        let names = ["dev".to_string(), "prod".to_string(), "lab".to_string()];
        let risky = ["prod".to_string()];
        let chosen = [("prod".to_string(), false), ("dev".to_string(), true)].into_iter().collect();
        assert_eq!(protected_among(&chosen, &names, &risky), ["dev"]);
        assert_eq!(protected_among(&Default::default(), &names, &risky), ["prod"]);
    }

    const DAY: i64 = 24 * 3600;

    fn visit(name: &str, count: u32, last: i64) -> NamespaceVisit {
        NamespaceVisit { name: name.into(), count, last }
    }

    #[test]
    fn score_halves_every_week() {
        let now = 100 * DAY;
        assert_eq!(visit_score(&visit("a", 8, now), now), 8.0);
        assert!((visit_score(&visit("a", 8, now - 7 * DAY), now) - 4.0).abs() < 1e-9);
        assert!((visit_score(&visit("a", 8, now - 21 * DAY), now) - 1.0).abs() < 1e-9);
        // A clock that went backwards does not inflate the score.
        assert_eq!(visit_score(&visit("a", 8, now + DAY), now), 8.0);
    }

    #[test]
    fn frequent_and_recent_beat_old_habits() {
        let now = 100 * DAY;
        let mut list = vec![visit("legacy", 20, now - 60 * DAY), visit("monitoring", 3, now - DAY)];
        let names = |l: &[NamespaceVisit]| l.iter().map(|v| v.name.clone()).collect::<Vec<_>>();
        record_visits(&mut list, &["kube-system".into()], now);
        // 20 visits two months ago weigh less than one today.
        assert_eq!(names(&list), ["monitoring", "kube-system", "legacy"]);
        for _ in 0..3 {
            record_visits(&mut list, &["kube-system".into()], now);
        }
        assert_eq!(names(&list), ["kube-system", "monitoring", "legacy"]);
        assert_eq!(list[0].count, 4);
        assert_eq!(list[0].last, now);
    }

    #[test]
    fn visits_are_capped() {
        let mut list = Vec::new();
        for i in 0..40 {
            record_visits(&mut list, &[format!("ns-{i}")], i);
        }
        assert_eq!(list.len(), 30);
        // The oldest single visits go first.
        assert_eq!(list[0].name, "ns-39");
        assert!(!list.iter().any(|v| v.name == "ns-9"));
    }

    #[test]
    fn old_recent_namespaces_still_load() {
        let s: Settings = serde_json::from_str(r#"{"recent_namespaces": {"kind-dev": ["b", "a"]}}"#).unwrap();
        assert_eq!(s.recent_namespaces["kind-dev"], ["b", "a"]);
        assert!(s.namespace_visits.is_empty());
        let visits = recent_as_visits(&s.recent_namespaces["kind-dev"], 1000);
        assert_eq!(visits, [visit("b", 1, 1000), visit("a", 1, 999)]);
        // Once migrated, the old key is not written back.
        let s = Settings { recent_namespaces: Default::default(), ..s };
        assert!(!serde_json::to_string(&s).unwrap().contains("recent_namespaces"));
    }

    #[test]
    fn old_protection_lists_still_load() {
        let s: Settings = serde_json::from_str(r#"{"protected_contexts": ["prod-eu"]}"#).unwrap();
        assert_eq!(s.protected_contexts.as_deref(), Some(&["prod-eu".to_string()][..]));
        assert!(s.protection.is_empty());
        // Migrated: written back without the old key.
        let s = Settings { protected_contexts: None, ..s };
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("protected_contexts"));
        let s: Settings = serde_json::from_str(r#"{"protection": {"kind-dev": false}}"#).unwrap();
        assert_eq!(s.protection.get("kind-dev"), Some(&false));
    }

    #[test]
    fn deletes_follow_how_much_a_kind_matters_by_default() {
        // Older settings files have neither key and get the defaults.
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.delete_confirm, DeleteConfirm::Ask);
        for k in ["Node", "Namespace", "StatefulSet", "Deployment", "PersistentVolumeClaim"] {
            assert_eq!(s.delete_confirm_kinds.get(k), Some(&DeleteConfirm::Type), "{k}");
        }
        assert_eq!(s.delete_confirm_kinds.get("Pod"), Some(&DeleteConfirm::Off));
        assert_eq!(s.delete_confirm_kinds.get("Service"), None, "a Service asks, like the default");
        let s: Settings =
            serde_json::from_str(r#"{"delete_confirm": "ask", "delete_confirm_kinds": {"Namespace": "type"}}"#)
                .unwrap();
        assert_eq!(s.delete_confirm, DeleteConfirm::Ask);
        assert_eq!(s.delete_confirm_kinds.len(), 1);
    }

    #[test]
    fn a_kind_saved_in_lowercase_still_applies() {
        let s: Settings =
            serde_json::from_str(r#"{"delete_confirm": "off", "delete_confirm_kinds": {"deployment": "type"}}"#)
                .unwrap();
        assert_eq!(delete_confirm_in(&s, "apps", "Deployment"), DeleteConfirm::Type);
        assert_eq!(delete_confirm_in(&s, "", "Service"), DeleteConfirm::Off);
    }

    #[test]
    fn a_custom_resource_is_not_a_built_in_kind_of_the_same_name() {
        // The default deletes a Job right away; a Volcano Job asks, like any custom resource.
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(delete_confirm_in(&s, "batch", "Job"), DeleteConfirm::Off);
        assert_eq!(delete_confirm_in(&s, "batch.volcano.sh", "Job"), DeleteConfirm::Ask);
        // A choice for one group's Cluster leaves another's alone.
        let s: Settings = serde_json::from_str(
            r#"{"delete_confirm": "ask", "delete_confirm_kinds": {"Cluster.cluster.x-k8s.io": "off"}}"#,
        )
        .unwrap();
        assert_eq!(delete_confirm_in(&s, "cluster.x-k8s.io", "Cluster"), DeleteConfirm::Off);
        assert_eq!(delete_confirm_in(&s, "postgresql.cnpg.io", "Cluster"), DeleteConfirm::Ask);
        // Saved without its group by an older version: still applies when it asks more.
        let s: Settings =
            serde_json::from_str(r#"{"delete_confirm": "off", "delete_confirm_kinds": {"Cluster": "type"}}"#).unwrap();
        assert_eq!(delete_confirm_in(&s, "postgresql.cnpg.io", "Cluster"), DeleteConfirm::Type);
    }

    #[test]
    fn old_list_becomes_choices() {
        let mut protection = std::collections::BTreeMap::from([("kind-dev".to_string(), false)]);
        migrate_list(&mut protection, vec!["staging".into(), "kind-dev".into()]);
        // Listed ones are protected; a choice made since is kept.
        assert_eq!(protection.get("staging"), Some(&true));
        assert_eq!(protection.get("kind-dev"), Some(&false));
        assert_eq!(protection.len(), 2);
    }

    fn scratch() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("recouvrance-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn bad_json_is_kept_aside() {
        let d = scratch();
        let p = d.join("settings.json");
        std::fs::write(&p, b"{ not json").unwrap();
        let v: std::collections::HashMap<String, i32> = load_json(&p);
        assert!(v.is_empty());
        assert!(!p.exists());
        let backups: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("settings.json.bad-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(backups[0].path()).unwrap(), b"{ not json");
        // Missing: defaults, nothing created.
        let v: Vec<String> = load_json(&d.join("missing.json"));
        assert!(v.is_empty());
        std::fs::remove_dir_all(d).unwrap();
    }

    /// A symlinked file that doesn't read is kept aside where it is, the link left in place:
    /// the next save writes through the link, which stays one.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_bad_file_keeps_its_link() {
        let d = scratch();
        let repo = d.join("dotfiles");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("settings.json"), b"{ not json").unwrap();
        let p = d.join("settings.json");
        std::os::unix::fs::symlink("dotfiles/settings.json", &p).unwrap();
        let v: std::collections::HashMap<String, i32> = load_json(&p);
        assert!(v.is_empty());
        let backups: Vec<_> = std::fs::read_dir(&repo).unwrap().filter_map(Result::ok).collect();
        assert_eq!(backups.len(), 1);
        assert!(backups[0].file_name().to_string_lossy().starts_with("settings.json.bad-"));
        assert_eq!(std::fs::read(backups[0].path()).unwrap(), b"{ not json");
        crate::private_files::write_atomic(&p, b"{}").unwrap();
        assert!(std::fs::symlink_metadata(&p).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(repo.join("settings.json")).unwrap(), b"{}");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn a_settings_file_that_cant_be_read_is_left_alone() {
        let d = scratch();
        // A folder where the file should be: it exists, but can't be read.
        let p = d.join("settings.json");
        std::fs::create_dir_all(&p).unwrap();
        let (s, error) = load_from(&p);
        assert!(s.protection.is_empty());
        assert!(error.is_some_and(|e| e.contains("settings.json")));
        assert!(p.is_dir());
        // Missing: the defaults, without an error.
        let (_, error) = load_from(&d.join("missing.json"));
        assert_eq!(error, None);
        // Other files are kept aside, like one that does not parse.
        let other = d.join("favorites.json");
        std::fs::create_dir_all(&other).unwrap();
        let v: Vec<String> = load_json(&other);
        assert!(v.is_empty());
        assert!(!other.exists());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn lost_protection_choices_stay_lost_after_a_save() {
        let d = scratch();
        let p = d.join("settings.json");
        std::fs::write(&p, br#"{"protection": "not a map", "metrics_refresh": 7}"#).unwrap();
        let (s, error) = load_from(&p);
        assert_eq!(error, None);
        assert!(s.protect_unset);
        // The next save keeps it, so the next launch doesn't guess the contexts again.
        let saved = serde_json::to_vec(&s).unwrap();
        let (again, _, lost) = super::from_bytes(&saved);
        assert!(!lost);
        assert!(again.protect_unset);
        // A file without it guesses; a value that isn't false (a hand edit) doesn't.
        assert!(!super::from_bytes(b"{}").0.protect_unset);
        assert!(super::from_bytes(br#"{"protect_unset": "x"}"#).0.protect_unset);
        assert!(!serde_json::to_string(&Settings::default()).unwrap().contains("protect_unset"));
        std::fs::remove_dir_all(d).unwrap();
    }

    /// By default no context is read-only: a new install, and a file from before the setting.
    /// A settings file that can't be read makes every context without a choice read-only.
    #[test]
    fn no_context_is_read_only_unless_set_or_lost() {
        let ro = super::read_only_in;
        assert!(!ro(&Settings::new_install(), "prod", false));
        assert!(!ro(&super::from_bytes(br#"{"protection": {"prod": true}}"#).0, "prod", false));
        let set = super::from_bytes(br#"{"read_only": {"prod": true, "dev": false}}"#).0;
        assert!(ro(&set, "prod", false) && !ro(&set, "dev", false) && !ro(&set, "other", false));
        // The file unreadable: only a context set back stays writable.
        assert!(ro(&set, "other", true) && !ro(&set, "dev", true));
        assert!(super::from_bytes(br#"{"read_only_unset": "x"}"#).0.read_only_unset);
        assert!(!serde_json::to_string(&Settings::default()).unwrap().contains("read_only_unset"));
    }

    /// `serve` follows the file: a context made read-only there is read-only here, the other
    /// settings are left alone, and choices that no longer read make every context read-only.
    #[test]
    fn read_only_choices_follow_the_file() {
        let mut s = Settings { metrics_refresh: 9, ..Settings::default() };
        super::read_only_from(&mut s, br#"{"read_only": {"prod": true}, "metrics_refresh": 3}"#);
        assert!(super::read_only_in(&s, "prod", false) && !super::read_only_in(&s, "dev", false));
        assert_eq!(s.metrics_refresh, 9);
        super::read_only_from(&mut s, br#"{"read_only": {}}"#);
        assert!(!super::read_only_in(&s, "prod", false));
        super::read_only_from(&mut s, br#"{"read_only": {"prod": fal"#);
        assert!(super::read_only_in(&s, "dev", false));
    }

    /// What's new is for updates: a new install has seen the version it starts with, a file
    /// from before What's new has seen none (so this version's notes show once).
    #[test]
    fn a_new_install_has_seen_its_version_and_an_older_file_none() {
        let d = scratch();
        let (s, error) = load_from(&d.join("settings.json"));
        assert_eq!(error, None);
        assert_eq!(s.last_seen_version.as_deref(), Some(crate::updates::VERSION));
        let s: Settings = serde_json::from_str(r#"{"metrics_refresh": 7}"#).unwrap();
        assert_eq!(s.last_seen_version, None);
        assert!(!serde_json::to_string(&s).unwrap().contains("last_seen_version"), "not written while unknown");
        // One that doesn't read is unknown too, and costs nothing else.
        let (s, _, lost) = super::from_bytes(br#"{"last_seen_version": 9, "metrics_refresh": 7}"#);
        assert!(!lost);
        assert_eq!((s.last_seen_version, s.metrics_refresh), (None, 7));
        let s: Settings = serde_json::from_str(r#"{"last_seen_version": "0.9.0"}"#).unwrap();
        assert_eq!(s.last_seen_version.as_deref(), Some("0.9.0"));
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn sessions_and_a_window_that_do_not_read_cost_nothing_else() {
        // Never an error: the file is not kept aside, and every other setting stays.
        for (sessions, window) in [
            (r#""x""#, r#"{"x": "left"}"#),
            ("[1, null]", "[]"),
            (r#"{"contexts": ["a"]}"#, r#"{"x": 1e40, "y": 0, "width": 1000, "height": 700}"#),
        ] {
            let text = format!(r#"{{"sessions": {sessions}, "window": {window}, "metrics_refresh": 42}}"#);
            let (s, error, lost) = super::from_bytes(text.as_bytes());
            assert!(error.is_none() && !lost, "{text}");
            assert!(s.sessions.is_empty() && s.window.is_none(), "{text}");
            assert_eq!(s.metrics_refresh, 42);
        }
        // Older files have neither.
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(s.sessions.is_empty() && s.window.is_none());
        assert!(!serde_json::to_string(&Settings::default()).unwrap().contains("\"window\""));
    }

    #[test]
    fn a_session_entry_that_does_not_read_costs_only_itself() {
        let text = br#"{"sessions": [
            {"contexts": ["prod", "dev", "prod"], "active_tab": -1, "filters": {"pods": "web", "nodes": 3},
             "tabs": [{"page": "from-a-newer-version", "kind": "deployments.apps", "namespaces": ["a", 2], "filter": 5},
                      "not a tab",
                      {"page": "problems"}]},
            {"contexts": []},
            "not a session",
            {"contexts": ["dev", "prod"], "tabs": [{"page": "helm"}]},
            {"contexts": ["staging"], "tabs": {"page": "helm"}}
        ]}"#;
        let s: Settings = serde_json::from_slice(text).unwrap();
        let [first, staging] = &s.sessions[..] else { panic!("{:?}", s.sessions) };
        // Sorted, repeats dropped; the second entry for the same set is dropped too.
        assert_eq!(first.contexts, ["dev", "prod"]);
        assert_eq!(first.active_tab, 0);
        assert_eq!(first.filters, std::collections::BTreeMap::from([("pods".to_string(), "web".to_string())]));
        let tab = super::SavedTab {
            page: super::SavedPage::List,
            kind: Some("deployments.apps".into()),
            namespaces: vec!["a".into()],
            filter: String::new(),
        };
        let problems = super::SavedTab { page: super::SavedPage::Problems, ..Default::default() };
        assert_eq!(first.tabs, [tab, problems]);
        assert_eq!(staging.contexts, ["staging"]);
        assert!(staging.tabs.is_empty());
    }

    #[test]
    fn the_newest_session_comes_first_once_per_set() {
        let session = |contexts: &[&str], active_tab| super::Session {
            contexts: super::session_set(&contexts.iter().map(ToString::to_string).collect::<Vec<_>>()),
            active_tab,
            ..Default::default()
        };
        let mut list = Vec::new();
        super::keep_session(&mut list, session(&["a", "b"], 0));
        super::keep_session(&mut list, session(&["c"], 0));
        // The same set picked in another order replaces the one saved.
        super::keep_session(&mut list, session(&["b", "a"], 3));
        assert_eq!(list.len(), 2);
        assert_eq!((list[0].contexts.clone(), list[0].active_tab), (vec!["a".to_string(), "b".into()], 3));
        for i in 0..40 {
            super::keep_session(&mut list, session(&[format!("ctx-{i}").as_str()], 0));
        }
        assert_eq!(list.len(), super::SESSIONS_KEPT);
        assert_eq!(list[0].contexts, ["ctx-39"]);
    }

    #[test]
    fn a_window_place_is_at_least_the_smallest_window() {
        let place = |width, height| super::WindowPlace { x: -40., y: 30., width, height, maximized: true };
        let p = place(300., 200.).usable().unwrap();
        assert_eq!((p.x, p.y, p.width, p.height), (-40., 30., super::WINDOW_MIN.0, super::WINDOW_MIN.1));
        assert!(p.maximized);
        assert!(place(f32::INFINITY, 900.).usable().is_none());
        assert!(place(1200., f32::NAN).usable().is_none());
        assert!(place(1e9, 900.).usable().is_none());
    }

    #[test]
    fn an_older_save_never_overwrites_a_newer_one() {
        let saved = std::sync::Mutex::new(0);
        let mut written = Vec::new();
        save_newest(&saved, 2, || {
            written.push(2);
            true
        });
        save_newest(&saved, 1, || {
            written.push(1);
            true
        });
        // A failed save leaves room for the next one.
        save_newest(&saved, 3, || false);
        save_newest(&saved, 4, || {
            written.push(4);
            true
        });
        assert_eq!(written, [2, 4]);
    }
}

/// settings.json over many generated files with hand edits gone wrong: a bad value costs
/// only itself, and protection choices are never lost in a way that unprotects a context.
#[cfg(test)]
mod properties {
    use super::{Settings, from_bytes};
    use proptest::prelude::*;
    use serde_json::{Value, json};

    /// Values no setting reads (each field fails on at least one).
    fn bad() -> impl Strategy<Value = Value> {
        prop_oneof![
            Just(json!("from-a-newer-version")),
            Just(json!(-1)),
            Just(json!([[1]])),
            Just(json!({"x": [null]}))
        ]
    }

    fn protection() -> impl Strategy<Value = std::collections::BTreeMap<String, bool>> {
        prop::collection::btree_map("[a-z]{1,6}", any::<bool>(), 0..5)
    }

    /// Any JSON, with the keys sessions and windows have, nested a few levels.
    fn junk() -> impl Strategy<Value = Value> {
        let key = prop::sample::select(vec![
            "contexts",
            "tabs",
            "active_tab",
            "filters",
            "page",
            "kind",
            "namespaces",
            "filter",
            "x",
            "y",
            "width",
            "height",
            "maximized",
            "name",
            "other",
        ]);
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::from),
            any::<i64>().prop_map(Value::from),
            any::<f64>().prop_map(|n| json!(n)),
            "[a-z-]{0,6}".prop_map(Value::from),
        ];
        leaf.prop_recursive(4, 48, 6, move |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
                prop::collection::btree_map(key.clone(), inner, 0..6)
                    .prop_map(|m| Value::Object(m.into_iter().map(|(k, v)| (k.to_string(), v)).collect())),
            ]
        })
    }

    fn page() -> impl Strategy<Value = super::SavedPage> {
        use super::SavedPage::{Argo, Cluster, Helm, List, Problems, Timeline, Workloads};
        prop::sample::select(vec![List, Workloads, Cluster, Problems, Timeline, Helm, Argo])
    }

    fn tab() -> impl Strategy<Value = super::SavedTab> {
        let kind =
            prop::option::of(prop::sample::select(vec!["pods", "deployments.apps", "certificates.cert-manager.io"]));
        (page(), kind, prop::collection::vec("[a-z]{1,5}", 0..3), "[ -~]{0,8}").prop_map(
            |(page, kind, namespaces, filter)| super::SavedTab {
                page,
                kind: kind.map(String::from),
                namespaces,
                filter,
            },
        )
    }

    /// Sessions as the app saves them: one per set of contexts.
    fn sessions() -> impl Strategy<Value = Vec<super::Session>> {
        let session = (
            prop::collection::vec(tab(), 0..4),
            0..5usize,
            prop::collection::btree_map("[a-z.]{1,8}", "[ -~]{0,8}", 0..3),
        );
        prop::collection::btree_map(prop::collection::btree_set("[a-z]{1,4}", 1..3), session, 0..6).prop_map(|m| {
            (m.into_iter())
                .map(|(contexts, (tabs, active_tab, filters))| super::Session {
                    contexts: contexts.into_iter().collect(),
                    tabs,
                    active_tab,
                    filters,
                })
                .collect()
        })
    }

    proptest! {
        /// Spoiling some fields keeps every other one as it was, protection included.
        #[test]
        fn a_bad_field_costs_only_itself(
            protection in protection(),
            read_only in protection(),
            refresh in 1..1000u64,
            spoiled in prop::collection::btree_map(0..40usize, bad(), 0..4),
        ) {
            // The version seen is written only when set: set, so it can be spoiled too.
            let good = Settings {
                protection: protection.clone(),
                read_only: read_only.clone(),
                metrics_refresh: refresh,
                last_seen_version: Some("0.9.0".into()),
                ..Settings::default()
            };
            let Value::Object(mut fields) = serde_json::to_value(&good).unwrap() else { unreachable!() };
            let keys: Vec<String> =
                fields.keys().filter(|k| *k != "protection" && *k != "read_only").cloned().collect();
            let mut hit = std::collections::HashSet::new();
            for (i, v) in &spoiled {
                let key = &keys[i % keys.len()];
                hit.insert(key.clone());
                fields.insert(key.clone(), v.clone());
            }
            let (read, _, lost) = from_bytes(&serde_json::to_vec(&fields).unwrap());
            prop_assert!(!lost);
            prop_assert_eq!(&read.protection, &protection);
            prop_assert_eq!(&read.read_only, &read_only);
            prop_assert!(!read.read_only_unset);
            let read = serde_json::to_value(&read).unwrap();
            let want = serde_json::to_value(&good).unwrap();
            for key in &keys {
                if !hit.contains(key) {
                    prop_assert_eq!(&read[key], &want[key], "{} changed", key);
                }
            }
        }

        /// A confirm style that isn't exactly `wait` asks for names typed, and a wait is
        /// always within its range, whatever the file says.
        #[test]
        fn a_confirmation_never_reads_weaker_than_set(style in prop_oneof![bad(), Just(json!("wait")), Just(json!("type")), any::<i64>().prop_map(|n| json!(n))], secs in prop_oneof![bad(), any::<i64>().prop_map(|n| json!(n)), any::<f64>().prop_map(|n| json!(n))]) {
            let text = json!({"confirm_style": style, "confirm_wait_secs": secs, "metrics_refresh": 7});
            let (read, _, lost) = from_bytes(&serde_json::to_vec(&text).unwrap());
            prop_assert!(!lost);
            prop_assert_eq!(read.metrics_refresh, 7);
            match super::confirm_wait_in(&read) {
                None => prop_assert_ne!(&style, &json!("wait")),
                Some(w) => {
                    prop_assert_eq!(&style, &json!("wait"));
                    prop_assert!(super::CONFIRM_WAIT_RANGE.contains(&w));
                }
            }
        }

        /// Whatever width the file holds, the sidebar opens within its range, at the width
        /// saved when that one is.
        #[test]
        fn a_saved_sidebar_width_stays_in_range(width in prop_oneof![bad(), any::<f64>().prop_map(|n| json!(n))]) {
            let text = json!({"sidebar_width": width, "metrics_refresh": 7});
            let (read, _, lost) = from_bytes(&serde_json::to_vec(&text).unwrap());
            prop_assert!(!lost);
            prop_assert_eq!(read.metrics_refresh, 7);
            let w = super::sidebar_width_in(&read);
            prop_assert!(super::SIDEBAR_WIDTH_RANGE.contains(&w));
            if let Some(n) = width.as_f64().filter(|n| super::SIDEBAR_WIDTH_RANGE.contains(&(*n as f32))) {
                prop_assert_eq!(w, n as f32);
            }
        }

        /// Whatever `sessions` and `window` hold, the file reads without an error (it is not
        /// kept aside), the other settings stay, and what is read can be used as is: sessions
        /// for a set of contexts each, sorted, at most `SESSIONS_KEPT`; a window place with
        /// finite numbers, at least the smallest window.
        #[test]
        fn sessions_and_the_window_never_break_loading(
            sessions in junk(),
            window in junk(),
            protection in protection(),
        ) {
            let text = json!({"sessions": sessions, "window": window, "protection": protection, "metrics_refresh": 7});
            let (read, error, lost) = from_bytes(&serde_json::to_vec(&text).unwrap());
            prop_assert!(error.is_none(), "{error:?}");
            prop_assert!(!lost);
            prop_assert_eq!(read.metrics_refresh, 7);
            prop_assert_eq!(&read.protection, &protection);
            prop_assert!(read.sessions.len() <= super::SESSIONS_KEPT);
            let mut sets = std::collections::HashSet::new();
            for s in &read.sessions {
                prop_assert!(!s.contexts.is_empty());
                prop_assert_eq!(&super::session_set(&s.contexts), &s.contexts);
                prop_assert!(sets.insert(s.contexts.clone()), "{:?} twice", s.contexts);
            }
            if let Some(w) = read.window {
                prop_assert!([w.x, w.y, w.width, w.height].iter().all(|n| n.is_finite()));
                prop_assert!(w.width >= super::WINDOW_MIN.0 && w.height >= super::WINDOW_MIN.1);
            }
        }

        /// Whatever `context_sets` holds, the file reads without an error and the other
        /// settings stay; each set read has a trimmed name of its own and contexts without
        /// repeats or empty names.
        #[test]
        fn context_sets_never_break_loading(sets in junk(), protection in protection()) {
            let text = json!({"context_sets": sets, "protection": protection, "metrics_refresh": 7});
            let (read, error, lost) = from_bytes(&serde_json::to_vec(&text).unwrap());
            prop_assert!(error.is_none(), "{error:?}");
            prop_assert!(!lost);
            prop_assert_eq!(read.metrics_refresh, 7);
            prop_assert_eq!(&read.protection, &protection);
            let mut names = std::collections::HashSet::new();
            for set in &read.context_sets {
                prop_assert!(!set.name.is_empty() && set.name.trim() == set.name);
                prop_assert!(names.insert(set.name.clone()), "{} twice", set.name);
                prop_assert!(!set.contexts.is_empty());
                prop_assert!(set.contexts.iter().all(|c| !c.is_empty()));
                let mut seen = std::collections::HashSet::new();
                prop_assert!(set.contexts.iter().all(|c| seen.insert(c)));
            }
        }

        /// Sets saved are read back as they were, their contexts in the order saved; saving
        /// one again by name replaces it in place.
        #[test]
        fn context_sets_read_back_as_saved(
            saves in prop::collection::vec(("[a-c]", prop::collection::vec("[a-e]", 1..4)), 1..12),
        ) {
            let mut list: Vec<super::ContextSet> = Vec::new();
            for (name, contexts) in &saves {
                let set = super::ContextSet::new(name, contexts).unwrap();
                let before: Vec<String> = list.iter().map(|x| x.name.clone()).collect();
                super::put_context_set(&mut list, set.clone());
                let after: Vec<String> = list.iter().map(|x| x.name.clone()).collect();
                if before.contains(name) {
                    prop_assert_eq!(&before, &after);
                } else {
                    prop_assert_eq!(&after[..before.len()], &before[..]);
                    prop_assert_eq!(after.len(), before.len() + 1);
                }
                prop_assert_eq!(list.iter().find(|x| &x.name == name), Some(&set));
                prop_assert_eq!(set.contexts.first(), contexts.first());
            }
            let saved = Settings { context_sets: list.clone(), ..Settings::default() };
            let (read, error, _) = from_bytes(&serde_json::to_vec(&saved).unwrap());
            prop_assert!(error.is_none());
            prop_assert_eq!(read.context_sets, list);
        }

        /// Sessions and a window place saved are read back as they were.
        #[test]
        fn sessions_and_the_window_read_back_as_saved(
            sessions in sessions(),
            x in -5000i16..5000,
            y in -5000i16..5000,
            width in 900i16..8000,
            height in 560i16..5000,
            maximized in any::<bool>(),
        ) {
            // Whole pixels, as windows are placed.
            let (x, y, width, height) = (f32::from(x), f32::from(y), f32::from(width), f32::from(height));
            let window = Some(super::WindowPlace { x, y, width, height, maximized });
            let saved = Settings { sessions: sessions.clone(), window, ..Settings::default() };
            let (read, error, _) = from_bytes(&serde_json::to_vec(&saved).unwrap());
            prop_assert!(error.is_none());
            prop_assert_eq!(read.sessions, sessions);
            prop_assert_eq!(read.window, window);
        }

        /// However sessions are saved, one set of contexts keeps one session, the newest first,
        /// and no more than `SESSIONS_KEPT` are kept.
        #[test]
        fn the_newest_session_is_kept_once(saves in prop::collection::vec(prop::collection::vec("[a-c]", 1..3), 1..40)) {
            let mut list = Vec::new();
            for contexts in &saves {
                let set = super::session_set(contexts);
                super::keep_session(&mut list, super::Session { contexts: set.clone(), ..Default::default() });
                prop_assert_eq!(&list[0].contexts, &set);
            }
            prop_assert!(list.len() <= super::SESSIONS_KEPT);
            let mut sets = std::collections::HashSet::new();
            prop_assert!(list.iter().all(|s| sets.insert(s.contexts.clone())));
        }

        /// A protection entry that isn't true or false protects its context and keeps the others.
        #[test]
        fn a_bad_protection_entry_protects(protection in protection(), name in "[A-Z]{1,6}", value in bad()) {
            let mut text = json!({"protection": protection});
            text["protection"][&name] = value;
            let (read, _, lost) = from_bytes(&serde_json::to_vec(&text).unwrap());
            prop_assert!(!lost);
            prop_assert_eq!(read.protection.get(&name), Some(&true));
            for (k, v) in &protection {
                prop_assert_eq!(read.protection.get(k), Some(v));
            }
        }

        /// A file that doesn't read as an object, or whose protection isn't one, says the
        /// choices are lost (so every context is protected), never that there are none.
        #[test]
        fn lost_choices_are_reported(cut in 1..200usize, protection in protection(), bad in bad()) {
            prop_assume!(!protection.is_empty());
            let text = serde_json::to_vec(&json!({"protection": protection, "metrics_refresh": 5})).unwrap();
            let truncated = &text[..cut.min(text.len() - 1)];
            prop_assert!(from_bytes(truncated).2);
            let replaced = serde_json::to_vec(&json!({"protection": bad})).unwrap();
            prop_assert_eq!(from_bytes(&replaced).2, !bad.is_object());
        }

        /// Whatever the file holds, a context saved read-only reads read-only: a value that
        /// isn't true or false makes its context read-only, and when the choices can't be
        /// read at all (the field, or the file cut short), every context without one is.
        /// Saved again, it stays so.
        #[test]
        fn a_read_only_context_never_reads_writable(
            read_only in protection(),
            other in junk(),
            spoiled in prop::option::of(bad()),
            cut in prop::option::of(1..200usize),
            name in "[A-Z]{1,6}",
        ) {
            let mut text = json!({"read_only": read_only, "sessions": other, "metrics_refresh": 7});
            if let Some(bad) = &spoiled {
                text["read_only"] = bad.clone();
            }
            let bytes = serde_json::to_vec(&text).unwrap();
            let bytes = match cut {
                Some(cut) => &bytes[..cut.min(bytes.len() - 1)],
                None => &bytes[..],
            };
            let (read, _, _) = from_bytes(bytes);
            let ro = |s: &Settings, c: &str| super::read_only_in(s, c, false);
            if cut.is_some() || spoiled.as_ref().is_some_and(|b| !b.is_object()) {
                prop_assert!(ro(&read, &name), "a context without a choice is writable");
            } else if let Some(bad) = &spoiled {
                for k in bad.as_object().into_iter().flat_map(|o| o.keys()) {
                    prop_assert!(ro(&read, k));
                }
            } else {
                prop_assert!(!ro(&read, &name));
                for (k, v) in &read_only {
                    prop_assert_eq!(ro(&read, k), *v);
                }
            }
            let (again, error, _) = from_bytes(&serde_json::to_vec(&read).unwrap());
            prop_assert!(error.is_none());
            prop_assert_eq!(&again.read_only, &read.read_only);
            prop_assert_eq!(again.read_only_unset, read.read_only_unset);
        }
    }
}
