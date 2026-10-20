//! RBAC reading: which rules a role grants, which subjects a binding names, who can
//! do something ("who can"), and what a ServiceAccount may do through its bindings.

use serde_json::Value;

/// Roles, ClusterRoles, RoleBindings and ClusterRoleBindings, as listed.
#[derive(Default, Clone)]
pub struct Snapshot {
    pub roles: Vec<Value>,
    pub cluster_roles: Vec<Value>,
    pub role_bindings: Vec<Value>,
    pub cluster_role_bindings: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Grant {
    pub subject_kind: String,
    pub subject: String,
    pub subject_ns: Option<String>,
    /// "RoleBinding ns/name" or "ClusterRoleBinding name".
    pub binding: String,
    pub role: String,
    /// Namespace the grant applies in; None = every namespace.
    pub scope: Option<String>,
    /// Only for these object names.
    pub names: Vec<String>,
}

fn strs(v: &Value, key: &str) -> Vec<String> {
    v[key].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// Whether one PolicyRule allows `verb` on `group`/`resource` (resource may be `pods/log`).
pub fn rule_allows(rule: &Value, verb: &str, group: &str, resource: &str) -> bool {
    let verbs = strs(rule, "verbs");
    let groups = strs(rule, "apiGroups");
    let resources = strs(rule, "resources");
    let base = resource.split('/').next().unwrap_or(resource);
    let sub = resource.split_once('/').map(|(_, s)| s);
    verbs.iter().any(|v| v == "*" || v == verb)
        && groups.iter().any(|g| g == "*" || g == group)
        && resources.iter().any(|r| {
            r == "*"
                || r == resource
                || (sub.is_some() && *r == format!("{base}/*"))
                || (sub.is_some() && *r == format!("*/{}", sub.unwrap_or_default()))
        })
}

fn role_rules<'a>(snap: &'a Snapshot, binding: &Value) -> Option<(&'a Value, String)> {
    let kind = binding.pointer("/roleRef/kind").and_then(Value::as_str)?;
    let name = binding.pointer("/roleRef/name").and_then(Value::as_str)?;
    let ns = binding.pointer("/metadata/namespace").and_then(Value::as_str);
    let found = if kind == "ClusterRole" {
        snap.cluster_roles.iter().find(|r| r.pointer("/metadata/name").and_then(Value::as_str) == Some(name))
    } else {
        snap.roles.iter().find(|r| {
            r.pointer("/metadata/name").and_then(Value::as_str) == Some(name)
                && r.pointer("/metadata/namespace").and_then(Value::as_str) == ns
        })
    }?;
    Some((found, format!("{kind} {name}")))
}

fn binding_label(b: &Value) -> String {
    let name = b.pointer("/metadata/name").and_then(Value::as_str).unwrap_or("?");
    match b.pointer("/metadata/namespace").and_then(Value::as_str) {
        Some(ns) => format!("RoleBinding {ns}/{name}"),
        None => format!("ClusterRoleBinding {name}"),
    }
}

/// Every subject allowed `verb` on `group`/`resource` in `namespace` (None: cluster-wide only).
pub fn who_can(snap: &Snapshot, verb: &str, group: &str, resource: &str, namespace: Option<&str>) -> Vec<Grant> {
    let mut out = Vec::new();
    let bindings = snap.cluster_role_bindings.iter().chain(
        snap.role_bindings
            .iter()
            .filter(|b| namespace.is_some() && b.pointer("/metadata/namespace").and_then(Value::as_str) == namespace),
    );
    for b in bindings {
        let Some((role, role_label)) = role_rules(snap, b) else { continue };
        let matching: Vec<&Value> =
            role["rules"].as_array().into_iter().flatten().filter(|r| rule_allows(r, verb, group, resource)).collect();
        if matching.is_empty() {
            continue;
        }
        let names: Vec<String> = if matching.iter().any(|r| strs(r, "resourceNames").is_empty()) {
            Vec::new()
        } else {
            matching.iter().flat_map(|r| strs(r, "resourceNames")).collect()
        };
        for s in b["subjects"].as_array().into_iter().flatten() {
            out.push(Grant {
                subject_kind: s["kind"].as_str().unwrap_or("?").to_string(),
                subject: s["name"].as_str().unwrap_or("?").to_string(),
                subject_ns: s["namespace"].as_str().map(String::from),
                binding: binding_label(b),
                role: role_label.clone(),
                scope: b.pointer("/metadata/namespace").and_then(Value::as_str).map(String::from),
                names: names.clone(),
            });
        }
    }
    out.sort_by(|a, b| {
        (a.subject_kind.as_str(), a.subject.as_str()).cmp(&(b.subject_kind.as_str(), b.subject.as_str()))
    });
    out
}

/// Whether a binding subject covers a ServiceAccount (directly or by its groups).
pub fn names_service_account(subject: &Value, ns: &str, name: &str) -> bool {
    match subject["kind"].as_str() {
        Some("ServiceAccount") => subject["name"] == name && subject["namespace"].as_str() == Some(ns),
        Some("Group") => {
            matches!(subject["name"].as_str(), Some(g) if g == "system:serviceaccounts" || g == format!("system:serviceaccounts:{ns}") || g == "system:authenticated")
        }
        Some("User") => subject["name"].as_str() == Some(&format!("system:serviceaccount:{ns}:{name}")),
        _ => false,
    }
}

/// The bindings that apply to a ServiceAccount, with their role and its rules.
pub fn service_account_grants(
    snap: &Snapshot,
    ns: &str,
    name: &str,
) -> Vec<(String, String, Option<String>, Vec<Value>)> {
    let mut out = Vec::new();
    for b in snap.cluster_role_bindings.iter().chain(snap.role_bindings.iter()) {
        let applies = b["subjects"].as_array().into_iter().flatten().any(|s| names_service_account(s, ns, name));
        if !applies {
            continue;
        }
        let rules =
            role_rules(snap, b).map(|(r, _)| r["rules"].as_array().cloned().unwrap_or_default()).unwrap_or_default();
        let role = format!(
            "{} {}",
            b.pointer("/roleRef/kind").and_then(Value::as_str).unwrap_or("?"),
            b.pointer("/roleRef/name").and_then(Value::as_str).unwrap_or("?")
        );
        out.push((
            binding_label(b),
            role,
            b.pointer("/metadata/namespace").and_then(Value::as_str).map(String::from),
            rules,
        ));
    }
    out
}

/// Bindings that point at a role.
pub fn bindings_of(snap: &Snapshot, kind: &str, name: &str, ns: Option<&str>) -> Vec<Value> {
    snap.cluster_role_bindings
        .iter()
        .chain(snap.role_bindings.iter())
        .filter(|b| {
            b.pointer("/roleRef/kind").and_then(Value::as_str) == Some(kind)
                && b.pointer("/roleRef/name").and_then(Value::as_str) == Some(name)
                && (kind == "ClusterRole" || b.pointer("/metadata/namespace").and_then(Value::as_str) == ns)
        })
        .cloned()
        .collect()
}

/// "deployments.apps" / "pods/log" / "apps/deployments" → (group, resource).
pub fn split_resource(text: &str) -> (String, String) {
    let t = text.trim();
    // "apps/deployments", "cert-manager.io/certificates": group first.
    if let Some((g, r)) = t.split_once('/')
        && (g.contains('.') || matches!(g, "apps" | "batch" | "policy" | "autoscaling"))
    {
        return (g.to_string(), r.to_string());
    }
    let (res, sub) = match t.split_once('/') {
        Some((r, s)) => (r, Some(s)),
        None => (t, None),
    };
    let (base, group) = res.split_once('.').unwrap_or((res, ""));
    (group.to_string(), sub.map(|s| format!("{base}/{s}")).unwrap_or_else(|| base.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn snap() -> Snapshot {
        Snapshot {
            roles: vec![
                json!({"metadata": {"name": "pod-reader", "namespace": "demo"}, "rules": [{"apiGroups": [""], "resources": ["pods", "pods/log"], "verbs": ["get", "list"]}]}),
            ],
            cluster_roles: vec![
                json!({"metadata": {"name": "admin-ish"}, "rules": [{"apiGroups": ["*"], "resources": ["*"], "verbs": ["*"]}]}),
            ],
            role_bindings: vec![
                json!({"metadata": {"name": "rb", "namespace": "demo"}, "roleRef": {"kind": "Role", "name": "pod-reader"}, "subjects": [{"kind": "ServiceAccount", "name": "deployer", "namespace": "demo"}]}),
            ],
            cluster_role_bindings: vec![
                json!({"metadata": {"name": "ops"}, "roleRef": {"kind": "ClusterRole", "name": "admin-ish"}, "subjects": [{"kind": "Group", "name": "ops"}]}),
            ],
        }
    }

    #[test]
    fn who() {
        let s = snap();
        let g = who_can(&s, "get", "", "pods/log", Some("demo"));
        assert_eq!(g.len(), 2);
        assert!(g.iter().any(|x| x.subject == "deployer" && x.scope.as_deref() == Some("demo")));
        let g = who_can(&s, "delete", "", "pods", Some("demo"));
        assert_eq!(g.iter().map(|x| x.subject.as_str()).collect::<Vec<_>>(), ["ops"]);
        assert_eq!(service_account_grants(&s, "demo", "deployer").len(), 1);
        assert_eq!(bindings_of(&s, "Role", "pod-reader", Some("demo")).len(), 1);
        assert_eq!(split_resource("deployments.apps"), ("apps".into(), "deployments".into()));
        assert_eq!(split_resource("pods/log"), ("".into(), "pods/log".into()));
    }
}
