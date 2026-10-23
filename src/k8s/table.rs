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
    /// The object's uid; `namespace/name` for a row listed without its object (see
    /// `is_partial`).
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
    /// Listed from its cells alone: no uid, labels or deletion time, and a creation time
    /// read from its Age cell. Real uids never contain a `/`.
    pub fn is_partial(&self) -> bool {
        self.uid.contains('/')
    }

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
    row_from(cells, meta)
}

/// A row from its cells and its object's metadata (None without a uid).
fn row_from(cells: Vec<String>, meta: LeanMeta) -> Option<Arc<Row>> {
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
    #[cfg(feature = "dev")]
    let _t = crate::dev::timing::span("watch snapshot (bg)");
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
    list_paged_with(client, url, lp, &Extras::default(), |_, _, _| {}).await
}

/// Like `list_paged`, calling `on_page` with the rows so far after each page, and whether
/// more pages follow.
pub(super) async fn list_paged_with(
    client: &Client,
    url: &str,
    mut lp: ListParams,
    extras: &Extras,
    mut on_page: impl FnMut(&[ColumnDef], &[Arc<Row>], bool),
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
        let next = page.metadata.continue_.filter(|c| !c.is_empty());
        on_page(&defs, &rows, next.is_some());
        match next {
            Some(token) => lp.continue_token = Some(token),
            None => break,
        }
    }
    Ok((defs, rows, version))
}

/// Namespaces listed at once by `cells_by_namespace`, across all its callers (lists,
/// problem scan, overviews), to stay gentle with the API server.
const FAST_PARALLEL: usize = 32;
static FAST_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(FAST_PARALLEL);

/// A big list's rows without their objects, one namespace at a time, many at once, from the
/// API server's cache. The objects' metadata (managedFields above all) is most of a list's
/// bytes: on a cluster with 23k pods, 160 MB against 6 MB, so this shows the whole list in
/// about 2 s where the full one takes 8 to 10 s over a laptop's link. Rows have no uid,
/// labels or creation time (they come with the full list, which replaces them).
async fn cells_by_namespace(client: &Client, url: &str) -> Result<Vec<Arc<Row>>> {
    let (prefix, resource) = url.rsplit_once('/').ok_or_else(|| anyhow!("unexpected path {url}"))?;
    let mut req = http::Request::get("/api/v1/namespaces?includeObject=None").body(Vec::new())?;
    req.headers_mut().insert(http::header::ACCEPT, http::HeaderValue::from_static(TABLE_ACCEPT));
    let namespaces: ServerTable = client.request(req).await?;
    let names: Vec<String> =
        namespaces.rows.iter().filter_map(|r| r.cells.first()?.as_str().map(String::from)).collect();
    let jobs = names.into_iter().map(|ns| {
        let client = client.clone();
        let path = format!(
            "{prefix}/namespaces/{ns}/{resource}?includeObject=None&resourceVersion=0&resourceVersionMatch=NotOlderThan"
        );
        async move {
            let _slot = FAST_SLOTS.acquire().await?;
            let mut req = http::Request::get(&path).body(Vec::new())?;
            req.headers_mut().insert(http::header::ACCEPT, http::HeaderValue::from_static(TABLE_ACCEPT));
            let page: ServerTable = client.request(req).await?;
            let age = page.column_definitions.iter().position(|d| d.name == "Age");
            Ok::<_, anyhow::Error>(
                page.rows.into_iter().filter_map(|r| cells_row(r, Some(&ns), age)).collect::<Vec<_>>(),
            )
        }
    });
    let pages: Vec<Result<Vec<Arc<Row>>>> = futures::stream::iter(jobs).buffer_unordered(FAST_PARALLEL).collect().await;
    let mut rows = Vec::new();
    for page in pages {
        rows.extend(page?);
    }
    rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok(rows)
}

/// The first list of a namespaced kind in every namespace, shown as soon as it can be: its
/// first page (2000 rows) at once; when more follow, every namespace's rows without their
/// objects (`cells_by_namespace`: about 2 s for 23k pods over a laptop's link); then the
/// whole list with its objects' metadata in one response from the API server's cache,
/// parsed as it streams in (about half the time of paging through it).
async fn list_fast(
    client: &Client,
    url: &str,
    tx: &mpsc::UnboundedSender<TableMsg>,
    pause: &Pause,
) -> Result<(Vec<ColumnDef>, Vec<Arc<Row>>, String)> {
    let lp = ListParams::default().limit(PAGE_SIZE);
    let page: ServerTable = client.request(with_table_format(Request::new(url).list(&lp)?, false)?).await?;
    let defs = page.column_definitions.clone();
    let _ = tx.unbounded_send(TableMsg::Columns(Arc::new(defs.clone())));
    let more = page.metadata.continue_.as_ref().is_some_and(|c| !c.is_empty());
    let version = page.metadata.resource_version.clone().unwrap_or_default();
    let mut rows: Vec<Arc<Row>> = page.rows.into_iter().filter_map(|r| to_row(r, &Extras::default())).collect();
    rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    if !more {
        return Ok((defs, rows, version));
    }
    if !pause.is_paused() {
        let _ = tx.unbounded_send(TableMsg::Rows(rows));
    }
    // A failure here only means waiting for the whole list.
    if let Ok(cells) = cells_by_namespace(client, url).await
        && !pause.is_paused()
    {
        let _ = tx.unbounded_send(TableMsg::Rows(cells));
    }
    match list_whole(client, url).await {
        Ok(listed) => Ok(listed),
        // An API server that can't serve it from its cache: page through it as before.
        Err(_) => list_paged(client, url, ListParams::default()).await,
    }
}

/// A Table whose rows' objects are parsed lean while reading (managedFields and the rest
/// skipped, never held).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LeanTable {
    #[serde(default, deserialize_with = "null_default")]
    column_definitions: Vec<ColumnDef>,
    #[serde(default, deserialize_with = "null_default")]
    rows: Vec<LeanRow>,
    #[serde(default, deserialize_with = "null_default")]
    metadata: ListMeta,
}

#[derive(Deserialize)]
struct LeanRow {
    cells: Vec<serde_json::Value>,
    #[serde(default)]
    object: Option<PartialObject>,
}

/// The whole list in one response from the API server's cache (`resourceVersion=0`), parsed
/// as it arrives on a blocking thread: memory stays at the rows, not the 160 MB of JSON a
/// big cluster sends.
async fn list_whole(client: &Client, url: &str) -> Result<(Vec<ColumnDef>, Vec<Arc<Row>>, String)> {
    use tokio_util::compat::FuturesAsyncReadCompatExt as _;
    let path = format!("{url}?includeObject=Metadata&resourceVersion=0&resourceVersionMatch=NotOlderThan");
    let mut req = http::Request::get(&path).body(Vec::new())?;
    req.headers_mut().insert(http::header::ACCEPT, http::HeaderValue::from_static(TABLE_ACCEPT));
    let body = client.request_stream(req).await?;
    let reader = tokio_util::io::SyncIoBridge::new(body.compat());
    let table: LeanTable = tokio::task::spawn_blocking(move || {
        serde_json::from_reader(std::io::BufReader::with_capacity(256 * 1024, reader))
    })
    .await??;
    // A server that pages it anyway: the rows would be partial.
    if table.metadata.continue_.as_ref().is_some_and(|c| !c.is_empty()) {
        return Err(anyhow!("the API server paged a cached list"));
    }
    let mut rows: Vec<Arc<Row>> = table
        .rows
        .into_iter()
        .filter_map(|r| row_from(r.cells.iter().map(cell_text).collect(), r.object?.metadata))
        .collect();
    rows.sort_by(|a, b| (&a.namespace, &a.name).cmp(&(&b.namespace, &b.name)));
    Ok((table.column_definitions, rows, table.metadata.resource_version.unwrap_or_default()))
}

/// Every row of `url` from its cells alone, in one request from the API server's cache;
/// `namespace` is the one `url` is in, if any.
async fn cells_list(client: &Client, url: &str, namespace: Option<&str>) -> Result<Vec<Arc<Row>>> {
    let path = format!("{url}?includeObject=None&resourceVersion=0&resourceVersionMatch=NotOlderThan");
    let mut req = http::Request::get(&path).body(Vec::new())?;
    req.headers_mut().insert(http::header::ACCEPT, http::HeaderValue::from_static(TABLE_ACCEPT));
    let page: ServerTable = client.request(req).await?;
    let age = page.column_definitions.iter().position(|d| d.name == "Age");
    Ok(page.rows.into_iter().filter_map(|r| cells_row(r, namespace, age)).collect())
}

/// A row from its cells alone (`includeObject=None`): the name is the first cell, and
/// namespace/name stands in for the uid until the full list brings it.
fn cells_row(row: ServerRow, namespace: Option<&str>, age: Option<usize>) -> Option<Arc<Row>> {
    let name = row.cells.first()?.as_str()?.to_string();
    let created = age
        .and_then(|i| row.cells.get(i)?.as_str().and_then(age_seconds))
        .and_then(|s| jiff::Timestamp::now().checked_sub(jiff::SignedDuration::from_secs(s)).ok());
    Some(Arc::new(Row {
        uid: format!("{}/{name}", namespace.unwrap_or_default()),
        name,
        namespace: namespace.map(String::from),
        created,
        cells: row.cells.iter().map(cell_text).collect(),
        deleting: false,
        deleted_at: None,
        labels: "".into(),
    }))
}

/// kubectl's age ("45s", "2m3s", "5h", "3d4h", "2y") in seconds.
pub(crate) fn age_seconds(age: &str) -> Option<i64> {
    let (mut total, mut num) = (0i64, 0i64);
    let mut digits = false;
    for c in age.chars() {
        if let Some(d) = c.to_digit(10) {
            num = num.checked_mul(10)?.checked_add(i64::from(d))?;
            digits = true;
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            'y' => 31_536_000,
            _ => return None,
        };
        if !digits {
            return None;
        }
        total += num * unit;
        (num, digits) = (0, false);
    }
    (!digits && total > 0 || age == "0s").then_some(total)
}

/// Rows of a kind for counts and checks (problems, overviews). A small list comes with its
/// objects' metadata as usual. A big one comes from cells alone: on a cluster with 23k pods,
/// 6 MB instead of 160 MB. Those rows are `is_partial`. `with_namespaces`: each row needs its
/// namespace (`cells_by_namespace`, one request per namespace); without, one request does,
/// and rows of every namespace have none (enough for counts).
pub async fn list_light(
    client: Client,
    kind: Kind,
    namespace: Option<String>,
    with_namespaces: bool,
) -> Result<(Vec<ColumnDef>, Vec<Arc<Row>>)> {
    run(async move {
        let ns = if kind.namespaced { namespace } else { None };
        let url = DynamicObject::url_path(&kind.ar, ns.as_deref());
        let lp = ListParams::default().limit(LIGHT_PROBE);
        let page: ServerTable = client.request(with_table_format(Request::new(&url).list(&lp)?, false)?).await?;
        let defs = page.column_definitions.clone();
        if page.metadata.continue_.as_ref().is_none_or(|c| c.is_empty()) {
            let rows = page.rows.into_iter().filter_map(|r| to_row(r, &Extras::default())).collect();
            return Ok((defs, rows));
        }
        let rows = match (&ns, kind.namespaced) {
            (None, true) if with_namespaces => cells_by_namespace(&client, &url).await?,
            (None, true) => cells_list(&client, &url, None).await?,
            // One namespace, or a cluster-scoped kind: it pages with its metadata as usual.
            _ => list_paged(&client, &url, ListParams::default()).await?.1,
        };
        Ok((defs, rows))
    })
    .await
}

/// `list_light` lists a kind with its objects' metadata when it has at most this many.
const LIGHT_PROBE: u32 = 1000;

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
    watch_table_paused(client, kind, namespace, extras, Pause::default(), true)
}

/// Like `watch_table`, holding snapshots back while `pause` is set. `pages`: show the first
/// list page by page as it arrives (false when rows from before are on screen: the list
/// then comes whole, replacing them in one go).
pub fn watch_table_paused(
    client: Client,
    kind: Kind,
    namespace: Option<String>,
    extras: Extras,
    pause: Pause,
    pages: bool,
) -> (TaskGuard, mpsc::UnboundedReceiver<TableMsg>) {
    let (tx, rx) = mpsc::unbounded();
    let guard = spawn_guarded(async move {
        let ns = if kind.namespaced { namespace.as_deref() } else { None };
        let url = DynamicObject::url_path(&kind.ar, ns);
        // In every namespace of a namespaced kind, a big list can show early by namespace.
        let by_namespace = kind.namespaced && ns.is_none();
        let mut first = if pages { FirstList::Pages { by_namespace } } else { FirstList::Whole };
        let mut backoff = Backoff::default();
        loop {
            // Whether this round's watch got events: only then does the wait start over. A
            // list alone doesn't count, or a watch that keeps failing would relist every second.
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

/// Where a watch is with its first list.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum FirstList {
    /// Not listed yet: show pages as they arrive (and, `by_namespace`, a big list's rows
    /// without objects as soon as they are in: see `cells_by_namespace`).
    Pages { by_namespace: bool },
    /// Not listed yet, rows from before on screen: send the list whole.
    Whole,
    /// Listed (`Listed` sent): a relist keeps the rows until it is complete.
    Done,
}

/// One list then watch. Until a first list completes, pages are shown as they arrive
/// (`FirstList::Pages`) and `Listed` ends them; on a relist the table keeps its rows
/// until the new list is complete, then gets it whole.
pub(super) async fn watch_once(
    client: &Client,
    url: &str,
    tx: &mpsc::UnboundedSender<TableMsg>,
    extras: &Extras,
    first: &mut FirstList,
    pause: &Pause,
    progress: &mut bool,
) -> Result<()> {
    let request = Request::new(url);
    let full = needs_full(extras);
    let (_, list_rows, version) = match *first {
        // A namespaced kind in every namespace, no custom columns: the fast way (see
        // `list_fast`) when the list turns out big.
        FirstList::Pages { by_namespace: true } if extras.is_empty() => list_fast(client, url, tx, pause).await?,
        FirstList::Pages { .. } => {
            // Show rows as pages arrive; big clusters take tens of seconds to list.
            let mut sent_columns = false;
            let listed = list_paged_with(client, url, ListParams::default(), extras, |defs, rows, _| {
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
                let _ = tx.unbounded_send(TableMsg::Columns(Arc::new(listed.0.clone())));
            }
            listed
        }
        _ => {
            let listed = list_paged_with(client, url, ListParams::default(), extras, |_, _, _| {}).await?;
            let _ = tx.unbounded_send(TableMsg::Columns(Arc::new(listed.0.clone())));
            listed
        }
    };
    let mut rows: HashMap<String, Arc<Row>> = list_rows.into_iter().map(|r| (r.uid.clone(), r)).collect();
    let mut snaps = Snapshots { dirty: true, sent: None };
    snaps.flush(&rows, tx, pause)?;
    if *first != FirstList::Done {
        tx.unbounded_send(TableMsg::Listed)?;
        *first = FirstList::Done;
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
                    for line in lines {
                        let line = line?;
                        match apply_event(&line, &mut rows, &mut version, extras)? {
                            Applied::Changed => snaps.dirty = true,
                            Applied::Unchanged => {}
                            // Our resourceVersion is too old: relist (which sends every row).
                            Applied::Relist => return Ok(()),
                        }
                        *progress = true;
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
    #[cfg(feature = "dev")]
    let _t = crate::dev::timing::span("watch event parse (bg)");
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

/// Names (and namespaces) of every object of a kind, in `namespace` or everywhere,
/// from metadata only.
pub async fn list_names(
    client: Client,
    ar: ApiResource,
    namespace: Option<String>,
) -> Result<Vec<(Option<String>, String)>> {
    run(async move {
        let api: Api<DynamicObject> = match &namespace {
            Some(ns) => Api::namespaced_with(client, ns, &ar),
            None => Api::all_with(client, &ar),
        };
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
mod age_tests {
    #[test]
    fn kubectl_ages_read_as_seconds() {
        let a = super::age_seconds;
        assert_eq!(a("45s"), Some(45));
        assert_eq!(a("2m3s"), Some(123));
        assert_eq!(a("5h"), Some(18_000));
        assert_eq!(a("3d4h"), Some(3 * 86_400 + 4 * 3600));
        assert_eq!(a("2y"), Some(63_072_000));
        assert_eq!(a("0s"), Some(0));
        assert_eq!(a("<invalid>"), None);
        assert_eq!(a(""), None);
        assert_eq!(a("5"), None);
        assert_eq!(a("h5"), None);
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
