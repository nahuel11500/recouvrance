//! A smart diff: what changed between two sets of Kubernetes objects (or two values trees),
//! the useful changes first.
//!
//! - Each change is read as a fact when it is a known one: an image's tag (`image web 1.15.0 →
//!   1.16.0`), a digest alone, replicas, an env var added, removed or changed, a ConfigMap's or a
//!   Secret's key, a resource added or pruned; else a field (`spec.x: a → b`).
//! - The same fact in several resources is one [`Group`], with the list of them.
//! - Noise is folded away and counted: what follows from another change or changes on every
//!   release (checksum annotations, chart and version labels, generated hashes and names,
//!   timestamps, the status, the field managers...). The rules are data, in [`rules`].
//! - A Secret's values are never shown nor compared in clear: its `data`, `stringData` and
//!   last-applied annotation are masked before anything else (only Secrets: a value that looks
//!   like a password elsewhere is shown).
//!
//! Pure: no I/O. [`compare`] for objects keyed by group, kind, namespace and name ([`objects`]
//! builds the map), [`compare_values`] for two values trees.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use serde_json::Value;

use crate::history::{DiffLine, diff_lines, diff_text, drop_nulls};

pub mod rules;

/// Which object: its API group, kind, namespace and name. A values tree's is empty.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectKey {
    pub group: String,
    pub kind: String,
    pub namespace: String,
    pub name: String,
}

impl ObjectKey {
    /// The key of a Kubernetes object; None without a kind or a name.
    pub fn of(obj: &Value) -> Option<Self> {
        let kind = obj.get("kind")?.as_str().filter(|k| !k.is_empty())?.to_string();
        let name = obj.pointer("/metadata/name")?.as_str().filter(|n| !n.is_empty())?.to_string();
        let api = obj.get("apiVersion").and_then(Value::as_str).unwrap_or_default();
        let group = api.rsplit_once('/').map_or("", |(g, _)| g).to_string();
        let namespace = obj.pointer("/metadata/namespace").and_then(Value::as_str).unwrap_or_default().to_string();
        Some(Self { group, kind, namespace, name })
    }

    /// `Deployment shop/web`, `ClusterRole reader`.
    pub fn label(&self) -> String {
        if self.namespace.is_empty() {
            format!("{} {}", self.kind, self.name)
        } else {
            format!("{} {}/{}", self.kind, self.namespace, self.name)
        }
    }

    fn is_secret(&self) -> bool {
        self.group.is_empty() && self.kind == "Secret"
    }

    fn is_config_map(&self) -> bool {
        self.group.is_empty() && self.kind == "ConfigMap"
    }
}

/// Objects by key.
pub type Objects = BTreeMap<ObjectKey, Value>;

/// The objects of `list` by key; those without a kind or a name are left out (a later copy
/// of the same key wins). A `*List` (`List`, `SecretList`...) counts as its items, so a Secret
/// in one is masked like any other.
pub fn objects(list: impl IntoIterator<Item = Value>) -> Objects {
    let mut out = Objects::new();
    for o in list {
        add_object(o, &mut out);
    }
    out
}

fn add_object(o: Value, out: &mut Objects) {
    let is_list = o.get("kind").and_then(Value::as_str).is_some_and(|k| k.ends_with("List"));
    if is_list && let Some(Value::Array(items)) = o.get("items") {
        for i in items.clone() {
            add_object(i, out);
        }
        return;
    }
    if let Some(k) = ObjectKey::of(&o) {
        out.insert(k, o);
    }
}

// MARK: Leaves

/// A step in a path: a key, a list item by its `name`, or by its index (lists whose items
/// have no unique names, of the same length on both sides).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Seg {
    Key(String),
    Item(String),
    Index(usize),
}

impl Seg {
    pub fn text(&self) -> String {
        match self {
            Self::Key(k) | Self::Item(k) => k.clone(),
            Self::Index(i) => i.to_string(),
        }
    }

    fn is_key(&self, key: &str) -> bool {
        matches!(self, Self::Key(k) if k == key)
    }
}

/// `spec.template.spec.containers[web].image`.
pub fn path_text(path: &[Seg]) -> String {
    let mut out = String::new();
    for s in path {
        match s {
            Seg::Key(k) => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(k);
            }
            Seg::Item(n) => out.push_str(&format!("[{n}]")),
            Seg::Index(i) => out.push_str(&format!("[{i}]")),
        }
    }
    out
}

/// One place where two values differ: the deepest point both sides have, with what each side
/// has there (None: nothing).
#[derive(Clone, Debug, PartialEq)]
pub struct Leaf {
    pub path: Vec<Seg>,
    pub before: Option<Value>,
    pub after: Option<Value>,
}

/// Where `before` and `after` differ. Objects are compared key by key; lists by their items'
/// `name` when every item has a unique one (containers, env, ports, volumes), else item by
/// item when as long, else whole. No leaf is the prefix of another.
pub fn leaves(before: &Value, after: &Value) -> Vec<Leaf> {
    let mut out = Vec::new();
    walk(&mut Vec::new(), Some(before), Some(after), &mut out);
    out
}

fn walk(path: &mut Vec<Seg>, b: Option<&Value>, a: Option<&Value>, out: &mut Vec<Leaf>) {
    match (b, a) {
        (None, None) => {}
        (Some(b), Some(a)) if b == a => {}
        (Some(Value::Object(bm)), Some(Value::Object(am))) => {
            let keys: BTreeSet<&String> = bm.keys().chain(am.keys()).collect();
            for k in keys {
                path.push(Seg::Key(k.clone()));
                walk(path, bm.get(k), am.get(k), out);
                path.pop();
            }
        }
        (Some(Value::Array(old_items)), Some(Value::Array(new_items))) => {
            if let (Some(old_named), Some(new_named)) = (named(old_items), named(new_items)) {
                let names: BTreeSet<&str> = old_named.keys().chain(new_named.keys()).copied().collect();
                for n in names {
                    path.push(Seg::Item(n.to_string()));
                    walk(path, old_named.get(n).copied(), new_named.get(n).copied(), out);
                    path.pop();
                }
            } else if old_items.len() == new_items.len() {
                for (i, (x, y)) in old_items.iter().zip(new_items).enumerate() {
                    path.push(Seg::Index(i));
                    walk(path, Some(x), Some(y), out);
                    path.pop();
                }
            } else {
                out.push(Leaf { path: path.clone(), before: b.cloned(), after: a.cloned() });
            }
        }
        _ => out.push(Leaf { path: path.clone(), before: b.cloned(), after: a.cloned() }),
    }
}

/// A list's items by their `name`, when each has a unique one.
fn named(items: &[Value]) -> Option<BTreeMap<&str, &Value>> {
    let mut by = BTreeMap::new();
    for item in items {
        let name = item.get("name")?.as_str()?;
        if by.insert(name, item).is_some() {
            return None;
        }
    }
    Some(by)
}

// MARK: Facts

/// How a thing changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Edit {
    Added,
    Removed,
    Changed,
}

impl Edit {
    fn of(leaf: &Leaf) -> Self {
        match (&leaf.before, &leaf.after) {
            (None, Some(_)) => Self::Added,
            (Some(_), None) => Self::Removed,
            _ => Self::Changed,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Changed => "changed",
        }
    }
}

/// A change, as read. Values longer than a line are left out (`from`/`to` None).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Fact {
    /// An image's tag, or its repository too.
    Image {
        from_repo: String,
        from_tag: String,
        to_repo: String,
        to_tag: String,
    },
    /// An image's digest, its tag the same.
    Digest {
        repo: String,
        tag: String,
    },
    /// Only noise changed in a pod template (a `checksum/*` annotation, a label): its pods
    /// restart all the same. The keys that changed.
    PodTemplate {
        keys: String,
    },
    Replicas {
        from: Option<String>,
        to: Option<String>,
    },
    Env {
        name: String,
        edit: Edit,
        from: Option<String>,
        to: Option<String>,
    },
    ConfigKey {
        key: String,
        edit: Edit,
    },
    /// Never with its value.
    SecretKey {
        key: String,
        edit: Edit,
    },
    /// A ConfigMap or Secret kustomize regenerated under a new name (its content's hash).
    Renamed {
        from: String,
        to: String,
    },
    /// The resource is new.
    Added,
    /// The resource is gone.
    Pruned,
    /// Any other field.
    Field {
        path: String,
        edit: Edit,
        from: Option<String>,
        to: Option<String>,
    },
}

/// The repository's last part: `web` for `registry.example.com/shop/web`.
fn component(repo: &str) -> &str {
    repo.rsplit('/').next().unwrap_or(repo)
}

fn from_to(edit: Edit, from: Option<&String>, to: Option<&String>) -> String {
    match (edit, from, to) {
        (Edit::Changed, Some(f), Some(t)) => format!(": {f} → {t}"),
        (Edit::Added, _, Some(t)) => format!(" added: {t}"),
        _ => format!(" {}", edit.word()),
    }
}

impl Fact {
    /// The fact in a line: `image web 1.15.0 → 1.16.0`, `replicas 3 → 5`, `env FOO added`.
    pub fn text(&self) -> String {
        let or_unset = |v: &Option<String>| v.clone().unwrap_or_else(|| "unset".into());
        match self {
            Self::Image { from_repo, from_tag, to_repo, to_tag } if from_repo == to_repo => {
                format!("image {} {from_tag} → {to_tag}", component(to_repo))
            }
            Self::Image { from_repo, from_tag, to_repo, to_tag } => {
                format!("image {from_repo}:{from_tag} → {to_repo}:{to_tag}")
            }
            Self::Digest { repo, tag } => format!("image {} {tag}: digest changed", component(repo)),
            Self::PodTemplate { keys } => format!("pod template changed (pods restart): {keys}"),
            Self::Replicas { from, to } => format!("replicas {} → {}", or_unset(from), or_unset(to)),
            Self::Env { name, edit, from, to } => format!("env {name}{}", from_to(*edit, from.as_ref(), to.as_ref())),
            Self::ConfigKey { key, edit } => format!("config key {key} {}", edit.word()),
            Self::SecretKey { key, edit } => format!("Secret key {key} {}", edit.word()),
            Self::Renamed { from, to } => format!("renamed {from} → {to} (regenerated)"),
            Self::Added => "added".into(),
            Self::Pruned => "pruned".into(),
            Self::Field { path, edit, from, to } => format!("{path}{}", from_to(*edit, from.as_ref(), to.as_ref())),
        }
    }

    /// Summary order: images first, then pod templates, replicas, env, keys, resources, fields.
    pub fn rank(&self) -> u8 {
        match self {
            Self::Image { .. } => 0,
            Self::Digest { .. } => 1,
            Self::PodTemplate { .. } => 2,
            Self::Replicas { .. } => 3,
            Self::Env { .. } => 4,
            Self::ConfigKey { .. } => 5,
            Self::SecretKey { .. } => 6,
            Self::Renamed { .. } => 7,
            Self::Added => 8,
            Self::Pruned => 9,
            Self::Field { .. } => 10,
        }
    }
}

/// A value in a line, when it is short enough: a string, a number or a boolean.
fn short(v: Option<&Value>) -> Option<String> {
    let text = match v? {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    (text.chars().count() <= 60 && !text.contains('\n')).then_some(text)
}

/// `registry:5000/team/web:1.2@sha256:…` → (repository, tag, digest).
pub fn parse_image(image: &str) -> (&str, Option<&str>, Option<&str>) {
    let (name, digest) = match image.split_once('@') {
        Some((n, d)) => (n, Some(d)),
        None => (image, None),
    };
    let last = name.rfind('/').map_or(0, |i| i + 1);
    match name[last..].rfind(':') {
        Some(i) => (&name[..last + i], Some(&name[last + i + 1..]), digest),
        None => (name, None, digest),
    }
}

/// An image's tag as shown: its tag, else its digest's first 12 characters, else `latest`.
fn tag_text(tag: Option<&str>, digest: Option<&str>) -> String {
    match (tag, digest) {
        (Some(t), _) => t.to_string(),
        (None, Some(d)) => format!("@{}", d.split_once(':').map_or(d, |(_, h)| h).chars().take(12).collect::<String>()),
        (None, None) => "latest".into(),
    }
}

/// The fact of an image going from `before` to `after`, and whether its digest changed along
/// with its tag (noise).
fn image_fact(before: &str, after: &str) -> Option<(Fact, bool)> {
    if before == after {
        return None;
    }
    let (br, bt, bd) = parse_image(before);
    let (ar, at, ad) = parse_image(after);
    if br == ar && bt == at && bt.is_some() {
        return Some((Fact::Digest { repo: ar.to_string(), tag: tag_text(at, None) }, false));
    }
    let digest_too = bd.is_some() && ad.is_some() && bd != ad;
    let fact = Fact::Image {
        from_repo: br.to_string(),
        from_tag: tag_text(bt, bd),
        to_repo: ar.to_string(),
        to_tag: tag_text(at, ad),
    };
    Some((fact, digest_too && bt != at))
}

/// The value at `path` in `root`, items found by name.
fn value_at<'a>(root: &'a Value, path: &[Seg]) -> Option<&'a Value> {
    let mut cur = root;
    for s in path {
        cur = match (s, cur) {
            (Seg::Key(k), Value::Object(m)) => m.get(k)?,
            (Seg::Item(n), Value::Array(a)) => a.iter().find(|i| i.get("name").and_then(Value::as_str) == Some(n))?,
            (Seg::Index(i), Value::Array(a)) => a.get(*i)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// The image fact of a leaf that is an image (`image`, an entry of `images`), and whether its
/// digest changed along with its tag. Read before any noise rule: a tag is never noise, however
/// much it looks like a hash or a date.
fn image_leaf(leaf: &Leaf) -> Option<(Fact, bool)> {
    let path = &leaf.path;
    let last = path.last().is_some_and(|s| s.is_key("image"));
    let parent = path.len().checked_sub(2).and_then(|i| path.get(i)).is_some_and(|p| p.is_key("images"));
    let (x, y) = (leaf.before.as_ref()?.as_str()?, leaf.after.as_ref()?.as_str()?);
    if last || parent { image_fact(x, y) } else { None }
}

/// What a leaf of `key` says, from the two whole sides `b` and `a` (images aside: see
/// [`image_leaf`]).
fn fact_of(key: &ObjectKey, leaf: &Leaf, b: &Value, a: &Value) -> Fact {
    let edit = Edit::of(leaf);
    let path = &leaf.path;
    let last = match path.last() {
        Some(Seg::Key(k)) => Some(k.as_str()),
        _ => None,
    };
    let strs = (leaf.before.as_ref().and_then(Value::as_str), leaf.after.as_ref().and_then(Value::as_str));
    if path.len() == 2 {
        let field = path[0].text();
        let k = path[1].text();
        if key.is_secret() && matches!(field.as_str(), "data" | "stringData") {
            return Fact::SecretKey { key: k, edit };
        }
        if key.is_config_map() && matches!(field.as_str(), "data" | "binaryData") {
            return Fact::ConfigKey { key: k, edit };
        }
    }
    // A values tree's `image: {repository, tag}`.
    if last == Some("tag")
        && let (Some(x), Some(y)) = strs
    {
        let parent_path = &path[..path.len() - 1];
        let repo_of = |root: &Value| {
            value_at(root, parent_path).and_then(|p| p.get("repository")).and_then(Value::as_str).map(String::from)
        };
        if let Some(to_repo) = repo_of(a) {
            let from_repo = repo_of(b).unwrap_or_else(|| to_repo.clone());
            return Fact::Image { from_repo, from_tag: x.into(), to_repo, to_tag: y.into() };
        }
    }
    let number_or_none = |v: Option<&Value>| v.is_none_or(Value::is_number);
    if matches!(last, Some("replicas" | "replicaCount"))
        && number_or_none(leaf.before.as_ref())
        && number_or_none(leaf.after.as_ref())
    {
        return Fact::Replicas { from: short(leaf.before.as_ref()), to: short(leaf.after.as_ref()) };
    }
    if let Some(i) = path.iter().rposition(|s| s.is_key("env"))
        && let Some(Seg::Item(name)) = path.get(i + 1)
    {
        let (edit, from, to) = if path.len() == i + 2 {
            let value = |v: Option<&Value>| short(v.and_then(|v| v.get("value")));
            (edit, value(leaf.before.as_ref()), value(leaf.after.as_ref()))
        } else if path.len() == i + 3 && last == Some("value") {
            (Edit::Changed, short(leaf.before.as_ref()), short(leaf.after.as_ref()))
        } else {
            (Edit::Changed, None, None)
        };
        return Fact::Env { name: name.clone(), edit, from, to };
    }
    Fact::Field { path: path_text(path), edit, from: short(leaf.before.as_ref()), to: short(leaf.after.as_ref()) }
}

// MARK: Secrets

const MASK: &str = "••••••";
const MASK_CHANGED: &str = "•••••• (changed)";
const SECRET_FIELDS: [&str; 2] = ["data", "stringData"];
const LAST_APPLIED: &str = "/metadata/annotations/kubectl.kubernetes.io~1last-applied-configuration";

/// Argo CD hides a Secret's values in what it renders with runs of `+`: two of them say
/// nothing about whether the values are the same.
fn hidden_by_argo(v: &Value) -> bool {
    v.as_str().is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b == b'+'))
}

/// Mask both sides of a Secret: the keys stay, a value that changed reads as changed, none is
/// shown; `kubectl apply`'s copy of them is masked the same way.
fn masked_secret(before: &Value, after: &Value) -> (Value, Value) {
    let (mut b, mut a) = (before.clone(), after.clone());
    let mask = |x: &mut Value, same: bool| *x = Value::String((if same { MASK } else { MASK_CHANGED }).into());
    for f in SECRET_FIELDS {
        let old = before.get(f);
        if let Some(data) = a.get_mut(f).and_then(Value::as_object_mut) {
            for (k, x) in data.iter_mut() {
                let was = old.and_then(|o| o.get(k));
                let same = was == Some(&*x) || (was.is_some_and(hidden_by_argo) && hidden_by_argo(x));
                mask(x, same);
            }
        }
        if let Some(data) = b.get_mut(f).and_then(Value::as_object_mut) {
            data.values_mut().for_each(|x| mask(x, true));
        }
    }
    let old_applied = before.pointer(LAST_APPLIED);
    if let Some(x) = a.pointer_mut(LAST_APPLIED) {
        let same = old_applied == Some(&*x);
        mask(x, same);
    }
    if let Some(x) = b.pointer_mut(LAST_APPLIED) {
        mask(x, true);
    }
    (b, a)
}

// MARK: Diff

/// A change folded away as noise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Noise {
    /// The rule's name (see [`rules`]).
    pub rule: &'static str,
    pub path: Vec<Seg>,
    /// Taken out of the diff's lines when noise is hidden; false when it shares a line with a
    /// fact (a digest next to its tag).
    pub own_line: bool,
}

/// What changed in one resource.
#[derive(Clone, Debug)]
pub struct ResourceChange {
    pub key: ObjectKey,
    /// What changed, each once.
    pub facts: Vec<Fact>,
    pub noise: Vec<Noise>,
    /// Both sides, a Secret's values masked.
    before: Value,
    after: Value,
    /// The diff's lines without noise, and with it: worked out once.
    lines: [OnceLock<Vec<DiffLine>>; 2],
}

impl ResourceChange {
    fn new(key: ObjectKey, facts: Vec<Fact>, noise: Vec<Noise>, before: Value, after: Value) -> Self {
        Self { key, facts, noise, before, after, lines: Default::default() }
    }

    /// The diff's lines (only changed lines and some context, `-`/`+`), the noise left out
    /// unless `with_noise`.
    pub fn lines(&self, with_noise: bool) -> &[DiffLine] {
        self.lines[usize::from(with_noise)].get_or_init(|| {
            if with_noise {
                return diff_lines(&diff_text(&self.before), &diff_text(&self.after));
            }
            let (mut b, mut a) = (self.before.clone(), self.after.clone());
            for n in self.noise.iter().filter(|n| n.own_line) {
                remove_at(&mut b, &n.path);
                remove_at(&mut a, &n.path);
            }
            diff_lines(&diff_text(&b), &diff_text(&a))
        })
    }
}

/// Take what is at `path` out of `v`: a key or a named item removed, an indexed item nulled
/// (the others keep their index).
fn remove_at(v: &mut Value, path: &[Seg]) {
    let Some((last, parents)) = path.split_last() else {
        *v = Value::Null;
        return;
    };
    let mut cur = v;
    for s in parents {
        let next = match (s, cur) {
            (Seg::Key(k), Value::Object(m)) => m.get_mut(k),
            (Seg::Item(n), Value::Array(a)) => a.iter_mut().find(|i| i.get("name").and_then(Value::as_str) == Some(n)),
            (Seg::Index(i), Value::Array(a)) => a.get_mut(*i),
            _ => None,
        };
        match next {
            Some(n) => cur = n,
            None => return,
        }
    }
    match (last, cur) {
        (Seg::Key(k), Value::Object(m)) => {
            m.remove(k);
        }
        (Seg::Item(n), Value::Array(a)) => a.retain(|i| i.get("name").and_then(Value::as_str) != Some(n)),
        (Seg::Index(i), Value::Array(a)) => {
            if let Some(x) = a.get_mut(*i) {
                *x = Value::Null;
            }
        }
        _ => {}
    }
}

/// The facts of a ConfigMap's or Secret's data come by key: a `data` added or removed whole
/// is split into its keys.
fn split_data(key: &ObjectKey, leaf: Leaf) -> Vec<Leaf> {
    let data_field =
        matches!(leaf.path.as_slice(), [Seg::Key(f)] if matches!(f.as_str(), "data" | "stringData" | "binaryData"));
    if !(data_field && (key.is_secret() || key.is_config_map())) {
        return vec![leaf];
    }
    // A side without data is an empty map; data that isn't a map is compared whole.
    let side = |v: Option<&Value>| match v {
        None => Some(serde_json::Map::new()),
        Some(v) => v.as_object().cloned(),
    };
    let (Some(b), Some(a)) = (side(leaf.before.as_ref()), side(leaf.after.as_ref())) else {
        return vec![leaf];
    };
    let keys: BTreeSet<&String> = b.keys().chain(a.keys()).collect();
    keys.into_iter()
        .filter(|k| b.get(*k) != a.get(*k))
        .map(|k| {
            let mut path = leaf.path.clone();
            path.push(Seg::Key(k.clone()));
            Leaf { path, before: b.get(k).cloned(), after: a.get(k).cloned() }
        })
        .collect()
}

/// Names kustomize regenerated (a ConfigMap's or Secret's content hash): old name → new.
pub type Renames = BTreeMap<String, String>;

/// Whether `path` is in a pod template (a workload's `spec.template`, a CronJob's
/// `spec.jobTemplate.spec.template`): a change there restarts its pods.
fn in_pod_template(path: &[Seg]) -> bool {
    path.first().is_some_and(|s| s.is_key("spec")) && path.iter().any(|s| s.is_key("template"))
}

/// The facts and the noise between two sides of `key`, both present. `renames`: the names
/// regenerated in the same diff, the only generated names folded as noise. When nothing but
/// noise changed in a pod template, that is a fact: its pods restart (over Argo CD's API a
/// Secret's values are hidden, so a `checksum/secret` annotation may be all that says it).
fn classify(key: &ObjectKey, b: &Value, a: &Value, renames: &Renames) -> (Vec<Fact>, Vec<Noise>) {
    let mut facts: Vec<Fact> = Vec::new();
    let mut seen: BTreeSet<Fact> = BTreeSet::new();
    let mut noise = Vec::new();
    let mut template_fact = false;
    let mut add = |fact: Fact, facts: &mut Vec<Fact>| {
        if seen.insert(fact.clone()) {
            facts.push(fact);
        }
    };
    for leaf in leaves(b, a).into_iter().flat_map(|l| split_data(key, l)) {
        let in_template = in_pod_template(&leaf.path);
        if let Some((fact, digest_too)) = image_leaf(&leaf) {
            if digest_too {
                noise.push(Noise { rule: rules::DIGEST, path: leaf.path, own_line: false });
            }
            template_fact |= in_template;
            add(fact, &mut facts);
            continue;
        }
        if let Some(rule) = rules::noise_of(key, &leaf, renames) {
            noise.push(Noise { rule, path: leaf.path, own_line: true });
            continue;
        }
        template_fact |= in_template;
        add(fact_of(key, &leaf, b, a), &mut facts);
    }
    if !template_fact {
        let keys: BTreeSet<String> = noise
            .iter()
            .filter(|n| n.own_line && in_pod_template(&n.path))
            .filter_map(|n| n.path.last().map(Seg::text))
            .collect();
        if !keys.is_empty() {
            // Shown in the lines too: they are what says it.
            for n in noise.iter_mut().filter(|n| in_pod_template(&n.path)) {
                n.own_line = false;
            }
            add(Fact::PodTemplate { keys: keys.into_iter().collect::<Vec<_>>().join(", ") }, &mut facts);
        }
    }
    (facts, noise)
}

/// What changed in `key` from `before` to `after` (either may be missing); None when nothing.
fn diff_resource(
    key: &ObjectKey,
    before: Option<&Value>,
    after: Option<&Value>,
    renames: &Renames,
) -> Option<ResourceChange> {
    let prep = |v: Option<&Value>| {
        let mut v = v.cloned().unwrap_or(Value::Null);
        drop_nulls(&mut v);
        v
    };
    let (mut b, mut a) = (prep(before), prep(after));
    if key.is_secret() {
        (b, a) = masked_secret(&b, &a);
    }
    let (facts, noise) = match (b.is_null(), a.is_null()) {
        (true, true) => return None,
        (true, false) => (vec![Fact::Added], Vec::new()),
        (false, true) => (vec![Fact::Pruned], Vec::new()),
        _ => classify(key, &b, &a, renames),
    };
    if facts.is_empty() && noise.is_empty() {
        return None;
    }
    Some(ResourceChange::new(key.clone(), facts, noise, b, a))
}

/// The same fact in several resources.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub fact: Fact,
    /// Where, in order.
    pub resources: Vec<ObjectKey>,
}

impl Group {
    /// The line of the summary: the fact (`image web 1.15.0 → 1.16.0`), or how many resources
    /// were added or pruned.
    pub fn text(&self) -> String {
        let n = self.resources.len();
        match self.fact {
            Fact::Added => format!("{} added", crate::plural::count(n, "resource")),
            Fact::Pruned => format!("{} pruned", crate::plural::count(n, "resource")),
            _ => self.fact.text(),
        }
    }
}

/// The facts of `resources`, each once with every resource it is in: images first, then
/// replicas, env, keys, resources added and pruned, fields.
pub fn group(resources: &[ResourceChange]) -> Vec<Group> {
    let mut by: BTreeMap<(u8, &Fact), Vec<ObjectKey>> = BTreeMap::new();
    for r in resources {
        for f in &r.facts {
            let keys = by.entry((f.rank(), f)).or_default();
            if !keys.contains(&r.key) {
                keys.push(r.key.clone());
            }
        }
    }
    by.into_iter().map(|((_, fact), resources)| Group { fact: fact.clone(), resources }).collect()
}

/// What changed between two sets of objects, or two values trees.
#[derive(Clone, Debug, Default)]
pub struct SmartDiff {
    /// Each resource that changed (facts, or noise only), by key.
    pub resources: Vec<ResourceChange>,
    /// The summary: each fact once.
    pub groups: Vec<Group>,
    /// Resources the same on both sides.
    pub unchanged: usize,
}

impl SmartDiff {
    fn of(mut resources: Vec<ResourceChange>, unchanged: usize) -> Self {
        resources.sort_by(|a, b| a.key.cmp(&b.key));
        let groups = group(&resources);
        Self { resources, groups, unchanged }
    }

    /// Nothing changed, not even noise.
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    /// How many changes are folded away as noise.
    pub fn noise(&self) -> usize {
        self.resources.iter().map(|r| r.noise.len()).sum()
    }

    /// The noise by rule, the most first: (`checksum annotation`, 12).
    pub fn noise_by_rule(&self) -> Vec<(&'static str, usize)> {
        let mut by: BTreeMap<&'static str, usize> = BTreeMap::new();
        for n in self.resources.iter().flat_map(|r| &r.noise) {
            *by.entry(n.rule).or_default() += 1;
        }
        let mut out: Vec<(&'static str, usize)> = by.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        out
    }

    /// Work out every resource's lines now (off the UI thread), rather than when first shown.
    pub fn prepare(&self) {
        for r in &self.resources {
            r.lines(false);
            r.lines(true);
        }
    }
}

/// What changed from `before` to `after`. A ConfigMap or Secret gone whose name ends with
/// kustomize's content hash and a new one of the same kind, namespace and name but the hash
/// (one each) are the same one regenerated: compared, with a [`Fact::Renamed`], not pruned and
/// added; references to it that changed only by that rename are noise.
pub fn compare(before: &Objects, after: &Objects) -> SmartDiff {
    type Stem = (String, String, String);
    let stem = |k: &ObjectKey| -> Option<Stem> {
        if !(k.is_config_map() || k.is_secret()) {
            return None;
        }
        let s = rules::kustomize_stem(&k.name)?;
        Some((k.kind.clone(), k.namespace.clone(), s.to_string()))
    };
    let mut gone: BTreeMap<Stem, Vec<&ObjectKey>> = BTreeMap::new();
    for k in before.keys().filter(|k| !after.contains_key(*k)) {
        if let Some(s) = stem(k) {
            gone.entry(s).or_default().push(k);
        }
    }
    let mut new: BTreeMap<Stem, Vec<&ObjectKey>> = BTreeMap::new();
    for k in after.keys().filter(|k| !before.contains_key(*k)) {
        if let Some(s) = stem(k) {
            new.entry(s).or_default().push(k);
        }
    }
    let mut renamed: BTreeMap<&ObjectKey, &ObjectKey> = BTreeMap::new();
    for (s, olds) in &gone {
        if let ([old], Some([fresh])) = (olds.as_slice(), new.get(s).map(Vec::as_slice)) {
            renamed.insert(*fresh, *old);
        }
    }
    let renames: Renames = renamed.iter().map(|(fresh, old)| (old.name.clone(), fresh.name.clone())).collect();
    let was_renamed: BTreeSet<&ObjectKey> = renamed.values().copied().collect();
    let mut resources = Vec::new();
    let mut unchanged = 0;
    for (k, a) in after {
        let old = renamed.get(k).copied();
        let b = before.get(k).or_else(|| old.and_then(|o| before.get(o)));
        let found = diff_resource(k, b, Some(a), &renames);
        match (found, old) {
            (Some(mut r), Some(o)) => {
                r.facts.insert(0, Fact::Renamed { from: o.name.clone(), to: k.name.clone() });
                resources.push(r);
            }
            (Some(r), None) => resources.push(r),
            (None, _) => unchanged += 1,
        }
    }
    for (k, b) in before {
        if after.contains_key(k) || was_renamed.contains(k) {
            continue;
        }
        resources.extend(diff_resource(k, Some(b), None, &renames));
    }
    SmartDiff::of(resources, unchanged)
}

/// What changed from one values tree to another (Helm values, an Application's sources): one
/// resource with an empty key. No noise rule applies to values.
pub fn compare_values(before: &Value, after: &Value) -> SmartDiff {
    let as_tree = |v: &Value| if v.is_null() { Value::Object(serde_json::Map::new()) } else { v.clone() };
    let key = ObjectKey::default();
    let (b, a) = (as_tree(before), as_tree(after));
    match diff_resource(&key, Some(&b), Some(&a), &Renames::new()) {
        Some(r) => SmartDiff::of(vec![r], 0),
        None => SmartDiff::of(Vec::new(), 1),
    }
}

#[cfg(test)]
mod tests;
