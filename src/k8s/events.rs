//! A cluster's events for the Timeline: every page listed once, then a watch from that list
//! keeps them current, so a refresh brings only what changed. Only the newest are held: a big
//! cluster keeps hundreds of thousands of events, and the Timeline shows them newest first.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use futures::channel::mpsc;
use futures::{AsyncBufReadExt as _, StreamExt as _};
use k8s_openapi::api::core::v1::Event;
use kube::api::{ListParams, WatchParams};
use kube::core::Request;
use kube::{Client, Resource as _};
use serde::Deserialize;

use super::pods::{ClusterEvent, LeanEvent, cluster_event, grouped};
use super::table::{Backoff, resumable, truncate};
use super::{Pause, TaskGuard, api_status, lists, short_error, spawn_guarded};

/// Events held for one cluster, the newest: its namespaces read one by one share them.
pub const EVENTS_KEPT: usize = 20_000;

/// Events read per request: one request brings every Warning of most clusters.
const EVENTS_PAGE: u32 = 5000;

/// A watch that hears nothing for this long is opened again from where it was: a connection
/// that died without a word (sleep, VPN) says nothing either. Events have no watch cache by
/// default, so the API server sends no bookmarks on a quiet watch.
const WATCH_IDLE: Duration = Duration::from_secs(120);

/// While the page is off screen, how often to look whether it is shown again.
const PAUSED_POLL: Duration = Duration::from_millis(500);

/// Once the page is shown again, how long the watch has to bring what changed meanwhile
/// before the rows go.
const CATCH_UP: Duration = Duration::from_secs(1);

/// What a watch of events sends.
pub enum EventsMsg {
    Rows(EventRows),
    /// Reading or watching failed. It is tried again, unless refused (403).
    Error(String),
}

/// The events held, those of one object with the same type and reason as one row
/// (`grouped`), newest first.
pub struct EventRows {
    pub rows: Vec<Arc<ClusterEvent>>,
    /// How many events they are, of how many the cluster keeps there.
    pub kept: usize,
    pub total: usize,
    /// When the oldest of them last happened.
    pub since: Option<jiff::Timestamp>,
}

/// Follow the events of `namespace` (every namespace when None), Warnings only unless
/// `normal`: every page listed once, the newest `cap` held, then a watch from the list's
/// resourceVersion keeps them current. The rows go once listed, then at most every `every`
/// while they change. A watch whose resourceVersion is too old (410 Gone) lists again. While
/// `pause` is set (the page off screen), nothing is read and no watch is open: once it is
/// cleared, the watch goes on from where it was, with what changed meanwhile. A refused list
/// (403) stops it; a refused watch lists again every `every` instead. Other failures are tried
/// again, less and less often. Dropping the guard stops it.
pub fn watch_events(
    client: Client,
    namespace: Option<String>,
    normal: bool,
    cap: usize,
    every: Duration,
    pause: Pause,
) -> (TaskGuard, mpsc::UnboundedReceiver<EventsMsg>) {
    let (tx, rx) = mpsc::unbounded();
    let url = Event::url_path(&(), namespace.as_deref());
    let follow = Follow { client, url, normal, cap, every, tx, pause };
    let guard = spawn_guarded(async move {
        let mut backoff = Backoff::default();
        loop {
            // Whether the watch brought changes: only then does the wait start over, or a watch
            // refused at once would list again and again.
            let mut progress = false;
            let result = follow.run(&mut progress).await;
            if let Err(e) = &result
                && (follow.tx.unbounded_send(EventsMsg::Error(short_error(e))).is_err() || api_status(e) == Some(403))
            {
                return;
            }
            if follow.tx.is_closed() {
                return;
            }
            if progress {
                backoff = Backoff::default();
            }
            if result.is_err() || !progress {
                tokio::time::sleep(backoff.next()).await;
            }
        }
    });
    (guard, rx)
}

/// What a watch of events follows, and where its rows go.
struct Follow {
    client: Client,
    url: String,
    normal: bool,
    cap: usize,
    every: Duration,
    tx: mpsc::UnboundedSender<EventsMsg>,
    pause: Pause,
}

impl Follow {
    /// One list, then the watch from it: Ok once its resourceVersion is too old (list again)
    /// or nobody takes the rows any more. A refused watch lists again every `every`.
    async fn run(&self, progress: &mut bool) -> Result<()> {
        if !self.shown().await {
            return Ok(());
        }
        let (mut held, mut version) = list_events(&self.client, &self.url, self.normal, self.cap).await?;
        let mut rows = Rows { dirty: true, ..Rows::default() };
        rows.flush(&mut held, self)?;
        let mut wp = WatchParams::default().timeout(290);
        if !self.normal {
            wp = wp.fields("type=Warning");
        }
        let request = Request::new(&self.url);
        // A cut stream or a failed reopen watches again from `version`, waiting longer each time.
        let mut backoff = Backoff::default();
        let mut retrying = false;
        loop {
            // Off screen, no watch is open. Shown again, it goes on from `version`, and the rows
            // wait a moment for what changed meanwhile.
            if self.pause.is_paused() {
                if !self.shown().await {
                    return Ok(());
                }
                rows.not_before = Some(Instant::now() + CATCH_UP);
            }
            let reader = match self.client.request_stream(request.watch(&wp, &version)?).await {
                Ok(reader) => reader,
                Err(kube::Error::Api(status)) if status.code == 410 => return Ok(()),
                // Allowed to list the events but not to watch them (RBAC grants the two apart):
                // they are listed again every `every`, as the page did before it watched them.
                Err(kube::Error::Api(status)) if status.code == 403 => {
                    drop(held);
                    return self.list_each().await;
                }
                Err(e) if resumable(&e) => {
                    self.tx.unbounded_send(EventsMsg::Error(short_error(&e.into())))?;
                    retrying = true;
                    tokio::time::sleep(backoff.next()).await;
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            if std::mem::take(&mut retrying) {
                // Sent again at once, the rows clear the error shown meanwhile.
                (rows.dirty, rows.sent) = (true, None);
            }
            let mut chunks = std::pin::pin!(reader.lines().ready_chunks(256));
            let mut heard = tokio::time::Instant::now();
            let mut cut = false;
            while !cut && !self.pause.is_paused() {
                let flush = rows.wait(self);
                let next = tokio::select! {
                    lines = chunks.next() => Some(lines),
                    () = sleep_or_never(flush) => None,
                    // Wakes up to see whether the page went off screen.
                    () = tokio::time::sleep(PAUSED_POLL) => None,
                    () = tokio::time::sleep_until(heard + WATCH_IDLE) => break,
                };
                match next {
                    // The server ended the watch (its timeout): watch again from `version`.
                    Some(None) => break,
                    Some(Some(lines)) => {
                        heard = tokio::time::Instant::now();
                        for line in lines {
                            // The connection broke (a load balancer, an API server restart).
                            let Ok(line) = line else {
                                cut = true;
                                break;
                            };
                            backoff = Backoff::default();
                            match apply_line(&line, &mut held, &mut version)? {
                                Applied::Changed => rows.dirty = true,
                                Applied::Unchanged => {}
                                Applied::Relist => return Ok(()),
                            }
                            *progress = true;
                        }
                    }
                    None => {}
                }
                rows.flush(&mut held, self)?;
            }
            if cut {
                tokio::time::sleep(backoff.next()).await;
            }
        }
    }

    /// Without the right to watch: every `every`, while the page is on screen, the events are
    /// listed again and their rows sent. The watch isn't asked again, which the API server
    /// would refuse and log each time.
    async fn list_each(&self) -> Result<()> {
        loop {
            tokio::time::sleep(self.every).await;
            if !self.shown().await {
                return Ok(());
            }
            let (mut held, _) = list_events(&self.client, &self.url, self.normal, self.cap).await?;
            Rows { dirty: true, ..Rows::default() }.flush(&mut held, self)?;
        }
    }

    /// Waits while the page is off screen; false once nobody takes the rows.
    async fn shown(&self) -> bool {
        while self.pause.is_paused() && !self.tx.is_closed() {
            tokio::time::sleep(PAUSED_POLL).await;
        }
        !self.tx.is_closed()
    }
}

async fn sleep_or_never(wait: Option<Duration>) {
    match wait {
        Some(wait) => tokio::time::sleep(wait).await,
        None => std::future::pending().await,
    }
}

/// When the rows changed since they went last, and when that was.
#[derive(Default)]
struct Rows {
    dirty: bool,
    sent: Option<Instant>,
    /// Not before then: the watch is bringing what changed while the page was off screen.
    not_before: Option<Instant>,
}

impl Rows {
    /// How long until changed rows may go: None when they didn't change or the page is off
    /// screen.
    fn wait(&self, follow: &Follow) -> Option<Duration> {
        if !self.dirty || follow.pause.is_paused() {
            return None;
        }
        let due = self.sent.map(|sent| sent + follow.every).max(self.not_before);
        Some(due.map_or_default(|due| due.saturating_duration_since(Instant::now())))
    }

    /// Send the rows when they changed, the page is on screen and the last ones are old enough.
    fn flush(&mut self, held: &mut Held, follow: &Follow) -> Result<()> {
        if self.wait(follow) == Some(Duration::ZERO) {
            follow.tx.unbounded_send(EventsMsg::Rows(held.rows()))?;
            *self = Self { sent: Some(Instant::now()), ..Self::default() };
        }
        Ok(())
    }
}

/// Every event at `url`, Warnings only unless `normal`, the newest `cap` held, and the
/// resourceVersion to watch from. Every page is read: the API server lists events by namespace
/// and name, never by time, so the newest can be on any page. A list that pages for longer than
/// etcd keeps its history (5 minutes by default) is refused its next page (410 Gone): it
/// starts over, once.
async fn list_events(client: &Client, url: &str, normal: bool, cap: usize) -> Result<(Held, String)> {
    let mut lp = ListParams::default().limit(EVENTS_PAGE);
    // Every event in one list, not a list per type: the API server goes through all the events
    // it stores either way, a selector only filters what it sends.
    if !normal {
        lp = lp.fields("type=Warning");
    }
    let mut again = true;
    loop {
        let mut held = Held::new(cap);
        let read = lists::each_lean_page(client, url, lp.clone(), |page: Vec<LeanEvent>| {
            for e in page {
                held.listed(cluster_event(e));
            }
        })
        .await;
        match read {
            Ok(version) => return Ok((held, version)),
            Err(e) if api_status(&e) == Some(410) && std::mem::take(&mut again) => {}
            Err(e) => return Err(e),
        }
    }
}

/// An event's namespace and name: what a change names it by.
type Key = (Option<String>, String);

/// The events a watch holds, by namespace and name: the newest `cap` by when they last
/// happened (between two trims, up to a quarter more), and how many the cluster keeps.
struct Held {
    cap: usize,
    events: HashMap<Key, ClusterEvent>,
    /// The events listed, plus those added since, minus those deleted.
    total: usize,
}

impl Held {
    fn new(cap: usize) -> Self {
        Self { cap: cap.max(1), events: HashMap::new(), total: 0 }
    }

    fn key(e: &ClusterEvent) -> Key {
        (e.line.namespace.clone(), e.line.name.clone())
    }

    /// An event of the list: each one the cluster keeps comes once.
    fn listed(&mut self, e: ClusterEvent) {
        self.total += 1;
        self.put(e);
    }

    /// A change the watch brought.
    fn change(&mut self, what: &str, e: ClusterEvent) {
        if what == "DELETED" {
            self.total = self.total.saturating_sub(1);
            self.events.remove(&Self::key(&e));
        } else if self.put(e) && what == "ADDED" {
            // One dropped as too old comes back when it happens again, as MODIFIED.
            self.total += 1;
        }
    }

    /// Holds `e` in place of what it was; whether it is new to what is held.
    fn put(&mut self, e: ClusterEvent) -> bool {
        let new = self.events.insert(Self::key(&e), e).is_none();
        // Dropping the oldest goes through every event held: done once a quarter more are
        // held, it costs a few steps an event.
        if self.events.len() > self.cap + self.cap / 4 {
            self.trim();
        }
        new
    }

    /// Drops the oldest, by when they last happened then by name, down to `cap`.
    fn trim(&mut self) {
        let excess = self.events.len().saturating_sub(self.cap);
        if excess == 0 {
            return;
        }
        let mut order: Vec<(Option<jiff::Timestamp>, &Key)> =
            self.events.iter().map(|(k, e)| (e.line.last, k)).collect();
        order.select_nth_unstable(excess - 1);
        let oldest: Vec<Key> = order[..excess].iter().map(|&(_, k)| k.clone()).collect();
        for k in &oldest {
            self.events.remove(k);
        }
    }

    /// What is held, as the Timeline shows it.
    fn rows(&mut self) -> EventRows {
        self.trim();
        let mut rows = grouped(self.events.values().cloned());
        // Newest first, and the same order each time: rows don't trade places from one to the
        // next.
        rows.sort_unstable_by(|a, b| {
            (b.line.last.cmp(&a.line.last))
                .then_with(|| {
                    (&a.object_kind, &a.object_namespace, &a.object_name).cmp(&(
                        &b.object_kind,
                        &b.object_namespace,
                        &b.object_name,
                    ))
                })
                .then_with(|| (&a.line.kind, &a.line.reason).cmp(&(&b.line.kind, &b.line.reason)))
        });
        EventRows {
            rows: rows.into_iter().map(Arc::new).collect(),
            kept: self.events.len(),
            total: self.total.max(self.events.len()),
            since: self.events.values().filter_map(|e| e.line.last).min(),
        }
    }
}

/// What one line of a watch did.
#[derive(Debug, PartialEq, Eq)]
enum Applied {
    Changed,
    Unchanged,
    /// 410 Gone: the resourceVersion is too old, list again.
    Relist,
}

/// A line of a watch, its object kept as written until its type says how to read it.
#[derive(Deserialize)]
struct Frame {
    #[serde(rename = "type")]
    type_: String,
    object: Box<serde_json::value::RawValue>,
}

/// An error a watch sends in its stream.
#[derive(Default, Deserialize)]
#[serde(default)]
struct WatchStatus {
    code: Option<u16>,
    message: Option<String>,
}

/// Apply one line of the watch to what is held, keeping the resourceVersion to go on from. An
/// error other than 410 ends the watch.
fn apply_line(line: &str, held: &mut Held, version: &mut String) -> Result<Applied> {
    if line.trim().is_empty() {
        return Ok(Applied::Unchanged);
    }
    let frame: Frame =
        serde_json::from_str(line).with_context(|| format!("bad watch frame: {}", truncate(line, 200)))?;
    if frame.type_ == "ERROR" {
        let status: WatchStatus = serde_json::from_str(frame.object.get()).unwrap_or_default();
        if status.code == Some(410) {
            return Ok(Applied::Relist);
        }
        return Err(anyhow!(status.message.unwrap_or_else(|| "watch error".into())));
    }
    // A bookmark reads as an event with only a newer resourceVersion: going on from it avoids
    // a 410 and a list.
    let event: LeanEvent = serde_json::from_str(frame.object.get())?;
    if let Some(v) = event.metadata.resource_version.as_ref().filter(|v| !v.is_empty()) {
        version.clone_from(v);
    }
    Ok(match frame.type_.as_str() {
        what @ ("ADDED" | "MODIFIED" | "DELETED") => {
            held.change(what, cluster_event(event));
            Applied::Changed
        }
        _ => Applied::Unchanged,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use bytes::Bytes;
    use futures::StreamExt as _;
    use futures::channel::mpsc;
    use http_body_util::BodyExt as _;
    use http_body_util::combinators::UnsyncBoxBody;
    use hyper::body::Frame;
    use proptest::prelude::*;
    use serde_json::{Value, json};

    use super::{Applied, EVENTS_KEPT, EventRows, EventsMsg, Held, Pause};
    use crate::k8s::{ClusterEvent, EventLine};

    /// Where the fake events' times start.
    const BASE: i64 = 1_767_225_600;

    #[derive(Default)]
    struct Stored {
        per_namespace: usize,
        page: usize,
        added: Vec<Value>,
        version: String,
        /// `continue` requests still to refuse as too old.
        expire: usize,
        /// What each watch request gets, in turn: its lines, and whether it ends after them.
        watches: VecDeque<(Vec<String>, bool)>,
        /// Watches are refused (403): the user may list events, not watch them.
        no_watch: bool,
        asked: Vec<String>,
    }

    /// An API server keeping `per_namespace` Warning events and as many Normal ones in each
    /// namespace from a to z, each about its own pod, and those `add`ed. It lists them as a
    /// real one does: by namespace and name, never by time, `limit` at a time (at most `page`:
    /// a server may send fewer), with a `continue` token while more follow. A `continue` is
    /// refused as too old (410 Gone) while `expire` says so. A watch gets the lines queued
    /// with `watch` and stays open after them, unless they end it; after `refuse_watches`,
    /// it is refused. Records the requests it was sent, and counts the watches open.
    #[derive(Clone)]
    struct Events {
        stored: Arc<Mutex<Stored>>,
        open: Arc<AtomicUsize>,
    }

    impl Events {
        fn new(per_namespace: usize, page: usize) -> Self {
            let stored = Stored { per_namespace, page, version: "7".into(), ..Stored::default() };
            Self { stored: Arc::new(Mutex::new(stored)), open: Arc::default() }
        }

        fn add(&self, event: Value) {
            self.stored.lock().unwrap().added.push(event);
        }

        fn set_version(&self, version: &str) {
            version.clone_into(&mut self.stored.lock().unwrap().version);
        }

        fn expire(&self, n: usize) {
            self.stored.lock().unwrap().expire = n;
        }

        fn watch(&self, lines: Vec<String>, ends: bool) {
            self.stored.lock().unwrap().watches.push_back((lines, ends));
        }

        fn refuse_watches(&self) {
            self.stored.lock().unwrap().no_watch = true;
        }

        fn asked(&self) -> Vec<String> {
            self.stored.lock().unwrap().asked.clone()
        }

        fn lists(&self) -> usize {
            self.asked().iter().filter(|q| !q.contains("watch=true")).count()
        }

        fn watches_asked(&self) -> Vec<String> {
            self.asked().into_iter().filter(|q| q.contains("watch=true")).collect()
        }

        fn open_watches(&self) -> usize {
            self.open.load(Ordering::SeqCst)
        }
    }

    /// The `per_namespace` events of each namespace from a to z.
    fn alphabet(per_namespace: usize) -> impl Iterator<Item = Value> {
        ('a'..='z').flat_map(move |ns| {
            (0..per_namespace).flat_map(move |i| {
                [false, true].map(|warning| {
                    let (kind, reason) = if warning { ("Warning", "BackOff") } else { ("Normal", "Pulled") };
                    // A pod's Warning a second after its Normal event.
                    let at = BASE + 2 * i64::try_from(i).unwrap() + i64::from(warning);
                    json!({
                        "metadata": {
                            "name": format!("web-{i:05}.{kind}"), "namespace": ns.to_string(),
                            "creationTimestamp": "2026-01-01T00:00:00Z", "resourceVersion": "5",
                            // Most of a stored event's bytes, which the views never read.
                            "managedFields": [{"manager": "kubelet", "operation": "Update", "apiVersion": "v1",
                                "fieldsType": "FieldsV1", "fieldsV1": {"f:count": {}, "f:lastTimestamp": {}, "f:message": {}}}],
                        },
                        "involvedObject": {"kind": "Pod", "namespace": ns.to_string(), "name": format!("web-{i}"), "uid": format!("{ns}-{i}")},
                        "type": kind, "reason": reason, "message": "something happened", "count": 1,
                        "lastTimestamp": jiff::Timestamp::from_second(at).unwrap().to_string(),
                        "source": {"component": "kubelet", "host": "node-1"},
                    })
                })
            })
        })
    }

    /// A Warning `name` in `ns` about pod `web-<name>`, last at `at` seconds, `count` times, at
    /// resourceVersion `rv`.
    fn warning(ns: &str, name: &str, at: i64, count: i32, rv: &str) -> Value {
        json!({
            "metadata": {"name": name, "namespace": ns, "resourceVersion": rv},
            "involvedObject": {"kind": "Pod", "namespace": ns, "name": format!("web-{name}"), "uid": format!("{ns}-{name}")},
            "type": "Warning", "reason": "BackOff", "message": "restarting", "count": count,
            "lastTimestamp": jiff::Timestamp::from_second(BASE + at).unwrap().to_string(),
        })
    }

    /// A line of a watch.
    fn line(what: &str, object: &Value) -> String {
        json!({"type": what, "object": object}).to_string()
    }

    /// Counts a watch open until its body is dropped.
    struct Open(Arc<AtomicUsize>);

    impl Drop for Open {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    type Body = UnsyncBoxBody<Bytes, std::convert::Infallible>;

    /// A watch's answer: `lines`, then open until dropped unless it `ends`, counted as open
    /// while it lives.
    fn watch_body(lines: Vec<String>, ends: bool, open: Open) -> Body {
        let frames = futures::stream::iter(lines.into_iter().map(|l| Ok(Frame::data(Bytes::from(l + "\n")))));
        let rest =
            if ends { futures::stream::empty().left_stream() } else { futures::stream::pending().right_stream() };
        let frames = frames.chain(rest).map(move |frame| {
            let _open = &open;
            frame
        });
        http_body_util::StreamBody::new(frames).boxed_unsync()
    }

    fn json_body(status: u16, value: &Value) -> http::Response<Body> {
        let body = http_body_util::Full::new(Bytes::from(value.to_string())).boxed_unsync();
        http::Response::builder().status(status).body(body).unwrap()
    }

    impl tower::Service<http::Request<kube::client::Body>> for Events {
        type Response = http::Response<Body>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<kube::client::Body>) -> Self::Future {
            let (path, query) = (req.uri().path(), req.uri().query().unwrap_or_default());
            let mut stored = self.stored.lock().unwrap();
            stored.asked.push(format!("{path}?{query}"));
            let param = |key: &str| query.split('&').find_map(|p| p.strip_prefix(key)?.strip_prefix('='));
            if param("watch") == Some("true") {
                if stored.no_watch {
                    let refused = json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "Forbidden",
                        "code": 403, "message": "events is forbidden: User \"me\" cannot watch resource \"events\""});
                    return std::future::ready(Ok(json_body(403, &refused)));
                }
                // None queued: open, and quiet.
                let (lines, ends) = stored.watches.pop_front().unwrap_or_default();
                self.open.fetch_add(1, Ordering::SeqCst);
                let body = watch_body(lines, ends, Open(self.open.clone()));
                return std::future::ready(Ok(http::Response::new(body)));
            }
            let selector = param("fieldSelector").unwrap_or_default();
            let warnings_only = selector == "type%3DWarning";
            let uid = selector.strip_prefix("involvedObject.uid%3D");
            let limit = param("limit").and_then(|l| l.parse().ok()).unwrap_or(usize::MAX).min(stored.page);
            let from: usize = param("continue").and_then(|c| c.parse().ok()).unwrap_or(0);
            if from > 0 && stored.expire > 0 {
                stored.expire -= 1;
                let gone = json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "Expired", "code": 410,
                    "message": "The provided continue parameter is too old to display a consistent list result."});
                return std::future::ready(Ok(json_body(410, &gone)));
            }
            let one = path.strip_prefix("/api/v1/namespaces/").and_then(|p| p.strip_suffix("/events"));
            let text = |e: &Value, p: &str| e.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string();
            let mut kept: Vec<Value> = alphabet(stored.per_namespace)
                .chain(stored.added.iter().cloned())
                .filter(|e| one.is_none_or(|o| text(e, "/metadata/namespace") == o))
                .filter(|e| !warnings_only || text(e, "/type") == "Warning")
                .filter(|e| uid.is_none_or(|u| text(e, "/involvedObject/uid") == u))
                .collect();
            // In key order: by namespace, then by name.
            kept.sort_by_key(|e| (text(e, "/metadata/namespace"), text(e, "/metadata/name")));
            let items: Vec<Value> = kept.iter().skip(from).take(limit).cloned().collect();
            let next = from + items.len();
            let mut metadata = json!({"resourceVersion": stored.version});
            if next < kept.len() {
                metadata["continue"] = next.to_string().into();
            }
            std::future::ready(Ok(json_body(
                200,
                &json!({"kind": "EventList", "apiVersion": "v1", "metadata": metadata, "items": items}),
            )))
        }
    }

    /// The names of the events in `rows`, as shown: newest first.
    fn names(rows: &EventRows) -> Vec<String> {
        rows.rows.iter().map(|e| e.line.name.clone()).collect()
    }

    /// The rows the watch sends, until `ok` holds for them (10 s at most).
    async fn until(rx: &mut mpsc::UnboundedReceiver<EventsMsg>, ok: impl Fn(&EventRows) -> bool) -> EventRows {
        let rows = async {
            loop {
                match rx.next().await.expect("the watch stopped") {
                    EventsMsg::Rows(rows) if ok(&rows) => return rows,
                    EventsMsg::Rows(_) => {}
                    EventsMsg::Error(e) => panic!("events: {e}"),
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(10), rows).await.expect("the rows never came")
    }

    /// Waits until `ok` holds (5 s at most).
    async fn eventually(ok: impl Fn() -> bool) {
        for _ in 0..250 {
            if ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("never happened");
    }

    fn follow(
        cluster: &Events,
        normal: bool,
        pause: &Pause,
    ) -> (crate::k8s::TaskGuard, mpsc::UnboundedReceiver<EventsMsg>) {
        let client = kube::Client::new(cluster.clone(), "default");
        super::watch_events(client, None, normal, 100, Duration::ZERO, pause.clone())
    }

    /// After one list, the watch from its resourceVersion brings each change: an event added,
    /// one happening again, one deleted. When the server ends the watch, the next one goes on
    /// from the last change: nothing is listed again.
    #[tokio::test(flavor = "multi_thread")]
    async fn after_one_list_the_watch_brings_only_changes() {
        let cluster = Events::new(0, 500);
        for (name, at) in [("a", 10), ("b", 20), ("c", 30)] {
            cluster.add(warning("shop", name, at, 1, "5"));
        }
        let changes = [
            line("ADDED", &warning("shop", "d", 40, 1, "8")),
            line("MODIFIED", &warning("shop", "a", 50, 2, "9")),
            line("DELETED", &warning("shop", "b", 20, 1, "10")),
        ];
        cluster.watch(changes.to_vec(), true);
        cluster.watch(vec![line("ADDED", &warning("shop", "e", 60, 1, "11"))], false);
        let (_watch, mut rx) = follow(&cluster, false, &Pause::default());
        let first = until(&mut rx, |_| true).await;
        assert_eq!(names(&first), ["c", "b", "a"]);
        assert_eq!((first.kept, first.total), (3, 3));
        let last = until(&mut rx, |r| names(r) == ["e", "a", "d", "c"]).await;
        assert_eq!((last.kept, last.total), (4, 4));
        assert_eq!(last.rows[1].line.count, 2, "a happened again");
        assert_eq!(cluster.lists(), 1, "{:?}", cluster.asked());
        let watches = cluster.watches_asked();
        assert!(watches[0].contains("resourceVersion=7") && watches[1].contains("resourceVersion=10"), "{watches:?}");
        assert!(watches.iter().all(|q| q.contains("fieldSelector=type%3DWarning")), "{watches:?}");
    }

    /// A watch too far behind (410 Gone) lists again, then watches from that list.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_watch_too_far_behind_lists_again() {
        let cluster = Events::new(0, 500);
        cluster.add(warning("shop", "a", 10, 1, "5"));
        let gone =
            json!({"kind": "Status", "code": 410, "reason": "Expired", "message": "too old resource version: 7 (12)"});
        cluster.watch(vec![line("ERROR", &gone)], false);
        let (_watch, mut rx) = follow(&cluster, true, &Pause::default());
        assert_eq!(names(&until(&mut rx, |_| true).await), ["a"]);
        // What the cluster keeps by the time it lists again.
        cluster.add(warning("shop", "b", 20, 1, "15"));
        cluster.set_version("20");
        until(&mut rx, |r| names(r) == ["b", "a"]).await;
        assert_eq!(cluster.lists(), 2, "{:?}", cluster.asked());
        eventually(|| cluster.watches_asked().last().is_some_and(|q| q.contains("resourceVersion=20"))).await;
        assert!(cluster.watches_asked().iter().all(|q| !q.contains("fieldSelector")), "every event: no selector");
    }

    /// Off screen, nothing is read and no watch is open; shown again, the watch goes on from
    /// where it was, with what changed meanwhile, without listing again.
    #[tokio::test(flavor = "multi_thread")]
    async fn off_screen_no_watch_is_open() {
        let cluster = Events::new(0, 500);
        cluster.add(warning("shop", "a", 10, 1, "5"));
        cluster.watch(vec![], false);
        cluster.watch(vec![line("ADDED", &warning("shop", "b", 20, 1, "8"))], false);
        let pause = Pause::default();
        pause.set(true);
        let (_watch, mut rx) = follow(&cluster, false, &pause);
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(cluster.asked().is_empty(), "read while off screen: {:?}", cluster.asked());
        pause.set(false);
        assert_eq!(names(&until(&mut rx, |_| true).await), ["a"]);
        eventually(|| cluster.open_watches() == 1).await;
        pause.set(true);
        eventually(|| cluster.open_watches() == 0).await;
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(cluster.watches_asked().len(), 1, "a watch opened while off screen");
        pause.set(false);
        assert_eq!(names(&until(&mut rx, |r| r.rows.len() == 2).await), ["b", "a"]);
        assert_eq!(cluster.lists(), 1, "{:?}", cluster.asked());
        assert!(cluster.watches_asked()[1].contains("resourceVersion=7"), "{:?}", cluster.asked());
    }

    /// Allowed to list the events but not to watch them: they are listed again every `every`,
    /// with what changed and no error, but not while off screen; the watch isn't asked again.
    #[tokio::test(flavor = "multi_thread")]
    async fn without_the_right_to_watch_they_are_listed_again() {
        let cluster = Events::new(0, 500);
        cluster.add(warning("shop", "a", 10, 1, "5"));
        cluster.refuse_watches();
        let (client, pause) = (kube::Client::new(cluster.clone(), "default"), Pause::default());
        let every = Duration::from_millis(200);
        let (_watch, mut rx) = super::watch_events(client, None, false, 100, every, pause.clone());
        assert_eq!(names(&until(&mut rx, |_| true).await), ["a"]);
        cluster.add(warning("shop", "b", 20, 1, "6"));
        assert_eq!(names(&until(&mut rx, |r| r.rows.len() == 2).await), ["b", "a"]);
        assert!(cluster.lists() >= 2, "{:?}", cluster.asked());
        assert_eq!(cluster.watches_asked().len(), 1, "{:?}", cluster.asked());
        pause.set(true);
        // A list already started ends.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let listed = cluster.lists();
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(cluster.lists(), listed, "listed while off screen: {:?}", cluster.asked());
    }

    /// A list that pages for longer than etcd keeps its history is refused its next page: it
    /// starts over once, and says so when refused again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_list_refused_as_too_old_starts_over_once() {
        let cluster = Events::new(3, 10);
        cluster.expire(1);
        let client = kube::Client::new(cluster.clone(), "default");
        let (mut held, _) = super::list_events(&client, "/api/v1/events", false, EVENTS_KEPT).await.unwrap();
        assert_eq!((held.rows().kept, held.total), (26 * 3, 26 * 3));
        let first_pages = |c: &Events| c.asked().iter().filter(|q| !q.contains("continue=")).count();
        assert_eq!(first_pages(&cluster), 2, "{:?}", cluster.asked());
        let cluster = Events::new(3, 10);
        cluster.expire(2);
        let client = kube::Client::new(cluster.clone(), "default");
        let Err(e) = super::list_events(&client, "/api/v1/events", false, EVENTS_KEPT).await else {
            panic!("refused twice, and read");
        };
        assert_eq!(crate::k8s::api_status(&e), Some(410));
        assert_eq!(first_pages(&cluster), 2, "{:?}", cluster.asked());
    }

    /// More Warnings than one page holds, over namespaces a to z: those of every namespace come
    /// back, z's too, though the API server lists them by namespace and the first page ends
    /// before z.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_page_of_warnings_is_read() {
        let cluster = Events::new(200, usize::MAX);
        let client = kube::Client::new(cluster.clone(), "default");
        let (mut held, version) = super::list_events(&client, "/api/v1/events", false, EVENTS_KEPT).await.unwrap();
        let events = held.rows().rows;
        let namespaces: BTreeSet<&str> = events.iter().filter_map(|e| e.object_namespace.as_deref()).collect();
        assert_eq!(namespaces.len(), 26, "{namespaces:?}");
        assert_eq!(events.len(), 26 * 200);
        assert!(events.iter().all(|e| e.line.kind == "Warning" && e.line.source == "kubelet"));
        assert_eq!(version, "7", "the list's resourceVersion, to watch from");
        // 5,200 Warnings: two pages, both of Warnings only.
        let asked = cluster.asked();
        assert_eq!(asked.len(), 2, "{asked:?}");
        assert!(asked.iter().all(|q| q.starts_with("/api/v1/events?") && q.contains("fieldSelector=type%3DWarning")));
    }

    /// With Normal events shown too, every event of every namespace, in one list; for one
    /// namespace, its own.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_event_when_normal_ones_are_shown_too() {
        let cluster = Events::new(200, usize::MAX);
        let client = kube::Client::new(cluster.clone(), "default");
        let events = super::list_events(&client, "/api/v1/events", true, EVENTS_KEPT).await.unwrap().0.rows().rows;
        assert_eq!(events.len(), 26 * 200 * 2);
        let in_z = |kind: &str| {
            events.iter().filter(|e| e.object_namespace.as_deref() == Some("z") && e.line.kind == kind).count()
        };
        assert_eq!((in_z("Warning"), in_z("Normal")), (200, 200));
        // 10,400 events: three pages, with no type asked for.
        let asked = cluster.asked();
        assert_eq!(asked.len(), 3, "{asked:?}");
        assert!(asked.iter().all(|q| !q.contains("fieldSelector")), "{asked:?}");
        let events =
            super::list_events(&client, "/api/v1/namespaces/z/events", false, EVENTS_KEPT).await.unwrap().0.rows().rows;
        assert_eq!(events.len(), 200);
        assert!(events.iter().all(|e| e.object_namespace.as_deref() == Some("z")));
        assert!(cluster.asked().last().unwrap().starts_with("/api/v1/namespaces/z/events?"));
    }

    /// More events than the cap: the newest are held, and the rows say how many of how many,
    /// and from when.
    #[tokio::test(flavor = "multi_thread")]
    async fn only_the_newest_are_held() {
        let cluster = Events::new(200, usize::MAX);
        let client = kube::Client::new(cluster.clone(), "default");
        let (mut held, _) = super::list_events(&client, "/api/v1/events", false, 1000).await.unwrap();
        let rows = held.rows();
        assert_eq!((rows.rows.len(), rows.kept, rows.total), (1000, 1000, 26 * 200));
        // Each namespace's pods 0 to 199 have their Warnings 2 s apart: the newest 1000 are
        // those of pods 162 to 199 (26 × 38 = 988), then 12 of pod 161's (by namespace).
        let oldest = rows.rows.iter().filter_map(|e| e.line.last).min();
        assert_eq!(rows.since, oldest);
        assert_eq!(oldest.map(jiff::Timestamp::as_second), Some(BASE + 2 * 161 + 1));
        assert!(rows.rows.windows(2).all(|w| w[0].line.last >= w[1].line.last), "newest first");
    }

    /// An object's events are read the same lean way, by its uid, newest first.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_object_s_events_newest_first() {
        let cluster = Events::new(3, usize::MAX);
        let kind = crate::k8s::Kind::fake("", "Pod");
        let pod = crate::k8s::ObjectRef { kind, name: "web-1".into(), namespace: Some("z".into()), uid: "z-1".into() };
        let lines = crate::k8s::events_for(kube::Client::new(cluster.clone(), "default"), pod).await.unwrap();
        let read: Vec<(&str, &str)> = lines.iter().map(|l| (l.kind.as_str(), l.name.as_str())).collect();
        assert_eq!(read, [("Warning", "web-00001.Warning"), ("Normal", "web-00001.Normal")]);
        let asked = cluster.asked();
        assert_eq!(asked.len(), 1, "{asked:?}");
        assert!(asked[0].starts_with("/api/v1/namespaces/z/events?") && asked[0].contains("involvedObject.uid%3Dz-1"));
    }

    /// The lines of a watch: a bookmark moves the resourceVersion on, 410 lists again, another
    /// error ends the watch with its message.
    #[test]
    fn watch_lines() {
        let (mut held, mut version) = (Held::new(10), "7".to_string());
        let mut apply = |l: &str| super::apply_line(l, &mut held, &mut version);
        let bookmark = r#"{"type":"BOOKMARK","object":{"kind":"Event","metadata":{"resourceVersion":"42","creationTimestamp":null}}}"#;
        assert_eq!(apply(bookmark).unwrap(), Applied::Unchanged);
        assert_eq!(apply("").unwrap(), Applied::Unchanged);
        let gone = r#"{"type":"ERROR","object":{"kind":"Status","code":410,"message":"too old"}}"#;
        assert_eq!(apply(gone).unwrap(), Applied::Relist);
        let failed = r#"{"type":"ERROR","object":{"kind":"Status","code":500,"message":"etcd timed out"}}"#;
        assert_eq!(apply(failed).unwrap_err().to_string(), "etcd timed out");
        assert!(apply("not json").is_err());
        assert_eq!(apply(&line("ADDED", &warning("shop", "a", 1, 1, "43"))).unwrap(), Applied::Changed);
        assert_eq!(version, "43");
        assert_eq!((held.events.len(), held.total), (1, 1));
    }

    /// An event of pod `web-<k>` named `e<k>`, last at `at` seconds.
    fn event_at(k: u16, at: i64) -> ClusterEvent {
        ClusterEvent {
            line: EventLine {
                namespace: Some("shop".into()),
                name: format!("e{k:03}"),
                age: String::new(),
                first: None,
                last: Some(jiff::Timestamp::from_second(BASE + at).unwrap()),
                kind: "Warning".into(),
                reason: "BackOff".into(),
                message: String::new(),
                count: 1,
                source: String::new(),
            },
            object_kind: "Pod".into(),
            object_namespace: Some("shop".into()),
            object_name: format!("web-{k}"),
            events: 1,
        }
    }

    fn held_keys(held: &Held) -> BTreeSet<u16> {
        held.events.values().map(|e| e.line.name[1..].parse().unwrap()).collect()
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]
        /// Every event the API server keeps is read once, however few it sends a page: none
        /// left out, none twice, and no request more than its pages.
        #[test]
        fn every_event_is_read_once(per_namespace in 0usize..8, page in 1usize..60, normal in any::<bool>()) {
            let cluster = Events::new(per_namespace, page);
            let client = cluster.clone();
            let (mut held, _) = futures::executor::block_on(crate::k8s::run(async move {
                super::list_events(&kube::Client::new(client, "default"), "/api/v1/events", normal, EVENTS_KEPT).await
            }))
            .unwrap();
            let events = held.rows().rows;
            let stored = 26 * per_namespace * (1 + usize::from(normal));
            // Each event is about its own pod, so each is a row of its own.
            prop_assert_eq!(events.len(), stored);
            prop_assert_eq!(events.iter().map(|e| e.events).sum::<usize>(), stored);
            prop_assert!(normal || events.iter().all(|e| e.line.kind == "Warning"));
            let namespaces: BTreeSet<&str> = events.iter().filter_map(|e| e.object_namespace.as_deref()).collect();
            prop_assert_eq!(namespaces.len(), if per_namespace == 0 { 0 } else { 26 });
            prop_assert_eq!(cluster.asked().len(), stored.div_ceil(page).max(1));
        }
    }

    proptest! {
        /// Whatever the list and the changes after it: right after the list, the newest `cap`
        /// are held; then what is held is always part of what the cluster keeps, as it changed
        /// last, never more than a quarter over the cap (and never over it once rows go); the
        /// count is what the cluster keeps; and rows come newest first.
        #[test]
        fn the_newest_are_held(
            listed in prop::collection::btree_map(0u16..60, 0i64..100, 0..40),
            changes in prop::collection::vec((0u8..3, 0u16..80, 0i64..200), 0..60),
            cap in 1usize..20,
        ) {
            let mut held = Held::new(cap);
            let mut live: BTreeMap<u16, i64> = BTreeMap::new();
            for (&k, &at) in &listed {
                held.listed(event_at(k, at));
                live.insert(k, at);
            }
            held.trim();
            let mut by_time: Vec<(i64, u16)> = live.iter().map(|(&k, &at)| (at, k)).collect();
            by_time.sort_unstable();
            let newest: BTreeSet<u16> = by_time.iter().rev().take(cap).map(|&(_, k)| k).collect();
            prop_assert_eq!(held_keys(&held), newest);
            for (what, k, at) in changes {
                // As a real server says it: ADDED for one it didn't have, MODIFIED or DELETED
                // for one it has.
                let what = match (what, live.contains_key(&k)) {
                    (_, false) => "ADDED",
                    (0 | 1, true) => "MODIFIED",
                    _ => "DELETED",
                };
                held.change(what, event_at(k, at));
                if what == "DELETED" {
                    live.remove(&k);
                } else {
                    live.insert(k, at);
                }
                prop_assert!(held.events.len() <= cap + cap / 4);
                prop_assert_eq!(held.total, live.len());
                for e in held.events.values() {
                    let k: u16 = e.line.name[1..].parse().unwrap();
                    prop_assert_eq!(live.get(&k).copied(), e.line.last.map(|t| t.as_second() - BASE));
                }
            }
            let rows = held.rows();
            prop_assert!(rows.kept <= cap);
            prop_assert_eq!(rows.kept, held.events.len());
            prop_assert_eq!(rows.total, live.len());
            prop_assert_eq!(rows.rows.iter().map(|e| e.events).sum::<usize>(), rows.kept);
            prop_assert!(rows.rows.windows(2).all(|w| w[0].line.last >= w[1].line.last), "newest first");
        }
    }
}
