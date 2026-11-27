//! What changed between two deployments of an Application's history, with the smart diff
//! (`crate::smartdiff`):
//!
//! - **its sources**: what each deployment asked for, from `status.history[].source(s)` (the
//!   revision deployed, Helm values, `valuesObject`, parameters and value files, kustomize
//!   images and patches, a plugin's env), as values trees;
//! - **its rendered manifests**: what Argo CD renders at each deployment's revision (`GET
//!   /api/v1/applications/{name}/manifests`), compared per resource. Argo CD renders them from
//!   the Application's sources as they are now, at the revision asked: a change of its own
//!   parameters shows in the sources' diff, and a deployment made from other sources (another
//!   count, one source then several) can't be rendered as it was ([`plan`] says so).
//!
//! Read on demand only (when a diff is opened), each revision's manifests kept once read (the
//! last few, bounded in size), under the Application's sources as they are now: a change of
//! them renders anew.

use std::collections::VecDeque;
use std::future::Future;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::server::encode;
use crate::json::arr_at as arr;
use crate::smartdiff::{self, Objects, SmartDiff};

/// Revisions whose manifests are kept, at most.
const KEPT: usize = 16;
/// The manifests kept, at most this many bytes of text in all.
const KEPT_BYTES: usize = 64 * 1024 * 1024;
/// How long Argo CD may take to render manifests: its repo-server may have to fetch and
/// render a large chart.
pub(super) const RENDER_TIMEOUT: Duration = Duration::from_secs(120);

/// What a deployment rendered: its revision, or one per source (in the sources' order).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Revision {
    One(String),
    Many(Vec<String>),
}

impl Revision {
    /// The revision(s) of a deployment of the history; None when it has none.
    pub fn of(entry: &Value) -> Option<Self> {
        let many: Vec<String> =
            arr(entry, "/revisions").iter().map(|r| r.as_str().unwrap_or_default().to_string()).collect();
        if many.iter().any(|r| !r.is_empty()) {
            return Some(Self::Many(many));
        }
        entry.get("revision").and_then(Value::as_str).filter(|r| !r.is_empty()).map(|r| Self::One(r.to_string()))
    }
}

/// The shape of a set of sources: one, several (how many), or a source hydrator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Shape {
    #[default]
    One,
    Many(usize),
    Hydrator,
}

impl Shape {
    fn of(holder: &Value) -> Self {
        if holder.get("sourceHydrator").is_some_and(Value::is_object) {
            return Self::Hydrator;
        }
        match arr(holder, "/sources").len() {
            0 => Self::One,
            n => Self::Many(n),
        }
    }

    fn text(self) -> String {
        match self {
            Self::One => "one source".into(),
            Self::Many(n) => crate::plural::count(n, "source"),
            Self::Hydrator => "a source hydrator".into(),
        }
    }
}

/// A deployment of the history, as compared.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: i64,
    /// Its sources as a values tree (see [`source_tree`]).
    pub sources: Value,
    pub revision: Option<Revision>,
    pub shape: Shape,
}

/// The deployments of `app`'s history that have an id, oldest first.
pub fn entries(app: &Value) -> Vec<Entry> {
    arr(app, "/status/history")
        .iter()
        .filter_map(|h| {
            Some(Entry {
                id: h.get("id")?.as_i64()?,
                sources: source_tree(h),
                revision: Revision::of(h),
                shape: Shape::of(h),
            })
        })
        .collect()
}

/// The Application's sources as they are now: what Argo CD renders any revision with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Spec {
    pub shape: Shape,
    /// A hash of `spec.source`, `spec.sources` and `spec.sourceHydrator`: the manifests kept are
    /// kept under it.
    pub hash: u64,
}

impl Spec {
    pub fn of(app: &Value) -> Self {
        let spec = app.get("spec").unwrap_or(&Value::Null);
        let sources = json!([spec.get("source"), spec.get("sources"), spec.get("sourceHydrator")]);
        let mut h = DefaultHasher::new();
        sources.to_string().hash(&mut h);
        Self { shape: Shape::of(spec), hash: h.finish() }
    }
}

/// Whether the manifests of `from` and `to` can be rendered with the Application's sources as
/// they are now, and what to say about them: Err with why not (another shape of sources, no
/// revision, a source hydrator), else the notes (a source with no revision recorded is rendered
/// at its target revision now).
pub fn plan(spec: Spec, from: &Entry, to: &Entry) -> Result<Vec<String>, String> {
    if spec.shape == Shape::Hydrator {
        return Err(
            "This Application uses a source hydrator: its manifests per deployment aren't rendered here.".into()
        );
    }
    let mut notes = Vec::new();
    for e in [from, to] {
        let Some(revision) = &e.revision else {
            return Err(format!("#{} has no revision recorded: its manifests can't be rendered.", e.id));
        };
        let fits = match (revision, spec.shape) {
            (Revision::One(_), Shape::One) => e.shape == Shape::One,
            (Revision::Many(rs), Shape::Many(n)) => e.shape == Shape::Many(n) && rs.len() == n,
            _ => false,
        };
        if !fits {
            return Err(format!(
                "#{} was deployed from {} and the Application now has {}: Argo CD renders a revision with \
                 today's sources, so its manifests can't be rendered as deployed.",
                e.id,
                e.shape.text(),
                spec.shape.text()
            ));
        }
        if let Revision::Many(rs) = revision {
            for (i, _) in rs.iter().enumerate().filter(|(_, r)| r.is_empty()) {
                notes.push(format!(
                    "#{}: source {} has no revision recorded: rendered at its target revision now.",
                    e.id,
                    i + 1
                ));
            }
        }
    }
    Ok(notes)
}

/// A deployment's sources as a values tree that compares well: one source as is; several as
/// `source 1`, `source 2`... Each gets the revision deployed (`revision`), its Helm values text
/// read as a tree (`valuesObject` wins, as in Argo CD), and its kustomize images by name.
pub fn source_tree(entry: &Value) -> Value {
    let many = arr(entry, "/sources");
    if many.is_empty() {
        let one = entry.get("source").filter(|v| v.is_object()).unwrap_or(&Value::Null);
        return source(one, entry.get("revision").and_then(Value::as_str));
    }
    let revisions = arr(entry, "/revisions");
    let tree: Map<String, Value> = many
        .iter()
        .enumerate()
        .map(|(i, s)| (format!("source {}", i + 1), source(s, revisions.get(i).and_then(Value::as_str))))
        .collect();
    Value::Object(tree)
}

fn source(src: &Value, revision: Option<&str>) -> Value {
    let mut s = src.clone();
    let Some(o) = s.as_object_mut() else { return s };
    if let Some(r) = revision.filter(|r| !r.is_empty()) {
        o.insert("revision".into(), Value::String(r.to_string()));
    }
    if let Some(h) = o.get_mut("helm").and_then(Value::as_object_mut) {
        let text = h.remove("values");
        let object = h.remove("valuesObject").filter(|v| !v.is_null());
        let values = object.or_else(|| match text {
            Some(Value::String(t)) => Some(crate::yaml::from_str::<Value>(&t).unwrap_or(Value::String(t))),
            other => other,
        });
        if let Some(v) = values.filter(|v| !v.is_null()) {
            h.insert("values".into(), v);
        }
    }
    if let Some(k) = o.get_mut("kustomize").and_then(Value::as_object_mut)
        && let Some(list) = k.get("images").and_then(Value::as_array)
    {
        // `[name=]repo:tag`: by the name, else the repository; a repository there twice keeps
        // both (`repo (2)` for the second, in order).
        let mut by: Map<String, Value> = Map::new();
        for i in list.iter().filter_map(Value::as_str) {
            let (name, image) = match i.split_once('=') {
                Some((name, image)) => (name.to_string(), image),
                None => (smartdiff::parse_image(i).0.to_string(), i),
            };
            let mut key = name.clone();
            let mut n = 1;
            while by.contains_key(&key) {
                n += 1;
                key = format!("{name} ({n})");
            }
            by.insert(key, Value::String(image.to_string()));
        }
        k.insert("images".into(), Value::Object(by));
    }
    s
}

// MARK: Manifests

/// The manifests Argo CD renders for the Application `namespace/name` at `revision`: for
/// several sources, each source's position (from 1) with its revision, as the API pairs them.
pub(super) fn manifests_path(namespace: &str, name: &str, revision: &Revision) -> String {
    let mut path = format!("/api/v1/applications/{}/manifests?appNamespace={}", encode(name), encode(namespace));
    match revision {
        Revision::One(r) => path.push_str(&format!("&revision={}", encode(r))),
        Revision::Many(rs) => {
            for (i, r) in rs.iter().enumerate().filter(|(_, r)| !r.is_empty()) {
                path.push_str(&format!("&sourcePositions={}&revisions={}", i + 1, encode(r)));
            }
        }
    }
    path
}

/// The objects of Argo CD's answer (`manifests` holds each as JSON or YAML text), and how many
/// bytes of text they were.
pub(super) fn manifests_of(body: &Value) -> Result<(Objects, usize), String> {
    let mut out = Vec::new();
    let mut bytes = 0;
    for text in arr(body, "/manifests").iter().filter_map(Value::as_str) {
        bytes += text.len();
        let v: Value = serde_json::from_str(text)
            .or_else(|_| crate::yaml::from_str(text))
            .map_err(|e| format!("A manifest Argo CD rendered couldn't be read: {e}"))?;
        out.push(v);
    }
    Ok((smartdiff::objects(out), bytes))
}

/// Which manifests: by Argo CD, Application, its sources now (their hash) and revision.
type CacheKey = (String, String, String, u64, Revision);
/// The manifests kept with their size, the last used last.
type Kept = VecDeque<(CacheKey, Arc<Objects>, usize)>;
static MANIFESTS: LazyLock<Mutex<Kept>> = LazyLock::new(Default::default);

fn kept(key: &CacheKey) -> Option<Arc<Objects>> {
    let mut all = MANIFESTS.lock().unwrap_or_else(PoisonError::into_inner);
    let i = all.iter().position(|(k, _, _)| k == key)?;
    let found = all.remove(i)?;
    let objects = found.1.clone();
    all.push_back(found);
    Some(objects)
}

fn keep(key: CacheKey, objects: Arc<Objects>, bytes: usize) {
    let mut all = MANIFESTS.lock().unwrap_or_else(PoisonError::into_inner);
    all.retain(|(k, _, _)| *k != key);
    all.push_back((key, objects, bytes));
    let total = |all: &Kept| all.iter().map(|(_, _, b)| b).sum::<usize>();
    while all.len() > KEPT || (all.len() > 1 && total(&all) > KEPT_BYTES) {
        all.pop_front();
    }
}

/// Which Application's manifests: its Argo CD (the server's address), namespace, name, and its
/// sources now ([`Spec::hash`]).
pub struct Target {
    pub server: String,
    pub namespace: String,
    pub name: String,
    pub spec: u64,
}

/// The manifests of `target` at `revision`, read with `get` (a GET of a path of Argo CD's API)
/// unless kept.
pub(super) async fn manifests<F, Fut>(target: &Target, revision: &Revision, get: &F) -> Result<Arc<Objects>, String>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    let key = (target.server.clone(), target.namespace.clone(), target.name.clone(), target.spec, revision.clone());
    if let Some(objects) = kept(&key) {
        return Ok(objects);
    }
    let body = get(manifests_path(&target.namespace, &target.name, revision)).await.map_err(|e| {
        if e.contains("did not answer within") {
            format!("{e}: rendering a large Application's manifests can take a while, try again")
        } else {
            e
        }
    })?;
    let (objects, bytes) = manifests_of(&body)?;
    let objects = Arc::new(objects);
    keep(key, objects.clone(), bytes);
    Ok(objects)
}

/// The smart diff of the manifests rendered at `from` and at `to`, each read (or kept); compared
/// on a blocking thread. Each resource's lines are worked out when first shown.
pub(super) async fn manifests_diff<F, Fut>(
    target: Target,
    from: Revision,
    to: Revision,
    get: F,
) -> Result<Arc<SmartDiff>, String>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    let (before, after) = futures::join!(manifests(&target, &from, &get), manifests(&target, &to, &get));
    let (before, after) = (before?, after?);
    let diff = tokio::task::spawn_blocking(move || smartdiff::compare(&before, &after))
        .await
        .map_err(|e| format!("Comparing the manifests failed: {e}"))?;
    Ok(Arc::new(diff))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Revision, Shape, Spec, entries, manifests_of, manifests_path, plan, source_tree};
    use crate::smartdiff::{self, compare_values};

    fn texts(a: &serde_json::Value, b: &serde_json::Value) -> Vec<String> {
        compare_values(&source_tree(a), &source_tree(b)).groups.iter().map(smartdiff::Group::text).collect()
    }

    #[test]
    fn the_manifests_are_asked_as_the_api_routes_them() {
        assert_eq!(
            manifests_path("team a", "web", &Revision::One("abc123".into())),
            "/api/v1/applications/web/manifests?appNamespace=team%20a&revision=abc123"
        );
        // Several sources: each one's position (from 1) and revision, paired.
        assert_eq!(
            manifests_path("argocd", "web", &Revision::Many(vec!["1.2.0".into(), String::new(), "def".into()])),
            "/api/v1/applications/web/manifests?appNamespace=argocd&sourcePositions=1&revisions=1.2.0\
             &sourcePositions=3&revisions=def"
        );
    }

    #[test]
    fn revisions_come_from_the_history() {
        assert_eq!(Revision::of(&json!({"revision": "abc"})), Some(Revision::One("abc".into())));
        assert_eq!(Revision::of(&json!({"revisions": ["a", "b"]})), Some(Revision::Many(vec!["a".into(), "b".into()])));
        assert_eq!(Revision::of(&json!({"revision": ""})), None);
        let app = json!({"status": {"history": [{"id": 1, "revision": "a"}, {"revision": "no id"}, {"id": 2}]}});
        assert_eq!(entries(&app).iter().map(|e| e.id).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn manifests_are_read_from_json_or_yaml_text() {
        let body = json!({"manifests": [
            "{\"apiVersion\":\"v1\",\"kind\":\"ConfigMap\",\"metadata\":{\"name\":\"a\"}}",
            "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\n",
        ]});
        let (objects, bytes) = manifests_of(&body).unwrap();
        assert!(bytes > 100);
        let labels: Vec<String> = objects.keys().map(smartdiff::ObjectKey::label).collect();
        assert_eq!(labels, ["ConfigMap a", "Deployment web"], "by group, kind, namespace, name");
        assert!(manifests_of(&json!({})).unwrap().0.is_empty(), "none rendered");
        assert!(manifests_of(&json!({"manifests": ["{: ["]})).is_err());
    }

    #[test]
    fn helm_values_of_two_deployments() {
        let entry = |rev: &str, values: &str, params: serde_json::Value| {
            json!({"id": 1, "revision": rev, "source": {"repoURL": "https://charts.example.com", "chart": "web",
                "targetRevision": "1.x", "helm": {"values": values, "parameters": params, "valueFiles": ["values-prod.yaml"]}}})
        };
        let before = entry(
            "1.2.0",
            "image:\n  repository: registry.example.com/web\n  tag: 1.15.0\nreplicaCount: 2\n",
            json!([{"name": "feature.flag", "value": "off"}]),
        );
        let after = entry(
            "1.3.0",
            "image:\n  repository: registry.example.com/web\n  tag: 1.16.0\nreplicaCount: 2\n",
            json!([{"name": "feature.flag", "value": "on"}]),
        );
        assert_eq!(
            texts(&before, &after),
            ["image web 1.15.0 → 1.16.0", "helm.parameters[feature.flag].value: off → on", "revision: 1.2.0 → 1.3.0"]
        );
        // valuesObject wins over the values text.
        let mut object = after.clone();
        object["source"]["helm"]["valuesObject"] = json!({"replicaCount": 3});
        assert!(texts(&after, &object).contains(&"helm.values.image removed".to_string()));
    }

    #[test]
    fn kustomize_images_of_two_deployments() {
        let entry = |images: serde_json::Value| {
            json!({"id": 1, "revision": "abc", "source": {"repoURL": "https://git.example.com/deploy.git",
                "path": "overlays/prod", "kustomize": {"images": images}}})
        };
        let before =
            entry(json!(["registry.example.com/shop/api:1.4.1", "worker=registry.example.com/shop/worker:1.4.1"]));
        let after = entry(json!([
            "registry.example.com/shop/api:1.5.0",
            "worker=registry.example.com/shop/worker:1.4.1",
            "registry.example.com/shop/mail:2.0"
        ]));
        assert_eq!(
            texts(&before, &after),
            [
                "image api 1.4.1 → 1.5.0",
                "kustomize.images.registry.example.com/shop/mail added: registry.example.com/shop/mail:2.0"
            ]
        );
    }

    #[test]
    fn several_sources_compare_each_with_its_revision() {
        let entry = |chart: &str, git: &str| {
            json!({"id": 1, "revisions": [chart, git], "sources": [
                {"repoURL": "https://charts.example.com", "chart": "web", "targetRevision": "1.x"},
                {"repoURL": "https://git.example.com/values.git", "ref": "values", "targetRevision": "main"},
            ]})
        };
        assert_eq!(texts(&entry("1.2.0", "aaa"), &entry("1.2.0", "bbb")), ["source 2.revision: aaa → bbb"]);
    }

    #[test]
    fn a_repository_there_twice_keeps_both_images() {
        let entry = |images: serde_json::Value| {
            json!({"id": 1, "revision": "abc", "source": {"repoURL": "https://git.example.com/deploy.git",
                "kustomize": {"images": images}}})
        };
        let before = entry(json!(["registry.example.com/web:1.0", "registry.example.com/web:2.0"]));
        let after = entry(json!(["registry.example.com/web:1.0", "registry.example.com/web:2.1"]));
        assert_eq!(texts(&before, &after), ["image web 2.0 → 2.1"]);
    }

    fn app(spec: serde_json::Value) -> serde_json::Value {
        json!({"spec": spec})
    }

    fn entry(id: i64, history: serde_json::Value) -> super::Entry {
        let mut h = history;
        h["id"] = json!(id);
        entries(&json!({"status": {"history": [h]}})).remove(0)
    }

    #[test]
    fn manifests_are_rendered_only_with_the_same_shape_of_sources() {
        let one = Spec::of(&app(json!({"source": {"repoURL": "https://git.example.com/a.git"}})));
        let two = Spec::of(&app(json!({"sources": [{"repoURL": "a"}, {"repoURL": "b"}]})));
        let hydrated = Spec::of(&app(json!({"sourceHydrator": {"drySource": {"repoURL": "a"}}})));
        assert_eq!((one.shape, two.shape, hydrated.shape), (Shape::One, Shape::Many(2), Shape::Hydrator));
        let single = |id| entry(id, json!({"revision": "abc", "source": {"repoURL": "a"}}));
        let double = |id, second: &str| {
            entry(id, json!({"revisions": ["abc", second], "sources": [{"repoURL": "a"}, {"repoURL": "b"}]}))
        };
        assert_eq!(plan(one, &single(1), &single(2)), Ok(Vec::new()));
        assert_eq!(plan(two, &double(1, "x"), &double(2, "y")), Ok(Vec::new()));
        // One source then, two now: refused, and said why.
        let why = plan(two, &single(1), &double(2, "y")).unwrap_err();
        assert!(why.contains("#1 was deployed from one source") && why.contains("2 sources"), "{why}");
        assert!(plan(one, &single(1), &double(2, "y")).is_err());
        assert!(plan(hydrated, &single(1), &single(2)).unwrap_err().contains("source hydrator"));
        // A source with no revision recorded is said.
        let notes = plan(two, &double(1, ""), &double(2, "y")).unwrap();
        assert_eq!(notes, ["#1: source 2 has no revision recorded: rendered at its target revision now."]);
        let none = entry(3, json!({"source": {"repoURL": "a"}}));
        assert!(plan(one, &none, &single(2)).unwrap_err().contains("no revision"));
    }

    #[test]
    fn a_change_of_the_sources_keys_the_manifests_anew() {
        let a = Spec::of(&app(json!({"source": {"repoURL": "a", "helm": {"values": "x: 1"}}})));
        let b = Spec::of(&app(json!({"source": {"repoURL": "a", "helm": {"values": "x: 2"}}})));
        assert_ne!(a.hash, b.hash);
        let same = Spec::of(&app(json!({"source": {"repoURL": "a", "helm": {"values": "x: 1"}}, "project": "other"})));
        assert_eq!(a.hash, same.hash, "only the sources count");
    }
}
