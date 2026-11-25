//! Favorites: starred resource kinds, contexts, namespaces, Argo CD Applications and objects,
//! and saved table views, in one store (`favorites.json`) that every window shares (see
//! [`shared`]), shown at the top of the sidebar and found by the palette.
//!
//! Loading fails closed, entry by entry: an entry that doesn't read (a wrong type, a missing or
//! empty field, one written by a newer version) is dropped and the others are kept, and the
//! file as it was is copied aside before the first save writes it without them. Keys this
//! version doesn't know are written back as they were read. A file whose text isn't a JSON
//! object is kept aside; one that can't be read at all (an I/O error) is left alone, and
//! nothing is saved over it in this session.

use std::path::{Path, PathBuf};

use gpui_kit::*;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct FavObject {
    pub context: String,
    /// kubectl-style kind, e.g. `deployments` or `certificates.cert-manager.io`.
    pub kind: String,
    pub namespace: Option<String>,
    pub name: String,
}

/// A namespace of one context. With several contexts combined, the namespace picker's star
/// marks it on each of them.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct FavNamespace {
    pub context: String,
    pub namespace: String,
}

/// An Argo CD Application, by its Argo CD instance, namespace and name: the same favorite
/// whichever way the Application is read.
///
/// `argocd` is the instance as the Argo CD page keys it (`api::Endpoint::instance`, the API
/// server's `https://host`). One starred before Argo CD was read only through its API, or on
/// an Application's own page, names its hub context instead (`context:<hub context>`): it is
/// rewritten to the server's instance once exactly one server's feed lists an Application of
/// that namespace and name ([`adopted`]), and never dropped meanwhile. `context` is the hub
/// context the star was put on: `context:<context>` is another name of the same instance. Two
/// entries naming one instance are one favorite.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct FavApp {
    pub argocd: String,
    pub namespace: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// The instance name of the Argo CD on hub context `context` that no API server matches.
pub fn hub_alias(context: &str) -> String {
    format!("context:{context}")
}

/// The `context:` entries of `apps` that exactly one Argo CD server lists (`found`: the
/// instances whose feeds hold an Application of that namespace and name), rewritten to that
/// server's instance. None is ever dropped: one no server lists, or several do, stays as it
/// is. Entries that end up the same are kept once. Whether anything changed.
pub fn adopted(apps: &mut Vec<FavApp>, found: impl Fn(&str, &str) -> Vec<String>) -> bool {
    let mut changed = false;
    for a in apps.iter_mut().filter(|a| a.unresolved()) {
        let mut on = found(&a.namespace, &a.name);
        on.sort();
        on.dedup();
        if let [one] = on.as_slice() {
            a.argocd.clone_from(one);
            a.context = None;
            changed = true;
        }
    }
    if changed {
        let mut kept: Vec<FavApp> = Vec::with_capacity(apps.len());
        for a in apps.drain(..) {
            if !kept.contains(&a) {
                kept.push(a);
            }
        }
        *apps = kept;
    }
    changed
}

impl FavApp {
    /// The names its instance is known by: `argocd`, and the hub alias of `context`.
    pub fn names(&self) -> Vec<String> {
        let mut names = vec![self.argocd.clone()];
        if let Some(c) = &self.context {
            let alias = hub_alias(c);
            if alias != self.argocd {
                names.push(alias);
            }
        }
        names
    }

    /// Whether this is the Application `namespace`/`name` of the Argo CD instance known by any
    /// of the names in `instance` (its key, its hub alias).
    pub fn is(&self, instance: &[&str], namespace: &str, name: &str) -> bool {
        self.namespace == namespace && self.name == name && self.names().iter().any(|n| instance.contains(&n.as_str()))
    }

    /// Whether `other` names the same Application (through any name of its instance).
    pub fn same(&self, other: &Self) -> bool {
        let names = other.names();
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        self.is(&names, &other.namespace, &other.name)
    }

    /// Named by the hub context it was starred on, not yet by an Argo CD server (see
    /// [`adopted`]).
    pub fn unresolved(&self) -> bool {
        self.argocd.starts_with("context:")
    }
}

/// A named table view: kind, namespaces and filter.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct SavedView {
    pub name: String,
    pub kind: String,
    pub namespaces: Vec<String>,
    pub filter: String,
}

/// One favorite, whatever it points at.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Favorite {
    Context(String),
    Namespace(FavNamespace),
    Application(FavApp),
    Object(FavObject),
}

impl Favorite {
    /// The favorite for an object shown on `context`: an Argo CD Application is one by its
    /// instance (see [`FavApp`]), anything else an object.
    pub fn for_object(context: &str, kind: &crate::k8s::Kind, namespace: Option<String>, name: String) -> Self {
        if kind.is("argoproj.io", "Application") {
            Self::Application(FavApp {
                argocd: hub_alias(context),
                namespace: namespace.unwrap_or_default(),
                name,
                context: Some(context.to_string()),
            })
        } else {
            Self::Object(FavObject { context: context.to_string(), kind: kind.kubectl_name(), namespace, name })
        }
    }

    /// The kubeconfig context it is on; none for an Application, which opens on the Argo CD
    /// page.
    pub fn context(&self) -> Option<&str> {
        match self {
            Self::Context(c) => Some(c),
            Self::Namespace(n) => Some(&n.context),
            Self::Application(_) => None,
            Self::Object(o) => Some(&o.context),
        }
    }

    /// Its name, as the sidebar and the palette show it.
    pub fn title(&self) -> String {
        match self {
            Self::Context(c) => c.clone(),
            Self::Namespace(n) => n.namespace.clone(),
            Self::Application(a) => a.name.clone(),
            Self::Object(o) => o.name.clone(),
        }
    }

    /// What it is and where, under its name.
    pub fn what(&self) -> String {
        match self {
            Self::Context(_) => "context".into(),
            Self::Namespace(n) => format!("namespace · {}", n.context),
            Self::Application(a) => {
                let on = a.argocd.strip_prefix("context:").unwrap_or(&a.argocd);
                format!("Argo CD Application · {} · {on}", a.namespace)
            }
            Self::Object(o) => {
                let kind = o.kind.split('.').next().unwrap_or_default();
                match &o.namespace {
                    Some(ns) => format!("{kind} · {ns} · {}", o.context),
                    None => format!("{kind} · {}", o.context),
                }
            }
        }
    }
}

/// The lists the file holds; any other top-level key is kept in `extra`.
const LISTS: [&str; 6] = ["kinds", "objects", "views", "contexts", "namespaces", "applications"];

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
struct Stored {
    kinds: Vec<String>,
    objects: Vec<FavObject>,
    views: Vec<SavedView>,
    contexts: Vec<String>,
    namespaces: Vec<FavNamespace>,
    applications: Vec<FavApp>,
    /// Top-level keys this version doesn't know (a newer one's), written back as read.
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

/// The entries of `fields[key]` that read as `T` and that `keep` keeps, and whether any
/// other was dropped (not a list, an entry of another shape or kept out). An entry listed
/// twice is kept once without counting as dropped.
fn entries<T: DeserializeOwned + PartialEq>(
    fields: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    keep: impl Fn(&T) -> bool,
) -> (Vec<T>, bool) {
    let items = match fields.get(key) {
        None => return (Vec::new(), false),
        Some(serde_json::Value::Array(items)) => items,
        Some(_) => return (Vec::new(), true),
    };
    let (mut out, mut dropped): (Vec<T>, bool) = (Vec::new(), false);
    for item in items {
        match serde_json::from_value::<T>(item.clone()) {
            Ok(t) if keep(&t) => {
                if !out.contains(&t) {
                    out.push(t);
                }
            }
            _ => dropped = true,
        }
    }
    (out, dropped)
}

/// The favorites in a file's bytes, entry by entry (see the module's comment), and whether
/// any entry was dropped. None when the text isn't a JSON object: the file is then kept aside.
///
/// An Application starred before Applications had their own list (an `objects` entry of kind
/// `applications.argoproj.io`) moves to `applications`, keyed by its hub context.
fn from_bytes(bytes: &[u8]) -> Option<(Stored, bool)> {
    let serde_json::Value::Object(fields) = serde_json::from_slice(bytes).ok()? else { return None };
    let set = |s: &String| !s.is_empty();
    let mut lost = false;
    fn take<T>(lost: &mut bool, (list, dropped): (Vec<T>, bool)) -> Vec<T> {
        *lost |= dropped;
        list
    }
    let mut views: Vec<SavedView> =
        take(&mut lost, entries(&fields, "views", |v: &SavedView| set(&v.name) && set(&v.kind)));
    // One view a name: the first.
    let mut names = std::collections::HashSet::new();
    views.retain(|v| names.insert(v.name.clone()));
    let kinds = take(&mut lost, entries(&fields, "kinds", set));
    let mut objects: Vec<FavObject> = take(
        &mut lost,
        entries(&fields, "objects", |o: &FavObject| {
            set(&o.context) && set(&o.kind) && set(&o.name) && o.namespace.as_ref().is_none_or(set)
        }),
    );
    let contexts = take(&mut lost, entries(&fields, "contexts", set));
    let namespaces =
        take(&mut lost, entries(&fields, "namespaces", |n: &FavNamespace| set(&n.context) && set(&n.namespace)));
    let mut applications: Vec<FavApp> = take(
        &mut lost,
        entries(&fields, "applications", |a: &FavApp| {
            set(&a.argocd) && set(&a.namespace) && set(&a.name) && a.context.as_ref().is_none_or(set)
        }),
    );
    for o in objects.extract_if(.., |o| o.kind == "applications.argoproj.io" && o.namespace.is_some()) {
        let app = FavApp {
            argocd: hub_alias(&o.context),
            namespace: o.namespace.unwrap_or_default(),
            name: o.name,
            context: Some(o.context),
        };
        if !applications.iter().any(|a| a.same(&app)) {
            applications.push(app);
        }
    }
    let extra = fields.into_iter().filter(|(k, _)| !LISTS.contains(&k.as_str())).collect();
    Some((Stored { kinds, objects, views, contexts, namespaces, applications, extra }, lost))
}

/// How the file read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Read {
    /// Whole (or missing).
    Whole,
    /// Some entries were dropped: copy the file aside before the first save.
    Partly,
    /// It couldn't be read: save nothing over it this session.
    Unreadable,
}

/// The favorites saved at `path`: none when it is missing; when its text isn't a JSON object,
/// none and the file kept aside (so the next save doesn't overwrite it); when it can't be read,
/// none and `Read::Unreadable`.
fn load_from(path: &Path) -> (Stored, Read) {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Stored::default(), Read::Whole),
        Err(e) => {
            eprintln!("{}: {e}; no favorites, and none saved until the app restarts", path.display());
            return (Stored::default(), Read::Unreadable);
        }
    };
    match from_bytes(&bytes) {
        Some((stored, false)) => (stored, Read::Whole),
        Some((stored, true)) => {
            eprintln!("{}: some favorites don't read and were left out", path.display());
            (stored, Read::Partly)
        }
        None => {
            eprintln!("{}: not a JSON object; no favorites ({})", path.display(), crate::settings::keep_aside(path));
            (Stored::default(), Read::Whole)
        }
    }
}

/// Copy `path` to `<name>.bad-<unix time>`, leaving it in place; says where.
fn copy_aside(path: &Path) -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut bad = path.as_os_str().to_owned();
    bad.push(format!(".bad-{secs}"));
    match std::fs::copy(path, &bad) {
        Ok(_) => format!("copied to {}", Path::new(&bad).display()),
        Err(e) => format!("could not copy it: {e}"),
    }
}

/// The namespaces of `all` (in their order) starred on any of `contexts`: those of the
/// connected contexts, all of them when several are combined. A favorite of a context that
/// isn't connected, or a namespace not listed, is left out.
pub fn favorite_namespaces(all: &[String], favorites: &[FavNamespace], contexts: &[String]) -> Vec<String> {
    all.iter()
        .filter(|ns| favorites.iter().any(|f| f.namespace == **ns && contexts.contains(&f.context)))
        .cloned()
        .collect()
}

/// `items` with those `is_favorite` says first, each part in its order.
pub fn favorites_first<T>(items: Vec<T>, is_favorite: impl Fn(&T) -> bool) -> Vec<T> {
    let (mut first, rest): (Vec<T>, Vec<T>) = items.into_iter().partition(|t| is_favorite(t));
    first.extend(rest);
    first
}

pub struct Favorites {
    stored: Stored,
    /// Where it is saved.
    path: PathBuf,
    /// How the file read (see `Read`): a copy is kept before the first save after a partial
    /// read, and nothing is saved after a failed one.
    read: Read,
}

fn path() -> PathBuf {
    crate::paths::config_dir().join("favorites.json")
}

/// The one store every window shows and changes, so a window never saves over another's
/// stars.
struct Shared(Entity<Favorites>);

impl Global for Shared {}

/// The favorites every window shares, read from `favorites.json` the first time.
pub fn shared(cx: &mut App) -> Entity<Favorites> {
    if let Some(s) = cx.try_global::<Shared>() {
        return s.0.clone();
    }
    let favorites = cx.new(|_| Favorites::load());
    cx.set_global(Shared(favorites.clone()));
    favorites
}

impl Favorites {
    fn load() -> Self {
        Self::load_at(path())
    }

    fn load_at(path: PathBuf) -> Self {
        let (stored, read) = load_from(&path);
        Self { stored, path, read }
    }

    fn save(&mut self) {
        match self.read {
            Read::Unreadable => {
                eprintln!("{}: not saved, it couldn't be read when the app started", self.path.display());
                return;
            }
            Read::Partly => {
                eprintln!("{}: entries that didn't read are dropped ({})", self.path.display(), copy_aside(&self.path));
                self.read = Read::Whole;
            }
            Read::Whole => {}
        }
        save_json(&self.path, &self.stored);
    }

    pub fn kinds(&self) -> &[String] {
        &self.stored.kinds
    }

    pub fn toggle_kind(&mut self, key: String, cx: &mut Context<Self>) {
        if let Some(i) = self.stored.kinds.iter().position(|k| *k == key) {
            self.stored.kinds.remove(i);
        } else {
            self.stored.kinds.push(key);
        }
        self.save();
        cx.notify();
    }

    pub fn views(&self) -> &[SavedView] {
        &self.stored.views
    }

    /// Add a view, replacing one with the same name.
    pub fn save_view(&mut self, v: SavedView, cx: &mut Context<Self>) {
        self.stored.views.retain(|x| x.name != v.name);
        self.stored.views.push(v);
        self.save();
        cx.notify();
    }

    pub fn remove_view(&mut self, name: &str, cx: &mut Context<Self>) {
        self.stored.views.retain(|x| x.name != name);
        self.save();
        cx.notify();
    }

    pub fn namespaces(&self) -> &[FavNamespace] {
        &self.stored.namespaces
    }

    pub fn contexts(&self) -> &[String] {
        &self.stored.contexts
    }

    /// Every favorite but the kinds and views: contexts, namespaces, Applications, objects.
    pub fn all(&self) -> Vec<Favorite> {
        let s = &self.stored;
        (s.contexts.iter().cloned().map(Favorite::Context))
            .chain(s.namespaces.iter().cloned().map(Favorite::Namespace))
            .chain(s.applications.iter().cloned().map(Favorite::Application))
            .chain(s.objects.iter().cloned().map(Favorite::Object))
            .collect()
    }

    /// Whether `f` is a favorite; an Application through any name of its instance.
    pub fn has(&self, f: &Favorite) -> bool {
        let s = &self.stored;
        match f {
            Favorite::Context(c) => s.contexts.contains(c),
            Favorite::Namespace(n) => s.namespaces.contains(n),
            Favorite::Application(a) => s.applications.iter().any(|x| x.same(a)),
            Favorite::Object(o) => s.objects.contains(o),
        }
    }

    /// Star (`on`) or unstar `f`; whether that changed anything. Unstarring an Application
    /// removes every entry naming it, through any name of its instance.
    fn set(&mut self, f: Favorite, on: bool) -> bool {
        fn put<T: PartialEq>(list: &mut Vec<T>, t: T, on: bool) -> bool {
            match (list.iter().position(|x| *x == t), on) {
                (None, true) => {
                    list.push(t);
                    true
                }
                (Some(i), false) => {
                    list.remove(i);
                    true
                }
                _ => false,
            }
        }
        let s = &mut self.stored;
        match f {
            Favorite::Context(c) => put(&mut s.contexts, c, on),
            Favorite::Namespace(n) => put(&mut s.namespaces, n, on),
            Favorite::Application(a) => {
                let before = s.applications.len();
                if on {
                    if s.applications.iter().any(|x| x.same(&a)) {
                        return false;
                    }
                    s.applications.push(a);
                } else {
                    s.applications.retain(|x| !x.same(&a));
                }
                s.applications.len() != before
            }
            Favorite::Object(o) => put(&mut s.objects, o, on),
        }
    }

    /// Rewrite the Applications starred by their hub context to the one Argo CD server that
    /// lists them (see [`adopted`]), saved when anything changed.
    pub fn adopt_apps(&mut self, found: impl Fn(&str, &str) -> Vec<String>, cx: &mut Context<Self>) {
        if adopted(&mut self.stored.applications, found) {
            self.save();
            cx.notify();
        }
    }

    /// Star or unstar `f`, saved at once.
    pub fn toggle(&mut self, f: Favorite, cx: &mut Context<Self>) {
        let on = !self.has(&f);
        self.set(f, on);
        self.save();
        cx.notify();
    }

    /// Star (`on`) or unstar several at once (a namespace on each context combined).
    pub fn set_all(&mut self, list: Vec<Favorite>, on: bool, cx: &mut Context<Self>) {
        let mut changed = false;
        for f in list {
            changed |= self.set(f, on);
        }
        if changed {
            self.save();
            cx.notify();
        }
    }
}

/// Columns chosen per resource type (kubectl name -> visible column names).
#[derive(Default)]
pub struct ColumnPrefs {
    /// Visible columns per kind, in display order.
    map: std::collections::HashMap<String, Vec<String>>,
    /// Column widths per kind (at zoom 1).
    widths: std::collections::HashMap<String, std::collections::HashMap<String, f32>>,
    /// Custom columns per kind: (header, JSONPath).
    custom: std::collections::HashMap<String, Vec<(String, String)>>,
}

/// Write JSON atomically (creating the directory); failures are reported on stderr.
fn save_json(path: &Path, value: &impl Serialize) {
    let written = serde_json::to_vec_pretty(value)
        .map_err(std::io::Error::other)
        .and_then(|json| crate::private_files::write_atomic(path, &json));
    if let Err(e) = written {
        eprintln!("{}: {e}", path.display());
    }
}

fn custom_path() -> PathBuf {
    path().with_file_name("custom-columns.json")
}

fn columns_path() -> PathBuf {
    path().with_file_name("columns.json")
}

fn widths_path() -> PathBuf {
    path().with_file_name("column-widths.json")
}

impl ColumnPrefs {
    pub fn load() -> Self {
        use crate::settings::load_json;
        Self { map: load_json(&columns_path()), widths: load_json(&widths_path()), custom: load_json(&custom_path()) }
    }

    pub fn custom(&self, kind: &str) -> Vec<(String, String)> {
        self.custom.get(kind).cloned().unwrap_or_default()
    }

    /// Add (or replace by header) a custom column for a kind.
    pub fn add_custom(&mut self, kind: &str, header: String, path: String) {
        let list = self.custom.entry(kind.to_string()).or_default();
        list.retain(|(h, _)| *h != header);
        list.push((header, path));
        self.save_custom();
    }

    pub fn remove_custom(&mut self, kind: &str, header: &str) {
        if let Some(list) = self.custom.get_mut(kind) {
            list.retain(|(h, _)| h != header);
        }
        self.save_custom();
    }

    fn save_custom(&self) {
        save_json(&custom_path(), &self.custom);
    }

    pub fn widths(&self, kind: &str) -> std::collections::HashMap<String, f32> {
        self.widths.get(kind).cloned().unwrap_or_default()
    }

    pub fn set_widths(&mut self, kind: String, widths: std::collections::HashMap<String, f32>) {
        self.widths.entry(kind).or_default().extend(widths);
        save_json(&widths_path(), &self.widths);
    }

    pub fn get(&self, kind: &str) -> Option<Vec<String>> {
        self.map.get(kind).cloned()
    }

    pub fn set(&mut self, kind: String, visible: Option<Vec<String>>) {
        match visible {
            Some(v) => self.map.insert(kind, v),
            None => self.map.remove(&kind),
        };
        save_json(&columns_path(), &self.map);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FavApp, FavNamespace, FavObject, Favorite, Favorites, Read, Stored, adopted, favorite_namespaces, from_bytes,
        load_from,
    };

    fn ns(context: &str, namespace: &str) -> FavNamespace {
        FavNamespace { context: context.into(), namespace: namespace.into() }
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    fn app(argocd: &str, context: Option<&str>) -> FavApp {
        FavApp {
            argocd: argocd.into(),
            namespace: "argocd".into(),
            name: "shop".into(),
            context: context.map(Into::into),
        }
    }

    fn scratch() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("recouvrance-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn backups(d: &std::path::Path) -> Vec<Vec<u8>> {
        (std::fs::read_dir(d).unwrap().filter_map(Result::ok))
            .filter(|e| e.file_name().to_string_lossy().starts_with("favorites.json.bad-"))
            .map(|e| std::fs::read(e.path()).unwrap())
            .collect()
    }

    /// A file of an older version (kinds, objects and views only) reads as it was, but for its
    /// starred Applications, which move to `applications`, keyed by their hub context, once.
    #[test]
    fn an_older_file_reads_as_it_was() {
        let text = br#"{"kinds": ["pods"], "objects": [{"context": "prod", "kind": "deployments.apps",
            "namespace": "web", "name": "api"}, {"context": "hub", "kind": "applications.argoproj.io",
            "namespace": "argocd", "name": "shop"}, {"context": "hub", "kind": "applications.argoproj.io",
            "namespace": "argocd", "name": "web"}], "applications": [{"argocd": "context:hub",
            "namespace": "argocd", "name": "web"}], "views": [{"name": "v", "kind": "pods",
            "namespaces": [], "filter": ""}]}"#;
        let (s, lost) = from_bytes(text).unwrap();
        assert!(!lost);
        assert_eq!(s.kinds, names(&["pods"]));
        assert_eq!(s.objects.len(), 1);
        assert_eq!(s.objects[0].name, "api");
        assert_eq!(s.views.len(), 1);
        let apps: Vec<(&str, &str)> = s.applications.iter().map(|a| (a.argocd.as_str(), a.name.as_str())).collect();
        assert_eq!(apps, [("context:hub", "web"), ("context:hub", "shop")]);
        assert!(s.applications[1].unresolved());
        assert!(s.contexts.is_empty() && s.namespaces.is_empty() && s.extra.is_empty());
    }

    /// One bad entry costs only itself, and says entries were dropped; a file that isn't an
    /// object reads as none (kept aside).
    #[test]
    fn a_bad_entry_is_dropped_and_the_others_kept() {
        let text = br#"{"contexts": ["prod", 3, "", "prod"], "namespaces": [{"context": "prod",
            "namespace": "web"}, {"context": "prod"}, {"context": "", "namespace": "x"}],
            "applications": [{"argocd": "context:hub", "namespace": "argocd", "name": "shop"}, {"name": "x"}],
            "objects": "not a list", "kinds": ["pods", null]}"#;
        let (s, lost) = from_bytes(text).unwrap();
        assert!(lost);
        assert_eq!(s.contexts, names(&["prod"]));
        assert_eq!(s.namespaces, vec![ns("prod", "web")]);
        assert_eq!(s.applications.len(), 1);
        assert!(s.objects.is_empty());
        assert_eq!(s.kinds, names(&["pods"]));
        // Listed twice is not lost.
        assert!(!from_bytes(br#"{"contexts": ["a", "a"]}"#).unwrap().1);
        for bad in [&b"[1]"[..], b"\"x\"", b"{ not json", b""] {
            assert_eq!(from_bytes(bad), None);
        }
    }

    /// Keys of a newer version are written back as they were read.
    #[test]
    fn unknown_keys_are_kept() {
        let text = br#"{"contexts": ["prod"], "pinned_tabs": [{"x": 1}], "version": 3}"#;
        let (s, lost) = from_bytes(text).unwrap();
        assert!(!lost);
        let saved: serde_json::Value = serde_json::to_value(&s).unwrap();
        assert_eq!(saved["pinned_tabs"], serde_json::json!([{"x": 1}]));
        assert_eq!(saved["version"], 3);
        assert_eq!(from_bytes(&serde_json::to_vec(&s).unwrap()), Some((s, false)));
    }

    /// A file that isn't an object is kept aside, and the favorites start empty; one with
    /// entries that don't read is copied aside once, at the first save, and stays in place
    /// until then.
    #[test]
    fn a_broken_file_is_kept_aside_and_a_partial_one_copied() {
        let d = scratch();
        let p = d.join("favorites.json");
        std::fs::write(&p, b"[1, 2]").unwrap();
        assert_eq!(load_from(&p), (Stored::default(), Read::Whole));
        assert!(!p.exists());
        assert_eq!(backups(&d), [b"[1, 2]".to_vec()]);
        assert_eq!(load_from(&d.join("missing.json")), (Stored::default(), Read::Whole));
        std::fs::remove_dir_all(&d).unwrap();

        let d = scratch();
        let p = d.join("favorites.json");
        let text = br#"{"contexts": ["prod", 7], "extra": true}"#;
        std::fs::write(&p, text).unwrap();
        let mut f = Favorites::load_at(p.clone());
        assert_eq!(f.read, Read::Partly);
        assert!(backups(&d).is_empty());
        assert!(f.set(Favorite::Context("dev".into()), true));
        f.save();
        f.save();
        assert_eq!(backups(&d), [text.to_vec()]);
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&p).unwrap()).unwrap();
        assert_eq!(saved["contexts"], serde_json::json!(["prod", "dev"]));
        assert_eq!(saved["extra"], true);
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// A file that exists but can't be read is left alone: nothing is saved over it.
    #[test]
    fn an_unreadable_file_is_never_saved_over() {
        let d = scratch();
        // A folder where the file should be: it exists, but can't be read.
        let p = d.join("favorites.json");
        std::fs::create_dir_all(&p).unwrap();
        let mut f = Favorites::load_at(p.clone());
        assert_eq!(f.read, Read::Unreadable);
        f.set(Favorite::Context("dev".into()), true);
        f.save();
        assert!(p.is_dir());
        assert!(backups(&d).is_empty());
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Each favorite is keyed by all it names: the same name on another context, kind or
    /// namespace is another favorite.
    #[test]
    fn favorites_are_keyed_by_context_kind_namespace_and_name() {
        let mut f = Favorites::load_at(scratch().join("favorites.json"));
        let obj = |ctx: &str, kind: &str, ns: Option<&str>| {
            Favorite::Object(FavObject {
                context: ctx.into(),
                kind: kind.into(),
                namespace: ns.map(String::from),
                name: "api".into(),
            })
        };
        assert!(f.set(obj("a", "deployments.apps", Some("web")), true));
        assert!(!f.set(obj("a", "deployments.apps", Some("web")), true));
        for other in [
            obj("b", "deployments.apps", Some("web")),
            obj("a", "services", Some("web")),
            obj("a", "deployments.apps", Some("db")),
            obj("a", "deployments.apps", None),
        ] {
            assert!(!f.has(&other), "{other:?}");
        }
        assert!(f.set(Favorite::Namespace(ns("a", "web")), true));
        assert!(!f.has(&Favorite::Namespace(ns("b", "web"))));
        assert!(f.set(obj("a", "deployments.apps", Some("web")), false));
        assert_eq!(f.all(), vec![Favorite::Namespace(ns("a", "web"))]);
    }

    /// An Application is one favorite through any name of its instance: its key (an API
    /// server's, or `context:<hub>`) and the hub alias of the context it was starred on.
    #[test]
    fn an_application_matches_on_any_name_of_its_instance() {
        let server = "https://argocd.example.com";
        let a = app(server, Some("hub"));
        assert!(a.is(&[server], "argocd", "shop"));
        assert!(a.is(&["context:hub"], "argocd", "shop"));
        assert!(!a.is(&["context:other"], "argocd", "shop"));
        assert!(!a.is(&[server], "other", "shop"));
        // Named by a hub context until a server lists it.
        assert!(!a.unresolved());
        assert!(app("context:hub", None).unresolved());
    }

    /// A favorite starred by its hub context is rewritten to the one Argo CD server that
    /// lists an Application of that namespace and name; with none or several, it stays as
    /// it is, never dropped.
    #[test]
    fn a_hub_favorite_is_rewritten_to_the_one_server_that_lists_it() {
        let server = "https://argocd.example.com";
        let found = |ns: &str, name: &str| match (ns, name) {
            ("argocd", "shop") => vec![server.to_string()],
            ("argocd", "twice") => vec![server.to_string(), "https://other.example.com".into()],
            _ => Vec::new(),
        };
        let mut apps = vec![
            app("context:hub", Some("hub")),
            FavApp { name: "twice".into(), ..app("context:hub", None) },
            FavApp { name: "gone".into(), ..app("context:hub", None) },
            app(server, None),
        ];
        assert!(adopted(&mut apps, found));
        let shown: Vec<(&str, &str)> = apps.iter().map(|a| (a.argocd.as_str(), a.name.as_str())).collect();
        assert_eq!(shown, [(server, "shop"), ("context:hub", "twice"), ("context:hub", "gone")]);
        assert_eq!(apps[0].context, None);
        assert!(!adopted(&mut apps, found));
    }

    /// Starred through one name, unstarred through another: every entry naming it goes.
    #[test]
    fn an_application_starred_by_one_name_is_unstarred_by_another() {
        let mut f = Favorites::load_at(scratch().join("favorites.json"));
        let server = "https://argocd.example.com";
        // Starred on the hub before its server was set, then read through the API.
        assert!(f.set(Favorite::Application(app("context:hub", Some("hub"))), true));
        let from_api = Favorite::Application(app(server, Some("hub")));
        assert!(f.has(&from_api));
        assert!(!f.set(from_api.clone(), true), "the same Application twice");
        // An older entry under the server's name too: both go.
        f.stored.applications.push(app(server, None));
        assert!(f.set(from_api, false));
        assert!(f.stored.applications.is_empty());
        assert!(!f.has(&Favorite::Application(app("context:hub", None))));
    }

    /// The picker lists the favorites of the connected contexts only, all of them when several
    /// are combined, in the list's order; a context that isn't connected adds none.
    #[test]
    fn favorite_namespaces_follow_the_contexts_connected() {
        let all = names(&["db", "monitoring", "web"]);
        let favs = vec![ns("prod", "web"), ns("staging", "db"), ns("gone", "monitoring"), ns("prod", "missing")];
        assert_eq!(favorite_namespaces(&all, &favs, &names(&["prod"])), names(&["web"]));
        assert_eq!(favorite_namespaces(&all, &favs, &names(&["prod", "staging"])), names(&["db", "web"]));
        assert!(favorite_namespaces(&all, &favs, &names(&["other"])).is_empty());
        assert!(favorite_namespaces(&all, &favs, &[]).is_empty());
    }

    /// An Application's page stars the Application by its instance, any other page the object.
    #[test]
    fn an_application_page_stars_the_application() {
        let a =
            Favorite::for_object("hub", &crate::k8s::argo::application_kind(), Some("argocd".into()), "shop".into());
        let Favorite::Application(fav) = &a else { panic!("{a:?}") };
        assert!(fav.is(&["context:hub"], "argocd", "shop"));
        // It opens on the Argo CD page, once a server lists it.
        assert_eq!(a.context(), None);
        assert!(fav.unresolved());
    }
}

/// Rules for any file: loading never fails on an entry, keeps every good one, and reads back
/// what it saved.
#[cfg(test)]
mod properties {
    use super::{FavApp, FavNamespace, FavObject, LISTS, SavedView, Stored, favorites_first, from_bytes};
    use proptest::prelude::*;
    use serde_json::{Value, json};

    fn name() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9-]{0,6}"
    }

    fn stored() -> impl Strategy<Value = Stored> {
        let object = (name(), prop::sample::select(vec!["pods", "deployments.apps"]), prop::option::of(name()), name())
            .prop_map(|(context, kind, namespace, name)| FavObject { context, kind: kind.into(), namespace, name });
        let namespace = (name(), name()).prop_map(|(context, namespace)| FavNamespace { context, namespace });
        let app = (name(), name(), name(), prop::option::of(name()))
            .prop_map(|(argocd, namespace, name, context)| FavApp { argocd, namespace, name, context });
        (
            prop::collection::btree_set(name(), 0..4),
            prop::collection::vec(object, 0..4),
            // Views are keyed by name: one a name, the map's.
            prop::collection::btree_map(name(), name(), 0..3),
            prop::collection::btree_set(name(), 0..4),
            prop::collection::vec(namespace, 0..4),
            prop::collection::vec(app, 0..4),
        )
            .prop_map(|(kinds, objects, views, contexts, namespaces, applications)| Stored {
                kinds: kinds.into_iter().collect(),
                objects: once(objects),
                views: (views.into_iter())
                    .map(|(name, kind)| SavedView { name, kind, namespaces: vec![], filter: String::new() })
                    .collect(),
                contexts: contexts.into_iter().collect(),
                namespaces: once(namespaces),
                applications: once(applications),
                extra: serde_json::Map::new(),
            })
    }

    /// Each item once, as the app keeps them.
    fn once<T: PartialEq>(items: Vec<T>) -> Vec<T> {
        let mut out = Vec::new();
        for x in items {
            if !out.contains(&x) {
                out.push(x);
            }
        }
        out
    }

    /// Values no entry reads as.
    fn bad() -> impl Strategy<Value = Value> {
        prop_oneof![
            Just(Value::Null),
            Just(json!(-1)),
            Just(json!("")),
            Just(json!([[1]])),
            Just(json!({"x": [null]})),
            Just(json!({"context": "", "namespace": "", "name": "", "argocd": "", "kind": ""})),
        ]
    }

    proptest! {
        /// What is saved reads back as it was, unknown keys included.
        #[test]
        fn favorites_read_back_as_saved(mut s in stored(), extra in prop::collection::btree_map("x[a-z]{0,4}", any::<i32>(), 0..3)) {
            s.extra = extra.into_iter().map(|(k, v)| (k, json!(v))).collect();
            let read = from_bytes(&serde_json::to_vec(&s).unwrap());
            prop_assert_eq!(read, Some((s, false)));
        }

        /// Bad entries slipped into any list, and whole lists spoiled, never break loading: every
        /// good entry of the other lists is kept, in its order, nothing bad is read, the read
        /// says something was dropped, and keys of a newer version are kept.
        #[test]
        fn a_bad_entry_costs_only_itself(
            s in stored(),
            inserted in prop::collection::vec((0..6usize, 0..8usize, bad()), 0..8),
            spoiled in prop::collection::btree_map(0..6usize, bad(), 0..2),
        ) {
            let Value::Object(mut fields) = serde_json::to_value(&s).unwrap() else { unreachable!() };
            for (list, at, value) in &inserted {
                if let Some(Value::Array(items)) = fields.get_mut(LISTS[*list]) {
                    let at = (*at).min(items.len());
                    items.insert(at, value.clone());
                }
            }
            for (list, value) in &spoiled {
                fields.insert(LISTS[*list].to_string(), value.clone());
            }
            fields.insert("from-a-newer-version".into(), json!({"x": 1}));
            let read = from_bytes(&serde_json::to_vec(&fields).unwrap());
            prop_assert!(read.is_some());
            let (read, lost) = read.unwrap();
            prop_assert_eq!(lost, !inserted.is_empty() || !spoiled.is_empty());
            prop_assert_eq!(&read.extra["from-a-newer-version"], &json!({"x": 1}));
            let read = serde_json::to_value(read).unwrap();
            let want = serde_json::to_value(&s).unwrap();
            for (i, key) in LISTS.iter().enumerate() {
                if spoiled.contains_key(&i) {
                    prop_assert_eq!(&read[key], &json!([]), "{} spoiled", key);
                } else {
                    prop_assert_eq!(&read[key], &want[key], "{} changed", key);
                }
            }
        }

        /// Favorites first keeps every item once, favorites before the others, each part in its
        /// order.
        #[test]
        fn favorites_come_first_in_their_order(items in prop::collection::vec(0u8..20, 0..20), favs in prop::collection::btree_set(0u8..20, 0..10)) {
            let out = favorites_first(items.clone(), |i| favs.contains(i));
            prop_assert_eq!(out.len(), items.len());
            let n = out.iter().take_while(|i| favs.contains(i)).count();
            prop_assert!(out[n..].iter().all(|i| !favs.contains(i)));
            let want_first: Vec<u8> = items.iter().copied().filter(|i| favs.contains(i)).collect();
            let want_rest: Vec<u8> = items.iter().copied().filter(|i| !favs.contains(i)).collect();
            prop_assert_eq!(&out[..n], &want_first[..]);
            prop_assert_eq!(&out[n..], &want_rest[..]);
        }
    }
}
