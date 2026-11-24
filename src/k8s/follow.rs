//! Following objects with a watch, the way the resource lists stay live (`table`): one list,
//! then the changes the API server streams from its watch cache, and a new list when the watch
//! is too old (410 Gone). A detail page follows its object this way (a field selector on its
//! name) and the pods it lists (their label or field selector), instead of reading them again
//! every few seconds.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures::channel::mpsc;
use futures::{AsyncBufReadExt as _, StreamExt as _};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ListMeta;
use kube::api::{DynamicObject, ListParams, WatchParams};
use kube::core::Request;
use kube::discovery::ApiResource;
use kube::{Client, Resource as _};
use serde::Deserialize;
use serde_json::Value;

use super::table::{Backoff, RawEvent, WATCH_IDLE, null_default, resumable, truncate};
use super::{Pause, TaskGuard, api_status, lists, spawn_guarded};

/// A page's own object: its changes go at most this often (each one redraws its YAML).
const OBJECT_EVERY: Duration = Duration::from_millis(250);

/// While paused, how often to look whether the page is shown again.
const PAUSED_POLL: Duration = Duration::from_millis(500);

/// How long the API server keeps a watch open before it ends it (it is opened again).
const WATCH_TIMEOUT: u32 = 290;

/// Whether a follow keeps an object (see `Follow::keep`).
pub type Keep = Arc<dyn Fn(&Value) -> bool + Send + Sync>;

/// Which objects of a kind to follow, and how.
#[derive(Clone)]
pub struct Follow {
    pub ar: ApiResource,
    /// Their namespace; None for a cluster-scoped kind, or every namespace.
    pub namespace: Option<String>,
    pub labels: Option<String>,
    pub fields: Option<String>,
    /// Only the objects this keeps, for what the API server can't select (the pods that mount a
    /// claim). One that stops being kept goes.
    pub keep: Option<Keep>,
    /// Keep `metadata.managedFields`: a page's own object shows them, its pods need none.
    pub managed: bool,
    /// The first list may come from the API server's cache (`resourceVersion=0`), a moment old
    /// at worst: for a big list with nothing shown before it. The lists after a failure or a
    /// 410 are consistent reads, never older than what was shown.
    pub cached: bool,
    /// The shortest time between two sends: changes closer together go as one.
    pub every: Duration,
    /// How often to list again where the watch is refused (403, or a kind that can't be
    /// watched) but lists work.
    pub poll: Duration,
}

impl Follow {
    /// One object, by its name: a field selector the API server serves from its watch cache.
    /// Its managed fields stay, and its first list is a consistent read: the page shows the
    /// object it just read, and an older copy must not replace it, nor say it is gone.
    pub fn object(ar: ApiResource, namespace: Option<String>, name: &str, poll: Duration) -> Self {
        let wanted = name.to_string();
        Self {
            ar,
            namespace,
            labels: None,
            fields: Some(format!("metadata.name={}", field_value(name))),
            // An aggregated API server may leave the selector out and send every object: the
            // page shows the one of its name, never another.
            keep: Some(Arc::new(move |obj: &Value| text(obj, "/metadata/name") == wanted)),
            managed: true,
            cached: false,
            every: OBJECT_EVERY,
            poll,
        }
    }

    /// An object as the follow holds it: typed (a list's items say neither their apiVersion nor
    /// their kind), without its managed fields unless they are kept; None when not kept.
    fn prepare(&self, obj: Value) -> Option<Value> {
        let Value::Object(mut map) = obj else { return None };
        if !self.managed
            && let Some(meta) = map.get_mut("metadata").and_then(Value::as_object_mut)
        {
            meta.remove("managedFields");
        }
        let obj = if map.contains_key("kind") {
            Value::Object(map)
        } else {
            // First, where the API server writes them (and the YAML shows them).
            let mut typed = serde_json::Map::with_capacity(map.len() + 2);
            typed.insert("apiVersion".into(), self.ar.api_version.clone().into());
            typed.insert("kind".into(), self.ar.kind.clone().into());
            typed.extend(map);
            Value::Object(typed)
        };
        self.keep.as_ref().is_none_or(|keep| keep(&obj)).then_some(obj)
    }
}

/// A value in a field selector, its `\`, `,` and `=` escaped as the API server reads them.
fn field_value(value: &str) -> String {
    value.replace('\\', r"\\").replace(',', r"\,").replace('=', r"\=")
}

/// What a follow says.
#[derive(Debug)]
pub enum Followed {
    /// The objects now, sorted by namespace and name: after the first list, then after changes.
    Objects(Arc<Vec<Value>>),
    /// A list or the watch failed, said as a section's note says it ("failed: …"). It is tried
    /// again, waiting longer each time; the objects come again once it works.
    Error(String),
    /// The objects can't be listed here (403, or a kind without lists): the follow stops.
    Refused(String),
}

/// Follow `what`: its objects once listed, then again after each change, at most every
/// `what.every`. While `pause` is set the changes still apply, and the latest objects go once
/// it is cleared. Dropping the guard stops it.
pub fn follow(client: Client, what: Follow, pause: Pause) -> (TaskGuard, mpsc::UnboundedReceiver<Followed>) {
    let (tx, rx) = mpsc::unbounded();
    let guard = spawn_guarded(run(client, what, pause, tx));
    (guard, rx)
}

async fn run(client: Client, what: Follow, pause: Pause, tx: mpsc::UnboundedSender<Followed>) {
    let url = DynamicObject::url_path(&what.ar, what.namespace.as_deref());
    let mut held = Held::default();
    let mut outbox = Outbox::default();
    let mut backoff = Backoff::default();
    let mut cached = what.cached;
    // The watch was refused while lists work: list every `poll` instead.
    let mut polling = false;
    // After the first list, or one that follows a failure, the objects go even if they didn't
    // change: the page learns they are listed, or that reads work again.
    let mut said = false;
    loop {
        if polling {
            // A hidden page lists nothing.
            wait_unpaused(&pause).await;
        }
        let listed = match list(&client, &url, &what, cached).await {
            Ok(listed) => Some(listed),
            Err(e) if refused(&e) => {
                let _ = tx.unbounded_send(Followed::Refused(lists::load_error(&e)));
                return;
            }
            // The kind is not served any more (its CRD deleted): none of its objects is left.
            Err(e) if api_status(&e) == Some(404) => None,
            Err(e) => {
                said = false;
                cached = false;
                if tx.unbounded_send(Followed::Error(lists::load_error(&e))).is_err() {
                    return;
                }
                tokio::time::sleep(backoff.next()).await;
                continue;
            }
        };
        let gone = listed.is_none();
        let (items, version) = listed.unwrap_or_default();
        if held.replace(items, version) || !said {
            outbox.dirty = true;
        }
        said = true;
        cached = false;
        if gone {
            // Said once the page is shown; asked again less and less often, until it is back.
            wait_unpaused(&pause).await;
            if outbox.send(&held, &tx).is_err() {
                return;
            }
            tokio::time::sleep(backoff.next()).await;
            continue;
        }
        if polling {
            backoff = Backoff::default();
            if outbox.send(&held, &tx).is_err() {
                return;
            }
            tokio::time::sleep(what.poll).await;
            continue;
        }
        // Whether the watch got frames: only then does the wait start over. A list alone doesn't
        // count, or a watch that keeps failing would list again every second.
        let mut progress = false;
        let result = watch(&client, &url, &what, &mut held, &mut outbox, &tx, &pause, &mut progress).await;
        let failed = match result {
            Ok(Ended::Relist) => false,
            Ok(Ended::Refused) => {
                polling = true;
                false
            }
            Err(e) => {
                said = false;
                if tx.unbounded_send(Followed::Error(lists::load_error(&e))).is_err() {
                    return;
                }
                true
            }
        };
        if tx.is_closed() {
            return;
        }
        if progress {
            backoff = Backoff::default();
        }
        // A new list right after progress (410 Gone) goes at once; a failing server is tried
        // less and less often.
        if !polling && (failed || !progress) {
            tokio::time::sleep(backoff.next()).await;
        }
    }
}

/// Whether a list was refused for good: forbidden, or a kind that can't be listed.
fn refused(e: &anyhow::Error) -> bool {
    matches!(api_status(e), Some(403 | 405))
}

/// While the page is hidden, wait.
async fn wait_unpaused(pause: &Pause) {
    while pause.is_paused() {
        tokio::time::sleep(PAUSED_POLL).await;
    }
}

/// A list's items, whole (a page shows them), and its resourceVersion.
#[derive(Deserialize)]
struct ListOf {
    #[serde(default, deserialize_with = "null_default")]
    metadata: ListMeta,
    #[serde(default, deserialize_with = "null_default")]
    items: Vec<Value>,
}

/// One list of what `what` follows: its objects as they are held, and the version to watch from.
async fn list(client: &Client, url: &str, what: &Follow, cached: bool) -> Result<(Vec<Value>, String)> {
    let mut params = ListParams::default();
    if cached {
        params = params.match_any();
    }
    if let Some(labels) = &what.labels {
        params = params.labels(labels);
    }
    if let Some(fields) = &what.fields {
        params = params.fields(fields);
    }
    let list: ListOf = lists::request_idle(client, Request::new(url).list(&params)?).await?;
    let items = list.items.into_iter().filter_map(|item| what.prepare(item)).collect();
    Ok((items, list.metadata.resource_version.unwrap_or_default()))
}

/// How a watch ended, when it did not fail.
enum Ended {
    /// Too old (410 Gone): list again.
    Relist,
    /// Refused (403, or a kind that can't be watched): list every `poll` instead.
    Refused,
}

/// Watch from the held version, applying the changes and sending them as they are due, until
/// the watch is too old or refused. A stream the server ended, one that broke, or one silent
/// for too long is watched again from the version; a server that fails is tried again, waiting
/// longer each time. `progress` is set once a frame came.
#[allow(clippy::too_many_arguments)]
async fn watch(
    client: &Client,
    url: &str,
    what: &Follow,
    held: &mut Held,
    outbox: &mut Outbox,
    tx: &mpsc::UnboundedSender<Followed>,
    pause: &Pause,
    progress: &mut bool,
) -> Result<Ended> {
    let mut params = WatchParams::default().timeout(WATCH_TIMEOUT);
    if let Some(labels) = &what.labels {
        params = params.labels(labels);
    }
    if let Some(fields) = &what.fields {
        params = params.fields(fields);
    }
    let request = Request::new(url);
    let mut backoff = Backoff::default();
    let mut retrying = false;
    loop {
        outbox.flush(held, tx, pause, what.every)?;
        let reader = match client.request_stream(request.watch(&params, &held.version)?).await {
            Ok(reader) => reader,
            Err(kube::Error::Api(status)) if status.code == 410 => return Ok(Ended::Relist),
            Err(kube::Error::Api(status)) if matches!(status.code, 403 | 405) => return Ok(Ended::Refused),
            Err(e) if resumable(&e) => {
                tx.unbounded_send(Followed::Error(lists::load_error(&e.into())))?;
                retrying = true;
                tokio::time::sleep(backoff.next()).await;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if std::mem::take(&mut retrying) {
            // Send the objects again: they clear the error said meanwhile.
            outbox.dirty = true;
        }
        let mut lines = std::pin::pin!(reader.lines());
        let mut heard = tokio::time::Instant::now();
        // Whether this stream brought a frame: one ended at once with none (a proxy that closes
        // it) waits like a broken one, rather than being opened again in a loop.
        let mut framed = false;
        let mut cut = false;
        loop {
            let due = outbox.wait(pause, what.every);
            let next = tokio::select! {
                line = lines.next() => Some(line),
                () = async {
                    match due {
                        Some(wait) => tokio::time::sleep(wait).await,
                        None => std::future::pending().await,
                    }
                } => None,
                // A healthy watch gets a bookmark about once a minute even when nothing
                // changes; silence this long is a connection that died without closing (sleep,
                // VPN, NAT). Watch again from the version, without a new list.
                () = tokio::time::sleep_until(heard + WATCH_IDLE) => break,
            };
            match next {
                // The server ended the watch (its timeout): again from the version.
                Some(None) => {
                    cut = !framed;
                    break;
                }
                // The connection broke (a load balancer, an API server restart).
                Some(Some(Err(_))) => {
                    cut = true;
                    break;
                }
                Some(Some(Ok(line))) => {
                    heard = tokio::time::Instant::now();
                    framed = true;
                    backoff = Backoff::default();
                    *progress = true;
                    match held.apply(&line, what)? {
                        Applied::Changed => outbox.dirty = true,
                        Applied::Unchanged => {}
                        Applied::Relist => return Ok(Ended::Relist),
                    }
                }
                None => {}
            }
            outbox.flush(held, tx, pause, what.every)?;
        }
        if cut {
            tokio::time::sleep(backoff.next()).await;
        }
    }
}

/// What the follow owes its page: whether the objects changed since they were last sent, and
/// when that was.
#[derive(Default)]
struct Outbox {
    dirty: bool,
    sent: Option<tokio::time::Instant>,
}

impl Outbox {
    /// How long until the objects may go: None when nothing is pending, a short wait while
    /// paused (to look again), else what is left of `every` since the last send.
    fn wait(&self, pause: &Pause, every: Duration) -> Option<Duration> {
        if !self.dirty {
            None
        } else if pause.is_paused() {
            Some(PAUSED_POLL)
        } else {
            Some(self.sent.map_or(Duration::ZERO, |at| every.saturating_sub(at.elapsed())))
        }
    }

    /// Send the objects if they are due.
    fn flush(
        &mut self,
        held: &Held,
        tx: &mpsc::UnboundedSender<Followed>,
        pause: &Pause,
        every: Duration,
    ) -> Result<()> {
        if self.wait(pause, every) == Some(Duration::ZERO) {
            self.send(held, tx)?;
        }
        Ok(())
    }

    /// Send the objects now, if they changed.
    fn send(&mut self, held: &Held, tx: &mpsc::UnboundedSender<Followed>) -> Result<()> {
        if self.dirty {
            tx.unbounded_send(Followed::Objects(held.objects.clone()))?;
            self.dirty = false;
            self.sent = Some(tokio::time::Instant::now());
        }
        Ok(())
    }
}

/// What one watch frame did to the objects.
#[derive(Debug, PartialEq, Eq)]
enum Applied {
    Changed,
    Unchanged,
    /// 410 Gone: the watch must start over from a new list.
    Relist,
}

/// The objects followed, sorted by namespace and name, and the resourceVersion to watch from.
/// The page holds the same objects (`Followed::Objects`): a change copies them once, then
/// changes the copy until it is sent.
#[derive(Default)]
struct Held {
    objects: Arc<Vec<Value>>,
    version: String,
}

/// A text field of an object, empty when it has none.
fn text<'a>(obj: &'a Value, path: &str) -> &'a str {
    obj.pointer(path).and_then(Value::as_str).unwrap_or_default()
}

/// Where an object goes among the others: by namespace, then name.
fn place_of(obj: &Value) -> (&str, &str) {
    (text(obj, "/metadata/namespace"), text(obj, "/metadata/name"))
}

impl Held {
    /// The objects of a new list, and its version: whether they differ from those held.
    fn replace(&mut self, mut items: Vec<Value>, version: String) -> bool {
        items.sort_by(|a, b| place_of(a).cmp(&place_of(b)));
        self.version = version;
        if *self.objects == items {
            return false;
        }
        self.objects = Arc::new(items);
        true
    }

    /// Apply one watch frame (a line of the stream) by namespace and name, keeping the
    /// resourceVersion to resume from. Errors other than 410 end the watch.
    fn apply(&mut self, line: &str, what: &Follow) -> Result<Applied> {
        if line.trim().is_empty() {
            return Ok(Applied::Unchanged);
        }
        let event: RawEvent =
            serde_json::from_str(line).map_err(|e| anyhow!("bad watch frame ({e}): {}", truncate(line, 200)))?;
        match event.type_.as_str() {
            "ADDED" | "MODIFIED" | "DELETED" => {
                let obj: Value = serde_json::from_str(event.object.get())?;
                if let Some(version) = obj.pointer("/metadata/resourceVersion").and_then(Value::as_str) {
                    version.clone_into(&mut self.version);
                }
                let namespace = text(&obj, "/metadata/namespace").to_string();
                let name = text(&obj, "/metadata/name").to_string();
                let uid = text(&obj, "/metadata/uid").to_string();
                let changed = if event.type_ == "DELETED" {
                    self.take_out(&namespace, &name, &uid)
                } else if let Some(obj) = what.prepare(obj) {
                    self.put(obj)
                } else {
                    self.take_out(&namespace, &name, &uid)
                };
                Ok(if changed { Applied::Changed } else { Applied::Unchanged })
            }
            // Only a newer resourceVersion: resuming from it avoids a 410 and a new list.
            "BOOKMARK" => {
                let obj: Value = serde_json::from_str(event.object.get()).unwrap_or_default();
                if let Some(version) =
                    obj.pointer("/metadata/resourceVersion").and_then(Value::as_str).filter(|v| !v.is_empty())
                {
                    version.clone_into(&mut self.version);
                }
                Ok(Applied::Unchanged)
            }
            "ERROR" => {
                let status: Value = serde_json::from_str(event.object.get()).unwrap_or_default();
                if status.get("code").and_then(Value::as_i64) == Some(410) {
                    return Ok(Applied::Relist);
                }
                Err(anyhow!("{}", status.get("message").and_then(Value::as_str).unwrap_or("watch error")))
            }
            _ => Ok(Applied::Unchanged),
        }
    }

    /// Where the object of this namespace and name is, or would go.
    fn find(&self, namespace: &str, name: &str) -> std::result::Result<usize, usize> {
        self.objects.binary_search_by(|o| place_of(o).cmp(&(namespace, name)))
    }

    /// Put an object in its place, over the one of its name: whether anything changed.
    fn put(&mut self, obj: Value) -> bool {
        let at = self.find(text(&obj, "/metadata/namespace"), text(&obj, "/metadata/name"));
        match at {
            Ok(i) if self.objects[i] == obj => false,
            Ok(i) => {
                Arc::make_mut(&mut self.objects)[i] = obj;
                true
            }
            Err(i) => {
                Arc::make_mut(&mut self.objects).insert(i, obj);
                true
            }
        }
    }

    /// Take out the object of this name if it is that one (by uid): whether it was there. A
    /// deletion of an object gone before, whose name was taken again, leaves the new one.
    fn take_out(&mut self, namespace: &str, name: &str, uid: &str) -> bool {
        match self.find(namespace, name) {
            Ok(i) if uid.is_empty() || text(&self.objects[i], "/metadata/uid") == uid => {
                Arc::make_mut(&mut self.objects).remove(i);
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use bytes::Bytes;
    use futures::StreamExt as _;
    use futures::channel::mpsc::UnboundedReceiver;
    use futures::stream::BoxStream;
    use http_body_util::StreamBody;
    use hyper::body::Frame;
    use k8s_openapi::api::core::v1::{ConfigMap, Pod};
    use proptest::prelude::*;
    use serde_json::{Value, json};

    use super::{Applied, Follow, Followed, Held, Pause, field_value, follow, text};
    use crate::k8s::ar_of;

    /// The pods labelled `app=web` in `shop`, each change sent at once.
    fn web_pods() -> Follow {
        Follow {
            ar: ar_of::<Pod>(),
            namespace: Some("shop".into()),
            labels: Some("app=web".into()),
            fields: None,
            keep: None,
            managed: false,
            cached: true,
            every: Duration::ZERO,
            poll: Duration::from_millis(50),
        }
    }

    /// A pod as a watch frame carries it: typed, with managed fields.
    fn pod(name: &str, uid: &str, version: &str) -> Value {
        json!({
            "apiVersion": "v1",
            "kind": "Pod",
            "metadata": {
                "name": name, "namespace": "shop", "uid": uid, "resourceVersion": version,
                "managedFields": [{"manager": "kubelet"}]
            },
            "status": {"phase": "Running"}
        })
    }

    /// A pod as a list's item carries it: without its apiVersion and kind.
    fn listed(name: &str, uid: &str, version: &str) -> Value {
        let mut pod = pod(name, uid, version);
        let map = pod.as_object_mut().unwrap();
        map.remove("apiVersion");
        map.remove("kind");
        pod
    }

    fn frame(kind: &str, obj: &Value) -> String {
        json!({"type": kind, "object": obj}).to_string()
    }

    fn bookmark(version: &str) -> String {
        json!({"type": "BOOKMARK", "object": {"kind": "Pod", "metadata": {"resourceVersion": version}}}).to_string()
    }

    fn pod_list(version: &str, items: Vec<Value>) -> Value {
        json!({"kind": "PodList", "apiVersion": "v1", "metadata": {"resourceVersion": version}, "items": items})
    }

    fn names(objects: &[Value]) -> Vec<(String, String)> {
        objects
            .iter()
            .map(|o| (text(o, "/metadata/name").to_string(), text(o, "/metadata/resourceVersion").to_string()))
            .collect()
    }

    #[test]
    fn events_apply_by_namespace_and_name() {
        let what = web_pods();
        let mut held = Held::default();
        let mut apply = |kind: &str, obj: &Value| held.apply(&frame(kind, obj), &what).unwrap();
        assert_eq!(apply("ADDED", &pod("web-2", "b", "5")), Applied::Changed);
        assert_eq!(apply("ADDED", &pod("web-1", "a", "6")), Applied::Changed);
        assert_eq!(apply("MODIFIED", &pod("web-1", "a", "7")), Applied::Changed);
        // A deletion of an object gone before, whose name was taken again, leaves the new one.
        assert_eq!(apply("DELETED", &pod("web-1", "old", "8")), Applied::Unchanged);
        assert_eq!(apply("DELETED", &pod("web-2", "b", "9")), Applied::Changed);
        assert_eq!(apply("DELETED", &pod("web-3", "c", "10")), Applied::Unchanged);
        assert_eq!(names(&held.objects), [("web-1".to_string(), "7".to_string())]);
        assert_eq!(held.version, "10");
        // A page's pods don't keep their managed fields.
        assert!(held.objects[0].pointer("/metadata/managedFields").is_none());
        // A bookmark only moves the version.
        assert_eq!(held.apply(&bookmark("42"), &what).unwrap(), Applied::Unchanged);
        assert_eq!(held.version, "42");
        // Too old: list again. Another error ends the watch.
        let gone = r#"{"type":"ERROR","object":{"kind":"Status","code":410,"message":"too old"}}"#;
        assert_eq!(held.apply(gone, &what).unwrap(), Applied::Relist);
        let failed = r#"{"type":"ERROR","object":{"kind":"Status","code":500,"message":"etcd is down"}}"#;
        assert!(held.apply(failed, &what).is_err());
        assert!(held.apply("not json", &what).is_err());
    }

    /// A pod that stops matching what the API server can't select (one done, here) goes.
    #[test]
    fn an_object_that_stops_being_kept_goes() {
        let mut what = web_pods();
        what.keep = Some(Arc::new(|o: &Value| text(o, "/status/phase") == "Running"));
        let mut held = Held::default();
        held.apply(&frame("ADDED", &pod("web-1", "a", "5")), &what).unwrap();
        let mut done = pod("web-1", "a", "6");
        done["status"]["phase"] = json!("Succeeded");
        assert_eq!(held.apply(&frame("MODIFIED", &done), &what).unwrap(), Applied::Changed);
        assert!(held.objects.is_empty());
        // Never kept: nothing to take out.
        assert_eq!(held.apply(&frame("ADDED", &done), &what).unwrap(), Applied::Unchanged);
    }

    #[test]
    fn an_object_is_followed_by_its_name_with_its_kind_and_managed_fields() {
        let what = Follow::object(ar_of::<ConfigMap>(), Some("shop".into()), "settings", Duration::from_secs(5));
        assert_eq!(what.fields.as_deref(), Some("metadata.name=settings"));
        let listed = json!({"metadata": {"name": "settings", "managedFields": [{"manager": "kubectl"}]}});
        let held = what.prepare(listed).unwrap();
        assert_eq!((held["apiVersion"].as_str(), held["kind"].as_str()), (Some("v1"), Some("ConfigMap")));
        // Its History tab reads its managed fields.
        assert!(held.pointer("/metadata/managedFields").is_some());
        // An object that says its kind is kept as it came.
        assert_eq!(what.prepare(held.clone()), Some(held));
        // An API server that leaves the selector out sends others: they are not the page's.
        assert_eq!(what.prepare(json!({"metadata": {"name": "other"}})), None);
        // Names RBAC allows that the selector would read otherwise.
        assert_eq!(field_value(r"a,b=c\d"), r"a\,b\=c\\d");
    }

    proptest! {
        /// The objects held are always the events' result: by namespace and name, the last
        /// that came of each, minus those deleted (that same object, by uid), sorted, each once.
        #[test]
        fn the_objects_held_follow_the_events(
            events in proptest::collection::vec((0usize..3, 0u8..2, 0u8..4, 0u8..3), 0..80),
        ) {
            let what = Follow { namespace: None, ..web_pods() };
            let mut held = Held::default();
            let mut model: BTreeMap<(String, String), (String, String)> = BTreeMap::new();
            for (i, (kind, ns, name, uid)) in events.into_iter().enumerate() {
                let (ns, name, uid, version) = (format!("ns{ns}"), format!("pod-{name}"), format!("u{uid}"), (i + 1).to_string());
                let obj = json!({"metadata": {"namespace": ns, "name": name, "uid": uid, "resourceVersion": version}});
                let kind = ["ADDED", "MODIFIED", "DELETED"][kind];
                held.apply(&frame(kind, &obj), &what).unwrap();
                let key = (ns, name);
                if kind != "DELETED" {
                    model.insert(key, (uid, version.clone()));
                } else if model.get(&key).is_some_and(|(u, _)| *u == uid) {
                    model.remove(&key);
                }
                prop_assert_eq!(&held.version, &version);
            }
            let got: Vec<(String, String, String, String)> = held
                .objects
                .iter()
                .map(|o| {
                    let t = |p| text(o, p).to_string();
                    (t("/metadata/namespace"), t("/metadata/name"), t("/metadata/uid"), t("/metadata/resourceVersion"))
                })
                .collect();
            let want: Vec<(String, String, String, String)> =
                model.into_iter().map(|((ns, name), (uid, version))| (ns, name, uid, version)).collect();
            prop_assert_eq!(got, want);
        }
    }

    /// What a fake API server answers.
    enum Answer {
        /// A JSON body with this HTTP status.
        Json(u16, Value),
        /// A watch: these lines, then the server ends it (false) or keeps it open for good (true).
        Watch(Vec<String>, bool),
    }

    type Body = StreamBody<BoxStream<'static, Result<Frame<Bytes>, Infallible>>>;

    /// How a fake API server answers a request (its path and query), the `n`th one (from 1).
    type Answers = dyn Fn(&str, usize) -> Answer + Send + Sync;

    /// A fake API server: answers the `n`th request (from 1) as `answer` says, and keeps every
    /// request's path and query.
    #[derive(Clone)]
    struct Fake {
        asked: Arc<Mutex<Vec<String>>>,
        answer: Arc<Answers>,
    }

    impl Fake {
        fn new(answer: impl Fn(&str, usize) -> Answer + Send + Sync + 'static) -> Self {
            Self { asked: Arc::default(), answer: Arc::new(answer) }
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl tower::Service<http::Request<kube::client::Body>> for Fake {
        type Response = http::Response<Body>;
        type Error = Infallible;
        type Future = futures::future::Ready<Result<Self::Response, Infallible>>;

        fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Infallible>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<kube::client::Body>) -> Self::Future {
            let asked = format!("{}?{}", req.uri().path(), req.uri().query().unwrap_or_default());
            let n = {
                let mut all = self.asked.lock().unwrap();
                all.push(asked.clone());
                all.len()
            };
            let (code, body): (u16, BoxStream<'static, Result<Frame<Bytes>, Infallible>>) =
                match (self.answer)(&asked, n) {
                    Answer::Json(code, value) => {
                        let body = Frame::data(Bytes::from(value.to_string()));
                        (code, futures::stream::once(futures::future::ready(Ok(body))).boxed())
                    }
                    Answer::Watch(lines, open) => {
                        let frames =
                            futures::stream::iter(lines.into_iter().map(|l| Ok(Frame::data(Bytes::from(l + "\n")))));
                        (200, if open { frames.chain(futures::stream::pending()).boxed() } else { frames.boxed() })
                    }
                };
            futures::future::ready(Ok(http::Response::builder().status(code).body(StreamBody::new(body)).unwrap()))
        }
    }

    fn status(code: u16) -> Answer {
        Answer::Json(
            code,
            json!({"kind": "Status", "apiVersion": "v1", "status": "Failure", "message": "no", "code": code}),
        )
    }

    /// The next message, within a few seconds.
    async fn next(rx: &mut UnboundedReceiver<Followed>) -> Option<Followed> {
        tokio::time::timeout(Duration::from_secs(5), rx.next()).await.expect("nothing came")
    }

    /// The objects sent, until they are `want` (by name and version).
    async fn until(rx: &mut UnboundedReceiver<Followed>, want: &[(&str, &str)]) -> Vec<Value> {
        let want: Vec<(String, String)> = want.iter().map(|(n, v)| ((*n).to_string(), (*v).to_string())).collect();
        loop {
            match next(rx).await {
                Some(Followed::Objects(objects)) if names(&objects) == want => return objects.to_vec(),
                Some(_) => {}
                None => panic!("the follow ended before {want:?}"),
            }
        }
    }

    /// Wait until the fake was asked `n` requests.
    async fn asked(fake: &Fake, n: usize) -> Vec<String> {
        for _ in 0..100 {
            let asked = fake.asked();
            if asked.len() >= n {
                return asked;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("asked only {:?}", fake.asked());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_follow_lists_then_applies_what_its_watch_says() {
        let fake = Fake::new(|asked, n| match n {
            1 => Answer::Json(200, pod_list("10", vec![listed("web-1", "a", "5")])),
            // The server ends the first watch after a bookmark: the next one goes from there.
            2 => Answer::Watch(
                vec![
                    frame("ADDED", &pod("web-2", "b", "11")),
                    frame("MODIFIED", &pod("web-1", "a", "12")),
                    bookmark("20"),
                ],
                false,
            ),
            _ if asked.contains("watch=true") => Answer::Watch(Vec::new(), true),
            _ => status(500),
        });
        let client = kube::Client::new(fake.clone(), "default");
        let (_follow, mut rx) = follow(client, web_pods(), Pause::default());
        let first = until(&mut rx, &[("web-1", "5")]).await;
        // Listed items are typed like the objects of a watch, and keep no managed fields.
        assert_eq!(first[0]["kind"], "Pod");
        assert!(first[0].pointer("/metadata/managedFields").is_none());
        until(&mut rx, &[("web-1", "12"), ("web-2", "11")]).await;
        let asked = asked(&fake, 3).await;
        // The first list from the watch cache, then watches from its version and the bookmark's.
        assert!(asked[0].contains("labelSelector=app%3Dweb") && asked[0].contains("resourceVersion=0"), "{asked:?}");
        assert!(!asked[0].contains("watch="), "{asked:?}");
        assert!(asked[1].contains("watch=true") && asked[1].contains("resourceVersion=10"), "{asked:?}");
        assert!(
            asked[1].contains("allowWatchBookmarks=true") && asked[1].contains("labelSelector=app%3Dweb"),
            "{asked:?}"
        );
        assert!(asked[2].contains("watch=true") && asked[2].contains("resourceVersion=20"), "{asked:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_watch_too_old_lists_again_from_etcd() {
        let gone = r#"{"type":"ERROR","object":{"kind":"Status","code":410,"message":"too old"}}"#.to_string();
        let fake = Fake::new(move |asked, n| match n {
            1 => Answer::Json(200, pod_list("10", vec![listed("web-1", "a", "5")])),
            2 => Answer::Watch(vec![gone.clone()], true),
            3 => Answer::Json(200, pod_list("30", vec![listed("web-1", "a", "30")])),
            _ if asked.contains("watch=true") => Answer::Watch(Vec::new(), true),
            _ => status(500),
        });
        let client = kube::Client::new(fake.clone(), "default");
        let (_follow, mut rx) = follow(client, web_pods(), Pause::default());
        until(&mut rx, &[("web-1", "30")]).await;
        let asked = asked(&fake, 4).await;
        // A consistent read this time: never older than what the page was shown.
        assert!(!asked[2].contains("watch=") && !asked[2].contains("resourceVersion"), "{asked:?}");
        assert!(asked[3].contains("watch=true") && asked[3].contains("resourceVersion=30"), "{asked:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_watch_lists_again_on_a_timer() {
        let fake = Fake::new(|asked, n| {
            if asked.contains("watch=true") {
                status(403)
            } else {
                Answer::Json(200, pod_list(&n.to_string(), vec![listed("web-1", "a", &n.to_string())]))
            }
        });
        let client = kube::Client::new(fake.clone(), "default");
        let (_follow, mut rx) = follow(client, web_pods(), Pause::default());
        until(&mut rx, &[("web-1", "1")]).await;
        // Listed again every `poll` (50 ms), and never watched again.
        until(&mut rx, &[("web-1", "4")]).await;
        let asked = fake.asked();
        assert_eq!(asked.iter().filter(|a| a.contains("watch=true")).count(), 1, "{asked:?}");
    }

    /// A kind the API server no longer serves (its CRD deleted) has no objects left: the page
    /// says its object is gone, and shows it again once the kind is back.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_kind_no_longer_served_has_no_objects() {
        let fake = Fake::new(|asked, n| match n {
            1 => status(404),
            _ if asked.contains("watch=true") => Answer::Watch(Vec::new(), true),
            _ => Answer::Json(200, pod_list("10", vec![listed("web-1", "a", "5")])),
        });
        let client = kube::Client::new(fake.clone(), "default");
        let (_follow, mut rx) = follow(client, web_pods(), Pause::default());
        until(&mut rx, &[]).await;
        until(&mut rx, &[("web-1", "5")]).await;
    }

    /// A watch the server (or a proxy) ends at once, with nothing in it, is opened again after
    /// a wait that grows, never in a loop.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_watch_ended_at_once_is_not_opened_again_in_a_loop() {
        let fake = Fake::new(|asked, _| {
            if asked.contains("watch=true") {
                Answer::Watch(Vec::new(), false)
            } else {
                Answer::Json(200, pod_list("10", vec![listed("web-1", "a", "5")]))
            }
        });
        let client = kube::Client::new(fake.clone(), "default");
        let (_follow, mut rx) = follow(client, web_pods(), Pause::default());
        until(&mut rx, &[("web-1", "5")]).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let asked = fake.asked();
        let watches = asked.iter().filter(|a| a.contains("watch=true")).count();
        assert!((1..=3).contains(&watches), "{asked:?}");
        // Watched again from the list's version: no new list.
        assert_eq!(asked.len() - watches, 1, "{asked:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_list_says_so_and_stops() {
        let fake = Fake::new(|_, _| status(403));
        let client = kube::Client::new(fake.clone(), "default");
        let (_follow, mut rx) = follow(client, web_pods(), Pause::default());
        assert!(matches!(next(&mut rx).await, Some(Followed::Refused(why)) if why == "forbidden"));
        assert!(next(&mut rx).await.is_none());
        assert_eq!(fake.asked().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_paused_follow_holds_back_the_objects_then_sends_the_latest() {
        let fake = Fake::new(|asked, n| match n {
            1 => Answer::Json(200, pod_list("10", vec![listed("web-1", "a", "5")])),
            _ if asked.contains("watch=true") => Answer::Watch(vec![frame("ADDED", &pod("web-2", "b", "11"))], true),
            _ => status(500),
        });
        let client = kube::Client::new(fake.clone(), "default");
        let pause = Pause::default();
        pause.set(true);
        let (_follow, mut rx) = follow(client, web_pods(), pause.clone());
        asked(&fake, 2).await;
        let held = tokio::time::timeout(Duration::from_millis(700), rx.next()).await;
        assert!(held.is_err(), "sent while paused: {held:?}");
        pause.set(false);
        // Everything that happened meanwhile, in one send.
        match next(&mut rx).await {
            Some(Followed::Objects(objects)) => {
                let want = [("web-1", "5"), ("web-2", "11")].map(|(n, v)| (n.to_string(), v.to_string()));
                assert_eq!(names(&objects), want);
            }
            other => panic!("{other:?}"),
        }
    }
}
