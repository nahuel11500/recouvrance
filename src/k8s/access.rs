//! What the current user may do (SelfSubject reviews) and RBAC listings.

#[allow(unused_imports)]
use super::*;

/// Whether the current user may do `verb` on this kind (and object), per the API server
/// (SelfSubjectAccessReview, like `kubectl auth can-i`). Errors count as allowed:
/// the server still decides when the action runs.
pub async fn can_i(
    client: Client,
    kind: Kind,
    verb: &str,
    ns: Option<String>,
    name: Option<String>,
    subresource: Option<&str>,
) -> bool {
    use k8s_openapi::api::authorization::v1::{
        ResourceAttributes, SelfSubjectAccessReview, SelfSubjectAccessReviewSpec,
    };
    let (verb, sub) = (verb.to_string(), subresource.map(String::from));
    run(async move {
        let review = SelfSubjectAccessReview {
            spec: SelfSubjectAccessReviewSpec {
                resource_attributes: Some(ResourceAttributes {
                    group: Some(kind.ar.group.clone()),
                    resource: Some(kind.ar.plural.clone()),
                    subresource: sub,
                    verb: Some(verb),
                    namespace: if kind.namespaced { ns } else { None },
                    name,
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let api: Api<SelfSubjectAccessReview> = Api::all(client);
        let r = api.create(&Default::default(), &review).await?;
        Ok(r.status.map(|s| s.allowed).unwrap_or(true))
    })
    .await
    .unwrap_or(true)
}

/// What the current user may do in a namespace (`kubectl auth can-i --list`):
/// (verbs, resources, resource names) per rule.
pub async fn my_rules(client: Client, ns: String) -> Result<Vec<(Vec<String>, Vec<String>, Vec<String>)>> {
    use k8s_openapi::api::authorization::v1::{SelfSubjectRulesReview, SelfSubjectRulesReviewSpec};
    run(async move {
        let review =
            SelfSubjectRulesReview { spec: SelfSubjectRulesReviewSpec { namespace: Some(ns) }, ..Default::default() };
        let api: Api<SelfSubjectRulesReview> = Api::all(client);
        let r = api.create(&Default::default(), &review).await?;
        let rules = r.status.map(|s| s.resource_rules).unwrap_or_default();
        Ok(rules
            .into_iter()
            .map(|rule| {
                let groups = rule.api_groups.unwrap_or_default();
                let resources = rule_resources(&groups, rule.resources.unwrap_or_default());
                (rule.verbs, resources, rule.resource_names.unwrap_or_default())
            })
            .collect())
    })
    .await
}

/// A rule's resources as `resource[.group]`: `pods` (core), `deployments.apps`,
/// `*` (every core resource), `*.*` (every resource in every group).
fn rule_resources(groups: &[String], resources: Vec<String>) -> Vec<String> {
    resources
        .into_iter()
        .flat_map(|res| {
            if groups.is_empty() {
                return vec![res];
            }
            groups.iter().map(|g| if g.is_empty() { res.clone() } else { format!("{res}.{g}") }).collect()
        })
        .collect()
}

/// Roles and bindings: those of `ns` (all namespaces when None) plus cluster-wide ones.
pub async fn rbac_snapshot(client: Client, ns: Option<String>) -> crate::rbac::Snapshot {
    use k8s_openapi::api::rbac::v1 as rbac;
    let (roles, cluster_roles, role_bindings, cluster_role_bindings) = futures::join!(
        list_values(client.clone(), ar_of::<rbac::Role>(), ns.clone(), None),
        list_values(client.clone(), ar_of::<rbac::ClusterRole>(), None, None),
        list_values(client.clone(), ar_of::<rbac::RoleBinding>(), ns, None),
        list_values(client, ar_of::<rbac::ClusterRoleBinding>(), None, None),
    );
    let mut errors = Vec::new();
    let mut keep = |list: &'static str, result: Result<Vec<serde_json::Value>>| {
        result.unwrap_or_else(|e| {
            errors.push((list, lists::load_error(&e)));
            Vec::new()
        })
    };
    let roles = keep("Roles", roles);
    let cluster_roles = keep("ClusterRoles", cluster_roles);
    let role_bindings = keep("RoleBindings", role_bindings);
    let cluster_role_bindings = keep("ClusterRoleBindings", cluster_role_bindings);
    crate::rbac::Snapshot { roles, cluster_roles, role_bindings, cluster_role_bindings, errors }
}

#[cfg(test)]
mod tests {
    use super::rule_resources;

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn core_and_all_groups_stay_apart() {
        assert_eq!(rule_resources(&strings(&[""]), strings(&["*"])), ["*"]);
        assert_eq!(rule_resources(&strings(&["*"]), strings(&["*"])), ["*.*"]);
        assert_eq!(rule_resources(&strings(&["", "apps"]), strings(&["*"])), ["*", "*.apps"]);
        assert_eq!(rule_resources(&strings(&["*"]), strings(&["pods", "*/scale"])), ["pods.*", "*/scale.*"]);
    }
}
