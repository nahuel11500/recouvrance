//! OpenAPI v3 schemas from the API server (built-in types and CRDs), for field docs
//! like `kubectl explain` and for forms. One group-version document is fetched per
//! cluster and kept for the session.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use kube::Client;
use serde_json::Value;

/// Documents by (context, group-version).
type Documents = HashMap<(String, String), Arc<Value>>;

static CACHE: Mutex<Option<Documents>> = Mutex::new(None);

/// The OpenAPI document of one group-version (`v1`, `apps/v1`, `cert-manager.io/v1`).
pub async fn document(context: &str, client: Client, api_version: &str) -> Result<Arc<Value>> {
    let key = (context.to_string(), api_version.to_string());
    if let Some(doc) = CACHE.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        return Ok(doc.clone());
    }
    let path = if api_version.contains('/') { format!("apis/{api_version}") } else { format!("api/{api_version}") };
    let index = crate::k8s::get_json(client.clone(), "/openapi/v3".into())
        .await
        .context("the API server did not return /openapi/v3")?;
    let url = index
        .pointer(&format!("/paths/{}/serverRelativeURL", path.replace('/', "~1")))
        .and_then(Value::as_str)
        .with_context(|| format!("no schema for {api_version}"))?
        .to_string();
    let doc = Arc::new(crate::k8s::get_json(client, url).await.context("fetching the schema")?);
    CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, doc.clone());
    Ok(doc)
}

/// A kind's schema within its group-version document.
#[derive(Clone)]
pub struct Schema {
    doc: Arc<Value>,
    root: Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub type_name: String,
    pub description: String,
    pub required: bool,
    pub enum_values: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Explained {
    pub path: Vec<String>,
    pub field: Field,
    pub children: Vec<Field>,
}

impl Schema {
    pub fn find(doc: Arc<Value>, api_version: &str, kind: &str) -> Option<Self> {
        let (group, version) = match api_version.split_once('/') {
            Some((g, v)) => (g, v),
            None => ("", api_version),
        };
        let schemas = doc.pointer("/components/schemas")?.as_object()?;
        let root = schemas
            .values()
            .find(|s| {
                s["x-kubernetes-group-version-kind"].as_array().is_some_and(|g| {
                    g.iter().any(|g| g["group"] == group && g["version"] == version && g["kind"] == kind)
                })
            })?
            .clone();
        Some(Self { doc, root })
    }

    /// Follow `$ref` and single `allOf` wrappers; the outer description wins.
    fn resolve(&self, node: &Value) -> Value {
        let mut out = node.clone();
        for _ in 0..8 {
            let target = out
                .get("$ref")
                .and_then(Value::as_str)
                .or_else(|| out.pointer("/allOf/0/$ref").and_then(Value::as_str))
                .map(String::from);
            let Some(r) = target else { break };
            let name = r.trim_start_matches("#/components/schemas/");
            let Some(inner) =
                self.doc.pointer(&format!("/components/schemas/{}", name.replace('~', "~0").replace('/', "~1")))
            else {
                break;
            };
            let mut merged = inner.clone();
            if let (Some(m), Some(o)) = (merged.as_object_mut(), out.as_object()) {
                for k in ["description", "default"] {
                    if let Some(v) = o.get(k) {
                        m.insert(k.into(), v.clone());
                    }
                }
            }
            out = merged;
        }
        out
    }

    pub fn type_name(&self, node: &Value) -> String {
        let n = self.resolve(node);
        match n["type"].as_str() {
            Some("array") => format!("[]{}", self.type_name(&n["items"])),
            Some("object") if n.get("properties").is_none() && n.get("additionalProperties").is_some() => {
                format!("map[string]{}", self.type_name(&n["additionalProperties"]))
            }
            Some(t) => t.to_string(),
            None if n.get("x-kubernetes-int-or-string").is_some() => "int-or-string".into(),
            None if n.get("x-kubernetes-preserve-unknown-fields").is_some() => "any".into(),
            None => "object".into(),
        }
    }

    fn field(&self, name: &str, node: &Value, required: bool) -> Field {
        let n = self.resolve(node);
        Field {
            name: name.to_string(),
            type_name: self.type_name(node),
            description: n["description"].as_str().unwrap_or_default().to_string(),
            required,
            enum_values: n["enum"]
                .as_array()
                .map(|e| e.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default(),
        }
    }

    /// The object node at a path (array items and map values are stepped through).
    pub fn node_at(&self, path: &[String]) -> Option<Value> {
        let mut node = self.root.clone();
        for key in path {
            // A list index: step into the items.
            if !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit()) {
                let n = self.resolve(&node);
                node = if n["type"] == "array" { n["items"].clone() } else { node };
                continue;
            }
            let mut n = self.resolve(&node);
            while n["type"] == "array" {
                n = self.resolve(&n["items"]);
            }
            node = match n.pointer(&format!("/properties/{}", key.replace('~', "~0").replace('/', "~1"))) {
                Some(p) => p.clone(),
                None => n.get("additionalProperties").filter(|a| a.is_object()).cloned()?,
            };
        }
        Some(node)
    }

    /// Like `kubectl explain`: the field at `path` and the fields it contains.
    pub fn explain(&self, path: &[String]) -> Option<Explained> {
        let node = self.node_at(path)?;
        let parent_required: Vec<String> = match path.split_last() {
            Some((_, parent)) => self.node_at(parent).map(|p| {
                let mut p = self.resolve(&p);
                while p["type"] == "array" {
                    p = self.resolve(&p["items"]);
                }
                p["required"]
                    .as_array()
                    .map(|r| r.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default()
            }),
            None => None,
        }
        .unwrap_or_default();
        let name = path.last().cloned().unwrap_or_else(|| {
            self.root["x-kubernetes-group-version-kind"][0]["kind"].as_str().unwrap_or("object").to_string()
        });
        let field = self.field(&name, &node, parent_required.contains(&name));
        let mut inner = self.resolve(&node);
        while inner["type"] == "array" {
            inner = self.resolve(&inner["items"]);
        }
        let required: Vec<String> = inner["required"]
            .as_array()
            .map(|r| r.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let mut children: Vec<Field> = inner["properties"]
            .as_object()
            .map(|props| props.iter().map(|(k, v)| self.field(k, v, required.contains(k))).collect())
            .unwrap_or_default();
        children.sort_by(|a, b| b.required.cmp(&a.required).then(a.name.cmp(&b.name)));
        Some(Explained { path: path.to_vec(), field, children })
    }
}

/// apiVersion and kind of the YAML document containing `line` (0-based).
pub fn doc_kind_at(text: &str, line: usize) -> Option<(String, String)> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let start = (0..=line.min(lines.len().saturating_sub(1)))
        .rev()
        .find(|&i| crate::yaml::doc_start(lines[i]).is_some())
        .map_or(0, |i| i + 1);
    let end = (line..lines.len()).find(|&i| crate::yaml::doc_start(lines[i]).is_some()).unwrap_or(lines.len());
    let top = |key: &str| {
        lines[start..end.max(start)]
            .iter()
            .find_map(|l| l.strip_prefix(&format!("{key}:")).map(|v| v.trim().trim_matches('"').to_string()))
            .filter(|v| !v.is_empty())
    };
    Some((top("apiVersion")?, top("kind")?))
}

fn key_of(body: &str) -> Option<&str> {
    let (k, _) = body.split_once(':')?;
    let k = k.trim().trim_matches('"').trim_matches('\'');
    (!k.is_empty() && !k.contains(' ') && !k.starts_with('#')).then_some(k)
}

/// The key path of the field on `line` (0-based), from indentation:
/// `spec.template.spec.containers.image` for an image inside a list item.
pub fn yaml_path_at(text: &str, line: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let Some(cur) = lines.get(line) else { return Vec::new() };
    let mut path = Vec::new();
    // (indent a parent key must be below, whether equal counts because a list dash sits there)
    let indent_of = |l: &str| l.len() - l.trim_start().len();
    let body = cur.trim_start();
    let mut limit = indent_of(cur);
    let mut equal_ok = false;
    if let Some(rest) = body.strip_prefix("- ") {
        if let Some(k) = key_of(rest) {
            path.push(k.to_string());
        }
        equal_ok = true;
    } else if let Some(k) = key_of(body) {
        path.push(k.to_string());
    }
    for l in lines[..line].iter().rev() {
        if crate::yaml::doc_start(l).is_some() {
            break;
        }
        let b = l.trim_start();
        if b.is_empty() || b.starts_with('#') {
            continue;
        }
        let ind = indent_of(l);
        if let Some(rest) = b.strip_prefix("- ") {
            // A sibling key inside the same list item (`- name: x` above `image: y`).
            if ind < limit {
                limit = ind;
                equal_ok = true;
                let _ = rest;
            }
            continue;
        }
        if (ind < limit || (equal_ok && ind == limit))
            && let Some(k) = key_of(b)
        {
            path.push(k.to_string());
            limit = ind;
            equal_ok = false;
        }
        if ind == 0 && !equal_ok {
            break;
        }
    }
    path.reverse();
    path
}

#[cfg(test)]
mod tests {
    use super::{doc_kind_at, yaml_path_at};

    #[test]
    fn paths() {
        let y = "apiVersion: apps/v1\nkind: Deployment\nspec:\n  template:\n    spec:\n      containers:\n      - name: a\n        image: x\n        ports:\n          - containerPort: 80\n";
        assert_eq!(yaml_path_at(y, 7), ["spec", "template", "spec", "containers", "image"]);
        assert_eq!(yaml_path_at(y, 6), ["spec", "template", "spec", "containers", "name"]);
        assert_eq!(yaml_path_at(y, 9), ["spec", "template", "spec", "containers", "ports", "containerPort"]);
        assert_eq!(yaml_path_at(y, 3), ["spec", "template"]);
        assert_eq!(doc_kind_at(y, 5), Some(("apps/v1".into(), "Deployment".into())));
        let two = "apiVersion: v1\nkind: Service\n---\napiVersion: v1\nkind: ConfigMap\ndata:\n  a: b\n";
        assert_eq!(doc_kind_at(two, 6), Some(("v1".into(), "ConfigMap".into())));
        assert_eq!(yaml_path_at(two, 6), ["data", "a"]);
        // A separator with a comment, as Apply splits it.
        let commented = "apiVersion: v1\nkind: ConfigMap\ndata:\n  replicas: x\n--- # web\napiVersion: apps/v1\nkind: Deployment\nspec:\n  replicas: 2\n";
        assert_eq!(doc_kind_at(commented, 8), Some(("apps/v1".into(), "Deployment".into())));
        assert_eq!(yaml_path_at(commented, 8), ["spec", "replicas"]);
        assert_eq!(doc_kind_at(commented, 3), Some(("v1".into(), "ConfigMap".into())));
    }

    #[test]
    fn empty_text() {
        assert_eq!(doc_kind_at("", 0), None);
        assert_eq!(doc_kind_at("", 5), None);
        assert!(yaml_path_at("", 0).is_empty());
        assert!(yaml_path_at("", 3).is_empty());
        // Cursor past the end of a non-empty document.
        assert_eq!(doc_kind_at("apiVersion: v1\nkind: Pod\n", 9), Some(("v1".into(), "Pod".into())));
        assert!(yaml_path_at("a: b\n", 9).is_empty());
    }
}
