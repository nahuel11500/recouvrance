//! Which Argo CD an Application is read from: an API server.

/// The Argo CD of an API server (its address, as `server_key` compares them).
pub fn server_instance(base: &str) -> String {
    super::server_key(base)
}

/// An Application: its Argo CD (`api::Endpoint::instance`), namespace and name.
pub type AppKey = (String, String, String);
