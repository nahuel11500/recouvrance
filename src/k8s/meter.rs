//! How long API requests take, by cluster and by kind of request, for Settings → API requests:
//! every client goes through `MeterLayer`, which times each request until its response starts.

use std::collections::{HashMap, VecDeque};
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

/// How many minutes of history each cluster keeps for the charts.
pub const WINDOW: i64 = 60;

/// Latency buckets for the percentiles: bucket `i` holds what took at most `BOUNDS[i]` and
/// more than the bucket before. Each bound is 1.25 times the one before, from 1 ms (21
/// minutes at the last one; the last bucket also takes anything longer), so a percentile
/// read from them is at most 25% above the real one.
const BUCKETS: usize = 64;

static BOUNDS: LazyLock<[u64; BUCKETS]> = LazyLock::new(|| {
    let mut b = [0u64; BUCKETS];
    let mut ns = 1_000_000f64;
    for x in &mut b {
        *x = ns.ceil() as u64;
        ns *= 1.25;
    }
    b
});

fn bucket(took: Duration) -> usize {
    let ns = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
    BOUNDS.partition_point(|&b| b < ns).min(BUCKETS - 1)
}

/// The requests of one minute.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Minute {
    pub count: u64,
    pub errors: u64,
    pub total: Duration,
}

impl Minute {
    /// The average time to answer, none in a minute without requests: no request is not a
    /// fast one.
    pub fn average(&self) -> Option<Duration> {
        (self.count > 0).then(|| average(self.total, self.count))
    }

    fn add(&mut self, other: &Self) {
        self.count += other.count;
        self.errors += other.errors;
        self.total += other.total;
    }
}

fn average(total: Duration, count: u64) -> Duration {
    if count == 0 { Duration::ZERO } else { Duration::from_nanos((total.as_nanos() / u128::from(count)) as u64) }
}

/// Requests of one class on one cluster.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Stats {
    pub count: u64,
    pub errors: u64,
    pub total: Duration,
    pub slowest: Duration,
    pub last: Duration,
    /// How many requests fell in each latency bucket (empty until the first one).
    buckets: Vec<u64>,
    /// The last `WINDOW` minutes that had requests, oldest first, by Unix minute.
    recent: VecDeque<(i64, Minute)>,
}

impl Stats {
    /// The latency under which a share `q` (0 to 1) of the requests answered: at most 25%
    /// above the real one, and never above the slowest.
    pub fn percentile(&self, q: f64) -> Duration {
        if self.count == 0 {
            return Duration::ZERO;
        }
        // The rank of the request at `q`, from 1: the median of 3 is the 2nd.
        let rank = ((q * self.count as f64).ceil() as u64).clamp(1, self.count);
        let mut seen = 0;
        for (i, n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= rank {
                return if i == BUCKETS - 1 { self.slowest } else { Duration::from_nanos(BOUNDS[i]).min(self.slowest) };
            }
        }
        self.slowest
    }

    fn record(&mut self, took: Duration, failed: bool, minute: i64) {
        self.count += 1;
        self.errors += u64::from(failed);
        self.total += took;
        self.slowest = self.slowest.max(took);
        self.last = took;
        if self.buckets.is_empty() {
            self.buckets = vec![0; BUCKETS];
        }
        self.buckets[bucket(took)] += 1;
        let one = Minute { count: 1, errors: u64::from(failed), total: took };
        match self.recent.back_mut() {
            // A clock set back lands in the latest minute rather than out of order.
            Some((m, s)) if *m >= minute => s.add(&one),
            _ => self.recent.push_back((minute, one)),
        }
        while self.recent.front().is_some_and(|(m, _)| *m <= minute - WINDOW) {
            self.recent.pop_front();
        }
    }
}

/// The requests of `stats` added up minute by minute, oldest first, over the minutes up to
/// `now` since the first of them had a request (at most `WINDOW`, at least 2), with the
/// quiet minutes at zero. Each point is (Unix minute, requests).
pub fn per_minute<'a>(stats: impl IntoIterator<Item = &'a Stats>, now: i64) -> Vec<(i64, Minute)> {
    let first = now - WINDOW + 1;
    let mut sums = vec![Minute::default(); WINDOW as usize];
    let mut start = now;
    for s in stats {
        for (m, minute) in &s.recent {
            if (first..=now).contains(m) {
                sums[(m - first) as usize].add(minute);
                start = start.min(*m);
            }
        }
    }
    let start = start.min(now - 1).max(first);
    sums.into_iter().enumerate().map(|(i, s)| (first + i as i64, s)).filter(|(m, _)| *m >= start).collect()
}

/// The current Unix minute.
pub fn now_minute() -> i64 {
    jiff::Timestamp::now().as_second().div_euclid(60)
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
    let minute = now_minute();
    let mut stats = STATS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    stats.entry((context.to_string(), class)).or_default().record(took, failed, minute);
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
    use super::{Class, Duration, Stats, WINDOW, per_minute};
    use http::Method;
    use proptest::prelude::*;

    proptest! {
        /// A percentile is never below the real one, and at most 25% above it (1 ms at the
        /// least, the first bucket's bound).
        #[test]
        fn percentiles_are_close_above(
            micros in proptest::collection::vec(1u64..1_000_000_000, 1..200),
            q in 0.0f64..=1.0,
        ) {
            let mut s = Stats::default();
            for &m in &micros {
                s.record(Duration::from_micros(m), false, 0);
            }
            let mut sorted = micros;
            sorted.sort_unstable();
            let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
            let real = Duration::from_micros(sorted[rank - 1]);
            let got = s.percentile(q);
            prop_assert!(got >= real, "{got:?} < {real:?}");
            prop_assert!(got <= (real.mul_f64(1.25) + Duration::from_nanos(1)).max(Duration::from_millis(1)), "{got:?} for {real:?}");
            prop_assert!(got <= s.slowest);
        }

        /// The minutes add up to every request of the window, whatever the clusters and
        /// classes, and run one per minute up to now.
        #[test]
        fn minutes_add_up(
            calls in proptest::collection::vec((0usize..3, 0i64..200, any::<bool>()), 0..300),
        ) {
            let mut stats = vec![Stats::default(); 3];
            let mut now = 0;
            for &(i, at, failed) in &calls {
                now = now.max(at);
                stats[i].record(Duration::from_millis(5), failed, now);
            }
            let points = per_minute(&stats, now);
            let in_window = |at: i64| at > now - WINDOW;
            let mut at = 0;
            let (mut count, mut errors) = (0, 0);
            for &(_, t, failed) in &calls {
                at = at.max(t);
                if in_window(at) {
                    count += 1;
                    errors += u64::from(failed);
                }
            }
            prop_assert_eq!(points.iter().map(|p| p.1.count).sum::<u64>(), count);
            prop_assert_eq!(points.iter().map(|p| p.1.errors).sum::<u64>(), errors);
            prop_assert!(points.len() >= 2 && points.len() <= WINDOW as usize);
            prop_assert_eq!(points.last().map(|p| p.0), Some(now));
            prop_assert!(points.windows(2).all(|w| w[1].0 == w[0].0 + 1));
        }
    }

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
