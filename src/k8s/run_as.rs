//! One write action run as another identity, like `kubectl --as`: for people whose own role
//! is limited and who may impersonate a stronger one for a single command.
//!
//! Only the UI does it, for the one action whose confirmation the user turned it on in, while
//! write mode is on, and only as the identity set for the context (Settings → Contexts); all
//! three are checked again when each request is sent ([`decide`]). Reads go as the user.
//! The agents' clients never do: nothing in the MCP server builds one.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use http::HeaderValue;
use kube::Client;
use kube::client::Body;
use serde::{Deserialize, Serialize};

/// Who to act as: a user, and groups when given (`--as`, `--as-group`).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Identity {
    pub user: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
}

impl Identity {
    /// The identity trimmed, or None when there is no user, or a part could not be sent as a
    /// header: never a different identity than the one written.
    pub fn usable(self) -> Option<Self> {
        let user = self.user.trim().to_string();
        let groups: Vec<String> = self.groups.iter().map(|g| g.trim().to_string()).filter(|g| !g.is_empty()).collect();
        let sendable = |s: &str| HeaderValue::from_str(s).is_ok();
        (!user.is_empty() && sendable(&user) && groups.iter().all(|g| sendable(g))).then_some(Self { user, groups })
    }

    /// `user`, then `(group, group)` when there are groups.
    pub fn label(&self) -> String {
        match self.groups.is_empty() {
            true => self.user.clone(),
            false => format!("{} ({})", self.user, self.groups.join(", ")),
        }
    }

    /// `--as user --as-group g…`, quoted for a shell.
    pub fn kubectl_flags(&self) -> String {
        let quote = |s: &str| crate::details::shell_word(s);
        let mut flags = format!("--as {}", quote(&self.user));
        for g in &self.groups {
            flags.push_str(&format!(" --as-group {}", quote(g)));
        }
        flags
    }
}

/// Why a request of an elevated action was not sent.
pub const RUN_AS_WRITES_OFF: &str = "write actions are off: nothing was sent as another identity";
pub const RUN_AS_CHANGED: &str =
    "the identity to run as changed in Settings → Contexts since the action was confirmed: nothing was sent";
pub const RUN_AS_NO_SHELLS: &str = "shells and port-forwards never run as another identity";

/// Who a request of one action is sent as: `Some(identity)` only when the user chose one for
/// this action (`chosen`), write mode is on, it is still the identity configured for the
/// context, and the request is not a read. Reads go as the user (`None`). A request that
/// would change the cluster while one of the others does not hold is refused (`Err`): sending
/// it as the user instead would not be what was confirmed.
pub fn decide<'a>(
    chosen: Option<&'a Identity>,
    writes_on: bool,
    configured: Option<&Identity>,
    method: &http::Method,
    path: &str,
) -> Result<Option<&'a Identity>, &'static str> {
    let Some(chosen) = chosen else { return Ok(None) };
    if ["/exec", "/attach", "/portforward"].iter().any(|s| path.ends_with(s)) {
        return Err(RUN_AS_NO_SHELLS);
    }
    if *method == http::Method::GET || *method == http::Method::HEAD {
        return Ok(None);
    }
    if !writes_on {
        return Err(RUN_AS_WRITES_OFF);
    }
    match configured {
        Some(c) if c == chosen && chosen.clone().usable().as_ref() == Some(chosen) => Ok(Some(chosen)),
        _ => Err(RUN_AS_CHANGED),
    }
}

/// A client for one action, which sends its writes through `client` as `identity` (see
/// [`decide`]). `client` keeps its own checks: the UI's write gate still applies.
pub fn run_as(client: &Client, context: &str, identity: Identity) -> Client {
    let service = RunAs { client: client.clone(), context: context.to_string(), identity };
    // The client's buffer starts a task on the runtime.
    let _runtime = super::RUNTIME.enter();
    Client::new(service, client.default_namespace().to_string())
}

#[derive(Clone)]
struct RunAs {
    client: Client,
    context: String,
    identity: Identity,
}

impl tower::Service<http::Request<Body>> for RunAs {
    type Response = http::Response<Body>;
    type Error = tower::BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut req: http::Request<Body>) -> Self::Future {
        let configured = crate::settings::run_as(&self.context);
        let writes_on = super::gate::ui_writes();
        match decide(Some(&self.identity), writes_on, configured.as_ref(), req.method(), req.uri().path()) {
            Err(why) => return Box::pin(async move { Err(why.into()) }),
            Ok(Some(identity)) => {
                let headers = req.headers_mut();
                // Checked sendable by `usable`, which `decide` requires.
                if let Ok(user) = HeaderValue::from_str(&identity.user) {
                    headers.insert("impersonate-user", user);
                }
                for g in &identity.groups {
                    if let Ok(group) = HeaderValue::from_str(g) {
                        headers.append("impersonate-group", group);
                    }
                }
            }
            Ok(None) => {}
        }
        let client = self.client.clone();
        Box::pin(async move { client.send(req).await.map_err(Into::into) })
    }
}

/// The API server's refusal when the user may not impersonate, said plainly.
pub fn impersonation_refused(message: &str) -> Option<String> {
    message.contains("cannot impersonate").then(|| {
        format!(
            "You may not run actions as another identity on this cluster (your role has no `impersonate` \
             permission for it): {message}"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::Method;
    use proptest::prelude::*;

    fn id(user: &str, groups: &[&str]) -> Identity {
        Identity { user: user.into(), groups: groups.iter().map(|g| g.to_string()).collect() }
    }

    #[test]
    fn an_identity_is_used_as_written_or_not_at_all() {
        assert_eq!(id(" admin ", &[" ", "ops"]).usable(), Some(id("admin", &["ops"])));
        assert_eq!(id("  ", &["ops"]).usable(), None);
        assert_eq!(id("admin\n", &[]).usable(), Some(id("admin", &[])));
        assert_eq!(id("ad\nmin", &[]).usable(), None);
        assert_eq!(id("admin", &["o\u{7f}ps"]).usable(), None);
        assert_eq!(id("super-admin", &["a b"]).kubectl_flags(), "--as super-admin --as-group 'a b'");
        assert_eq!(id("system:serviceaccount:x:y", &[]).label(), "system:serviceaccount:x:y");
    }

    #[test]
    fn only_the_writes_of_a_chosen_action_are_impersonated() {
        let admin = id("super-admin", &[]);
        let p = "/apis/rbac.authorization.k8s.io/v1/clusterroles/x";
        assert_eq!(decide(Some(&admin), true, Some(&admin), &Method::PATCH, p), Ok(Some(&admin)));
        // Not chosen, reads, and dry runs before the toggle exists: as the user.
        assert_eq!(decide(None, true, Some(&admin), &Method::PATCH, p), Ok(None));
        assert_eq!(decide(Some(&admin), true, Some(&admin), &Method::GET, p), Ok(None));
        // Write mode off, or the setting changed or removed since: refused.
        assert_eq!(decide(Some(&admin), false, Some(&admin), &Method::DELETE, p), Err(RUN_AS_WRITES_OFF));
        assert_eq!(decide(Some(&admin), true, Some(&id("other", &[])), &Method::DELETE, p), Err(RUN_AS_CHANGED));
        assert_eq!(decide(Some(&admin), true, None, &Method::POST, p), Err(RUN_AS_CHANGED));
        assert_eq!(
            decide(Some(&admin), true, Some(&admin), &Method::GET, "/api/v1/x/pods/y/exec"),
            Err(RUN_AS_NO_SHELLS)
        );
    }

    /// The agents' code never builds a client that runs as another identity, nor sets the
    /// headers itself: their rules can't reach it, whatever they allow.
    #[test]
    fn the_agents_never_run_as_another_identity() {
        fn files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    files(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push(path);
                }
            }
        }
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut all = vec![src.join("assistant.rs")];
        files(&src.join("mcp"), &mut all);
        files(&src.join("agents"), &mut all);
        assert!(all.len() > 3, "{all:?}");
        for path in all {
            let text = std::fs::read_to_string(&path).unwrap().to_lowercase();
            for word in ["run_as", "runas", "impersonat"] {
                assert!(!text.contains(word), "{} mentions {word}", path.display());
            }
        }
    }

    #[test]
    fn a_refused_impersonation_says_why() {
        let m = r#"users "super-admin" is forbidden: User "alice" cannot impersonate resource "users" in API group "" at the cluster scope"#;
        assert!(impersonation_refused(m).unwrap().contains("impersonate"));
        assert!(impersonation_refused("pods is forbidden").is_none());
    }

    fn identity() -> impl Strategy<Value = Identity> {
        ("[a-c ]{0,3}", proptest::collection::vec("[a-b ]{0,2}", 0..3))
            .prop_map(|(user, groups)| Identity { user, groups })
    }

    fn method() -> impl Strategy<Value = Method> {
        prop_oneof![
            Just(Method::GET),
            Just(Method::HEAD),
            Just(Method::POST),
            Just(Method::PUT),
            Just(Method::PATCH),
            Just(Method::DELETE)
        ]
    }

    proptest! {
        /// A request is impersonated only when the user chose it for that one action, write
        /// mode is on, the identity is the one configured for that context, and it is not a
        /// read; then as that identity exactly.
        #[test]
        fn impersonated_only_when_chosen_writes_on_and_configured(
            chosen in proptest::option::of(identity()),
            writes_on in any::<bool>(),
            configured in proptest::option::of(identity()),
            method in method(),
            path in prop_oneof![Just("/api/v1/namespaces/a/configmaps/b"), Just("/api/v1/namespaces/a/pods/b/exec"), Just("/api/v1/namespaces/a/pods/b/portforward")],
        ) {
            let got = decide(chosen.as_ref(), writes_on, configured.as_ref(), &method, path);
            let read = method == Method::GET || method == Method::HEAD;
            let stream = !path.ends_with("/b");
            let expected = chosen.is_some()
                && writes_on
                && !read
                && !stream
                && configured == chosen
                && chosen.clone().and_then(Identity::usable) == chosen;
            match got {
                Ok(Some(who)) => {
                    prop_assert!(expected);
                    prop_assert_eq!(Some(who), chosen.as_ref());
                }
                Ok(None) => prop_assert!(chosen.is_none() || (read && !stream)),
                Err(_) => prop_assert!(chosen.is_some() && !expected),
            }
            prop_assert_eq!(matches!(got, Ok(Some(_))), expected);
        }
    }
}
