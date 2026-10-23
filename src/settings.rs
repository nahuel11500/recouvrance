//! User settings, stored in ~/.config/recouvrance/settings.json and readable from
//! any thread (the Kubernetes side reads refresh rates and kubeconfig paths).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock, RwLock};

use serde::{Deserialize, Deserializer, Serialize};

/// `protection` entry by entry: a value that isn't `true` or `false` (a hand edit) protects
/// its context, and never costs the other entries theirs.
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

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum Startup {
    /// Always show the context picker ("ask").
    #[serde(rename = "ask")]
    Picker,
    /// Reconnect to the contexts used last time (the default; older files said "picker").
    #[serde(alias = "picker")]
    Reconnect,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum InitialView {
    Pods,
    Workloads,
    Cluster,
}

/// A port-forward to bring back next time (pods are found again by service or name).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
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
    pub assistant_backend: String,
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
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NamespaceVisit {
    pub name: String,
    pub count: u32,
    pub last: i64,
}

/// Settings for one cluster (context).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
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
            prometheus: Default::default(),
            usage_range: 60,
            recent_contexts: Vec::new(),
            recent_namespaces: Default::default(),
            namespace_visits: Default::default(),
            saved_forwards: Vec::new(),
            assistant_model: crate::assistant::DEFAULT_MODEL.into(),
            assistant_backend: "auto".into(),
            assistant_notes: String::new(),
            assistant_repos: Vec::new(),
            metrics_enabled: true,
            accent: "brass".into(),
            filter_history: Vec::new(),
            saved_searches: Vec::new(),
            extra_namespaces: Default::default(),
            clusters: Default::default(),
            kubeconfig_folders: Vec::new(),
            context_files: Default::default(),
            open_browser_on_forward: true,
            recent_forwards: Vec::new(),
            delete_confirm: DeleteConfirm::Ask,
            delete_confirm_kinds: default_delete_confirm(),
        }
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
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("recouvrance").join("settings.json")
}

/// The settings file. A field that does not read (a hand edit, or a value from a newer
/// version) gets its default and the others, such as `protection`, are kept; the file
/// is kept aside as `load_json` does.
fn load() -> Settings {
    let (settings, error) = load_from(&path());
    if let Some(e) = error {
        eprintln!("{e}; using defaults, every context not set is protected, and nothing is saved");
        let _ = LOAD_ERROR.set(e);
    }
    settings
}

/// The settings at `p`, or the defaults and why when the file exists but can't be read
/// (permissions, a folder): then the file is left as it is.
fn load_from(p: &Path) -> (Settings, Option<String>) {
    let bytes = match std::fs::read(p) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Settings::default(), None),
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
/// protection choices were lost with it.
fn from_bytes(bytes: &[u8]) -> (Settings, Option<serde_json::Error>, bool) {
    match serde_json::from_slice(bytes) {
        Ok(v) => (v, None, false),
        Err(e) => {
            let lost = match serde_json::from_slice::<serde_json::Value>(bytes) {
                Ok(serde_json::Value::Object(fields)) => {
                    fields.get("protection").is_some_and(|p| !p.is_object())
                        || fields
                            .get("protected_contexts")
                            .is_some_and(|p| serde_json::from_value::<Option<Vec<String>>>(p.clone()).is_err())
                }
                _ => true,
            };
            (by_field(bytes).unwrap_or_default(), Some(e), lost)
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
/// not overwrite it. Says where it went, or why it could not be moved.
pub fn keep_aside(path: &Path) -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut bad = path.as_os_str().to_owned();
    bad.push(format!(".bad-{secs}"));
    match std::fs::rename(path, &bad) {
        Ok(()) => format!("kept as {}", Path::new(&bad).display()),
        Err(e) => format!("could not keep it: {e}"),
    }
}

/// Write a file atomically: a temporary file in the same directory (created if
/// needed), flushed, then renamed over the target. A crash never leaves half a file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp-{}", uuid::Uuid::new_v4().simple()));
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
        f.write_all(bytes)?;
        f.sync_all()
    });
    match written.and_then(|_| std::fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
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
            write_atomic(&p, &json).map_err(|e| eprintln!("{}: {e}", p.display())).is_ok()
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

/// Expand `~/` in a user-entered path.
pub fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(p),
    }
}

/// Whether writes on this context need an extra typed confirmation. A context never set
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
        DeleteConfirm, NamespaceVisit, Settings, by_field, delete_confirm_in, from_bytes, guess_protected, load_from,
        load_json, migrate_list, protected_among, recent_as_visits, record_visits, save_newest, visit_score,
        write_atomic,
    };

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
    fn a_bad_delete_kind_asks_for_the_name_and_keeps_the_others() {
        let text = br#"{"delete_confirm_kinds": {"Pod": "off", "Node": "maybe"}, "metrics_refresh": 42}"#;
        let s: Settings = serde_json::from_slice(text).unwrap();
        assert_eq!(s.delete_confirm_kinds.get("Pod"), Some(&DeleteConfirm::Off));
        assert_eq!(s.delete_confirm_kinds.get("Node"), Some(&DeleteConfirm::Type));
        assert_eq!(s.metrics_refresh, 42);
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
    fn atomic_write_replaces_and_creates_the_directory() {
        let d = scratch();
        let p = d.join("sub").join("x.json");
        write_atomic(&p, b"{\"a\":1}").unwrap();
        write_atomic(&p, b"{\"a\":2}").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"{\"a\":2}");
        // No temporary file left behind.
        assert_eq!(std::fs::read_dir(d.join("sub")).unwrap().count(), 1);
        std::fs::remove_dir_all(d).unwrap();
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
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("settings.json.bad-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(backups[0].path()).unwrap(), b"{ not json");
        // Missing: defaults, nothing created.
        let v: Vec<String> = load_json(&d.join("missing.json"));
        assert!(v.is_empty());
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
        let (again, _, lost) = from_bytes(&saved);
        assert!(!lost);
        assert!(again.protect_unset);
        // A file without it guesses; a value that isn't false (a hand edit) doesn't.
        assert!(!from_bytes(b"{}").0.protect_unset);
        assert!(from_bytes(br#"{"protect_unset": "x"}"#).0.protect_unset);
        assert!(!serde_json::to_string(&Settings::default()).unwrap().contains("protect_unset"));
        std::fs::remove_dir_all(d).unwrap();
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

    proptest! {
        /// Spoiling some fields keeps every other one as it was, protection included.
        #[test]
        fn a_bad_field_costs_only_itself(
            protection in protection(),
            refresh in 1..1000u64,
            spoiled in prop::collection::btree_map(0..40usize, bad(), 0..4),
        ) {
            let good = Settings { protection: protection.clone(), metrics_refresh: refresh, ..Settings::default() };
            let mut fields = match serde_json::to_value(&good).unwrap() {
                Value::Object(f) => f,
                _ => unreachable!(),
            };
            let keys: Vec<String> = fields.keys().filter(|k| *k != "protection").cloned().collect();
            let mut hit = std::collections::HashSet::new();
            for (i, v) in &spoiled {
                let key = &keys[i % keys.len()];
                hit.insert(key.clone());
                fields.insert(key.clone(), v.clone());
            }
            let (read, _, lost) = from_bytes(&serde_json::to_vec(&fields).unwrap());
            prop_assert!(!lost);
            prop_assert_eq!(&read.protection, &protection);
            let read = serde_json::to_value(&read).unwrap();
            let want = serde_json::to_value(&good).unwrap();
            for key in &keys {
                if !hit.contains(key) {
                    prop_assert_eq!(&read[key], &want[key], "{} changed", key);
                }
            }
        }

        /// A protection entry that isn't true or false protects its context and keeps the others.
        #[test]
        fn a_bad_protection_entry_protects(protection in protection(), name in "[A-Z]{1,6}", value in bad()) {
            let mut text = json!({"protection": protection.clone()});
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
            let replaced = serde_json::to_vec(&json!({"protection": bad.clone()})).unwrap();
            prop_assert_eq!(from_bytes(&replaced).2, !bad.is_object());
        }
    }
}
