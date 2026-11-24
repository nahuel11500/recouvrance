//! Favorites: starred resource kinds, contexts, namespaces, Argo CD Applications and objects,
//! and saved table views, in one store (`favorites.json`), shown at the top of the sidebar and
//! found by the palette.
//!
//! Loading fails closed, entry by entry: an entry that doesn't read (a wrong type, a missing or
//! empty field, one written by a newer version) is dropped and the others are kept. Only a file
//! that isn't a JSON object at all is kept aside, as other JSON files are.

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
/// whichever source shows it.
///
/// `argocd` names the instance as the kubeconfig context Argo CD runs on (its hub). A source
/// that reads Argo CD through its API, without that context, names the instance by its server
/// address instead; one that knows both (it does to show an Application once when the hub is
/// read too) passes both to [`FavApp::is`], so a star put from one source holds in the other.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct FavApp {
    pub argocd: String,
    pub namespace: String,
    pub name: String,
}

impl FavApp {
    /// Whether this is the Application `namespace`/`name` of the Argo CD instance known by any
    /// of the names in `instance` (its hub context, its server address).
    pub fn is(&self, instance: &[&str], namespace: &str, name: &str) -> bool {
        self.namespace == namespace && self.name == name && instance.contains(&self.argocd.as_str())
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
            Self::Application(FavApp { argocd: context.to_string(), namespace: namespace.unwrap_or_default(), name })
        } else {
            Self::Object(FavObject { context: context.to_string(), kind: kind.kubectl_name(), namespace, name })
        }
    }

    /// The context it is on (for an Application, Argo CD's).
    pub fn context(&self) -> &str {
        match self {
            Self::Context(c) => c,
            Self::Namespace(n) => &n.context,
            Self::Application(a) => &a.argocd,
            Self::Object(o) => &o.context,
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
            Self::Application(a) => format!("Argo CD Application · {} · {}", a.namespace, a.argocd),
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

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Debug)]
#[serde(default)]
struct Stored {
    kinds: Vec<String>,
    objects: Vec<FavObject>,
    views: Vec<SavedView>,
    contexts: Vec<String>,
    namespaces: Vec<FavNamespace>,
    applications: Vec<FavApp>,
}

/// The entries of `fields[key]` that read as `T` and that `keep` keeps, each once. Anything
/// else (not a list, an entry of another shape) is dropped.
fn entries<T: DeserializeOwned + PartialEq>(
    fields: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    keep: impl Fn(&T) -> bool,
) -> Vec<T> {
    let Some(serde_json::Value::Array(items)) = fields.get(key) else { return Vec::new() };
    let mut out: Vec<T> = Vec::new();
    for item in items {
        if let Ok(t) = serde_json::from_value::<T>(item.clone())
            && keep(&t)
            && !out.contains(&t)
        {
            out.push(t);
        }
    }
    out
}

/// The favorites in a file's bytes, entry by entry (see the module's comment). None when the
/// text isn't a JSON object: the file is then kept aside.
fn from_bytes(bytes: &[u8]) -> Option<Stored> {
    let serde_json::Value::Object(fields) = serde_json::from_slice(bytes).ok()? else { return None };
    let set = |s: &String| !s.is_empty();
    let mut views: Vec<SavedView> = entries(&fields, "views", |v: &SavedView| set(&v.name) && set(&v.kind));
    // One view a name: the first.
    let mut names = std::collections::HashSet::new();
    views.retain(|v| names.insert(v.name.clone()));
    Some(Stored {
        kinds: entries(&fields, "kinds", set),
        objects: entries(&fields, "objects", |o: &FavObject| {
            set(&o.context) && set(&o.kind) && set(&o.name) && o.namespace.as_ref().is_none_or(set)
        }),
        views,
        contexts: entries(&fields, "contexts", set),
        namespaces: entries(&fields, "namespaces", |n: &FavNamespace| set(&n.context) && set(&n.namespace)),
        applications: entries(&fields, "applications", |a: &FavApp| {
            set(&a.argocd) && set(&a.namespace) && set(&a.name)
        }),
    })
}

/// The favorites saved at `path`: none when it is missing; when it can't be read or isn't a
/// JSON object, none and the file kept aside (so the next save doesn't overwrite it).
fn load_from(path: &Path) -> Stored {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Stored::default(),
        Err(e) => {
            eprintln!("{}: {e}; no favorites ({})", path.display(), crate::settings::keep_aside(path));
            return Stored::default();
        }
    };
    from_bytes(&bytes).unwrap_or_else(|| {
        eprintln!("{}: not a JSON object; no favorites ({})", path.display(), crate::settings::keep_aside(path));
        Stored::default()
    })
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
}

fn path() -> PathBuf {
    crate::paths::config_dir().join("favorites.json")
}

impl Favorites {
    pub fn load() -> Self {
        Self { stored: load_from(&path()) }
    }

    fn save(&self) {
        save_json(&path(), &self.stored);
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

    pub fn has(&self, f: &Favorite) -> bool {
        let s = &self.stored;
        match f {
            Favorite::Context(c) => s.contexts.contains(c),
            Favorite::Namespace(n) => s.namespaces.contains(n),
            Favorite::Application(a) => s.applications.contains(a),
            Favorite::Object(o) => s.objects.contains(o),
        }
    }

    /// Whether the Application `namespace`/`name` of the instance known by `instance` is one.
    pub fn has_app(&self, instance: &[&str], namespace: &str, name: &str) -> bool {
        self.stored.applications.iter().any(|a| a.is(instance, namespace, name))
    }

    /// Star (`on`) or unstar `f`; whether that changed anything.
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
            Favorite::Application(a) => put(&mut s.applications, a, on),
            Favorite::Object(o) => put(&mut s.objects, o, on),
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
    use super::{FavApp, FavNamespace, FavObject, Favorite, Favorites, Stored, favorite_namespaces, from_bytes};

    fn ns(context: &str, namespace: &str) -> FavNamespace {
        FavNamespace { context: context.into(), namespace: namespace.into() }
    }

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(ToString::to_string).collect()
    }

    /// A file of an older version (kinds, objects and views only) reads as it was.
    #[test]
    fn an_older_file_reads_as_it_was() {
        let text = br#"{"kinds": ["pods"], "objects": [{"context": "prod", "kind": "deployments.apps",
            "namespace": "web", "name": "api"}], "views": [{"name": "v", "kind": "pods", "namespaces": [],
            "filter": ""}]}"#;
        let s = from_bytes(text).unwrap();
        assert_eq!(s.kinds, names(&["pods"]));
        assert_eq!(s.objects.len(), 1);
        assert_eq!(s.views.len(), 1);
        assert!(s.contexts.is_empty() && s.namespaces.is_empty() && s.applications.is_empty());
    }

    /// One bad entry costs only itself; a file that isn't an object reads as none (kept aside).
    #[test]
    fn a_bad_entry_is_dropped_and_the_others_kept() {
        let text = br#"{"contexts": ["prod", 3, "", "prod"], "namespaces": [{"context": "prod",
            "namespace": "web"}, {"context": "prod"}, {"context": "", "namespace": "x"}],
            "applications": [{"argocd": "hub", "namespace": "argocd", "name": "shop"}, {"name": "x"}],
            "objects": "not a list", "kinds": ["pods", null]}"#;
        let s = from_bytes(text).unwrap();
        assert_eq!(s.contexts, names(&["prod"]));
        assert_eq!(s.namespaces, vec![ns("prod", "web")]);
        assert_eq!(s.applications.len(), 1);
        assert!(s.objects.is_empty());
        assert_eq!(s.kinds, names(&["pods"]));
        for bad in [&b"[1]"[..], b"\"x\"", b"{ not json", b""] {
            assert_eq!(from_bytes(bad), None);
        }
    }

    /// A file that isn't an object is kept aside, and the favorites start empty.
    #[test]
    fn a_broken_file_is_kept_aside() {
        let d = std::env::temp_dir().join(format!("recouvrance-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("favorites.json");
        std::fs::write(&p, b"[1, 2]").unwrap();
        assert_eq!(super::load_from(&p), Stored::default());
        assert!(!p.exists());
        assert_eq!(super::load_from(&d.join("missing.json")), Stored::default());
        std::fs::remove_dir_all(d).unwrap();
    }

    /// Each favorite is keyed by all it names: the same name on another context, kind or
    /// namespace is another favorite.
    #[test]
    fn favorites_are_keyed_by_context_kind_namespace_and_name() {
        let mut f = Favorites { stored: Stored::default() };
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

    /// An Application is the same favorite whether its instance is named by its hub context or,
    /// read through the API, by a server address the source knows it by.
    #[test]
    fn an_application_matches_on_any_name_of_its_instance() {
        let app = FavApp { argocd: "hub".into(), namespace: "argocd".into(), name: "shop".into() };
        assert!(app.is(&["hub"], "argocd", "shop"));
        assert!(app.is(&["https://argocd.example.com", "hub"], "argocd", "shop"));
        assert!(!app.is(&["https://argocd.example.com"], "argocd", "shop"));
        assert!(!app.is(&["hub"], "other", "shop"));
        assert!(!app.is(&["other-hub"], "argocd", "shop"));
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
        let app =
            Favorite::for_object("hub", &crate::k8s::argo::application_kind(), Some("argocd".into()), "shop".into());
        assert_eq!(
            app,
            Favorite::Application(FavApp { argocd: "hub".into(), namespace: "argocd".into(), name: "shop".into() })
        );
        assert_eq!(app.context(), "hub");
    }
}

/// Rules for any file: loading never fails on an entry, keeps every good one, and reads back
/// what it saved.
#[cfg(test)]
mod properties {
    use super::{FavApp, FavNamespace, FavObject, SavedView, Stored, favorites_first, from_bytes};
    use proptest::prelude::*;
    use serde_json::{Value, json};

    fn name() -> impl Strategy<Value = String> {
        "[a-z][a-z0-9-]{0,6}"
    }

    fn stored() -> impl Strategy<Value = Stored> {
        let object = (name(), prop::sample::select(vec!["pods", "deployments.apps"]), prop::option::of(name()), name())
            .prop_map(|(context, kind, namespace, name)| FavObject { context, kind: kind.into(), namespace, name });
        let namespace = (name(), name()).prop_map(|(context, namespace)| FavNamespace { context, namespace });
        let app = (name(), name(), name()).prop_map(|(argocd, namespace, name)| FavApp { argocd, namespace, name });
        let view = (name(), name()).prop_map(|(name, kind)| SavedView {
            name,
            kind,
            namespaces: vec![],
            filter: String::new(),
        });
        (
            prop::collection::btree_set(name(), 0..4),
            prop::collection::vec(object, 0..4),
            prop::collection::btree_map(name(), view, 0..3),
            prop::collection::btree_set(name(), 0..4),
            prop::collection::vec(namespace, 0..4),
            prop::collection::vec(app, 0..4),
        )
            .prop_map(|(kinds, objects, views, contexts, namespaces, applications)| Stored {
                kinds: kinds.into_iter().collect(),
                objects: once(objects),
                views: views.into_values().collect(),
                contexts: contexts.into_iter().collect(),
                namespaces: once(namespaces),
                applications: once(applications),
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

    const LISTS: [&str; 6] = ["kinds", "objects", "views", "contexts", "namespaces", "applications"];

    proptest! {
        /// What is saved reads back as it was.
        #[test]
        fn favorites_read_back_as_saved(s in stored()) {
            let read = from_bytes(&serde_json::to_vec(&s).unwrap());
            prop_assert_eq!(read, Some(s));
        }

        /// Bad entries slipped into any list, and whole lists spoiled, never break loading: every
        /// good entry of the other lists is kept, in its order, and nothing bad is read.
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
            let read = serde_json::to_value(read.unwrap()).unwrap();
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
