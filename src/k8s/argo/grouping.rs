//! Which Applications are one service across environments, and which are one Application
//! read twice (from a hub's Kubernetes API and from Argo CD's own API).

use std::collections::BTreeMap;

/// Words that name an environment in an Application's name, compared in lowercase, digits
/// after them ignored (`prod2`, `dev01`).
const ENVIRONMENTS: [&str; 26] = [
    "dev",
    "develop",
    "development",
    "test",
    "testing",
    "qa",
    "int",
    "integration",
    "uat",
    "acc",
    "acceptance",
    "stage",
    "staging",
    "stg",
    "preprod",
    "pre",
    "prod",
    "production",
    "prd",
    "sandbox",
    "demo",
    "preview",
    "canary",
    "perf",
    "rec",
    "hotfix",
];

/// Whether a word of a name names an environment.
fn is_environment(word: &str) -> bool {
    let word = word.to_ascii_lowercase();
    let bare = word.trim_end_matches(|c: char| c.is_ascii_digit());
    ENVIRONMENTS.contains(&bare)
}

/// The service an Application belongs to, across its environments: the value of the label
/// `label_key` when it has it, else its name without the environment in it. The name is cut
/// into words at `-`, `_` and `.`; the last word naming an environment that isn't the first
/// one ends the service (`shop-prod-eu` and `shop-staging` are `shop`), with environment
/// words right before it (`shop-pre-prod`); a name starting with one is the rest of it
/// (`prod-shop` is `shop`); a name without one is its own service.
pub fn service_of(name: &str, labels: &[(String, String)], label_key: &str) -> String {
    let key = label_key.trim();
    if !key.is_empty()
        && let Some((_, v)) = labels.iter().find(|(k, v)| k == key && !v.trim().is_empty())
    {
        return v.trim().to_string();
    }
    // Each word with where it starts.
    let mut words: Vec<(usize, &str)> = Vec::new();
    let mut start = 0;
    for (i, c) in name.char_indices() {
        if matches!(c, '-' | '_' | '.') {
            words.push((start, &name[start..i]));
            start = i + c.len_utf8();
        }
    }
    words.push((start, &name[start..]));
    if let Some(mut last) = (1..words.len()).rev().find(|&i| is_environment(words[i].1)) {
        while last > 1 && is_environment(words[last - 1].1) {
            last -= 1;
        }
        // The text before the separator of that word.
        let end = words[last].0.saturating_sub(1);
        return name[..end].to_string();
    }
    if words.len() > 1 && is_environment(words[0].1) && !words[1].1.is_empty() {
        return name[words[1].0..].to_string();
    }
    name.to_string()
}

/// The Applications of each service: the services sorted, each one's indexes in the order
/// given. `services[i]` is the service of Application `i`.
pub fn by_service(services: &[String]) -> Vec<(String, Vec<usize>)> {
    let mut groups: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, s) in services.iter().enumerate() {
        groups.entry(s.as_str()).or_default().push(i);
    }
    groups.into_iter().map(|(s, ix)| (s.to_string(), ix)).collect()
}

/// The Argo CD an Application is read from, the same whichever way it is read: a hub
/// context's Argo CD server when one is set for it (Settings → Per cluster), as the API's own
/// key is; else the hub context itself, which no API server then matches.
pub fn hub_instance(context: &str, servers: &BTreeMap<String, String>) -> String {
    servers
        .get(context)
        .and_then(|text| super::server::Server::parse(text).ok())
        .map_or_else(|| format!("context:{context}"), |s| server_instance(&s.base))
}

/// The Argo CD of an API server (its address, as `server_key` compares them).
pub fn server_instance(base: &str) -> String {
    super::server_key(base)
}

/// An Application, the same whichever way it was read: its Argo CD (see `hub_instance`),
/// namespace and name.
pub type AppKey = (String, String, String);

/// Where each Application of `second` goes once added after `first`: `Ok(i)` a row of its
/// own, the `i`-th added; `Err(j)` the same Application as `first[j]`, shown once (the
/// first's). Two of `second` with the same key are one too.
pub fn merge_plan(first: &[AppKey], second: &[AppKey]) -> Vec<Result<usize, usize>> {
    let mut seen: std::collections::HashMap<&AppKey, Result<usize, usize>> =
        first.iter().enumerate().map(|(j, k)| (k, Err(j))).collect();
    let mut added = 0;
    second
        .iter()
        .map(|k| {
            *seen.entry(k).or_insert_with(|| {
                added += 1;
                Ok(added - 1)
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{by_service, hub_instance, merge_plan, server_instance, service_of};
    use std::collections::BTreeMap;

    #[test]
    fn a_service_is_the_name_without_its_environment() {
        let svc = |name: &str| service_of(name, &[], "");
        assert_eq!(svc("shop-prod"), "shop");
        assert_eq!(svc("shop-staging"), "shop");
        assert_eq!(svc("shop-prod-eu-west-1"), "shop");
        assert_eq!(svc("shop-api-dev2"), "shop-api");
        assert_eq!(svc("shop_api.PROD"), "shop_api");
        assert_eq!(svc("shop-pre-prod"), "shop");
        assert_eq!(svc("prod-shop"), "shop");
        // No environment word: its own service.
        assert_eq!(svc("shop"), "shop");
        assert_eq!(svc("production-line"), "line");
        assert_eq!(svc("devtools"), "devtools");
        assert_eq!(svc("prod"), "prod");
        assert_eq!(svc("prod-"), "prod-");
        assert_eq!(svc(""), "");
    }

    #[test]
    fn a_label_chosen_names_the_service() {
        let labels = vec![("app.kubernetes.io/part-of".to_string(), "checkout".to_string())];
        assert_eq!(service_of("shop-prod", &labels, "app.kubernetes.io/part-of"), "checkout");
        // Without the label, or with it empty, the name says.
        assert_eq!(service_of("shop-prod", &labels, "team"), "shop");
        let empty = vec![("team".to_string(), " ".to_string())];
        assert_eq!(service_of("shop-prod", &empty, "team"), "shop");
        assert_eq!(service_of("shop-prod", &labels, ""), "shop");
    }

    #[test]
    fn services_group_their_applications() {
        let names = ["web-prod", "api-dev", "web-dev", "api-prod", "solo"];
        let services: Vec<String> = names.iter().map(|n| service_of(n, &[], "")).collect();
        assert_eq!(
            by_service(&services),
            [("api".to_string(), vec![1, 3]), ("solo".to_string(), vec![4]), ("web".to_string(), vec![0, 2])]
        );
    }

    #[test]
    fn one_application_read_from_a_hub_and_the_api_is_one() {
        let mut servers = BTreeMap::new();
        servers.insert("hub".to_string(), "https://ArgoCD.example.com:443/".to_string());
        let hub = hub_instance("hub", &servers);
        assert_eq!(hub, server_instance("https://argocd.example.com"));
        // A hub without its server set is its own instance, never an API server's.
        assert_eq!(hub_instance("other", &servers), "context:other");
        let key = |i: &str, ns: &str, n: &str| (i.to_string(), ns.to_string(), n.to_string());
        let first = [key(&hub, "argocd", "web"), key("context:other", "argocd", "web")];
        let second = [
            key(&hub, "argocd", "web"),
            key(&hub, "argocd", "api"),
            key(&hub, "team", "web"),
            key(&hub, "argocd", "api"),
        ];
        assert_eq!(merge_plan(&first, &second), [Err(0), Ok(0), Ok(1), Ok(0)]);
        assert_eq!(merge_plan(&[], &second), [Ok(0), Ok(1), Ok(2), Ok(1)]);
    }
}
