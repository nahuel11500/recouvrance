//! Sending a request again when the API server asks to be asked later.
//!
//! kube's own retry (its `default_retry`, turned off in `config::client`) sends a request
//! that got a 429, 503 or 504 again up to 15 times, waiting twice as long each time: about
//! three minutes in all. An aggregated API whose backend is down (metrics-server scaled to
//! 0, restarting, not ready) answers 503 at once, every time, and a page or an MCP tool
//! reading it waited those three minutes for an answer it already had (#742). Here, as
//! client-go does, a request is sent again only when the server said when (`Retry-After`:
//! API Priority and Fairness on a 429, an overloaded server on a 503); any other refusal
//! is returned at once, for the caller to say what failed.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use http::{HeaderMap, Request, Response, StatusCode};
use kube::client::Body;

/// The most times one request is sent again.
const MAX_RETRIES: u32 = 5;
/// The longest wait honored: a page waits on the answer.
const MAX_WAIT: Duration = Duration::from_secs(5);

/// How long to wait before sending a request again, after this answer: only for a 429 or a
/// 5xx that says when (`Retry-After` in seconds, up to `MAX_WAIT`). None: return the answer.
pub fn retry_after(status: StatusCode, headers: &HeaderMap) -> Option<Duration> {
    if status != StatusCode::TOO_MANY_REQUESTS && !status.is_server_error() {
        return None;
    }
    let secs: u64 = headers.get(http::header::RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()?;
    Some(Duration::from_secs(secs).min(MAX_WAIT))
}

/// A copy of a request to send again; None for a streamed body, which can be sent once.
fn copy(req: &Request<Body>) -> Option<Request<Body>> {
    let body = req.body().try_clone()?;
    let mut copy = Request::new(body);
    *copy.method_mut() = req.method().clone();
    *copy.uri_mut() = req.uri().clone();
    *copy.version_mut() = req.version();
    *copy.headers_mut() = req.headers().clone();
    *copy.extensions_mut() = req.extensions().clone();
    Some(copy)
}

/// Sends a request again while the server answers [`retry_after`].
#[derive(Clone)]
pub struct RetryLayer;

impl<S> tower::Layer<S> for RetryLayer {
    type Service = Retry<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Retry { inner: Arc::new(tokio::sync::Mutex::new(inner)) }
    }
}

/// The client's stack is boxed (not `Clone`): each attempt takes it in turn, only to send
/// (readiness and `call`), not while waiting for the answer.
pub struct Retry<S> {
    inner: Arc<tokio::sync::Mutex<S>>,
}

impl<S, B> tower::Service<Request<Body>> for Retry<S>
where
    S: tower::Service<Request<Body>, Response = Response<B>> + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
    B: Send + 'static,
{
    type Response = Response<B>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Response<B>, S::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // The inner service's readiness is awaited by each attempt.
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let inner = self.inner.clone();
        Box::pin(async move {
            let mut req = req;
            let mut retries = 0;
            loop {
                let again = if retries < MAX_RETRIES { copy(&req) } else { None };
                let sent = {
                    let mut service = inner.lock().await;
                    std::future::poll_fn(|cx| service.poll_ready(cx)).await?;
                    service.call(req)
                };
                let res = sent.await?;
                let (Some(again), Some(wait)) = (again, retry_after(res.status(), res.headers())) else {
                    return Ok(res);
                };
                drop(res);
                tokio::time::sleep(wait).await;
                retries += 1;
                req = again;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use http::{HeaderMap, HeaderValue, Request, Response, StatusCode};
    use kube::client::Body;
    use tower::{Layer as _, Service as _};

    use super::*;

    fn headers(retry_after: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = retry_after {
            h.insert(http::header::RETRY_AFTER, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn only_a_server_that_says_when_is_asked_again() {
        // An aggregated API whose backend is down: 503 without Retry-After.
        assert_eq!(retry_after(StatusCode::SERVICE_UNAVAILABLE, &headers(None)), None);
        assert_eq!(retry_after(StatusCode::GATEWAY_TIMEOUT, &headers(None)), None);
        assert_eq!(retry_after(StatusCode::TOO_MANY_REQUESTS, &headers(None)), None);
        // API Priority and Fairness, an overloaded server.
        assert_eq!(retry_after(StatusCode::TOO_MANY_REQUESTS, &headers(Some("1"))), Some(Duration::from_secs(1)));
        assert_eq!(retry_after(StatusCode::SERVICE_UNAVAILABLE, &headers(Some("2"))), Some(Duration::from_secs(2)));
        // A long wait is cut short; a date or garbage is not a wait.
        assert_eq!(retry_after(StatusCode::TOO_MANY_REQUESTS, &headers(Some("600"))), Some(MAX_WAIT));
        assert_eq!(retry_after(StatusCode::TOO_MANY_REQUESTS, &headers(Some("Wed, 21 Oct 2015 07:28:00 GMT"))), None);
        // Not a refusal to wait out.
        assert_eq!(retry_after(StatusCode::OK, &headers(Some("1"))), None);
        assert_eq!(retry_after(StatusCode::FORBIDDEN, &headers(Some("1"))), None);
    }

    /// Answers with `statuses` in turn (then 200), counting the requests.
    #[derive(Clone)]
    struct Server {
        statuses: Vec<(StatusCode, Option<&'static str>)>,
        calls: Arc<AtomicU32>,
    }

    impl tower::Service<Request<Body>> for Server {
        type Response = Response<()>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Response<()>, Self::Error>>;

        fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn call(&mut self, _req: Request<Body>) -> Self::Future {
            let n = self.calls.fetch_add(1, Ordering::SeqCst) as usize;
            let (status, after) = self.statuses.get(n).copied().unwrap_or((StatusCode::OK, None));
            let mut res = Response::new(());
            *res.status_mut() = status;
            *res.headers_mut() = headers(after);
            std::future::ready(Ok(res))
        }
    }

    async fn send(statuses: Vec<(StatusCode, Option<&'static str>)>) -> (StatusCode, u32) {
        let calls = Arc::new(AtomicU32::new(0));
        let mut svc = RetryLayer.layer(Server { statuses, calls: calls.clone() });
        let req = Request::get("/apis/metrics.k8s.io/v1beta1/nodes").body(Body::empty()).unwrap();
        let res = svc.call(req).await.unwrap();
        (res.status(), calls.load(Ordering::SeqCst))
    }

    /// metrics-server down: the 503 comes back at once, sent once (#742).
    #[tokio::test]
    async fn an_unavailable_api_fails_at_once() {
        let started = std::time::Instant::now();
        let all_503 = vec![(StatusCode::SERVICE_UNAVAILABLE, None); 20];
        assert_eq!(send(all_503).await, (StatusCode::SERVICE_UNAVAILABLE, 1));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_request_told_to_wait_is_sent_again() {
        let throttled = vec![(StatusCode::TOO_MANY_REQUESTS, Some("0")), (StatusCode::TOO_MANY_REQUESTS, Some("0"))];
        assert_eq!(send(throttled).await, (StatusCode::OK, 3));
        // Not forever.
        let always = vec![(StatusCode::TOO_MANY_REQUESTS, Some("0")); 20];
        assert_eq!(send(always).await, (StatusCode::TOO_MANY_REQUESTS, MAX_RETRIES + 1));
    }
}
