//! How long API requests take and how much they bring, by cluster and by kind of request, for
//! Settings → API requests: every client goes through `MeterLayer`, which times each request
//! until its response starts, then counts its body's bytes as they arrive and times it to its
//! last byte (`Counted`).

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use anyhow::Result;
use bytes::Buf as _;
use hyper::body::{Body as HttpBody, Frame, SizeHint};

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
            Self::Watch => "Watches",
            Self::Metrics => "Metrics",
            Self::Logs => "Logs",
            Self::Stream => "Shells and forwards",
            Self::Write => "Writes",
        }
    }

    /// Whether its answers end by themselves. A watch, a log stream or a shell runs until it
    /// is closed, so its time to the last byte says nothing.
    pub fn ends(self) -> bool {
        !matches!(self, Self::Watch | Self::Logs | Self::Stream)
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

/// How long requests took, in latency buckets for the percentiles.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Latency {
    pub slowest: Duration,
    /// How many requests fell in each bucket (empty until the first one).
    buckets: Vec<u64>,
}

impl Latency {
    fn record(&mut self, took: Duration) {
        self.slowest = self.slowest.max(took);
        if self.buckets.is_empty() {
            self.buckets = vec![0; BUCKETS];
        }
        self.buckets[bucket(took)] += 1;
    }

    /// How many requests it holds.
    pub fn count(&self) -> u64 {
        self.buckets.iter().sum()
    }

    /// The latency under which a share `q` (0 to 1) of the requests answered, none without
    /// requests: at most 25% above the real one, and never above the slowest.
    pub fn percentile(&self, q: f64) -> Option<Duration> {
        let count = self.count();
        if count == 0 {
            return None;
        }
        // The rank of the request at `q`, from 1: the median of 3 is the 2nd.
        let rank = ((q * count as f64).ceil() as u64).clamp(1, count);
        let mut seen = 0;
        for (i, n) in self.buckets.iter().enumerate() {
            seen += n;
            if seen >= rank {
                return Some(if i == BUCKETS - 1 {
                    self.slowest
                } else {
                    Duration::from_nanos(BOUNDS[i]).min(self.slowest)
                });
            }
        }
        Some(self.slowest)
    }
}

/// The requests of one minute, and the bytes received in it.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Minute {
    pub count: u64,
    pub errors: u64,
    pub total: Duration,
    /// Bytes received in that minute, by requests sent then or before (a watch's events).
    pub bytes: u64,
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
        self.bytes += other.bytes;
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
    /// Time to the response headers, of every request.
    pub headers: Latency,
    /// Time to the last byte, of the answers read to their end: none for a class whose
    /// answers don't end (`Class::ends`). Filled in by `snapshot`.
    pub last_byte: Latency,
    /// Bytes received, after decompression. Filled in by `snapshot`.
    pub bytes: u64,
    /// The last `WINDOW` minutes that had requests (in a snapshot, or bytes), oldest first, by
    /// Unix minute.
    recent: VecDeque<(i64, Minute)>,
}

impl Stats {
    fn record(&mut self, took: Duration, failed: bool, minute: i64) {
        self.count += 1;
        self.errors += u64::from(failed);
        self.total += took;
        self.headers.record(took);
        let one = Minute { count: 1, errors: u64::from(failed), total: took, bytes: 0 };
        match self.recent.back_mut() {
            // A clock set back lands in the latest minute rather than out of order.
            Some((m, s)) if *m >= minute => s.add(&one),
            _ => self.recent.push_back((minute, one)),
        }
        while self.recent.front().is_some_and(|(m, _)| *m <= minute - WINDOW) {
            self.recent.pop_front();
        }
    }

    /// Adds the bytes received minute by minute, as (Unix minute, bytes): in a minute without
    /// requests too (a watch's events).
    fn add_bytes(&mut self, minutes: impl IntoIterator<Item = (i64, u64)>) {
        for (minute, bytes) in minutes {
            match self.recent.binary_search_by_key(&minute, |(m, _)| *m) {
                Ok(i) => self.recent[i].1.bytes += bytes,
                Err(i) => self.recent.insert(i, (minute, Minute { bytes, ..Minute::default() })),
            }
        }
    }

    /// Bytes received per minute over the last `WINDOW` minutes up to `now`, counted from the
    /// first of them with requests or bytes (the current one included): 0 without any.
    pub fn bytes_per_minute(&self, now: i64) -> u64 {
        let window = now - WINDOW + 1..=now;
        let mut first = None;
        let mut bytes = 0;
        for (m, minute) in self.recent.iter().filter(|(m, _)| window.contains(m)) {
            first.get_or_insert(*m);
            bytes += minute.bytes;
        }
        first.map_or(0, |first| bytes / (now - first + 1) as u64)
    }
}

/// The requests of `stats` added up minute by minute, oldest first, over the minutes up to
/// `now` since the first of them had a request or bytes (at most `WINDOW`, at least 2), with
/// the quiet minutes at zero. Each point is (Unix minute, requests and bytes).
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

/// How many of the heaviest requests are listed.
pub const HEAVIEST: usize = 10;

/// The heaviest request of one kind: one cluster, class, method and path with its names
/// replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Heavy {
    pub context: String,
    pub class: Class,
    pub method: http::Method,
    /// The path, its names replaced (`path_pattern`).
    pub path: String,
    /// Bytes received, after decompression.
    pub bytes: u64,
    /// Time to the response headers.
    pub headers: Duration,
    /// Time to the last byte: none for a stream, or an answer given up or cut before its end.
    pub last_byte: Option<Duration>,
}

impl Heavy {
    fn same_kind(&self, other: &Self) -> bool {
        self.context == other.context
            && self.class == other.class
            && self.method == other.method
            && self.path == other.path
    }
}

/// Puts `heavy` in `list` (heaviest first, at most `HEAVIEST`, one request a kind) when it is
/// at least as heavy as the one of its kind there, or as the lightest when its kind isn't:
/// of two as heavy, the latest is shown.
fn list_heavy(list: &mut Vec<Heavy>, heavy: Heavy) {
    if let Some(i) = list.iter().position(|h| h.same_kind(&heavy)) {
        if list[i].bytes > heavy.bytes {
            return;
        }
        list.remove(i);
    }
    let at = list.partition_point(|h| h.bytes > heavy.bytes);
    list.insert(at, heavy);
    list.truncate(HEAVIEST);
}

/// A request's path with the names in it replaced, so that requests to different objects of
/// one kind read the same and none is named: `{namespace}` for the namespace, `{name}` for
/// the object (`/api/v1/namespaces/{namespace}/pods/{name}/log`).
pub fn path_pattern(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    // "/api/v1/…" and "/apis/<group>/<version>/…": resources and their names follow. Other
    // paths ("/version", "/openapi/v3/…") name no object.
    let prefix = match parts.first() {
        Some(&"api") => 2,
        Some(&"apis") => 3,
        _ => return path.to_string(),
    };
    let Some(mut rest) = parts.get(prefix..) else { return path.to_string() };
    let mut out = parts[..prefix].to_vec();
    // The old `/watch/` prefix, before what it watches.
    if let Some((&"watch", after)) = rest.split_first() {
        out.push("watch");
        rest = after;
    }
    if let ["namespaces", _, after @ ..] = rest {
        out.push("namespaces");
        // `/namespaces/<name>`, with its `status` or `finalize`, is the namespace itself.
        if after.is_empty() || matches!(after, ["status" | "finalize"]) {
            out.push("{name}");
            out.extend(after);
            return format!("/{}", out.join("/"));
        }
        out.push("{namespace}");
        rest = after;
    }
    // A resource, then an object's name, then its subresource (and a proxied path).
    if let [resource, _, after @ ..] = rest {
        out.push(*resource);
        out.push("{name}");
        out.extend(after);
    } else {
        out.extend(rest);
    }
    format!("/{}", out.join("/"))
}

/// What a body's end is recorded with: its request, and how long the headers took.
struct Sent {
    method: http::Method,
    path: String,
    start: Instant,
    headers: Duration,
}

/// How one cluster's answers of one class ended.
#[derive(Default)]
struct Ends {
    /// Time to the last byte of the ones read to their end.
    last_byte: Latency,
    /// The heaviest of each kind, heaviest first (`list_heavy`).
    heaviest: Vec<Heavy>,
}

/// Bits of a minute's slot that hold its bytes (up to a terabyte a minute). The minute is in
/// the bits above, wrapped: a slot is only read for a minute of the window, and its minute
/// only compared with that one.
const BYTE_BITS: u32 = 40;
const BYTE_MASK: u64 = (1 << BYTE_BITS) - 1;

/// Unix minute `minute` in a slot's high bits.
fn tag(minute: i64) -> u64 {
    (minute as u64) << BYTE_BITS
}

/// One cluster's answers of one class after their headers: the bytes their bodies add as
/// they arrive, and how each ended. A body only adds to atomics, without a lock or an
/// allocation; `ends` is locked once a request, when it ends.
struct Flow {
    context: String,
    class: Class,
    /// Since Recouvrance started, or the last reset.
    total: AtomicU64,
    /// The bytes of the last `WINDOW` minutes: slot `m % WINDOW` holds minute `m` (`tag`)
    /// and the bytes received in it, in its low `BYTE_BITS`.
    minutes: [AtomicU64; WINDOW as usize],
    ends: Mutex<Ends>,
}

impl Flow {
    fn new(context: String, class: Class) -> Self {
        Self {
            context,
            class,
            total: AtomicU64::new(0),
            minutes: [const { AtomicU64::new(0) }; WINDOW as usize],
            ends: Mutex::default(),
        }
    }

    /// Adds `n` bytes received in Unix minute `minute`.
    fn add(&self, minute: i64, n: u64) {
        self.total.fetch_add(n, Ordering::Relaxed);
        let tag = tag(minute);
        let slot = &self.minutes[minute.rem_euclid(WINDOW) as usize];
        // A slot that still holds an older minute starts again from these bytes. `update` is a
        // compare-and-swap loop: bytes added at once from two threads all count.
        slot.update(Ordering::Relaxed, Ordering::Relaxed, |held| {
            let bytes = if (held & !BYTE_MASK) == tag { held & BYTE_MASK } else { 0 };
            tag | bytes.saturating_add(n).min(BYTE_MASK)
        });
    }

    /// The bytes of each of the `WINDOW` minutes up to `now` that received some, oldest first.
    fn recent(&self, now: i64) -> impl Iterator<Item = (i64, u64)> {
        (now - WINDOW + 1..=now).filter_map(move |m| {
            let held = self.minutes[m.rem_euclid(WINDOW) as usize].load(Ordering::Relaxed);
            let bytes = held & BYTE_MASK;
            ((held & !BYTE_MASK) == tag(m) && bytes > 0).then_some((m, bytes))
        })
    }

    /// Records how an answer ended: the `bytes` it brought, and its time to the last byte when
    /// it was read to its end.
    fn end(&self, sent: Sent, bytes: u64, last_byte: Option<Duration>) {
        let last_byte = last_byte.filter(|_| self.class.ends());
        let mut ends = self.ends.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(took) = last_byte {
            ends.last_byte.record(took);
        }
        // Only a request that may enter the list has its path rewritten.
        let lightest = if ends.heaviest.len() < HEAVIEST { 0 } else { ends.heaviest.last().map_or(0, |h| h.bytes) };
        if bytes > 0 && bytes >= lightest {
            let heavy = Heavy {
                context: self.context.clone(),
                class: self.class,
                method: sent.method,
                path: path_pattern(&sent.path),
                bytes,
                headers: sent.headers,
                last_byte,
            };
            list_heavy(&mut ends.heaviest, heavy);
        }
    }

    fn reset(&self) {
        self.total.store(0, Ordering::Relaxed);
        for slot in &self.minutes {
            slot.store(0, Ordering::Relaxed);
        }
        *self.ends.lock().unwrap_or_else(PoisonError::into_inner) = Ends::default();
    }
}

/// A response body that adds its bytes to its cluster's class as they arrive (decompressed:
/// `MeterLayer` is outside kube's decompression), and records once how it ended: read to its
/// end, failed, or dropped before.
pub struct Counted<B: HttpBody> {
    inner: B,
    flow: Arc<Flow>,
    /// This answer's bytes so far: one task reads a body at a time, no atomic needed.
    bytes: u64,
    /// Taken when the end is recorded, so that it is recorded once.
    sent: Option<Sent>,
}

impl<B: HttpBody> Counted<B> {
    fn new(inner: B, flow: Arc<Flow>, sent: Sent) -> Self {
        Self { inner, flow, bytes: 0, sent: Some(sent) }
    }

    fn finish(&mut self, whole: bool) {
        if let Some(sent) = self.sent.take() {
            let last_byte = whole.then(|| sent.start.elapsed());
            self.flow.end(sent, self.bytes, last_byte);
        }
    }
}

impl<B: HttpBody + Unpin> HttpBody for Counted<B> {
    type Data = B::Data;
    type Error = B::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<B::Data>, B::Error>>> {
        let this = &mut *self;
        let polled = Pin::new(&mut this.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                let n = frame.data_ref().map_or(0, |data| data.remaining() as u64);
                if n > 0 {
                    this.bytes += n;
                    this.flow.add(now_minute(), n);
                }
                // A body that knows it has no more is whole now, even if it is never polled again.
                if this.inner.is_end_stream() {
                    this.finish(true);
                }
            }
            Poll::Ready(None) => this.finish(true),
            Poll::Ready(Some(Err(_))) => this.finish(false),
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<B: HttpBody> Drop for Counted<B> {
    fn drop(&mut self) {
        // Given up before its end (a view closed, a stream stopped), unless all of it came.
        let whole = self.inner.is_end_stream();
        self.finish(whole);
    }
}

/// One cluster's requests of one class: what their headers said, under the lock, and what
/// their bodies bring after.
struct Entry {
    stats: Stats,
    flow: Arc<Flow>,
}

static STATS: LazyLock<Mutex<HashMap<(String, Class), Entry>>> = LazyLock::new(Default::default);

fn lock() -> MutexGuard<'static, HashMap<(String, Class), Entry>> {
    STATS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the meter has counted, read at a Unix minute.
pub struct Snapshot {
    /// Every cluster's numbers so far, sorted by cluster then class.
    pub classes: Vec<(String, Class, Stats)>,
    /// The heaviest requests of all clusters, heaviest first: at most `HEAVIEST`, one a kind.
    pub heaviest: Vec<Heavy>,
}

/// Every cluster's numbers so far, at Unix minute `now`.
pub fn snapshot(now: i64) -> Snapshot {
    let mut classes = Vec::new();
    let mut heaviest = Vec::new();
    let state = lock();
    for ((context, class), entry) in &*state {
        let mut stats = entry.stats.clone();
        stats.bytes = entry.flow.total.load(Ordering::Relaxed);
        stats.add_bytes(entry.flow.recent(now));
        {
            let ends = entry.flow.ends.lock().unwrap_or_else(PoisonError::into_inner);
            stats.last_byte.clone_from(&ends.last_byte);
            heaviest.extend(ends.heaviest.iter().cloned());
        }
        // A class reset since its last request, with nothing received since, shows nothing.
        if stats.count > 0 || stats.bytes > 0 {
            classes.push((context.clone(), *class, stats));
        }
    }
    drop(state);
    classes.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));
    heaviest.sort_by(|a, b| {
        b.bytes.cmp(&a.bytes).then_with(|| {
            (&a.context, a.class, &a.path, a.method.as_str()).cmp(&(&b.context, b.class, &b.path, b.method.as_str()))
        })
    });
    heaviest.truncate(HEAVIEST);
    Snapshot { classes, heaviest }
}

/// Starts every count again. The entries stay: bodies still being read keep counting into
/// their flows.
pub fn reset() {
    let mut state = lock();
    for entry in state.values_mut() {
        entry.stats = Stats::default();
        entry.flow.reset();
    }
}

/// Records a request's headers, and gives the flow its body counts into.
fn record(context: String, class: Class, took: Duration, failed: bool) -> Arc<Flow> {
    let minute = now_minute();
    let mut state = lock();
    let entry = state.entry((context, class)).or_insert_with_key(|(context, class)| Entry {
        stats: Stats::default(),
        flow: Arc::new(Flow::new(context.clone(), *class)),
    });
    entry.stats.record(took, failed, minute);
    entry.flow.clone()
}

/// Times every request of a cluster's client until its response starts (headers): a watch
/// or a log stream counts as open there. Then its body counts the bytes it brings and when
/// its last byte came (`Counted`).
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
    R: HttpBody + Unpin + Send + 'static,
{
    type Response = http::Response<Counted<R>>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<http::Response<Counted<R>>, S::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<B>) -> Self::Future {
        let class = Class::of(req.method(), req.uri().path(), req.uri().query().unwrap_or_default());
        let (method, path) = (req.method().clone(), req.uri().path().to_string());
        let context = self.context.clone();
        let start = Instant::now();
        let fut = self.inner.call(req);
        Box::pin(async move {
            let result = fut.await;
            let headers = start.elapsed();
            let failed = match &result {
                Ok(resp) => resp.status().is_client_error() || resp.status().is_server_error(),
                Err(_) => true,
            };
            let flow = record(context, class, headers, failed);
            result.map(|resp| resp.map(|body| Counted::new(body, flow, Sent { method, path, start, headers })))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    use bytes::Bytes;
    use futures::executor::block_on;
    use http::Method;
    use http_body_util::{BodyExt as _, StreamBody};
    use hyper::body::{Body as HttpBody, Frame};
    use proptest::prelude::*;

    use super::{
        Class, Counted, Duration, Flow, HEAVIEST, Heavy, Latency, Sent, Stats, WINDOW, list_heavy, path_pattern,
        per_minute,
    };

    proptest! {
        /// A percentile is never below the real one, and at most 25% above it (1 ms at the
        /// least, the first bucket's bound).
        #[test]
        fn percentiles_are_close_above(
            micros in proptest::collection::vec(1u64..1_000_000_000, 1..200),
            q in 0.0f64..=1.0,
        ) {
            let mut l = Latency::default();
            for &m in &micros {
                l.record(Duration::from_micros(m));
            }
            let mut sorted = micros;
            sorted.sort_unstable();
            let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
            let real = Duration::from_micros(sorted[rank - 1]);
            let got = l.percentile(q).unwrap();
            prop_assert!(got >= real, "{got:?} < {real:?}");
            prop_assert!(got <= (real.mul_f64(1.25) + Duration::from_nanos(1)).max(Duration::from_millis(1)), "{got:?} for {real:?}");
            prop_assert!(got <= l.slowest);
        }

        /// The minutes add up to every request and every byte of the window, whatever the
        /// clusters and classes, and run one per minute up to now.
        #[test]
        fn minutes_add_up(
            calls in proptest::collection::vec((0usize..3, 0i64..200, any::<bool>()), 0..300),
            received in proptest::collection::vec((0usize..3, 0i64..WINDOW, 1u64..1_000_000), 0..100),
        ) {
            let mut stats = vec![Stats::default(); 3];
            let mut now = 0;
            for &(i, at, failed) in &calls {
                now = now.max(at);
                stats[i].record(Duration::from_millis(5), failed, now);
            }
            // Bytes over the last hour, in minutes with requests or without, as a snapshot adds them.
            for &(i, back, n) in &received {
                stats[i].add_bytes([(now - back, n)]);
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
            prop_assert_eq!(points.iter().map(|p| p.1.bytes).sum::<u64>(), received.iter().map(|r| r.2).sum::<u64>());
            prop_assert!(points.len() >= 2 && points.len() <= WINDOW as usize);
            prop_assert_eq!(points.last().map(|p| p.0), Some(now));
            prop_assert!(points.windows(2).all(|w| w[1].0 == w[0].0 + 1));
        }

        /// Bytes added minute by minute read back as added, for every minute of the window up
        /// to the last one, whatever the gaps between minutes: none lost or counted twice, none
        /// left from a minute gone out of the window, and the same far from 1970.
        #[test]
        fn flows_keep_the_bytes_of_each_minute(
            start in prop_oneof![0i64..1_000, 29_000_000i64..30_000_000, (1i64 << 50)..(1i64 << 51)],
            adds in proptest::collection::vec((0i64..90, 1u64..1_000_000), 1..200),
        ) {
            let flow = Flow::new("shop".into(), Class::Watch);
            let mut minute = start;
            let mut expected = BTreeMap::new();
            let mut added = Vec::new();
            for &(step, n) in &adds {
                minute += step;
                flow.add(minute, n);
                added.push((minute, n));
            }
            for &(m, n) in &added {
                if m > minute - WINDOW {
                    *expected.entry(m).or_insert(0) += n;
                }
            }
            prop_assert_eq!(flow.recent(minute).collect::<Vec<_>>(), expected.into_iter().collect::<Vec<_>>());
            prop_assert_eq!(flow.total.load(Ordering::Relaxed), adds.iter().map(|a| a.1).sum::<u64>());
        }

        /// The list holds the heaviest request of each kind, heaviest first, of the `HEAVIEST`
        /// heaviest kinds, whatever the order the requests end in.
        #[test]
        fn the_heaviest_of_each_kind_are_listed(ended in proptest::collection::vec((0usize..15, 1u64..50), 0..100)) {
            let mut list = Vec::new();
            let mut most: HashMap<String, u64> = HashMap::new();
            for &(kind, bytes) in &ended {
                let request = heavy(&format!("/api/v1/kind{kind}"), bytes);
                let m = most.entry(request.path.clone()).or_default();
                *m = (*m).max(bytes);
                list_heavy(&mut list, request);
            }
            let mut top: Vec<u64> = most.values().copied().collect();
            top.sort_unstable_by_key(|&b| std::cmp::Reverse(b));
            top.truncate(HEAVIEST);
            prop_assert_eq!(list.iter().map(|h| h.bytes).collect::<Vec<_>>(), top);
            for h in &list {
                prop_assert_eq!(Some(&h.bytes), most.get(&h.path));
            }
            prop_assert_eq!(list.iter().map(|h| &h.path).collect::<HashSet<_>>().len(), list.len());
        }

        /// No namespace or object name is left in a pattern, whatever the names, and nothing
        /// else changes.
        #[test]
        fn patterns_name_nothing(
            ns in "ns-[a-z0-9.-]{1,12}",
            name in "obj-[a-z0-9.:-]{1,12}",
            shape in 0usize..5,
        ) {
            let path = match shape {
                0 => format!("/api/v1/namespaces/{ns}/pods/{name}/log"),
                1 => format!("/apis/apps/v1/namespaces/{ns}/deployments/{name}"),
                2 => format!("/api/v1/nodes/{name}/proxy/stats/summary"),
                3 => format!("/api/v1/namespaces/{name}"),
                _ => format!("/api/v1/watch/namespaces/{ns}/configmaps/{name}"),
            };
            let pattern = path_pattern(&path);
            prop_assert!(!pattern.split('/').any(|p| p == ns || p == name), "{pattern}");
            prop_assert_eq!(pattern.split('/').count(), path.split('/').count());
        }
    }

    fn heavy(path: &str, bytes: u64) -> Heavy {
        Heavy {
            context: "shop".into(),
            class: Class::List,
            method: Method::GET,
            path: path.into(),
            bytes,
            headers: Duration::ZERO,
            last_byte: None,
        }
    }

    /// A body that sends `chunks`, then fails when `fail`.
    fn body(
        chunks: &[&'static str],
        fail: bool,
    ) -> impl HttpBody<Data = Bytes, Error = std::io::Error> + Unpin + use<> {
        let mut frames: Vec<Result<Frame<Bytes>, std::io::Error>> =
            chunks.iter().map(|c| Ok(Frame::data(Bytes::from_static(c.as_bytes())))).collect();
        if fail {
            frames.push(Err(std::io::Error::other("connection reset")));
        }
        StreamBody::new(futures::stream::iter(frames))
    }

    fn counted<B: HttpBody>(inner: B, flow: &Arc<Flow>) -> Counted<B> {
        let sent = Sent {
            method: Method::GET,
            path: "/api/v1/namespaces/shop/pods".into(),
            start: Instant::now(),
            headers: Duration::from_millis(5),
        };
        Counted::new(inner, flow.clone(), sent)
    }

    /// How many times to the last byte `flow` recorded, and the heaviest requests it lists.
    fn ends(flow: &Flow) -> (u64, Vec<Heavy>) {
        let ends = flow.ends.lock().unwrap();
        (ends.last_byte.count(), ends.heaviest.clone())
    }

    #[test]
    fn a_body_read_to_its_end_counts_its_bytes_and_ends_once() {
        let flow = Arc::new(Flow::new("shop".into(), Class::List));
        let mut answer = counted(body(&["abc", "de"], false), &flow);
        block_on(async {
            while let Some(frame) = answer.frame().await {
                frame.unwrap();
            }
            // Read again past its end.
            assert!(answer.frame().await.is_none());
        });
        assert!(answer.sent.is_none(), "the end is recorded");
        drop(answer);
        assert_eq!(flow.total.load(Ordering::Relaxed), 5);
        let (timed, heaviest) = ends(&flow);
        assert_eq!(timed, 1, "one time to the last byte");
        assert_eq!(heaviest.len(), 1);
        assert_eq!(heaviest[0].bytes, 5);
        assert_eq!(heaviest[0].path, "/api/v1/namespaces/{namespace}/pods");
        // Recorded again when dropped, it would have replaced this one, as given up.
        assert!(heaviest[0].last_byte.is_some());
    }

    #[test]
    fn a_body_given_up_or_cut_ends_once_without_a_time() {
        // Dropped after its first chunk: a view closed while a list came.
        let flow = Arc::new(Flow::new("shop".into(), Class::List));
        let mut early = counted(body(&["abc", "de", "f"], false), &flow);
        block_on(early.frame()).unwrap().unwrap();
        drop(early);
        assert_eq!(flow.total.load(Ordering::Relaxed), 3);
        let (timed, heaviest) = ends(&flow);
        assert_eq!(timed, 0, "an answer given up has no time to its last byte");
        assert_eq!((heaviest.len(), heaviest[0].bytes, heaviest[0].last_byte), (1, 3, None));

        // Cut by an error after its first chunk.
        let flow = Arc::new(Flow::new("shop".into(), Class::List));
        let mut cut = counted(body(&["abcd"], true), &flow);
        block_on(async {
            cut.frame().await.unwrap().unwrap();
            assert!(cut.frame().await.unwrap().is_err());
        });
        drop(cut);
        let (timed, heaviest) = ends(&flow);
        assert_eq!(timed, 0);
        assert_eq!((heaviest.len(), heaviest[0].bytes, heaviest[0].last_byte), (1, 4, None));
    }

    #[test]
    fn a_stream_counts_its_bytes_without_a_time_to_its_last_byte() {
        let flow = Arc::new(Flow::new("shop".into(), Class::Watch));
        let events = block_on(counted(body(&[r#"{"type":"ADDED"}"#, "\n"], false), &flow).collect()).unwrap();
        assert_eq!(flow.total.load(Ordering::Relaxed), events.to_bytes().len() as u64);
        let (timed, heaviest) = ends(&flow);
        assert_eq!(timed, 0);
        assert!(heaviest[0].last_byte.is_none());
    }

    #[test]
    fn bytes_added_at_once_from_many_threads_all_count() {
        let flow = Flow::new("shop".into(), Class::Watch);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..10_000 {
                        flow.add(7, 3);
                    }
                });
            }
        });
        assert_eq!(flow.total.load(Ordering::Relaxed), 120_000);
        assert_eq!(flow.recent(7).collect::<Vec<_>>(), vec![(7, 120_000)]);
    }

    #[test]
    fn bytes_per_minute_count_from_the_first_busy_minute() {
        let mut s = Stats::default();
        assert_eq!(s.bytes_per_minute(100), 0);
        s.record(Duration::from_millis(5), false, 96);
        s.add_bytes([(96, 3_000), (98, 1_000)]);
        // Minutes 96 to 100: five.
        assert_eq!(s.bytes_per_minute(100), 800);
        // An hour later minute 96 is out of the window: 1,000 bytes over minutes 98 to 156.
        assert_eq!(s.bytes_per_minute(96 + WINDOW), 1_000 / 59);
    }

    #[test]
    fn paths_lose_their_names() {
        let cases = [
            ("/api/v1/namespaces/shop/pods/web-1/log", "/api/v1/namespaces/{namespace}/pods/{name}/log"),
            ("/api/v1/namespaces/shop/pods", "/api/v1/namespaces/{namespace}/pods"),
            ("/api/v1/pods", "/api/v1/pods"),
            ("/api/v1/namespaces", "/api/v1/namespaces"),
            ("/api/v1/namespaces/shop", "/api/v1/namespaces/{name}"),
            ("/api/v1/namespaces/shop/finalize", "/api/v1/namespaces/{name}/finalize"),
            ("/api/v1/nodes/node-1/proxy/stats/summary", "/api/v1/nodes/{name}/proxy/stats/summary"),
            (
                "/apis/apps/v1/namespaces/shop/deployments/web/scale",
                "/apis/apps/v1/namespaces/{namespace}/deployments/{name}/scale",
            ),
            ("/apis/metrics.k8s.io/v1beta1/nodes", "/apis/metrics.k8s.io/v1beta1/nodes"),
            (
                "/api/v1/namespaces/monitoring/services/prometheus:9090/proxy/api/v1/query_range",
                "/api/v1/namespaces/{namespace}/services/{name}/proxy/api/v1/query_range",
            ),
            ("/api/v1/watch/namespaces/shop/pods", "/api/v1/watch/namespaces/{namespace}/pods"),
            ("/apis/apps/v1", "/apis/apps/v1"),
            ("/api", "/api"),
            ("/version", "/version"),
            ("/openapi/v3/apis/apps/v1", "/openapi/v3/apis/apps/v1"),
        ];
        for (path, pattern) in cases {
            assert_eq!(path_pattern(path), pattern, "{path}");
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
