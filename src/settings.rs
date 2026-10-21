//! User settings, stored in ~/.config/recouvrance/settings.json and readable from
//! any thread (the Kubernetes side reads refresh rates and kubeconfig paths).

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};

use serde::{Deserialize, Serialize};

/// What deleting asks before it runs (per kind, see `delete_confirm`).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
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
    pub protection: std::collections::BTreeMap<String, bool>,
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
    /// Open the browser after a one-click port-forward on a web port.
    pub open_browser_on_forward: bool,
    /// Port-forwards started recently, newest first (offered again in the sidebar).
    pub recent_forwards: Vec<SavedForward>,
    /// What deleting asks for kinds not in `delete_confirm_kinds`.
    pub delete_confirm: DeleteConfirm,
    /// What deleting asks, by kind (`Node`, `StatefulSet`…).
    pub delete_confirm_kinds: std::collections::BTreeMap<String, DeleteConfirm>,
}

/// What deleting an object of this kind (`Pod`, `Node`…) asks first.
pub fn delete_confirm(kind: &str) -> DeleteConfirm {
    with(|s| s.delete_confirm_kinds.get(kind).copied().unwrap_or(s.delete_confirm))
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
            prometheus: Default::default(),
            usage_range: 60,
            recent_contexts: Vec::new(),
            recent_namespaces: Default::default(),
            namespace_visits: Default::default(),
            saved_forwards: Vec::new(),
            assistant_model: crate::assistant::DEFAULT_MODEL.into(),
            assistant_backend: "auto".into(),
            metrics_enabled: true,
            accent: "brass".into(),
            filter_history: Vec::new(),
            saved_searches: Vec::new(),
            extra_namespaces: Default::default(),
            clusters: Default::default(),
            kubeconfig_folders: Vec::new(),
            open_browser_on_forward: true,
            recent_forwards: Vec::new(),
            // A node or a StatefulSet (and its pods' identity) is hard to get back: type to delete.
            delete_confirm: DeleteConfirm::Off,
            delete_confirm_kinds: [("Node", DeleteConfirm::Type), ("StatefulSet", DeleteConfirm::Type)]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        }
    }
}

static SETTINGS: LazyLock<RwLock<Settings>> = LazyLock::new(|| RwLock::new(load()));

fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config"));
    base.join("recouvrance").join("settings.json")
}

fn load() -> Settings {
    load_json(&path())
}

/// Read a JSON file; missing means the defaults. A file that does not parse is kept
/// aside as `<name>.bad-<unix time>` (so it is not overwritten by the next save) and
/// the defaults are used.
pub fn load_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> T {
    let Ok(bytes) = std::fs::read(path) else { return T::default() };
    match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
            let mut bad = path.as_os_str().to_owned();
            bad.push(format!(".bad-{secs}"));
            let kept = std::fs::rename(path, &bad).map(|_| format!("kept as {}", Path::new(&bad).display()));
            eprintln!(
                "{}: {e}; using defaults ({})",
                path.display(),
                kept.unwrap_or_else(|e| format!("could not keep it: {e}"))
            );
            T::default()
        }
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

/// Change and save settings.
pub fn update(f: impl FnOnce(&mut Settings)) {
    let json = {
        let mut s = SETTINGS.write().unwrap();
        f(&mut s);
        serde_json::to_vec_pretty(&*s)
    };
    if let Ok(json) = json {
        let p = path();
        if let Err(e) = write_atomic(&p, &json) {
            eprintln!("{}: {e}", p.display());
        }
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

/// Whether writes on this context need an extra typed confirmation.
pub fn is_protected(context: &str) -> bool {
    migrate_protection();
    match with(|s| s.protection.get(context).copied()) {
        Some(chosen) => chosen,
        None => crate::k8s::load_contexts().is_ok_and(|c| c.risky.iter().any(|r| r == context)),
    }
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
        DeleteConfirm, NamespaceVisit, Settings, load_json, migrate_list, recent_as_visits, record_visits, visit_score,
        write_atomic,
    };

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
    fn deletes_ask_only_for_nodes_and_statefulsets_by_default() {
        // Older settings files have neither key and get the defaults.
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.delete_confirm, DeleteConfirm::Off);
        assert_eq!(s.delete_confirm_kinds.get("Node"), Some(&DeleteConfirm::Type));
        assert_eq!(s.delete_confirm_kinds.get("StatefulSet"), Some(&DeleteConfirm::Type));
        assert_eq!(s.delete_confirm_kinds.get("Pod"), None);
        let s: Settings =
            serde_json::from_str(r#"{"delete_confirm": "ask", "delete_confirm_kinds": {"Namespace": "type"}}"#)
                .unwrap();
        assert_eq!(s.delete_confirm, DeleteConfirm::Ask);
        assert_eq!(s.delete_confirm_kinds.len(), 1);
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
}
