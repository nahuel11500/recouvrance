//! How long API requests take, by cluster and by kind of request, for Settings → API requests:
//! every client goes through `MeterLayer`, which times each request until its response starts.

use std::collections::HashMap;
use std::future::Future;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::Result;

/// What a request does, from its method and path.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Class {
    Discovery,
    List,
    Watch,
    Metrics,
    Logs,
    /// Exec, attach and port-forward: a stream opened by an upgrade.
    Stream,
    Write,
}

impl Class {
    pub fn label(self) -> &'static str {
        match self {
            Self::Discovery => "Discovery",
            Self::List => "Lists and reads",
            Self::Watch => "Watches (opening)",
            Self::Metrics => "Metrics",
            Self::Logs => "Logs",
            Self::Stream => "Shells and forwards",
            Self::Write => "Writes",
        }
    }

    pub fn of(method: &http::Method, path: &str, query: &str) -> Self {
        if method != http::Method::GET {
            // Exec and port-forward upgrade with a GET or a POST.
            return if path.ends_with("/exec") || path.ends_with("/attach") || path.ends_with("/portforward") {
                Self::Stream
            } else {
                Self::Write
            };
        }
        if query.split('&').any(|p| p == "watch=true" || p == "watch=1") {
            return Self::Watch;
        }
        if path.starts_with("/apis/metrics.k8s.io/") {
            return Self::Metrics;
        }
        if path.ends_with("/log") {
            return Self::Logs;
        }
        if path.ends_with("/exec") || path.ends_with("/attach") || path.ends_with("/portforward") {
            return Self::Stream;
        }
        // "/api", "/api/v1", "/apis", "/apis/apps/v1": what the server offers.
        let parts = path.trim_matches('/').split('/').count();
        match path.split('/').nth(1) {
            Some("api") if parts <= 2 => Self::Discovery,
            Some("apis") if parts <= 3 => Self::Discovery,
            _ => Self::List,
        }
    }
}

/// Requests of one class on one cluster.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Stats {
    pub count: u64,
    pub errors: u64,
    pub total: Duration,
    pub slowest: Duration,
    pub last: Duration,
}

impl Stats {
    pub fn average(&self) -> Duration {
        if self.count == 0 { Duration::ZERO } else { self.total / self.count as u32 }
    }

    fn record(&mut self, took: Duration, failed: bool) {
        self.count += 1;
        self.errors += u64::from(failed);
        self.total += took;
        self.slowest = self.slowest.max(took);
        self.last = took;
    }
}

static STATS: LazyLock<Mutex<HashMap<(String, Class), Stats>>> = LazyLock::new(Default::default);

/// Every cluster's numbers so far, sorted by cluster then class.
pub fn snapshot() -> Vec<(String, Class, Stats)> {
    let stats = STATS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut out: Vec<_> = stats.iter().map(|((c, k), s)| (c.clone(), *k, s.clone())).collect();
    out.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    out
}

pub fn reset() {
    STATS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clear();
}

fn record(context: &str, class: Class, took: Duration, failed: bool) {
    let mut stats = STATS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    stats.entry((context.to_string(), class)).or_default().record(took, failed);
}

/// Times every request of a cluster's client until its response starts (headers): a watch
/// or a log stream counts as open there, not when it ends.
#[derive(Clone)]
pub struct MeterLayer {
    pub context: String,
}

impl<S> tower::Layer<S> for MeterLayer {
    type Service = Meter<S>;

    fn layer(&self, inner: S) -> Self::Service {
        Meter { inner, context: self.context.clone() }
    }
}

#[derive(Clone)]
pub struct Meter<S> {
    inner: S,
    context: String,
}

impl<S, B, R> tower::Service<http::Request<B>> for Meter<S>
where
    S: tower::Service<http::Request<B>, Response = http::Response<R>>,
    S::Future: Send + 'static,
    S::Error: 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = std::pin::Pin<Box<dyn Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let class = Class::of(req.method(), req.uri().path(), req.uri().query().unwrap_or_default());
        let context = self.context.clone();
        let start = Instant::now();
        let fut = self.inner.call(req);
        Box::pin(async move {
            let result = fut.await;
            let failed = match &result {
                Ok(resp) => resp.status().is_client_error() || resp.status().is_server_error(),
                Err(_) => true,
            };
            record(&context, class, start.elapsed(), failed);
            result
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Class;
    use http::Method;

    #[test]
    fn requests_are_classified_by_what_they_do() {
        let get = |p: &str, q: &str| Class::of(&Method::GET, p, q);
        assert_eq!(get("/api", ""), Class::Discovery);
        assert_eq!(get("/api/v1", ""), Class::Discovery);
        assert_eq!(get("/apis", ""), Class::Discovery);
        assert_eq!(get("/apis/apps/v1", ""), Class::Discovery);
        assert_eq!(get("/api/v1/pods", "limit=2000"), Class::List);
        assert_eq!(get("/apis/apps/v1/namespaces/a/deployments/web", ""), Class::List);
        assert_eq!(get("/api/v1/pods", "watch=true&resourceVersion=5"), Class::Watch);
        assert_eq!(get("/apis/metrics.k8s.io/v1beta1/pods", ""), Class::Metrics);
        assert_eq!(get("/api/v1/namespaces/a/pods/p/log", "follow=true"), Class::Logs);
        assert_eq!(get("/api/v1/namespaces/a/pods/p/exec", "command=sh"), Class::Stream);
        assert_eq!(Class::of(&Method::POST, "/api/v1/namespaces/a/pods/p/portforward", ""), Class::Stream);
        assert_eq!(Class::of(&Method::PATCH, "/apis/apps/v1/namespaces/a/deployments/web", ""), Class::Write);
    }
}
