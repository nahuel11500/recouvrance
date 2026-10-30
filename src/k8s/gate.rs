//! Write mode, checked when a request is sent rather than when its dialog opened.
//!
//! The UI asks for write mode before a write, but a dialog, a dry run or a file picker can
//! sit between that check and the request; turning write mode off meanwhile must still stop
//! it. Every request of the UI's clients passes through [`WriteGate`], which refuses what
//! changes the cluster (or opens a shell in it) while write mode is off. The agents' clients
//! have no gate: their own rules decide, write mode is the UI's.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

/// Mirrors the UI's write mode; off until the user turns it on.
static UI_WRITES: AtomicBool = AtomicBool::new(false);

pub fn set_ui_writes(on: bool) {
    UI_WRITES.store(on, Ordering::SeqCst);
}

pub fn ui_writes() -> bool {
    UI_WRITES.load(Ordering::SeqCst)
}

/// The refusal, worded for the error shown where the write was started.
pub const WRITES_OFF: &str = "write actions are off: turn them on to change the cluster";

/// Whether a request changes the cluster or opens a shell in it, and so needs write mode.
/// Reads, watches, logs and port-forwards don't; neither do server-side dry runs, the
/// permission checks (`SelfSubjectAccessReview`, `SelfSubjectRulesReview`, who am I) and
/// the removal of a node shell pod or of a copy of a pod (`pod_copy`), which must go even
/// after write mode is turned off.
pub fn needs_writes(method: &http::Method, path: &str, query: &str) -> bool {
    let stream = path.ends_with("/exec") || path.ends_with("/attach");
    if *method == http::Method::GET {
        return stream;
    }
    if path.ends_with("/portforward") {
        return false;
    }
    if query.split('&').any(|p| p == "dryRun=All") {
        return false;
    }
    let checks = ["/apis/authorization.k8s.io/", "/apis/authentication.k8s.io/"];
    if *method == http::Method::POST && checks.iter().any(|c| path.starts_with(c)) && !path.contains("/tokenreviews") {
        return false;
    }
    let node_shell_cleanup = *method == http::Method::DELETE
        && path.starts_with(&format!("/api/v1/namespaces/{}/pods/recouvrance-node-shell-", crate::k8s::NODE_SHELL_NS));
    let copy_cleanup = *method == http::Method::DELETE && is_pod_copy(path);
    !node_shell_cleanup && !copy_cleanup
}

/// Whether `path` is a copy of a pod made to read its files, in any namespace.
fn is_pod_copy(path: &str) -> bool {
    let Some(rest) = path.strip_prefix("/api/v1/namespaces/") else { return false };
    match rest.split('/').collect::<Vec<_>>().as_slice() {
        [ns, "pods", name] => !ns.is_empty() && name.starts_with(crate::k8s::pod_copy::NAME_PREFIX),
        _ => false,
    }
}

#[derive(Clone, Copy)]
pub struct WriteGateLayer;

impl<S> tower::Layer<S> for WriteGateLayer {
    type Service = WriteGate<S>;

    fn layer(&self, inner: S) -> Self::Service {
        WriteGate { inner }
    }
}

#[derive(Clone)]
pub struct WriteGate<S> {
    inner: S,
}

impl<S, B, R> tower::Service<http::Request<B>> for WriteGate<S>
where
    S: tower::Service<http::Request<B>, Response = http::Response<R>>,
    S::Future: Send + 'static,
    S::Error: Into<tower::BoxError>,
{
    type Response = S::Response;
    type Error = tower::BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<S::Response, tower::BoxError>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let uri = req.uri();
        if !ui_writes() && needs_writes(req.method(), uri.path(), uri.query().unwrap_or_default()) {
            return Box::pin(async { Err(WRITES_OFF.into()) });
        }
        let fut = self.inner.call(req);
        Box::pin(async move { fut.await.map_err(Into::into) })
    }
}

#[cfg(test)]
mod tests {
    use super::needs_writes;
    use http::Method;

    #[test]
    fn writes_and_shells_need_write_mode_reads_do_not() {
        let p = "/api/v1/namespaces/shop/pods/web-1";
        assert!(!needs_writes(&Method::GET, p, ""));
        assert!(!needs_writes(&Method::GET, "/api/v1/pods", "watch=true"));
        assert!(!needs_writes(&Method::GET, &format!("{p}/log"), "follow=true"));
        assert!(needs_writes(&Method::DELETE, p, ""));
        assert!(needs_writes(&Method::PATCH, p, ""));
        assert!(needs_writes(&Method::PUT, p, ""));
        assert!(needs_writes(&Method::POST, "/api/v1/namespaces/shop/pods", ""));
        assert!(needs_writes(&Method::POST, &format!("{p}/eviction"), ""));
        // A shell opens with a GET (websocket upgrade) or a POST.
        assert!(needs_writes(&Method::GET, &format!("{p}/exec"), "command=sh"));
        assert!(needs_writes(&Method::POST, &format!("{p}/attach"), ""));
        // Port-forwards, dry runs and permission checks are not writes.
        assert!(!needs_writes(&Method::GET, &format!("{p}/portforward"), ""));
        assert!(!needs_writes(&Method::POST, &format!("{p}/portforward"), ""));
        assert!(!needs_writes(&Method::PATCH, p, "dryRun=All&fieldManager=recouvrance"));
        assert!(!needs_writes(&Method::POST, "/apis/authorization.k8s.io/v1/selfsubjectaccessreviews", ""));
        assert!(!needs_writes(&Method::POST, "/apis/authorization.k8s.io/v1/selfsubjectrulesreviews", ""));
        assert!(!needs_writes(&Method::POST, "/apis/authentication.k8s.io/v1/selfsubjectreviews", ""));
        // A ServiceAccount token is a write; removing a node shell pod is not.
        assert!(needs_writes(&Method::POST, "/api/v1/namespaces/shop/serviceaccounts/ci/token", ""));
        let shell = format!("/api/v1/namespaces/{}/pods/recouvrance-node-shell-abc123", crate::k8s::NODE_SHELL_NS);
        assert!(!needs_writes(&Method::DELETE, &shell, ""));
        assert!(needs_writes(&Method::DELETE, "/api/v1/namespaces/shop/pods/recouvrance-node-shell-abc123", ""));
        // Nor is removing a copy of a pod, in any namespace; only a pod, and only removing it.
        let copy = "/api/v1/namespaces/shop/pods/recouvrance-copy-abc123-web-1";
        assert!(!needs_writes(&Method::DELETE, copy, ""));
        assert!(needs_writes(&Method::POST, "/api/v1/namespaces/shop/pods", ""));
        assert!(needs_writes(&Method::PATCH, copy, ""));
        assert!(needs_writes(&Method::GET, &format!("{copy}/exec"), "command=sh"));
        assert!(needs_writes(&Method::DELETE, &format!("{copy}/ephemeralcontainers"), ""));
        assert!(needs_writes(&Method::DELETE, "/api/v1/namespaces/shop/configmaps/recouvrance-copy-abc123", ""));
        assert!(needs_writes(&Method::DELETE, "/api/v1/namespaces/shop/pods/web-recouvrance-copy-1", ""));
        assert!(needs_writes(&Method::DELETE, "/api/v1/namespaces/shop/pods", "labelSelector=recouvrance-copy-x"));
    }
}
