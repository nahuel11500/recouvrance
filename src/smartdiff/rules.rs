//! What a smart diff folds away as noise: changes that follow from another change, or that
//! change on every release. Kept as data: a rule is a name and a path pattern, matched against
//! a change's path in its object; two more rules look at the values (timestamps, generated
//! names). They apply to Kubernetes objects only, not to values trees.
//!
//! A pattern's segments: a key, a list item's name or index; `*` any one segment; `**` any
//! number (none too); `prefix*` any segment that starts with `prefix`.

use serde_json::Value;

use super::{Leaf, ObjectKey, Renames, Seg};

/// A noise rule: what it hides, and where.
pub struct NoiseRule {
    pub name: &'static str,
    pub path: &'static [&'static str],
}

const SERVER_SET: &str = "set by the API server";
const CHART_LABEL: &str = "chart label";
const HASH_LABEL: &str = "generated hash label";

/// The path rules, in the order they are tried.
pub const NOISE: &[NoiseRule] = &[
    NoiseRule { name: "status", path: &["status", "**"] },
    NoiseRule { name: "field managers", path: &["metadata", "managedFields", "**"] },
    NoiseRule { name: SERVER_SET, path: &["metadata", "resourceVersion"] },
    NoiseRule { name: SERVER_SET, path: &["metadata", "uid"] },
    NoiseRule { name: SERVER_SET, path: &["metadata", "generation"] },
    NoiseRule { name: SERVER_SET, path: &["metadata", "selfLink"] },
    NoiseRule { name: "creation timestamp", path: &["**", "creationTimestamp"] },
    NoiseRule {
        name: "last-applied annotation",
        path: &["metadata", "annotations", "kubectl.kubernetes.io/last-applied-configuration"],
    },
    NoiseRule { name: "revision annotation", path: &["metadata", "annotations", "deployment.kubernetes.io/revision"] },
    NoiseRule { name: "checksum annotation", path: &["**", "annotations", "checksum/*"] },
    NoiseRule { name: CHART_LABEL, path: &["**", "labels", "helm.sh/chart"] },
    NoiseRule { name: CHART_LABEL, path: &["**", "labels", "chart"] },
    NoiseRule { name: "version label", path: &["**", "labels", "app.kubernetes.io/version"] },
    NoiseRule { name: HASH_LABEL, path: &["**", "labels", "pod-template-hash"] },
    NoiseRule { name: HASH_LABEL, path: &["**", "labels", "controller-revision-hash"] },
    NoiseRule { name: HASH_LABEL, path: &["**", "selector", "matchLabels", "pod-template-hash"] },
];

pub const TIMESTAMP: &str = "timestamp";
pub const GENERATED_NAME: &str = "generated name";
/// Counted, not hidden: the digest is on the same line as the tag that changed.
pub const DIGEST: &str = "image digest (its tag changed too)";

/// Whether `pattern` matches the whole of `path`.
pub fn matches(pattern: &[&str], path: &[Seg]) -> bool {
    match pattern.split_first() {
        None => path.is_empty(),
        Some((&"**", rest)) => (0..=path.len()).any(|i| matches(rest, &path[i..])),
        Some((p, rest)) => path.split_first().is_some_and(|(s, tail)| segment(p, s) && matches(rest, tail)),
    }
}

fn segment(pattern: &str, s: &Seg) -> bool {
    let text = s.text();
    if pattern == "*" {
        true
    } else if let Some(prefix) = pattern.strip_suffix('*') {
        text.starts_with(prefix)
    } else {
        text == pattern
    }
}

/// The noise rule `leaf` falls under in the object `key`, if any. A values tree (no kind)
/// has no noise. Images are read before (never noise). Past the path rules:
/// - a timestamp is noise only in the object's own metadata (an annotation a tool stamps):
///   never in a pod template (`restartedAt` is a restart asked for), env or data;
/// - a generated name only where a ConfigMap or Secret is named (`metadata.name`, `configMap`,
///   `secretRef`...), and only for a rename `renames` holds (the same diff regenerated it).
pub fn noise_of(key: &ObjectKey, leaf: &Leaf, renames: &Renames) -> Option<&'static str> {
    if key.kind.is_empty() {
        return None;
    }
    if let Some(rule) = NOISE.iter().find(|r| matches(r.path, &leaf.path)) {
        return Some(rule.name);
    }
    let in_metadata = leaf.path.first().is_some_and(|s| s.is_key("metadata"));
    let sides = [leaf.before.as_ref(), leaf.after.as_ref()];
    let present: Vec<&Value> = sides.into_iter().flatten().collect();
    if in_metadata && !present.is_empty() && present.iter().all(|v| v.as_str().is_some_and(is_timestamp)) {
        return Some(TIMESTAMP);
    }
    if let (Some(a), Some(b)) =
        (leaf.before.as_ref().and_then(Value::as_str), leaf.after.as_ref().and_then(Value::as_str))
        && renames.get(a).is_some_and(|to| to == b)
        && names_a_config(&leaf.path)
    {
        return Some(GENERATED_NAME);
    }
    None
}

/// Where a ConfigMap's or Secret's name is: the object's own name, or a reference to one.
fn names_a_config(path: &[Seg]) -> bool {
    const HOLDERS: [&str; 6] = ["configMap", "secret", "configMapRef", "secretRef", "configMapKeyRef", "secretKeyRef"];
    if matches(&["metadata", "name"], path) {
        return true;
    }
    let n = path.len();
    n >= 2
        && (path[n - 1].is_key("name") || path[n - 1].is_key("secretName"))
        && HOLDERS.iter().any(|h| path[n - 2].is_key(h))
}

fn is_timestamp(s: &str) -> bool {
    s.parse::<jiff::Timestamp>().is_ok()
}

/// The name without kustomize's content hash (`app-config-7h2k9m4t5c` → `app-config`): ten
/// characters of its alphabet (hex digits with `0 1 3 a e` swapped for `g h k m t`).
pub fn kustomize_stem(name: &str) -> Option<&str> {
    let (stem, hash) = name.rsplit_once('-')?;
    let ok = !stem.is_empty() && hash.len() == 10 && hash.bytes().all(|b| b"bcdfghkmt2456789".contains(&b));
    ok.then_some(stem)
}
