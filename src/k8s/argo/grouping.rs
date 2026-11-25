//! Which Argo CD an Application is read from: an API server, or a hub context's.

use std::collections::BTreeMap;

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

/// An Application: its Argo CD (`api::Endpoint::instance`), namespace and name.
pub type AppKey = (String, String, String);

#[cfg(test)]
mod tests {
    use super::{hub_instance, server_instance};
    use std::collections::BTreeMap;

    #[test]
    fn a_hub_with_its_server_set_is_that_server_s_argo_cd() {
        let mut servers = BTreeMap::new();
        servers.insert("hub".to_string(), "https://ArgoCD.example.com:443/".to_string());
        let hub = hub_instance("hub", &servers);
        assert_eq!(hub, server_instance("https://argocd.example.com"));
        // A hub without its server set is its own instance, never an API server's.
        assert_eq!(hub_instance("other", &servers), "context:other");
    }
}
