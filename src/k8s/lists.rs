//! Lean lists: only the fields a page reads, a page at a time, and errors said plainly.

use anyhow::Result;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ListMeta;
use kube::api::{DynamicObject, ListParams};
use kube::core::Request;
use kube::discovery::ApiResource;
use kube::{Client, Resource as _};
use serde::Deserialize;

use super::{api_status, run_abortable, short_error};

/// Items per page for `list_lean`: the server never builds one giant response.
const LEAN_PAGE: u32 = 500;

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
    run_abortable(async move {
        let url = DynamicObject::url_path(&ar, namespace.as_deref());
        let mut lp = ListParams::default().limit(LEAN_PAGE);
        let mut out = Vec::new();
        loop {
            let page: LeanList<T> = request_idle(&client, Request::new(&url).list(&lp)?).await?;
            out.extend(page.items);
            match page.metadata.continue_.filter(|c| !c.is_empty()) {
                Some(token) => lp.continue_token = Some(token),
                None => break,
            }
        }
        Ok(out)
    })
    .await
}

/// Like `list_lean`, in one response from the API server's cache (`resourceVersion=0`):
/// twice as fast as paging a big list, which the server reads again for each page. Parsed
/// as it arrives on a blocking thread, so only the wanted fields are ever held. Pages
/// instead when the server will not answer that way.
pub async fn list_lean_cached<T>(client: Client, ar: ApiResource, namespace: Option<String>) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    let (c, ar2, ns) = (client.clone(), ar.clone(), namespace.clone());
    let whole = run_abortable(async move {
        use tokio_util::compat::FuturesAsyncReadCompatExt as _;
        let url = DynamicObject::url_path(&ar2, ns.as_deref());
        let path = format!("{url}?resourceVersion=0&resourceVersionMatch=NotOlderThan");
        let body = c.request_stream(http::Request::get(&path).body(Vec::new())?).await?;
        let list: LeanList<T> = parse_streamed(Idle::new(body.compat(), LIST_IDLE)).await?;
        anyhow::ensure!(list.metadata.continue_.as_ref().is_none_or(|c| c.is_empty()), "the API server paged it");
        Ok(list.items)
    })
    .await;
    match whole {
        Ok(items) => Ok(items),
        Err(_) => list_lean(client, ar, namespace).await,
    }
}

/// Parse a response body as it arrives, on a blocking thread, holding only what `T` keeps.
/// Dropping the future stops the read and closes the connection, even while it waits for data.
pub(super) async fn parse_streamed<T, R>(body: R) -> Result<T>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let (_reading, dropped) = tokio::sync::oneshot::channel();
    let reader = tokio_util::io::SyncIoBridge::new(Cancellable { inner: body, dropped, cancelled: false });
    let parsed = tokio::task::spawn_blocking(move || {
        serde_json::from_reader(std::io::BufReader::with_capacity(256 * 1024, reader))
    })
    .await??;
    Ok(parsed)
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
