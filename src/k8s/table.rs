//! Server-side tables (kubectl's columns) listed and watched for the resource lists.

#[allow(unused_imports)]
use super::*;

/// The server-side Table format, the same data `kubectl get` prints.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub(super) struct ServerTable {
    #[serde(default, deserialize_with = "null_default")]
    pub(super) column_definitions: Vec<ColumnDef>,
    #[serde(default, deserialize_with = "null_default")]
    pub(super) rows: Vec<ServerRow>,
    #[serde(default, deserialize_with = "null_default")]
    pub(super) metadata: ListMeta,
}

/// Watch frames send `null` for fields a list response omits.
pub(super) fn null_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Deserialize, Clone, Debug)]
pub struct ColumnDef {
    pub name: String,
    #[serde(default)]
    pub priority: i32,
}

#[derive(Deserialize, Debug)]
pub(super) struct ServerRow {
    pub(super) cells: Vec<serde_json::Value>,
    /// Kept raw: parsed lean (metadata only) unless custom columns need more.
    pub(super) object: Option<Box<serde_json::value::RawValue>>,
}

/// Custom columns: (header, JSONPath) per kind, evaluated on each object.
pub type Extras = Arc<Vec<(String, String)>>;

/// Evaluate a kubectl-style JSONPath (`.spec.replicas`, `{.a[0].b}`, `.metadata.labels['x/y']`, `.items[*].name`).
pub fn json_path(v: &serde_json::Value, path: &str) -> String {
    let p = path.trim().trim_start_matches('{').trim_end_matches('}').trim_start_matches('.');
    // Split into segments, keeping ['quoted.keys'] and [n] / [*] as their own steps.
    let mut steps: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = p.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '.' => {
                if !cur.is_empty() {
                    steps.push(std::mem::take(&mut cur));
                }
            }
            '[' => {
                if !cur.is_empty() {
                    steps.push(std::mem::take(&mut cur));
                }
                let mut inner = String::new();
                for d in chars.by_ref() {
                    if d == ']' {
                        break;
                    }
                    inner.push(d);
                }
                steps.push(format!("[{}", inner.trim_matches(|c| c == '\'' || c == '"')));
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        steps.push(cur);
    }
    let mut values = vec![v.clone()];
    for step in &steps {
        values = values
            .into_iter()
            .flat_map(|x| -> Vec<serde_json::Value> {
                if let Some(inner) = step.strip_prefix('[') {
                    match (inner, &x) {
                        ("*", serde_json::Value::Array(a)) => a.clone(),
                        ("*", serde_json::Value::Object(o)) => o.values().cloned().collect(),
                        (i, serde_json::Value::Array(a)) => {
                            i.parse::<usize>().ok().and_then(|i| a.get(i).cloned()).into_iter().collect()
                        }
                        (k, serde_json::Value::Object(o)) => o.get(k).cloned().into_iter().collect(),
                        _ => vec![],
                    }
                } else {
                    x.get(step).cloned().into_iter().collect()
                }
            })
            .collect();
    }
    values.iter().map(cell_text).collect::<Vec<_>>().join(",")
}

#[derive(Deserialize, Clone, Debug)]
pub(super) struct PartialObject {
    #[serde(default)]
    pub(super) metadata: LeanMeta,
}

/// Only the metadata the tables need. Everything else (annotations,
/// managedFields) is skipped while parsing, which keeps huge lists cheap.
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase", default)]
pub(super) struct LeanMeta {
    pub(super) name: Option<String>,
    pub(super) namespace: Option<String>,
    pub(super) uid: Option<String>,
    pub(super) resource_version: Option<String>,
    pub(super) creation_timestamp: Option<String>,
    pub(super) deletion_timestamp: Option<String>,
    pub(super) labels: Option<std::collections::BTreeMap<String, String>>,
}

#[derive(Debug)]
pub struct Row {
    pub uid: String,
    pub name: String,
    pub namespace: Option<String>,
    pub created: Option<jiff::Timestamp>,
    pub cells: Vec<String>,
    /// Labels as `key=value` lines (one allocation, for filtering).
    pub labels: Box<str>,
    /// Being deleted (deletionTimestamp set): shown dimmed.
    pub deleting: bool,
    /// The deletionTimestamp (when the grace period ends), if being deleted.
    pub deleted_at: Option<jiff::Timestamp>,
}

impl Row {
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.lines().find_map(|l| l.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
    }
}

pub enum TableMsg {
    Columns(Arc<Vec<ColumnDef>>),
    Rows(Vec<Arc<Row>>),
    /// The initial list is complete (rows before this were partial pages).
    Listed,
    Error(String),
    /// The user may not list this type here (403): the watch stops instead of retrying.
    Forbidden(String),
}

pub(super) const TABLE_ACCEPT: &str = "application/json;as=Table;g=meta.k8s.io;v=v1,application/json";

pub(super) fn with_table_format(mut req: http::Request<Vec<u8>>, full: bool) -> Result<http::Request<Vec<u8>>> {
    let uri = req.uri().to_string();
    let sep = if uri.contains('?') { '&' } else { '?' };
    let include = if full { "Object" } else { "Metadata" };
    *req.uri_mut() = format!("{uri}{sep}includeObject={include}").parse()?;
    req.headers_mut().insert(http::header::ACCEPT, http::HeaderValue::from_static(TABLE_ACCEPT));
    Ok(req)
}

/// Whether these custom columns need whole objects (anything beyond labels/annotations).
pub(super) fn needs_full(extras: &Extras) -> bool {
    extras.iter().any(|(_, p)| {
        let p = p.trim().trim_start_matches('{').trim_start_matches('.');
        !(p.starts_with("metadata.labels")
            || p.starts_with("metadata.annotations")
            || p.starts_with("metadata.name")
            || p.starts_with("metadata.namespace"))
    })
}

pub(super) fn to_row(row: ServerRow, extras: &Extras) -> Option<Arc<Row>> {
    let raw = row.object?;
    let meta = serde_json::from_str::<PartialObject>(raw.get()).ok()?.metadata;
    let mut cells: Vec<String> = row.cells.iter().map(cell_text).collect();
    if !extras.is_empty()
        && let Ok(obj) = serde_json::from_str::<serde_json::Value>(raw.get())
    {
        cells.extend(extras.iter().map(|(_, path)| json_path(&obj, path)));
    }
    Some(Arc::new(Row {
        uid: meta.uid?,
        name: meta.name.unwrap_or_default(),
        namespace: meta.namespace,
        created: meta.creation_timestamp.and_then(|t| t.parse::<jiff::Timestamp>().ok()),
        cells,
        deleting: meta.deletion_timestamp.is_some(),
        deleted_at: meta.deletion_timestamp.as_deref().and_then(|t| t.parse::<jiff::Timestamp>().ok()),
        labels: meta
            .labels
            .map(|l| l.into_iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("\n"))
            .unwrap_or_default()
            .into(),
    }))
}

pub(super) fn cell_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(a) => a.iter().map(cell_text).collect::<Vec<_>>().join(","),
        other => other.to_string(),
    }
}

pub(super) fn snapshot(rows: &HashMap<String, Arc<Row>>) -> Vec<Arc<Row>> {
    let mut out: Vec<Arc<Row>> = rows.values().cloned().collect();
    out.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    out
}

pub(super) const PAGE_SIZE: u32 = 2000;

/// A watch that hears nothing (not even a bookmark) for this long is reconnected.
const WATCH_IDLE: std::time::Duration = std::time::Duration::from_secs(120);

/// List in Table form, a page at a time: the server never builds one giant
/// response and we never hold one in memory.
pub(super) async fn list_paged(
    client: &Client,
    url: &str,
    lp: ListParams,
) -> Result<(Vec<ColumnDef>, Vec<Arc<Row>>, String)> {
    list_paged_with(client, url, lp, &Extras::default(), |_, _| {}).await
}

/// Like `list_paged`, calling `on_page` with the rows so far after each page.
pub(super) async fn list_paged_with(
    client: &Client,
    url: &str,
    mut lp: ListParams,
    extras: &Extras,
    mut on_page: impl FnMut(&[ColumnDef], &[Arc<Row>]),
) -> Result<(Vec<ColumnDef>, Vec<Arc<Row>>, String)> {
    lp.limit = Some(PAGE_SIZE);
    let full = needs_full(extras);
    let (mut defs, mut rows, mut version) = (Vec::new(), Vec::new(), String::new());
    loop {
        let page: ServerTable = client.request(with_table_format(Request::new(url).list(&lp)?, full)?).await?;
        if defs.is_empty() {
            defs = page.column_definitions;
            defs.extend(extras.iter().map(|(name, _)| ColumnDef { name: name.clone(), priority: 0 }));
        }
        rows.extend(page.rows.into_iter().filter_map(|r| to_row(r, extras)));
        version = page.metadata.resource_version.unwrap_or(version);
        on_page(&defs, &rows);
        match page.metadata.continue_.filter(|c| !c.is_empty()) {
            Some(token) => lp.continue_token = Some(token),
            None => break,
        }
    }
    Ok((defs, rows, version))
}

/// Holds back a watch's snapshots while its list is hidden: events still apply (the
/// watch stays current), and the rows are sent once it is shown again.
#[derive(Clone, Default)]
pub struct Pause(Arc<std::sync::atomic::AtomicBool>);

impl Pause {
    pub fn set(&self, paused: bool) {
        self.0.store(paused, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// At most this often a full snapshot goes to the table: a busy cluster's events
/// arrive in bursts, and each snapshot costs the UI a filter and a sort.
const SNAPSHOT_EVERY: std::time::Duration = std::time::Duration::from_millis(250);

/// While paused, how often to check whether the list is shown again.
const PAUSED_POLL: std::time::Duration = std::time::Duration::from_millis(500);

/// When the rows changed since the last snapshot, and when that was sent.
#[derive(Default)]
struct Snapshots {
    dirty: bool,
    sent: Option<std::time::Instant>,
}

impl Snapshots {
    /// How long until a pending snapshot may go; None when nothing is pending.
    fn wait(&self, pause: &Pause) -> Option<std::time::Duration> {
        if !self.dirty {
            None
        } else if pause.is_paused() {
            Some(PAUSED_POLL)
        } else {
            Some(self.sent.map_or(std::time::Duration::ZERO, |s| SNAPSHOT_EVERY.saturating_sub(s.elapsed())))
        }
    }

    /// Send the rows if they changed, the list is shown and the last snapshot is old enough.
    fn flush(
        &mut self,
        rows: &HashMap<String, Arc<Row>>,
        tx: &mpsc::UnboundedSender<TableMsg>,
        pause: &Pause,
    ) -> Result<()> {
        if self.wait(pause) == Some(std::time::Duration::ZERO) {
            tx.unbounded_send(TableMsg::Rows(snapshot(rows)))?;
            self.dirty = false;
            self.sent = Some(std::time::Instant::now());
        }
        Ok(())
    }
}

/// Stream a live table for `kind`: a full list, then watch events applied on
/// top. Relists automatically when the watch expires (410 Gone). The app uses
/// `watch_table_paused`; this is for the dev smoke run.
#[cfg(feature = "dev")]
pub fn watch_table(
    client: Client,
    kind: Kind,
    namespace: Option<String>,
    extras: Extras,
) -> (TaskGuard, mpsc::UnboundedReceiver<TableMsg>) {
    watch_table_paused(client, kind, namespace, extras, Pause::default())
}

/// Like `watch_table`, holding snapshots back while `pause` is set.
pub fn watch_table_paused(
    client: Client,
    kind: Kind,
    namespace: Option<String>,
    extras: Extras,
    pause: Pause,
) -> (TaskGuard, mpsc::UnboundedReceiver<TableMsg>) {
    let (tx, rx) = mpsc::unbounded();
    let guard = spawn_guarded(async move {
        let ns = if kind.namespaced { namespace.as_deref() } else { None };
        let url = DynamicObject::url_path(&kind.ar, ns);
        let mut first = true;
        let mut backoff = Backoff::default();
        loop {
            // Whether this round listed or got events: only then does the wait start over.
            let mut progress = false;
            let result = watch_once(&client, &url, &tx, &extras, &mut first, &pause, &mut progress).await;
            if let Err(e) = &result {
                if api_status(e) == Some(403) {
                    let _ = tx.unbounded_send(TableMsg::Forbidden(short_error(e)));
                    return;
                }
                if tx.unbounded_send(TableMsg::Error(short_error(e))).is_err() {
                    return;
                }
            }
            if tx.is_closed() {
                return;
            }
            if progress {
                backoff = Backoff::default();
            }
            // A relist right after progress (410 Gone) goes at once; a failing server is
            // retried less and less often, not every few seconds by every open list.
            if result.is_err() || !progress {
                tokio::time::sleep(backoff.next()).await;
            }
        }
    });
    (guard, rx)
}

/// Waits between failed attempts: 0.8s doubling up to 30s, each with up to 25% added at
/// random so lists of many users don't retry in step (the same curve as client-go).
#[derive(Default)]
struct Backoff {
    attempts: u32,
}

impl Backoff {
    fn next(&mut self) -> std::time::Duration {
        let base = (800u64 << self.attempts.min(6)).min(30_000);
        self.attempts += 1;
        let jitter = (uuid::Uuid::new_v4().as_u128() % 1000) as u64 * base / 4000;
        std::time::Duration::from_millis(base + jitter)
    }
}

/// One list then watch. Until a first list completes (`first`), pages are shown
/// as they arrive and `Listed` ends them (then `first` turns false); on a relist
/// the table keeps its rows until the new list is complete, then gets it whole.
pub(super) async fn watch_once(
    client: &Client,
    url: &str,
    tx: &mpsc::UnboundedSender<TableMsg>,
    extras: &Extras,
    first: &mut bool,
    pause: &Pause,
    progress: &mut bool,
) -> Result<()> {
    let request = Request::new(url);
    let full = needs_full(extras);
    // Show rows as pages arrive; big clusters take tens of seconds to list.
    let mut sent_columns = false;
    let progressive = *first;
    let (defs, list_rows, version) = list_paged_with(client, url, ListParams::default(), extras, |defs, rows| {
        if !progressive {
            return;
        }
        if !sent_columns {
            let _ = tx.unbounded_send(TableMsg::Columns(Arc::new(defs.to_vec())));
            sent_columns = true;
        }
        if pause.is_paused() {
            return;
        }
        let mut partial = rows.to_vec();
        partial.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
        let _ = tx.unbounded_send(TableMsg::Rows(partial));
    })
    .await?;
    if !sent_columns {
        let _ = tx.unbounded_send(TableMsg::Columns(Arc::new(defs)));
    }
    let mut rows: HashMap<String, Arc<Row>> = list_rows.into_iter().map(|r| (r.uid.clone(), r)).collect();
    let mut snaps = Snapshots { dirty: true, sent: None };
    snaps.flush(&rows, tx, pause)?;
    *progress = true;
    if *first {
        tx.unbounded_send(TableMsg::Listed)?;
        *first = false;
    }
    let mut version = version;

    loop {
        let wp = WatchParams::default().timeout(290);
        let req = with_table_format(request.watch(&wp, &version)?, full)?;
        let reader = client.request_stream(req).await?;
        let mut chunks = std::pin::pin!(reader.lines().ready_chunks(256));
        let mut heard = tokio::time::Instant::now();
        loop {
            // Wake up for a pending snapshot even when no event comes.
            let flush = snaps.wait(pause);
            let next = tokio::select! {
                lines = chunks.next() => Some(lines),
                () = async {
                    match flush {
                        Some(wait) => tokio::time::sleep(wait).await,
                        None => std::future::pending().await,
                    }
                } => None,
                // A healthy watch gets a bookmark about once a minute even when nothing
                // changes; silence this long is a connection that died without closing
                // (sleep, VPN, NAT). Watch again from `version`, without a relist.
                () = tokio::time::sleep_until(heard + WATCH_IDLE) => break,
            };
            match next {
                // The server ended the watch (timeout): watch again from `version`.
                Some(None) => break,
                Some(Some(lines)) => {
                    heard = tokio::time::Instant::now();
                    *progress = true;
                    for line in lines {
                        let line = line?;
                        match apply_event(&line, &mut rows, &mut version, extras)? {
                            Applied::Changed => snaps.dirty = true,
                            Applied::Unchanged => {}
                            // Our resourceVersion is too old: relist (which sends every row).
                            Applied::Relist => return Ok(()),
                        }
                    }
                }
                None => {}
            }
            snaps.flush(&rows, tx, pause)?;
        }
    }
}

/// What one watch frame did to the rows.
#[derive(Debug, PartialEq)]
pub(super) enum Applied {
    Changed,
    Unchanged,
    /// 410 Gone: the watch must start over from a fresh list.
    Relist,
}

/// Apply one watch frame (a line of the stream) to the rows by uid, keeping the
/// resourceVersion to resume from. Errors other than 410 end the watch.
pub(super) fn apply_event(
    line: &str,
    rows: &mut HashMap<String, Arc<Row>>,
    version: &mut String,
    extras: &Extras,
) -> Result<Applied> {
    if line.trim().is_empty() {
        return Ok(Applied::Unchanged);
    }
    let event: RawEvent =
        serde_json::from_str(line).with_context(|| format!("bad watch frame: {}", truncate(line, 200)))?;
    match event.type_.as_str() {
        "ADDED" | "MODIFIED" | "DELETED" => {
            let table: ServerTable = serde_json::from_str(event.object.get())?;
            if let Some(v) = table.metadata.resource_version.clone() {
                *version = v;
            }
            let mut changed = false;
            for row in table.rows {
                let meta = row
                    .object
                    .as_ref()
                    .and_then(|o| serde_json::from_str::<PartialObject>(o.get()).ok())
                    .map(|o| o.metadata);
                if let Some(v) = meta.as_ref().and_then(|m| m.resource_version.clone()) {
                    *version = v;
                }
                if event.type_ == "DELETED" {
                    if let Some(uid) = meta.as_ref().and_then(|m| m.uid.as_ref()) {
                        changed |= rows.remove(uid).is_some();
                    }
                } else if let Some(row) = to_row(row, extras) {
                    rows.insert(row.uid.clone(), row);
                    changed = true;
                }
            }
            Ok(if changed { Applied::Changed } else { Applied::Unchanged })
        }
        // Only a newer resourceVersion: resuming from it avoids a 410 and a full relist.
        "BOOKMARK" => {
            let v: serde_json::Value = serde_json::from_str(event.object.get()).unwrap_or_default();
            let rv = v
                .pointer("/metadata/resourceVersion")
                .or_else(|| v.pointer("/rows/0/object/metadata/resourceVersion"))
                .and_then(|r| r.as_str());
            if let Some(rv) = rv.filter(|r| !r.is_empty()) {
                *version = rv.to_string();
            }
            Ok(Applied::Unchanged)
        }
        "ERROR" => {
            let obj: serde_json::Value = serde_json::from_str(event.object.get()).unwrap_or_default();
            let code = obj.get("code").and_then(|c| c.as_i64());
            if code == Some(410) {
                return Ok(Applied::Relist);
            }
            let msg = obj.get("message").and_then(|m| m.as_str()).unwrap_or("watch error");
            Err(anyhow!("{msg}"))
        }
        _ => Ok(Applied::Unchanged),
    }
}

#[derive(Deserialize)]
pub(super) struct RawEvent {
    #[serde(rename = "type")]
    pub(super) type_: String,
    pub(super) object: Box<serde_json::value::RawValue>,
}

pub(super) fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// One-shot list in Table form, for agents.
pub async fn list_table(
    client: Client,
    kind: Kind,
    namespace: Option<String>,
    label_selector: Option<String>,
    field_selector: Option<String>,
) -> Result<(Vec<ColumnDef>, Vec<Arc<Row>>)> {
    run(async move {
        let ns = if kind.namespaced { namespace.as_deref() } else { None };
        let url = DynamicObject::url_path(&kind.ar, ns);
        let mut lp = ListParams::default();
        if let Some(l) = &label_selector {
            lp = lp.labels(l);
        }
        if let Some(f) = &field_selector {
            lp = lp.fields(f);
        }
        let (defs, rows, _) = list_paged(&client, &url, lp).await?;
        Ok((defs, rows))
    })
    .await
}

/// Labels of every object matching a selector, from metadata only (no data/spec).
pub async fn list_labels(
    client: Client,
    ar: ApiResource,
    namespace: Option<String>,
    selector: String,
) -> Result<Vec<(Option<String>, std::collections::BTreeMap<String, String>)>> {
    run(async move {
        let api: Api<DynamicObject> = match &namespace {
            Some(ns) => Api::namespaced_with(client, ns, &ar),
            None => Api::all_with(client, &ar),
        };
        let list = api.list_metadata(&ListParams::default().labels(&selector)).await?;
        Ok(list.items.into_iter().map(|o| (o.metadata.namespace, o.metadata.labels.unwrap_or_default())).collect())
    })
    .await
}

/// Names (and namespaces) of every object of a kind, from metadata only.
pub async fn list_names(client: Client, ar: ApiResource) -> Result<Vec<(Option<String>, String)>> {
    run(async move {
        let api: Api<DynamicObject> = Api::all_with(client, &ar);
        let list = api.list_metadata(&ListParams::default()).await?;
        Ok(list.items.into_iter().filter_map(|o| Some((o.metadata.namespace, o.metadata.name?))).collect())
    })
    .await
}

#[cfg(test)]
mod watch_tests {
    use super::{Applied, Arc, Extras, HashMap, Row, apply_event};

    /// A watch frame in Table form for one object.
    fn frame(kind: &str, uid: &str, name: &str, rv: &str, status: &str) -> String {
        serde_json::json!({
            "type": kind,
            "object": {
                "kind": "Table",
                "apiVersion": "meta.k8s.io/v1",
                "metadata": {},
                "columnDefinitions": null,
                "rows": [{
                    "cells": [name, status],
                    "object": {"metadata": {"name": name, "namespace": "default", "uid": uid, "resourceVersion": rv}}
                }]
            }
        })
        .to_string()
    }

    #[test]
    fn applies_events() {
        let (mut rows, mut version, extras) = (HashMap::<String, Arc<Row>>::new(), "1".to_string(), Extras::default());
        let mut apply =
            |line: &str, rows: &mut HashMap<String, Arc<Row>>| apply_event(line, rows, &mut version, &extras);
        assert_eq!(apply(&frame("ADDED", "u1", "web", "5", "Pending"), &mut rows).unwrap(), Applied::Changed);
        assert_eq!(apply(&frame("ADDED", "u2", "api", "6", "Running"), &mut rows).unwrap(), Applied::Changed);
        assert_eq!(rows.len(), 2);
        assert_eq!(apply(&frame("MODIFIED", "u1", "web", "7", "Running"), &mut rows).unwrap(), Applied::Changed);
        assert_eq!(rows["u1"].cells, ["web", "Running"]);
        assert_eq!(apply(&frame("DELETED", "u2", "api", "8", "Running"), &mut rows).unwrap(), Applied::Changed);
        assert!(!rows.contains_key("u2"));
        // Deleting what is not there changes nothing (but still moves the version).
        assert_eq!(apply(&frame("DELETED", "u9", "gone", "9", ""), &mut rows).unwrap(), Applied::Unchanged);
        assert_eq!(apply("", &mut rows).unwrap(), Applied::Unchanged);
        let bookmark = r#"{"type":"BOOKMARK","object":{"kind":"Table","metadata":{"resourceVersion":"42"}}}"#;
        assert_eq!(apply(bookmark, &mut rows).unwrap(), Applied::Unchanged);
        let gone = r#"{"type":"ERROR","object":{"kind":"Status","code":410,"message":"too old"}}"#;
        assert_eq!(apply(gone, &mut rows).unwrap(), Applied::Relist);
        let other = r#"{"type":"ERROR","object":{"kind":"Status","code":500,"message":"etcd down"}}"#;
        assert_eq!(apply(other, &mut rows).unwrap_err().to_string(), "etcd down");
        assert!(apply("not json", &mut rows).is_err());
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn tracks_resource_version() {
        let (mut rows, mut version, extras) = (HashMap::new(), "1".to_string(), Extras::default());
        apply_event(&frame("ADDED", "u1", "web", "5", "Running"), &mut rows, &mut version, &extras).unwrap();
        assert_eq!(version, "5");
        apply_event(&frame("DELETED", "u9", "gone", "9", ""), &mut rows, &mut version, &extras).unwrap();
        assert_eq!(version, "9");
        let bookmark = r#"{"type":"BOOKMARK","object":{"kind":"Table","metadata":{"resourceVersion":"42"}}}"#;
        apply_event(bookmark, &mut rows, &mut version, &extras).unwrap();
        assert_eq!(version, "42");
        // A bookmark without a version keeps ours.
        apply_event(r#"{"type":"BOOKMARK","object":{}}"#, &mut rows, &mut version, &extras).unwrap();
        assert_eq!(version, "42");
    }
}

#[cfg(test)]
mod backoff_tests {
    #[test]
    fn retries_slow_down_to_thirty_seconds() {
        let mut b = super::Backoff::default();
        let waits: Vec<u128> = (0..10).map(|_| b.next().as_millis()).collect();
        // 0.8s, 1.6s, 3.2s … each with up to 25% more, then 30s (up to 37.5s) from the 6th on.
        for (i, w) in waits.iter().enumerate() {
            let base = (800u128 << i.min(6)).min(30_000);
            assert!((base..=base + base / 4).contains(w), "attempt {i}: {w}ms");
        }
        assert!(super::Backoff::default().next().as_millis() < 1_000, "a new backoff starts short");
    }
}

#[cfg(test)]
mod json_path_tests {
    #[test]
    fn paths() {
        let v = serde_json::json!({"spec": {"replicas": 3, "containers": [{"image": "a"}, {"image": "b"}]}, "metadata": {"labels": {"app.kubernetes.io/name": "web"}}});
        assert_eq!(super::json_path(&v, ".spec.replicas"), "3");
        assert_eq!(super::json_path(&v, "{.spec.containers[0].image}"), "a");
        assert_eq!(super::json_path(&v, ".spec.containers[*].image"), "a,b");
        assert_eq!(super::json_path(&v, ".metadata.labels['app.kubernetes.io/name']"), "web");
        assert_eq!(super::json_path(&v, ".nope"), "");
    }
}
