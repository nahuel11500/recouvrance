//! How a call learns that its agent stopped waiting for the answer.

use std::pin::Pin;
use std::task::{Context, Poll};

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use bytes::Bytes;
use hyper::body::{Body as HttpBody, Frame, SizeHint};
use tokio_util::sync::{CancellationToken, DropGuard};

/// Tells a call that its agent stopped waiting for the answer: it cancelled the call (MCP's
/// `notifications/cancelled`; rmcp does the same when the session ends), or its HTTP
/// connection closed before the reply was sent ([`hangup`]). rmcp lets the call run on, so
/// the call itself ends its approval then, and makes no change after.
#[derive(Clone, Default)]
pub(crate) struct Gone {
    /// rmcp's token for the MCP request.
    pub(super) request: Option<CancellationToken>,
    /// Cancelled when the HTTP response is dropped before its end.
    pub(super) connection: Option<CancellationToken>,
}

impl Gone {
    pub(super) fn is_gone(&self) -> bool {
        self.request.iter().chain(&self.connection).any(CancellationToken::is_cancelled)
    }

    /// Ends once the agent stops waiting (never, when nothing can tell).
    pub(super) async fn wait(&self) {
        tokio::select! {
            () = until_cancelled(self.request.as_ref()) => {}
            () = until_cancelled(self.connection.as_ref()) => {}
        }
    }
}

async fn until_cancelled(token: Option<&CancellationToken>) {
    match token {
        Some(token) => token.cancelled().await,
        None => std::future::pending().await,
    }
}

/// Gives each request the [`Gone`] token of its connection: cancelled when its response is
/// dropped before its end, the client having hung up (the assistant's Stop drops its
/// connection, a killed `claude -p` closes its own). In rmcp's session mode nothing else tells
/// a running call: the session keeps the reply for a client that would resume the stream.
pub(super) async fn hangup(mut req: Request, next: Next) -> Response {
    let connection = CancellationToken::new();
    req.extensions_mut().insert(Gone { request: None, connection: Some(connection.clone()) });
    // Armed now: a client gone before the response starts drops this future, and the guard.
    let hangup = Some(connection.drop_guard());
    next.run(req).await.map(|body| axum::body::Body::new(UntilEnd { body, hangup }))
}

/// A response body that cancels its request's connection token when it is dropped before
/// its end (see [`hangup`]).
struct UntilEnd {
    body: axum::body::Body,
    hangup: Option<DropGuard>,
}

impl UntilEnd {
    /// Sent whole: the client has its reply, so dropping the body now tells nothing.
    fn sent(&mut self) {
        if let Some(guard) = self.hangup.take() {
            guard.disarm();
        }
    }
}

impl HttpBody for UntilEnd {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, axum::Error>>> {
        let frame = Pin::new(&mut self.body).poll_frame(cx);
        if matches!(frame, Poll::Ready(None)) || self.body.is_end_stream() {
            self.sent();
        }
        frame
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

impl Drop for UntilEnd {
    fn drop(&mut self) {
        // hyper does not poll a body it knows is whole (a 202 with nothing in it) to its end.
        if self.body.is_end_stream() {
            self.sent();
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::{FutureExt as _, StreamExt as _};
    use http_body_util::BodyExt as _;

    use super::*;

    /// Either rmcp's token for the request or its connection's tells the call its agent is
    /// gone; with neither (not an HTTP request), nothing does.
    #[tokio::test]
    async fn either_token_tells_a_call_its_agent_is_gone() {
        assert!(!Gone::default().is_gone());
        assert!(Gone::default().wait().now_or_never().is_none());
        for cancel_request in [true, false] {
            let (request, connection) = (CancellationToken::new(), CancellationToken::new());
            let gone = Gone { request: Some(request.clone()), connection: Some(connection.clone()) };
            assert!(!gone.is_gone() && gone.wait().now_or_never().is_none());
            if cancel_request {
                request.cancel();
            } else {
                connection.cancel();
            }
            assert!(gone.is_gone() && gone.wait().now_or_never().is_some(), "cancel_request={cancel_request}");
        }
    }

    /// A reply dropped before its end means the client hung up: the call's connection token is
    /// cancelled. A reply sent whole, or known to be whole when dropped, cancels nothing.
    #[tokio::test]
    async fn a_reply_dropped_before_its_end_tells_the_call() {
        let event = || Ok::<_, std::convert::Infallible>(Bytes::from("data: {}\n\n"));
        let reply = |body: axum::body::Body| {
            let token = CancellationToken::new();
            (UntilEnd { body, hangup: Some(token.clone().drop_guard()) }, token)
        };
        // An SSE reply still running when the client goes (hyper then drops it).
        let running = futures::stream::iter([event()]).chain(futures::stream::pending());
        let (mut body, token) = reply(axum::body::Body::from_stream(running));
        assert!(body.frame().await.is_some());
        assert!(!token.is_cancelled());
        drop(body);
        assert!(token.is_cancelled());
        // Sent whole.
        let (body, token) = reply(axum::body::Body::from_stream(futures::stream::iter([event(), event()])));
        assert_eq!(body.collect().await.unwrap().to_bytes().len(), 20);
        assert!(!token.is_cancelled());
        // Whole when dropped: an empty 202, a JSON reply in one frame.
        let (body, token) = reply(axum::body::Body::empty());
        drop(body);
        assert!(!token.is_cancelled());
        let (mut body, token) = reply(axum::body::Body::from("{}"));
        assert!(body.frame().await.is_some());
        drop(body);
        assert!(!token.is_cancelled());
    }
}
