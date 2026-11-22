//! Pods' requests and limits, kept per pod by a watch: the usage bars of the pods list and the
//! cluster overview's totals read them.
//!
//! The API server can't send part of a pod: listing pods means their whole objects, about
//! 300 MB of JSON on a cluster with 20,800 pods. So they are listed once, each pod kept only as
//! the sums `podspec` makes of it, then followed by a watch from that list's `resourceVersion`.
//! Requests and limits change only when pods come and go or are resized in place, and each of
//! those is an event of the watch. Listing every pod again each minute, as the pages used to,
//! had the API server encode them all each time.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use futures::{AsyncBufReadExt as _, StreamExt as _};
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ListMeta;
use kube::api::{DynamicObject, ListParams, WatchParams};
use kube::core::Request;
use kube::{Client, Resource as _};
use serde::Deserialize;

use super::metrics::{pod_resources, retry_wait, wait_unpaused};
use super::table::{Applied, Backoff, RawEvent, WATCH_IDLE, resumable};
use super::{Pause, PodResources, TaskGuard, ar_of, lists, podspec, spawn_guarded};

/// A pod's phase, as far as the totals go: running and pending pods hold what they request
/// on their nodes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PodPhase {
    Pending,
    Running,
    /// Finished, or not known yet.
    #[default]
    Other,
}

impl PodPhase {
    fn of(phase: &str) -> Self {
        match phase {
            "Pending" => Self::Pending,
            "Running" => Self::Running,
            _ => Self::Other,
        }
    }
}

/// What one pod requests and limits.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PodAlloc {
    /// Set against its usage, for its usage bars: what the running pod is held to.
    pub resources: PodResources,
    phase: PodPhase,
    /// CPU (millicores) and memory (bytes) requested, and GPUs, as the scheduler counts them
    /// on its node: init containers count.
    cpu: f64,
    memory: f64,
    gpus: f64,
}

impl PodAlloc {
    fn of(pod: &LeanPod) -> Self {
        let spec = &pod.spec;
        let scheduled = |r: &str| spec.effective(r, podspec::Count::Scheduled);
        // A GPU is never shared: its request is its limit, and a spec may set only the limit.
        let gpus =
            spec.resource_names().iter().filter(|r| r.ends_with("/gpu")).map(|r| scheduled(r).limit).sum::<f64>();
        Self {
            resources: pod_resources(spec),
            phase: PodPhase::of(&pod.status.phase),
            cpu: scheduled("cpu").request,
            memory: scheduled("memory").request,
            gpus,
        }
    }
}

/// Every pod's `PodAlloc`, by namespace then name: the table looks a row up with its own
/// strings. Each namespace's pods are shared, so a change while a reader holds what it was sent
/// copies that namespace's pods, not every pod.
#[derive(Clone, Debug, Default)]
pub struct PodAllocs {
    by_namespace: HashMap<String, Arc<HashMap<String, PodAlloc>>>,
}

impl PodAllocs {
    pub fn get(&self, namespace: &str, name: &str) -> Option<&PodAlloc> {
        self.by_namespace.get(namespace)?.get(name)
    }

    /// How many pods there are and run, and what the running and pending ones request.
    pub fn totals(&self) -> PodTotals {
        let mut totals = PodTotals::default();
        for pod in self.by_namespace.values().flat_map(|pods| pods.values()) {
            totals.pods += 1;
            totals.running += usize::from(pod.phase == PodPhase::Running);
            if matches!(pod.phase, PodPhase::Running | PodPhase::Pending) {
                totals.cpu += pod.cpu;
                totals.memory += pod.memory;
                totals.gpus += pod.gpus;
            }
        }
        totals
    }

    fn insert(&mut self, namespace: String, name: String, alloc: PodAlloc) {
        Arc::make_mut(self.by_namespace.entry(namespace).or_default()).insert(name, alloc);
    }

    fn remove(&mut self, namespace: &str, name: &str) {
        if let Some(names) = self.by_namespace.get_mut(namespace) {
            Arc::make_mut(names).remove(name);
            if names.is_empty() {
                self.by_namespace.remove(namespace);
            }
        }
    }

    /// Adds another page of the same list.
    fn extend(&mut self, page: Self) {
        for (namespace, names) in page.by_namespace {
            Arc::make_mut(self.by_namespace.entry(namespace).or_default()).extend(Arc::unwrap_or_clone(names));
        }
    }

    /// Puts a pod's sums in, copying only what a reader still holds of them; false when they
    /// are the ones there already.
    fn put(pods: &mut Arc<Self>, namespace: String, name: String, alloc: PodAlloc) -> bool {
        if pods.get(&namespace, &name) == Some(&alloc) {
            return false;
        }
        Arc::make_mut(pods).insert(namespace, name, alloc);
        true
    }

    /// Takes a pod out; false when it isn't there.
    fn take(pods: &mut Arc<Self>, namespace: &str, name: &str) -> bool {
        if pods.get(namespace, name).is_none() {
            return false;
        }
        Arc::make_mut(pods).remove(namespace, name);
        true
    }

    /// The pods of a JSON list's `items`, as a list reads them.
    #[cfg(test)]
    pub(crate) fn from_items(items: &serde_json::Value) -> Self {
        let list: AllocList = serde_json::from_value(serde_json::json!({ "items": items })).unwrap();
        list.items
    }
}

/// Pods counted, and what the running and pending ones request: CPU in millicores, memory in
/// bytes, and the GPUs they hold.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PodTotals {
    pub pods: usize,
    pub running: usize,
    pub cpu: f64,
    pub memory: f64,
    pub gpus: f64,
}

/// What a watch of the pods' requests and limits read last.
#[derive(Clone, Debug)]
pub struct PodRequests {
    /// Each pod's, for the usage bars.
    pub pods: Arc<PodAllocs>,
    /// Their totals, for the cluster overview.
    pub totals: PodTotals,
}

/// The pods' requests and limits as last read: none before the first list, why when a list
/// failed ("forbidden", "failed: …").
pub type PodRequestsRx = tokio::sync::watch::Receiver<Option<Result<PodRequests, String>>>;

type PodRequestsTx = tokio::sync::watch::Sender<Option<Result<PodRequests, String>>>;

/// The part of a pod the sums read: everything else is skipped while parsing, never held.
#[derive(Deserialize, Default)]
#[serde(default)]
struct LeanPod {
    metadata: LeanMeta,
    spec: podspec::LeanSpec,
    status: LeanStatus,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct LeanMeta {
    name: String,
    namespace: String,
    resource_version: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct LeanStatus {
    phase: String,
}

/// A pod list read straight into sums: each pod is parsed lean, kept as its `PodAlloc`, then
/// dropped, so the list is never held whole.
#[derive(Deserialize, Default)]
#[serde(default)]
struct AllocList {
    #[serde(deserialize_with = "allocs")]
    items: PodAllocs,
    metadata: ListMeta,
}

fn allocs<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<PodAllocs, D::Error> {
    struct Items;

    impl<'de> serde::de::Visitor<'de> for Items {
        type Value = PodAllocs;

        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a list of pods")
        }

        fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<PodAllocs, A::Error> {
            let mut pods = PodAllocs::default();
            while let Some(pod) = seq.next_element::<LeanPod>()? {
                let alloc = PodAlloc::of(&pod);
                pods.insert(pod.metadata.namespace, pod.metadata.name, alloc);
            }
            Ok(pods)
        }

        fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<PodAllocs, E> {
            Ok(PodAllocs::default())
        }
    }

    d.deserialize_any(Items)
}

/// Every pod at `url` as sums, with the list's `resourceVersion` to watch from: in one response
/// from the API server's cache, parsed as it arrives; in pages when the server won't answer
/// that way.
async fn list(client: &Client, url: &str) -> Result<(PodAllocs, String)> {
    match list_cached(client, url).await {
        Ok(listed) => Ok(listed),
        Err(_) => list_paged(client, url).await,
    }
}

async fn list_cached(client: &Client, url: &str) -> Result<(PodAllocs, String)> {
    use tokio_util::compat::FuturesAsyncReadCompatExt as _;
    let path = format!("{url}?resourceVersion=0&resourceVersionMatch=NotOlderThan");
    let body = client.request_stream(http::Request::get(&path).body(Vec::new())?).await?;
    let list: AllocList = lists::parse_streamed(lists::Idle::new(body.compat(), lists::LIST_IDLE)).await?;
    anyhow::ensure!(list.metadata.continue_.as_ref().is_none_or(String::is_empty), "the API server paged it");
    Ok((list.items, list.metadata.resource_version.unwrap_or_default()))
}

async fn list_paged(client: &Client, url: &str) -> Result<(PodAllocs, String)> {
    let mut lp = ListParams::default().limit(lists::LEAN_PAGE);
    let mut pods = PodAllocs::default();
    loop {
        let page: AllocList = lists::request_idle(client, Request::new(url).list(&lp)?).await?;
        pods.extend(page.items);
        match page.metadata.continue_.filter(|c| !c.is_empty()) {
            Some(token) => lp.continue_token = Some(token),
            None => return Ok((pods, page.metadata.resource_version.unwrap_or_default())),
        }
    }
}

/// A watch's ERROR frame: a Status.
#[derive(Deserialize, Default)]
#[serde(default)]
struct WatchError {
    code: u16,
    message: String,
}

/// Applies one watch frame (a line of the stream) to `pods`, keeping the `resourceVersion` to
/// go on from. A frame that doesn't read, or an error other than 410 Gone, ends the watch.
fn apply_event(line: &str, pods: &mut Arc<PodAllocs>, version: &mut String) -> Result<Applied> {
    if line.trim().is_empty() {
        return Ok(Applied::Unchanged);
    }
    let event: RawEvent = serde_json::from_str(line)?;
    match event.type_.as_str() {
        kind @ ("ADDED" | "MODIFIED" | "DELETED" | "BOOKMARK") => {
            let pod: LeanPod = serde_json::from_str(event.object.get())?;
            if !pod.metadata.resource_version.is_empty() {
                version.clone_from(&pod.metadata.resource_version);
            }
            let changed = match kind {
                // Only a newer resourceVersion: going on from it avoids a 410 and a list.
                "BOOKMARK" => false,
                "DELETED" => PodAllocs::take(pods, &pod.metadata.namespace, &pod.metadata.name),
                _ => {
                    let alloc = PodAlloc::of(&pod);
                    PodAllocs::put(pods, pod.metadata.namespace, pod.metadata.name, alloc)
                }
            };
            Ok(if changed { Applied::Changed } else { Applied::Unchanged })
        }
        "ERROR" => {
            let error: WatchError = serde_json::from_str(event.object.get()).unwrap_or_default();
            if error.code == 410 {
                return Ok(Applied::Relist);
            }
            Err(anyhow!("watch error {}: {}", error.code, error.message))
        }
        _ => Ok(Applied::Unchanged),
    }
}

/// At most this often a reader gets the pods while they change: a request matters next to
/// usage, which metrics-server measures every 15 s at best.
const SEND_EVERY: Duration = Duration::from_secs(2);

/// How long the watch goes on while its page is off screen. Back sooner, the pods are current
/// at once; later, they are listed again (watching every pod has a cost too: its events).
const LINGER: Duration = Duration::from_secs(5 * 60);

/// How often the watch looks at its page and its reader when no event comes.
const TICK: Duration = Duration::from_secs(1);

/// How a watch ended.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    /// The pods must be listed again: 410 Gone, or a frame that doesn't read.
    Relist,
    /// The server refuses to watch them (a user who may list pods but not watch them).
    Refused,
    /// Off screen for `LINGER`.
    Lingered,
    /// Nobody reads them any more.
    Closed,
}

/// What `Outbox` says to do now.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Wait,
    Send,
    Stop,
}

/// When the reader gets the pods, and when the watch stops for its page being away.
#[derive(Default)]
struct Outbox {
    /// Changed since last sent.
    dirty: bool,
    sent: Option<Instant>,
    /// Since when the page has been off screen.
    away: Option<Instant>,
}

impl Outbox {
    /// At `now`: send what changed (on screen only, at most every `SEND_EVERY`), stop after
    /// `LINGER` off screen, or wait.
    fn step(&mut self, paused: bool, now: Instant) -> Step {
        if paused {
            let away = *self.away.get_or_insert(now);
            return if now.duration_since(away) >= LINGER { Step::Stop } else { Step::Wait };
        }
        self.away = None;
        if self.dirty && self.sent.is_none_or(|sent| now.duration_since(sent) >= SEND_EVERY) {
            self.dirty = false;
            self.sent = Some(now);
            return Step::Send;
        }
        Step::Wait
    }

    /// Does what `step` says now; how the watch ends, when it must.
    fn after(&mut self, pods: &Arc<PodAllocs>, tx: &PodRequestsTx, pause: &Pause) -> Option<Ended> {
        if tx.is_closed() {
            return Some(Ended::Closed);
        }
        match self.step(pause.is_paused(), Instant::now()) {
            Step::Wait => None,
            Step::Stop => Some(Ended::Lingered),
            Step::Send => {
                let read = PodRequests { pods: pods.clone(), totals: pods.totals() };
                tx.send(Some(Ok(read))).is_err().then_some(Ended::Closed)
            }
        }
    }
}

/// Follows the pods from `version`, applying each change, and gives them to the reader as
/// `Outbox` says, starting with the list's. A cut stream or the server's timeout watches again
/// from where it got, without a list. `progress`: some frame came.
async fn follow(
    client: &Client,
    url: &str,
    mut pods: Arc<PodAllocs>,
    mut version: String,
    tx: &PodRequestsTx,
    pause: &Pause,
    progress: &mut bool,
) -> Ended {
    let mut out = Outbox { dirty: true, ..Outbox::default() };
    let mut backoff = Backoff::default();
    loop {
        if let Some(ended) = out.after(&pods, tx, pause) {
            return ended;
        }
        let Ok(req) = Request::new(url).watch(&WatchParams::default().timeout(290), &version) else {
            return Ended::Refused;
        };
        let reader = match client.request_stream(req).await {
            Ok(reader) => reader,
            Err(e) if resumable(&e) => {
                tokio::time::sleep(backoff.next()).await;
                continue;
            }
            Err(kube::Error::Api(status)) if status.code == 410 => return Ended::Relist,
            Err(_) => return Ended::Refused,
        };
        let mut lines = std::pin::pin!(reader.lines());
        let (mut heard, mut heard_any) = (tokio::time::Instant::now(), false);
        loop {
            let next = tokio::select! {
                line = lines.next() => Some(line),
                () = tokio::time::sleep(TICK) => None,
                // A healthy watch gets a bookmark about once a minute: silence this long is a
                // connection that died without closing (sleep, VPN). Watch again from `version`.
                () = tokio::time::sleep_until(heard + WATCH_IDLE) => break,
            };
            match next {
                // The server ended the watch (its timeout): watch again from `version`. After a
                // wait when it ended without a word: something between cuts every watch short.
                Some(None) => {
                    if !heard_any {
                        tokio::time::sleep(backoff.next()).await;
                    }
                    break;
                }
                // The connection broke: watch again from `version`, after a wait.
                Some(Some(Err(_))) => {
                    tokio::time::sleep(backoff.next()).await;
                    break;
                }
                Some(Some(Ok(line))) => {
                    (heard, heard_any) = (tokio::time::Instant::now(), true);
                    backoff = Backoff::default();
                    *progress = true;
                    match apply_event(&line, &mut pods, &mut version) {
                        Ok(Applied::Changed) => out.dirty = true,
                        Ok(Applied::Unchanged) => {}
                        Ok(Applied::Relist) | Err(_) => return Ended::Relist,
                    }
                }
                None => {}
            }
            if let Some(ended) = out.after(&pods, tx, pause) {
                return ended;
            }
        }
    }
}

/// The pods' requests and limits, for the usage bars of the pods list and the cluster overview:
/// one list, each pod kept only as its sums, then a watch from it, so pods that come, go or are
/// resized show without another list. Nothing is listed before `rows_due` is cleared (the
/// list's own rows go first: this list of whole pods is big), nor while `pause` is set; the
/// changes wait while it is, and after `LINGER` the watch stops until it is cleared. When the
/// server refuses to watch, the pods are listed again every minute, or ten times as long as a
/// list takes.
pub fn watch_pod_resources(
    client: Client,
    namespace: Option<String>,
    pause: Pause,
    rows_due: Pause,
) -> (TaskGuard, PodRequestsRx) {
    let (tx, rx) = tokio::sync::watch::channel(None);
    let guard = spawn_guarded(async move {
        let url = DynamicObject::url_path(&ar_of::<Pod>(), namespace.as_deref());
        wait_unpaused(&rows_due).await;
        let mut failures = 0;
        loop {
            wait_unpaused(&pause).await;
            if tx.is_closed() {
                return;
            }
            let started = Instant::now();
            let (pods, version) = match list(&client, &url).await {
                Ok(listed) => listed,
                Err(e) => {
                    if tx.send(Some(Err(lists::load_error(&e)))).is_err() {
                        return;
                    }
                    retry_wait(&mut failures).await;
                    continue;
                }
            };
            let took = started.elapsed();
            let mut progress = false;
            match follow(&client, &url, Arc::new(pods), version, &tx, &pause, &mut progress).await {
                Ended::Closed => return,
                // Listed, then refused at once: wait before listing every pod again.
                Ended::Relist if !progress => retry_wait(&mut failures).await,
                Ended::Relist => failures = 0,
                Ended::Lingered => {
                    // Not kept current any more: a reader waits for the next list.
                    tx.send_replace(None);
                    failures = 0;
                }
                Ended::Refused => tokio::time::sleep((took * 10).max(Duration::from_secs(60))).await,
            }
        }
    });
    (guard, rx)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use proptest::prelude::*;
    use serde_json::{Value, json};

    use super::{AllocList, Applied, Arc, Duration, Instant, Outbox, PodAlloc, PodAllocs, Step, apply_event};
    use super::{LINGER, SEND_EVERY};

    /// A pod with one container requesting `cpu` and `memory`, limited to that memory.
    fn pod(ns: &str, name: &str, rv: &str, cpu: &str, memory: &str, phase: &str) -> Value {
        json!({
            "kind": "Pod",
            "metadata": {"name": name, "namespace": ns, "resourceVersion": rv, "uid": format!("{ns}-{name}"),
                "managedFields": [{"manager": "kubelet", "fieldsV1": {"f:status": {}}}]},
            "spec": {"containers": [{"name": "app", "image": "app:1", "env": [{"name": "A", "value": "1"}],
                "resources": {"requests": {"cpu": cpu, "memory": memory}, "limits": {"memory": memory}}}]},
            "status": {"phase": phase, "containerStatuses": [{"name": "app", "ready": true}]},
        })
    }

    fn frame(kind: &str, object: &Value) -> String {
        json!({"type": kind, "object": object}).to_string()
    }

    fn entries(pods: &PodAllocs) -> BTreeMap<(String, String), PodAlloc> {
        pods.by_namespace
            .iter()
            .flat_map(|(ns, names)| names.iter().map(move |(name, alloc)| ((ns.clone(), name.clone()), *alloc)))
            .collect()
    }

    #[test]
    fn a_list_reads_into_sums() {
        let list = json!({"kind": "PodList", "metadata": {"resourceVersion": "42"}, "items": [
            pod("shop", "web", "1", "250m", "256Mi", "Running"),
            // The same name in another namespace stays apart.
            pod("staging", "web", "2", "100m", "64Mi", "Pending"),
            pod("shop", "job", "3", "1", "1Gi", "Succeeded"),
            {"metadata": {"name": "mesh", "namespace": "shop"}, "status": {"phase": "Running"}, "spec": {
                "initContainers": [{"restartPolicy": "Always", "resources": {"requests": {"cpu": "50m"}}},
                    {"resources": {"requests": {"cpu": "2"}}}],
                "containers": [{"resources": {"requests": {"cpu": "100m"}, "limits": {"nvidia.com/gpu": "1"}}}],
                "overhead": {"cpu": "10m"}}},
        ]});
        let list: AllocList = serde_json::from_str(&list.to_string()).unwrap();
        assert_eq!(list.metadata.resource_version.as_deref(), Some("42"));
        let pods = list.items;
        let web = pods.get("shop", "web").unwrap();
        assert_eq!(web.resources.cpu, (Some(250.0), None));
        assert_eq!(web.resources.mem, (Some(256.0 * 1024.0 * 1024.0), Some(256.0 * 1024.0 * 1024.0)));
        assert_eq!(pods.get("staging", "web").unwrap().resources.cpu.0, Some(100.0));
        // Against usage, the sidecar runs and the init container is done; the scheduler
        // counts the init container (with the sidecar started before it).
        let mesh = pods.get("shop", "mesh").unwrap();
        assert_eq!(mesh.resources.cpu.0, Some(160.0));
        assert_eq!(mesh.cpu, 2060.0);
        let totals = pods.totals();
        assert_eq!((totals.pods, totals.running), (4, 2));
        // The finished job requests nothing any more.
        assert_eq!(totals.cpu, 250.0 + 100.0 + 2060.0);
        assert_eq!(totals.memory, (256.0 + 64.0) * 1024.0 * 1024.0);
        assert_eq!(totals.gpus, 1.0);
        // No pods at all.
        let empty: AllocList = serde_json::from_str(r#"{"items": null, "metadata": {}}"#).unwrap();
        assert_eq!(empty.items.totals().pods, 0);
    }

    #[test]
    fn watch_events_change_the_pods() {
        let (mut pods, mut version) = (Arc::new(PodAllocs::default()), String::new());
        let mut apply = |line: &str, pods: &mut Arc<PodAllocs>| apply_event(line, pods, &mut version).unwrap();
        let web = pod("shop", "web", "5", "100m", "64Mi", "Pending");
        assert_eq!(apply(&frame("ADDED", &web), &mut pods), Applied::Changed);
        assert_eq!(pods.totals().running, 0);
        assert_eq!(
            apply(&frame("MODIFIED", &pod("shop", "web", "6", "100m", "64Mi", "Running")), &mut pods),
            Applied::Changed
        );
        assert_eq!(pods.totals().running, 1);
        // Its status changed, not its requests: nothing to send.
        assert_eq!(
            apply(&frame("MODIFIED", &pod("shop", "web", "7", "100m", "64Mi", "Running")), &mut pods),
            Applied::Unchanged
        );
        // Resized in place.
        assert_eq!(
            apply(&frame("MODIFIED", &pod("shop", "web", "8", "300m", "64Mi", "Running")), &mut pods),
            Applied::Changed
        );
        assert_eq!(pods.get("shop", "web").unwrap().resources.cpu.0, Some(300.0));
        assert_eq!(apply(&frame("DELETED", &web), &mut pods), Applied::Changed);
        assert!(pods.get("shop", "web").is_none());
        assert!(pods.by_namespace.is_empty(), "an emptied namespace goes");
        // Deleting what isn't there changes nothing.
        assert_eq!(apply(&frame("DELETED", &web), &mut pods), Applied::Unchanged);
        assert_eq!(apply("", &mut pods), Applied::Unchanged);
        let bookmark = r#"{"type":"BOOKMARK","object":{"kind":"Pod","metadata":{"resourceVersion":"42"}}}"#;
        assert_eq!(apply(bookmark, &mut pods), Applied::Unchanged);
        let gone = r#"{"type":"ERROR","object":{"kind":"Status","status":"Failure","code":410,"message":"too old"}}"#;
        assert_eq!(apply(gone, &mut pods), Applied::Relist);
        assert_eq!(version, "42");
        let mut version = String::new();
        let other =
            r#"{"type":"ERROR","object":{"kind":"Status","status":"Failure","code":500,"message":"etcd down"}}"#;
        assert!(apply_event(other, &mut pods, &mut version).is_err());
        assert!(apply_event("not json", &mut pods, &mut version).is_err());
    }

    #[test]
    fn a_reader_keeps_what_it_was_sent() {
        let (mut pods, mut version) = (Arc::new(PodAllocs::default()), String::new());
        for p in [pod("a", "web", "1", "100m", "64Mi", "Running"), pod("b", "db", "2", "100m", "64Mi", "Running")] {
            apply_event(&frame("ADDED", &p), &mut pods, &mut version).unwrap();
        }
        let sent = pods.clone();
        apply_event(&frame("MODIFIED", &pod("a", "web", "3", "500m", "64Mi", "Running")), &mut pods, &mut version)
            .unwrap();
        apply_event(&frame("ADDED", &pod("a", "api", "4", "100m", "64Mi", "Running")), &mut pods, &mut version)
            .unwrap();
        assert_eq!(sent.get("a", "web").unwrap().resources.cpu.0, Some(100.0));
        assert!(sent.get("a", "api").is_none());
        assert_eq!(pods.get("a", "web").unwrap().resources.cpu.0, Some(500.0));
        // Only the namespace that changed was copied.
        assert!(Arc::ptr_eq(&sent.by_namespace["b"], &pods.by_namespace["b"]));
        assert!(!Arc::ptr_eq(&sent.by_namespace["a"], &pods.by_namespace["a"]));
    }

    #[test]
    fn the_reader_gets_changes_on_screen_and_the_watch_stops_long_off_screen() {
        let start = Instant::now();
        let at = |secs: f64| start + Duration::from_secs_f64(secs);
        let mut out = Outbox { dirty: true, ..Outbox::default() };
        // The list goes at once.
        assert_eq!(out.step(false, at(0.0)), Step::Send);
        assert_eq!(out.step(false, at(0.5)), Step::Wait);
        // Changes, then at most every `SEND_EVERY`.
        out.dirty = true;
        assert_eq!(out.step(false, at(1.0)), Step::Wait);
        assert_eq!(out.step(false, at(SEND_EVERY.as_secs_f64())), Step::Send);
        // Off screen, changes wait; back, they go.
        out.dirty = true;
        assert_eq!(out.step(true, at(10.0)), Step::Wait);
        assert_eq!(out.step(false, at(11.0)), Step::Send);
        // Away for `LINGER` in one go, the watch stops; being back in between starts it over.
        let linger = LINGER.as_secs_f64();
        assert_eq!(out.step(true, at(20.0)), Step::Wait);
        assert_eq!(out.step(false, at(21.0)), Step::Wait);
        assert_eq!(out.step(true, at(22.0)), Step::Wait);
        assert_eq!(out.step(true, at(20.0 + linger)), Step::Wait);
        assert_eq!(out.step(true, at(22.0 + linger)), Step::Stop);
    }

    proptest! {
        /// However pods come, change and go, the pods a watch followed are the ones a list of
        /// the same end state reads; and a reader keeps what it was sent before each change.
        #[test]
        fn followed_pods_are_a_fresh_list(events in proptest::collection::vec(
            (0..3usize, 0..2usize, 0..3usize, 0..4u32, 0..3usize, any::<bool>()),
            0..40,
        )) {
            let (mut pods, mut version) = (Arc::new(PodAllocs::default()), String::new());
            let mut live: BTreeMap<(String, String), Value> = BTreeMap::new();
            for (i, &(kind, ns, name, cpu, phase, sidecar)) in events.iter().enumerate() {
                let (ns, name) = (["a", "b"][ns], ["web", "db", "api"][name]);
                let rv = (i + 1).to_string();
                // Whole quantities: their sums are exact in any order.
                let (cpu, memory) = (format!("{}m", 100 * cpu), format!("{}Mi", 64 * cpu));
                let mut object = pod(ns, name, &rv, &cpu, &memory, ["Pending", "Running", "Succeeded"][phase]);
                if sidecar {
                    let resources = json!({"requests": {"cpu": "50m"}, "limits": {"cpu": "50m"}});
                    object["spec"]["initContainers"] = json!([{"restartPolicy": "Always", "resources": resources}]);
                }
                let kind = ["ADDED", "MODIFIED", "DELETED"][kind];
                let sent = pods.clone();
                let before = entries(&sent);
                apply_event(&frame(kind, &object), &mut pods, &mut version).unwrap();
                prop_assert_eq!(entries(&sent), before);
                let key = (ns.to_string(), name.to_string());
                if kind == "DELETED" {
                    live.remove(&key);
                } else {
                    live.insert(key, object);
                }
                prop_assert_eq!(&version, &rv);
            }
            let list = json!({"metadata": {"resourceVersion": "1"}, "items": live.into_values().collect::<Vec<_>>()});
            let fresh: AllocList = serde_json::from_str(&list.to_string()).unwrap();
            prop_assert_eq!(entries(&pods), entries(&fresh.items));
            prop_assert_eq!(pods.totals(), fresh.items.totals());
        }
    }
}

#[cfg(test)]
mod watch_tests {
    use std::convert::Infallible;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use bytes::Bytes;
    use futures::StreamExt as _;
    use http_body_util::combinators::UnsyncBoxBody;
    use http_body_util::{BodyExt as _, Full, StreamBody};
    use hyper::body::Frame;
    use serde_json::{Value, json};

    use super::{Arc, Duration, Pause, PodRequests, PodRequestsRx, watch_pod_resources};

    /// A cluster with pods `a/api` and `b/db` at version 10. A watch from there adds `a/new`,
    /// resizes `a/api` and deletes `b/db`, then stays open. It counts the lists, and answers
    /// them 403 when it `refuses`.
    #[derive(Clone, Default)]
    struct Cluster {
        lists: Arc<AtomicUsize>,
        watched_from: Arc<Mutex<Vec<String>>>,
        refuses: bool,
    }

    fn pod(ns: &str, name: &str, rv: &str, cpu: &str) -> Value {
        json!({"metadata": {"name": name, "namespace": ns, "resourceVersion": rv},
            "spec": {"containers": [{"name": "app", "resources": {"requests": {"cpu": cpu}, "limits": {"cpu": cpu}}}]},
            "status": {"phase": "Running"}})
    }

    impl tower::Service<http::Request<kube::client::Body>> for Cluster {
        type Response = http::Response<UnsyncBoxBody<Bytes, Infallible>>;
        type Error = Infallible;
        type Future = futures::future::BoxFuture<'static, Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<kube::client::Body>) -> Self::Future {
            let query = req.uri().query().unwrap_or_default().to_string();
            let (status, body) = if query.contains("watch=true") {
                let from = query.split('&').find_map(|p| p.strip_prefix("resourceVersion=")).unwrap_or_default();
                self.watched_from.lock().unwrap().push(from.to_string());
                let events = [
                    json!({"type": "ADDED", "object": pod("a", "new", "11", "300m")}),
                    json!({"type": "MODIFIED", "object": pod("a", "api", "12", "200m")}),
                    json!({"type": "DELETED", "object": pod("b", "db", "13", "100m")}),
                ];
                let frames = events.map(|e| Ok::<_, Infallible>(Frame::data(Bytes::from(format!("{e}\n")))));
                let stream = futures::stream::iter(frames).chain(futures::stream::pending());
                (200, StreamBody::new(stream).boxed_unsync())
            } else {
                self.lists.fetch_add(1, Ordering::Relaxed);
                let (status, answer) = if self.refuses {
                    (403, json!({"kind": "Status", "status": "Failure", "reason": "Forbidden", "code": 403}))
                } else {
                    let items = [pod("a", "api", "8", "100m"), pod("b", "db", "9", "100m")];
                    (200, json!({"kind": "PodList", "metadata": {"resourceVersion": "10"}, "items": items}))
                };
                (status, Full::new(Bytes::from(answer.to_string())).boxed_unsync())
            };
            let response = http::Response::builder().status(status).body(body).unwrap();
            Box::pin(async move { Ok(response) })
        }
    }

    /// Whether `rx` gets a read that `seen` holds for within 10 s.
    async fn reads(rx: &mut PodRequestsRx, seen: impl FnMut(&Option<Result<PodRequests, String>>) -> bool) -> bool {
        tokio::time::timeout(Duration::from_secs(10), rx.wait_for(seen)).await.is_ok_and(|read| read.is_ok())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pods_that_come_change_and_go_show_without_another_list() {
        let cluster = Cluster::default();
        let client = kube::Client::new(cluster.clone(), "default");
        let (_watch, mut rx) = watch_pod_resources(client, None, Pause::default(), Pause::default());
        let followed = reads(
            &mut rx,
            |read| matches!(read, Some(Ok(r)) if r.pods.get("a", "new").is_some() && r.pods.get("b", "db").is_none()),
        );
        assert!(followed.await, "the watch's changes never came");
        let read = rx.borrow().clone();
        let Some(Ok(read)) = read else { panic!("no pods") };
        let cpu = |ns: &str, name: &str| read.pods.get(ns, name).map(|p| p.resources.cpu);
        assert_eq!(cpu("a", "api"), Some((Some(200.0), Some(200.0))), "resized in place");
        assert_eq!(cpu("a", "new"), Some((Some(300.0), Some(300.0))));
        assert_eq!((read.totals.pods, read.totals.cpu), (2, 500.0));
        assert_eq!(cluster.lists.load(Ordering::Relaxed), 1, "listed once");
        let watched_from = cluster.watched_from.lock().unwrap().clone();
        assert_eq!(watched_from, ["10"], "watched from the list's version");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_list_says_why() {
        let cluster = Cluster { refuses: true, ..Cluster::default() };
        let client = kube::Client::new(cluster, "default");
        let (_watch, mut rx) = watch_pod_resources(client, None, Pause::default(), Pause::default());
        assert!(reads(&mut rx, Option::is_some).await);
        let read = rx.borrow().clone();
        assert_eq!(read.map(Result::err), Some(Some("forbidden".to_string())));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn nothing_is_listed_before_the_rows_nor_off_screen() {
        let cluster = Cluster::default();
        let (away, rows_due) = (Pause::default(), Pause::default());
        away.set(true);
        rows_due.set(true);
        let client = kube::Client::new(cluster.clone(), "default");
        let (_watch, mut rx) = watch_pod_resources(client, None, away.clone(), rows_due.clone());
        tokio::time::sleep(Duration::from_millis(1200)).await;
        rows_due.set(false);
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(cluster.lists.load(Ordering::Relaxed), 0);
        away.set(false);
        assert!(reads(&mut rx, Option::is_some).await);
        assert_eq!(cluster.lists.load(Ordering::Relaxed), 1);
    }
}
