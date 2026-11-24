//! Lean lists: only the fields a page reads, a page at a time, and errors said plainly.

use std::marker::PhantomData;

use anyhow::Result;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ListMeta;
use kube::api::{DynamicObject, ListParams};
use kube::core::Request;
use kube::discovery::ApiResource;
use kube::{Client, Resource as _};
use serde::Deserialize;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};

use super::{api_status, run_abortable, short_error};

/// Items per page for `list_lean`: the server never builds one giant response.
pub(super) const LEAN_PAGE: u32 = 500;

#[derive(Deserialize)]
#[serde(bound = "T: serde::de::DeserializeOwned")]
struct LeanList<T> {
    #[serde(default)]
    items: Vec<T>,
    #[serde(default)]
    metadata: ListMeta,
}

/// Every object of a kind, parsed into `T`: a struct with only the fields wanted
/// (everything else is skipped while parsing, never held as JSON), listed in pages.
/// Dropping the future stops the paging.
pub async fn list_lean<T>(client: Client, ar: ApiResource, namespace: Option<String>) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    list_lean_by(client, ar, namespace, None).await
}

/// `list_lean` of the objects a label selector picks (every one when None).
async fn list_lean_by<T>(
    client: Client,
    ar: ApiResource,
    namespace: Option<String>,
    labels: Option<String>,
) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    run_abortable(async move {
        let url = DynamicObject::url_path(&ar, namespace.as_deref());
        let mut lp = ListParams::default();
        if let Some(labels) = &labels {
            lp = lp.labels(labels);
        }
        let mut out = Vec::new();
        each_lean_page(&client, &url, lp, |page: Vec<T>| out.extend(page)).await?;
        Ok(out)
    })
    .await
}

/// The objects at `url` that `lp` selects, parsed into `T` (only the wanted fields are ever
/// held), a page of `lp.limit` items at a time (`LEAN_PAGE` when unset), each page given to
/// `page` as soon as it is read: a caller keeps only what it needs of them. Every page is
/// read, to the last `continue` token; the list's resourceVersion (to watch from) comes back.
/// For work already on the runtime (`run`).
pub(super) async fn each_lean_page<T: serde::de::DeserializeOwned>(
    client: &Client,
    url: &str,
    mut lp: ListParams,
    mut page: impl FnMut(Vec<T>),
) -> Result<String> {
    lp.limit = lp.limit.or(Some(LEAN_PAGE));
    loop {
        let list: LeanList<T> = request_idle(client, Request::new(url).list(&lp)?).await?;
        page(list.items);
        match list.metadata.continue_.filter(|c| !c.is_empty()) {
            Some(token) => lp.continue_token = Some(token),
            None => return Ok(list.metadata.resource_version.unwrap_or_default()),
        }
    }
}

/// Like `list_lean`, in one response from the API server's cache (`resourceVersion=0`):
/// twice as fast as paging a big list, which the server reads again for each page. Parsed
/// as it arrives on a blocking thread, so only the wanted fields are ever held. Pages
/// instead when the server will not answer that way (see `list_lean_cached_by`).
pub async fn list_lean_cached<T>(client: Client, ar: ApiResource, namespace: Option<String>) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    list_lean_cached_by(client, ar, namespace, None).await
}

/// `list_lean_cached` of the objects a label selector picks (every one when None). Pages
/// instead when the server will not answer from its cache, unless it refused the list (401,
/// 403, 404): paging it would be refused the same, one request later.
pub async fn list_lean_cached_by<T>(
    client: Client,
    ar: ApiResource,
    namespace: Option<String>,
    labels: Option<String>,
) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    let (c, ar2, ns, selector) = (client.clone(), ar.clone(), namespace.clone(), labels.clone());
    let whole = run_abortable(async move {
        use tokio_util::compat::FuturesAsyncReadCompatExt as _;
        let url = DynamicObject::url_path(&ar2, ns.as_deref());
        let mut path = format!("{url}?resourceVersion=0&resourceVersionMatch=NotOlderThan");
        if let Some(selector) = &selector {
            path.push_str("&labelSelector=");
            path.push_str(&super::url_escape(selector));
        }
        let body = c.request_stream(http::Request::get(&path).body(Vec::new())?).await?;
        let list: LeanList<T> = parse_streamed(Idle::new(body.compat(), LIST_IDLE)).await?;
        anyhow::ensure!(list.metadata.continue_.as_ref().is_none_or(String::is_empty), "the API server paged it");
        Ok(list.items)
    })
    .await;
    match whole {
        Ok(items) => Ok(items),
        Err(e) if matches!(api_status(&e), Some(401 | 403 | 404)) => Err(e),
        Err(_) => list_lean_by(client, ar, namespace, labels).await,
    }
}

/// Every object at `url` that `lp` selects, a page of `lp.limit` at a time (`LEAN_PAGE` when
/// unset), to the last `continue` token: each page parsed as it streams in on a blocking
/// thread, its objects parsed into `T` and given to `add` with `acc` one at a time. Neither a
/// page nor its objects are ever held, only what `add` keeps, whatever the size of a page; the
/// API server reads and sends no more than a page at a time. Dropping the future stops it.
pub(super) async fn fold_lean_pages<T, A>(
    client: Client,
    url: String,
    mut lp: ListParams,
    acc: A,
    add: fn(&mut A, T),
) -> Result<A>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    A: Send + 'static,
{
    run_abortable(async move {
        use tokio_util::compat::FuturesAsyncReadCompatExt as _;
        lp.limit = lp.limit.or(Some(LEAN_PAGE));
        let mut acc = acc;
        loop {
            let body = client.request_stream(Request::new(&url).list(&lp)?).await?;
            let (folded, meta) = parse_streamed_seed(Idle::new(body.compat(), LIST_IDLE), Fold { acc, add }).await?;
            acc = folded;
            match meta.continue_.filter(|c| !c.is_empty()) {
                Some(token) => lp.continue_token = Some(token),
                None => return Ok(acc),
            }
        }
    })
    .await
}

/// Parse a response body as it arrives, on a blocking thread, holding only what `T` keeps.
/// Dropping the future stops the read and closes the connection, even while it waits for data.
pub(super) async fn parse_streamed<T, R>(body: R) -> Result<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    parse_streamed_seed(body, PhantomData::<T>).await
}

/// `parse_streamed` through a seed, which decides what is kept of the body as it is read
/// (`Fold`: what its accumulator keeps of each item).
async fn parse_streamed_seed<S, R>(body: R, seed: S) -> Result<S::Value>
where
    S: DeserializeSeed<'static> + Send + 'static,
    S::Value: Send + 'static,
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let (_reading, dropped) = tokio::sync::oneshot::channel();
    let reader = tokio_util::io::SyncIoBridge::new(Cancellable { inner: body, dropped, cancelled: false });
    let parsed = tokio::task::spawn_blocking(move || {
        let mut de = serde_json::Deserializer::from_reader(std::io::BufReader::with_capacity(256 * 1024, reader));
        let value = seed.deserialize(&mut de)?;
        de.end()?;
        Ok::<_, serde_json::Error>(value)
    })
    .await??;
    Ok(parsed)
}

/// A list's objects, each parsed into `T` and given to `add` as soon as it is read: the list
/// is never held, only what `acc` keeps of them. Its value is `acc`, and the list's metadata
/// (for its `continue` token). Other keys (`kind`, `apiVersion`) are skipped.
struct Fold<T, A> {
    acc: A,
    add: fn(&mut A, T),
}

impl<'de, T: Deserialize<'de>, A> DeserializeSeed<'de> for Fold<T, A> {
    type Value = (A, ListMeta);

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> std::result::Result<Self::Value, D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de, T: Deserialize<'de>, A> Visitor<'de> for Fold<T, A> {
    type Value = (A, ListMeta);

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a list")
    }

    fn visit_map<M: MapAccess<'de>>(mut self, mut map: M) -> std::result::Result<Self::Value, M::Error> {
        let mut meta = ListMeta::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "items" => map.next_value_seed(Items { acc: &mut self.acc, add: self.add })?,
                "metadata" => meta = map.next_value::<Option<ListMeta>>()?.unwrap_or_default(),
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok((self.acc, meta))
    }
}

/// A list's items (`null` reads as none), each given to `add` as soon as it is read.
struct Items<'a, T, A> {
    acc: &'a mut A,
    add: fn(&mut A, T),
}

impl<'de, T: Deserialize<'de>, A> DeserializeSeed<'de> for Items<'_, T, A> {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> std::result::Result<(), D::Error> {
        d.deserialize_option(self)
    }
}

impl<'de, T: Deserialize<'de>, A> Visitor<'de> for Items<'_, T, A> {
    type Value = ();

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a list's items")
    }

    fn visit_none<E: serde::de::Error>(self) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<(), E> {
        Ok(())
    }

    fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> std::result::Result<(), D::Error> {
        d.deserialize_seq(self)
    }

    fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> std::result::Result<(), S::Error> {
        while let Some(item) = seq.next_element::<T>()? {
            (self.add)(self.acc, item);
        }
        Ok(())
    }
}

/// A reader that fails once its `dropped` sender is gone.
struct Cancellable<R> {
    inner: R,
    dropped: tokio::sync::oneshot::Receiver<()>,
    cancelled: bool,
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Cancellable<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if !self.cancelled && std::pin::Pin::new(&mut self.dropped).poll(cx).is_ready() {
            self.cancelled = true;
        }
        if self.cancelled {
            return std::task::Poll::Ready(Err(std::io::Error::other("cancelled")));
        }
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

/// The longest a list's response may go without a byte. The request timeout covers only
/// the headers: on a connection that died without a word (sleep, VPN) the body would never
/// end, and the list would stay "Loading" for good.
pub(super) const LIST_IDLE: std::time::Duration = std::time::Duration::from_secs(60);

/// A response body that fails once no byte has come for `idle`.
pub(super) struct Idle<R> {
    inner: R,
    idle: std::time::Duration,
    deadline: std::pin::Pin<Box<tokio::time::Sleep>>,
}

impl<R> Idle<R> {
    pub(super) fn new(inner: R, idle: std::time::Duration) -> Self {
        Self { inner, idle, deadline: Box::pin(tokio::time::sleep(idle)) }
    }
}

impl<R: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Idle<R> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let (idle, before) = (self.idle, buf.filled().len());
        let polled = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if polled.is_ready() {
            if buf.filled().len() > before {
                self.deadline.as_mut().reset(tokio::time::Instant::now() + idle);
            }
            return polled;
        }
        match self.deadline.as_mut().poll(cx) {
            std::task::Poll::Ready(()) => std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("no data for {} s", idle.as_secs()),
            ))),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

/// A request's JSON answer, given up when its body stalls for `LIST_IDLE`.
pub(super) async fn request_idle<T: serde::de::DeserializeOwned>(
    client: &Client,
    req: http::Request<Vec<u8>>,
) -> Result<T> {
    use tokio::io::AsyncReadExt as _;
    use tokio_util::compat::FuturesAsyncReadCompatExt as _;
    let body = client.request_stream(req).await?;
    let mut bytes = Vec::new();
    Idle::new(body.compat(), LIST_IDLE).read_to_end(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Why something could not be loaded, as a section header says it: "forbidden"
/// (the user may not read it) or "failed: <the server's message>".
pub fn load_error(e: &anyhow::Error) -> String {
    match api_status(e) {
        Some(403) => "forbidden".into(),
        _ => format!("failed: {}", short_error(e)),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn lean_lists_skip_other_fields() {
        #[derive(serde::Deserialize)]
        struct Name {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
        }
        let json = r#"{"kind":"PodList","metadata":{"continue":"abc","resourceVersion":"9"},
            "items":[{"metadata":{"name":"a"},"spec":{"containers":[{"name":"c"}]},"status":{"phase":"Running"}}]}"#;
        let list: super::LeanList<Name> = serde_json::from_str(json).unwrap();
        assert_eq!(list.items[0].metadata.name.as_deref(), Some("a"));
        assert_eq!(list.metadata.continue_.as_deref(), Some("abc"));
    }

    /// An object's name: all a lean read here keeps of it.
    #[derive(serde::Deserialize)]
    struct Named {
        metadata: NameOnly,
    }

    #[derive(serde::Deserialize)]
    struct NameOnly {
        name: String,
    }

    fn push_name(names: &mut Vec<String>, item: Named) {
        names.push(item.metadata.name);
    }

    proptest::proptest! {
        /// A list folded as it is read gives each item once, in order, and the list's metadata,
        /// whatever order its keys come in, whatever else it and its items hold, and `null` or
        /// no items at all as none.
        #[test]
        fn a_folded_list_gives_every_item_in_order(
            names in proptest::collection::vec("[a-z]{1,8}", 0..30),
            more in proptest::option::of("[a-z0-9]{1,6}"),
            order in 0usize..8,
            items in 0u8..3,
        ) {
            use serde::de::DeserializeSeed as _;
            let objects: Vec<serde_json::Value> = names
                .iter()
                .map(|n| serde_json::json!({"metadata": {"name": n, "managedFields": [{"manager": "x"}]}, "spec": {"a": [1, 2]}}))
                .collect();
            let mut metadata = serde_json::json!({"resourceVersion": "7"});
            if let Some(token) = &more {
                metadata["continue"] = token.clone().into();
            }
            // 0: the items; 1: null; 2: no `items` key.
            let items_value = match items {
                0 => Some(serde_json::Value::Array(objects)),
                1 => Some(serde_json::Value::Null),
                _ => None,
            };
            let mut parts = vec![
                r#""kind":"PodList""#.to_string(),
                r#""apiVersion":"v1""#.to_string(),
                format!(r#""metadata":{metadata}"#),
            ];
            if let Some(v) = items_value {
                parts.push(format!(r#""items":{v}"#));
            }
            let keys = parts.len();
            parts.rotate_left(order % keys);
            if order >= 4 {
                parts.reverse();
            }
            let body = format!("{{{}}}", parts.join(","));
            let mut de = serde_json::Deserializer::from_str(&body);
            let (read, meta) = super::Fold { acc: Vec::new(), add: push_name }.deserialize(&mut de).unwrap();
            de.end().unwrap();
            let expected: Vec<String> = if items == 0 { names } else { Vec::new() };
            proptest::prop_assert_eq!(read, expected);
            proptest::prop_assert_eq!(meta.continue_, more);
            proptest::prop_assert_eq!(meta.resource_version.as_deref(), Some("7"));
        }
    }

    /// An API server that answers a list from its cache (`resourceVersion=0`) with `cached`, a
    /// status code (200: the list), and a paged one with the list. Records the queries.
    #[derive(Clone)]
    struct Answers {
        cached: u16,
        asked: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl tower::Service<http::Request<kube::client::Body>> for Answers {
        type Response = http::Response<http_body_util::Full<bytes::Bytes>>;
        type Error = std::convert::Infallible;
        type Future = std::future::Ready<Result<Self::Response, Self::Error>>;

        fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, req: http::Request<kube::client::Body>) -> Self::Future {
            let query = req.uri().query().unwrap_or_default().to_string();
            self.asked.lock().unwrap().push(query.clone());
            let (code, body) = if query.contains("resourceVersion=0") && self.cached != 200 {
                let refusal = serde_json::json!({"kind": "Status", "apiVersion": "v1", "status": "Failure",
                    "message": "no", "code": self.cached});
                (self.cached, refusal)
            } else {
                let list = serde_json::json!({"kind": "PodList", "apiVersion": "v1", "metadata": {"resourceVersion": "7"},
                    "items": [{"metadata": {"name": "a"}}, {"metadata": {"name": "b"}}]});
                (200, list)
            };
            let response = http::Response::builder().status(code).body(body.to_string().into_bytes().into()).unwrap();
            std::future::ready(Ok(response))
        }
    }

    /// A cached list that fails is read again page by page; one the server refuses is not
    /// asked again (paging it would be refused the same), and says why. The same with a label
    /// selector, which every request keeps.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_list_is_asked_once_and_a_failed_one_again() {
        let answers = [(200, 1, true), (500, 2, true), (401, 1, false), (403, 1, false), (404, 1, false)];
        for labels in [None, Some("app=web")] {
            for (cached, requests, read) in answers {
                let server = Answers { cached, asked: std::sync::Arc::default() };
                let client = kube::Client::new(server.clone(), "default");
                let ar = crate::k8s::ar_of::<k8s_openapi::api::core::v1::Pod>();
                let listed = match labels {
                    None => super::list_lean_cached::<Named>(client, ar, None).await,
                    Some(l) => super::list_lean_cached_by::<Named>(client, ar, None, Some(l.into())).await,
                };
                if read {
                    let names: Vec<String> = listed.unwrap().into_iter().map(|i| i.metadata.name).collect();
                    assert_eq!(names, ["a", "b"], "{cached} {labels:?}");
                } else {
                    assert_eq!(listed.err().and_then(|e| super::api_status(&e)), Some(cached), "{labels:?}");
                }
                let asked = server.asked.lock().unwrap().clone();
                assert_eq!(asked.len(), requests, "{cached} {labels:?}: {asked:?}");
                assert!(asked[0].contains("resourceVersion=0"), "{asked:?}");
                if labels.is_some() {
                    assert!(asked.iter().all(|q| q.contains("labelSelector=app%3Dweb")), "{asked:?}");
                }
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dropping_a_streamed_read_stops_it() {
        use tokio::io::AsyncWriteExt as _;
        let (mut server, client) = tokio::io::duplex(64);
        // The server sends part of a list, then nothing: the read waits on the connection.
        server.write_all(br#"{"items":["#).await.unwrap();
        let read = tokio::spawn(super::parse_streamed::<serde_json::Value, _>(client));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        read.abort();
        let mut closed = false;
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            if server.write_all(b" ").await.is_err() {
                closed = true;
                break;
            }
        }
        assert!(closed, "the read kept the connection open");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn streamed_reads_parse() {
        let (mut server, client) = tokio::io::duplex(64);
        let read = tokio::spawn(super::parse_streamed::<serde_json::Value, _>(client));
        tokio::io::AsyncWriteExt::write_all(&mut server, br#"{"items":[1,2]}"#).await.unwrap();
        drop(server);
        assert_eq!(read.await.unwrap().unwrap()["items"][1], 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stalled_list_is_given_up() {
        use tokio::io::AsyncWriteExt as _;
        let idle = std::time::Duration::from_millis(100);
        let (mut server, client) = tokio::io::duplex(64);
        // Part of a list, then nothing, the connection still open (a laptop gone to sleep).
        server.write_all(br#"{"items":["#).await.unwrap();
        let read = super::parse_streamed::<serde_json::Value, _>(super::Idle::new(client, idle));
        let done = tokio::time::timeout(std::time::Duration::from_secs(10), read).await;
        assert!(done.expect("the read waited for good").is_err());
        drop(server);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_slow_list_that_keeps_coming_is_read() {
        use tokio::io::AsyncWriteExt as _;
        let idle = std::time::Duration::from_millis(200);
        let (mut server, client) = tokio::io::duplex(64);
        let read = tokio::spawn(super::parse_streamed::<serde_json::Value, _>(super::Idle::new(client, idle)));
        // Longer than `idle` in all, but never that long between two writes.
        for part in [&br#"{"items":"#[..], b"[1,", b"2]", b"}"] {
            server.write_all(part).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        }
        drop(server);
        assert_eq!(read.await.unwrap().unwrap()["items"][1], 2);
    }

    #[test]
    fn load_errors() {
        assert_eq!(super::load_error(&anyhow::anyhow!("connection reset")), "failed: connection reset");
    }
}
