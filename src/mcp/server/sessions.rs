//! How long an agent's MCP session stays open.
//!
//! rmcp's own idle timer (`SessionConfig::keep_alive`, 5 minutes) counts only the messages of
//! a session, not its calls still running: it closed the session of a call that ran longer (an
//! approval answered slowly, then `wait_for`; a long `exec_command`), which cancelled the call,
//! and the agent still waiting never got the answer (#858). Here a session closes only once it
//! has been unused for [`IDLE`] with none of its calls running.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use rmcp::model::{ClientJsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::streamable_http_server::session::local::{LocalSessionManager, LocalSessionManagerError};
use rmcp::transport::streamable_http_server::session::{ServerSseMessage, SessionId, SessionManager};
use tokio::time::Instant;

use crate::mcp::lock;

/// How long a session with none of its calls running stays open after it was last used. Longer
/// than the Activity page's 10-minute split, so a pause in a task keeps its session; a session its
/// client never closes (a killed `claude -p`) is gone after that long.
pub(super) const IDLE: Duration = Duration::from_secs(60 * 60);

/// rmcp's sessions, closed once unused for `idle` with none of their calls running.
pub(super) struct Sessions(Arc<Shared>);

struct Shared {
    inner: LocalSessionManager,
    table: Mutex<Table>,
    idle: Duration,
}

impl Sessions {
    pub(super) fn new(idle: Duration) -> Self {
        let mut inner = LocalSessionManager::default();
        // Its timer would close a session whatever runs: the watch below closes them instead.
        inner.session_config.keep_alive = None;
        Self(Arc::new(Shared { inner, table: Mutex::default(), idle }))
    }

    /// Counts a request of the session as running until the guard is dropped. An error when the
    /// session is closed, or about to be: a request must not start in a session closing under it.
    fn begin(&self, id: &SessionId) -> Result<Running, LocalSessionManagerError> {
        if !lock(&self.0.table).begin(id, Instant::now()) {
            return Err(LocalSessionManagerError::SessionNotFound(id.clone()));
        }
        Ok(Running { shared: Arc::downgrade(&self.0), id: id.clone() })
    }

    fn touch(&self, id: &SessionId) {
        lock(&self.0.table).touch(id, Instant::now());
    }
}

impl SessionManager for Sessions {
    type Error = LocalSessionManagerError;
    type Transport = <LocalSessionManager as SessionManager>::Transport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        let (id, transport) = self.0.inner.create_session().await?;
        lock(&self.0.table).open(id.clone(), Instant::now());
        tokio::spawn(watch(Arc::downgrade(&self.0), id.clone()));
        Ok((id, transport))
    }

    async fn initialize_session(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<ServerJsonRpcMessage, Self::Error> {
        let _running = self.begin(id)?;
        self.0.inner.initialize_session(id, message).await
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        // One taken out to be closed is gone already: its client gets a 404 and opens another.
        let open = lock(&self.0.table).is_open(id);
        Ok(open && self.0.inner.has_session(id).await?)
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        lock(&self.0.table).close(id);
        self.0.inner.close_session(id).await
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        let running = self.begin(id)?;
        let stream = self.0.inner.create_stream(id, message).await?;
        Ok(Reply { stream: Box::pin(stream), running: Some(running) })
    }

    async fn accept_message(&self, id: &SessionId, message: ClientJsonRpcMessage) -> Result<(), Self::Error> {
        self.touch(id);
        self.0.inner.accept_message(id, message).await
    }

    async fn create_standalone_stream(
        &self,
        id: &SessionId,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.touch(id);
        self.0.inner.create_standalone_stream(id).await
    }

    async fn resume(
        &self,
        id: &SessionId,
        last_event_id: String,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.touch(id);
        self.0.inner.resume(id, last_event_id).await
    }
}

/// Closes the session once it has been unused for `idle` with none of its calls running. Ends
/// when the session is closed otherwise (by its client, or its handler ending), or the server
/// stops.
async fn watch(shared: Weak<Shared>, id: SessionId) {
    loop {
        let wake = {
            let Some(sessions) = shared.upgrade() else { return };
            let check = lock(&sessions.table).check(&id, Instant::now(), sessions.idle);
            match check {
                Check::Wait(wake) => wake,
                Check::Idle => {
                    let _ = sessions.inner.close_session(&id).await;
                    return;
                }
                Check::Gone => return,
            }
        };
        tokio::time::sleep_until(wake).await;
    }
}

/// When a session was last used, and how many of its requests wait for their reply.
#[derive(Clone, Copy)]
struct Activity {
    last: Instant,
    running: usize,
}

/// The open sessions.
#[derive(Default)]
struct Table(HashMap<SessionId, Activity>);

/// What a session's watch does next.
#[derive(Debug, PartialEq, Eq)]
enum Check {
    /// Look again then.
    Wait(Instant),
    /// Unused long enough with nothing running: taken out of the table, to be closed.
    Idle,
    /// Closed already.
    Gone,
}

impl Table {
    fn open(&mut self, id: SessionId, now: Instant) {
        self.0.insert(id, Activity { last: now, running: 0 });
    }

    fn is_open(&self, id: &SessionId) -> bool {
        self.0.contains_key(id)
    }

    /// A message of the session that waits for no reply: a notification, a stream opened.
    fn touch(&mut self, id: &SessionId, now: Instant) {
        if let Some(session) = self.0.get_mut(id) {
            session.last = now;
        }
    }

    /// A request of the session waits for its reply from now on; false when the session is not
    /// open (closed, or taken out to be closed).
    fn begin(&mut self, id: &SessionId, now: Instant) -> bool {
        let Some(session) = self.0.get_mut(id) else { return false };
        session.running += 1;
        session.last = now;
        true
    }

    /// A request's reply ended: sent, or its client hung up. With nothing else running, the
    /// session is unused from now on.
    fn end(&mut self, id: &SessionId, now: Instant) {
        if let Some(session) = self.0.get_mut(id) {
            session.running = session.running.saturating_sub(1);
            session.last = now;
        }
    }

    fn close(&mut self, id: &SessionId) {
        self.0.remove(id);
    }

    /// Whether the session closes now: once unused for `idle` with nothing running. One to close
    /// is taken out at once, so a request coming meanwhile finds it closed rather than starting.
    fn check(&mut self, id: &SessionId, now: Instant, idle: Duration) -> Check {
        let Some(&session) = self.0.get(id) else { return Check::Gone };
        if session.running > 0 {
            // A call may run for hours (`exec_timeout` has no limit): look again later.
            return Check::Wait(now + idle);
        }
        let due = session.last + idle;
        if now < due {
            return Check::Wait(due);
        }
        self.0.remove(id);
        Check::Idle
    }
}

/// A request of a session waiting for its reply.
struct Running {
    shared: Weak<Shared>,
    id: SessionId,
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(sessions) = self.shared.upgrade() {
            lock(&sessions.table).end(&self.id, Instant::now());
        }
    }
}

/// A request's reply stream: the request runs until the stream ends (the reply sent) or is
/// dropped (its client hung up).
struct Reply<S> {
    stream: Pin<Box<S>>,
    running: Option<Running>,
}

impl<S: Stream> Stream for Reply<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let next = self.stream.as_mut().poll_next(cx);
        if matches!(next, Poll::Ready(None)) {
            self.running = None;
        }
        next
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt as _;
    use rmcp::model::{RequestId, ServerResult};
    use rmcp::transport::Transport as _;
    use serde_json::json;

    use super::*;

    /// A session closes once unused for `idle` with none of its calls running, and never while
    /// one runs, however long it takes (#858).
    #[test]
    fn a_session_closes_once_unused_with_no_call_running() {
        let idle = Duration::from_secs(60);
        let start = Instant::now();
        let at = |secs: u64| start + Duration::from_secs(secs);
        let id: SessionId = "s1".into();
        let mut table = Table::default();
        table.open(id.clone(), at(0));
        assert_eq!(table.check(&id, at(59), idle), Check::Wait(at(60)));
        // Calls running far longer than `idle` keep it open, even after another one ended.
        assert!(table.begin(&id, at(30)));
        assert!(table.begin(&id, at(100)));
        table.end(&id, at(200));
        assert_eq!(table.check(&id, at(5000), idle), Check::Wait(at(5060)));
        // Unused from the end of its last call; a message is a use too.
        table.end(&id, at(6000));
        assert_eq!(table.check(&id, at(6059), idle), Check::Wait(at(6060)));
        table.touch(&id, at(6030));
        assert_eq!(table.check(&id, at(6060), idle), Check::Wait(at(6090)));
        // Unused long enough: taken out at once, so a request coming now finds it closed.
        assert_eq!(table.check(&id, at(6090), idle), Check::Idle);
        assert!(!table.is_open(&id));
        assert!(!table.begin(&id, at(6090)));
        assert_eq!(table.check(&id, at(6090), idle), Check::Gone);
        // Closed by its client: its watch ends.
        let other: SessionId = "s2".into();
        table.open(other.clone(), at(0));
        table.close(&other);
        assert_eq!(table.check(&other, at(1), idle), Check::Gone);
    }

    fn client_message(value: serde_json::Value) -> ClientJsonRpcMessage {
        serde_json::from_value(value).unwrap()
    }

    fn answer(id: i64) -> ServerJsonRpcMessage {
        ServerJsonRpcMessage::response(ServerResult::empty(()), RequestId::Number(id))
    }

    /// A session opened and initialized, with the transport its handler reads and answers on.
    async fn opened(sessions: &Sessions) -> (SessionId, <Sessions as SessionManager>::Transport) {
        let (id, mut transport) = sessions.create_session().await.unwrap();
        let client = json!({"name": "test", "version": "1"});
        let params = json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": client});
        let init = client_message(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": params}));
        let handler = async {
            assert!(transport.receive().await.is_some());
            transport.send(answer(1)).await.unwrap();
        };
        let (initialized, ()) = tokio::join!(sessions.initialize_session(&id, init), handler);
        initialized.unwrap();
        (id, transport)
    }

    /// Through rmcp's sessions: one stays open while a request waits for its reply, for longer
    /// than `idle`, then closes once unused for `idle` after the reply was sent or hung up on.
    #[tokio::test]
    async fn a_session_stays_open_while_a_request_waits_for_its_reply() {
        let idle = Duration::from_millis(100);
        let sessions = Sessions::new(idle);
        // rmcp's own timer would close it, call running or not.
        assert!(sessions.0.inner.session_config.keep_alive.is_none());
        for hang_up in [false, true] {
            let (id, mut transport) = opened(&sessions).await;
            let ping = client_message(json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}));
            let mut reply = Box::pin(sessions.create_stream(&id, ping).await.unwrap());
            assert!(transport.receive().await.is_some(), "the handler gets the request");
            tokio::time::sleep(idle * 5).await;
            assert!(sessions.has_session(&id).await.unwrap(), "closed while a request ran (hang_up={hang_up})");
            if hang_up {
                drop(reply);
            } else {
                transport.send(answer(2)).await.unwrap();
                while reply.next().await.is_some() {}
            }
            // Unused from then on: closed, which ends its handler's transport.
            let closed = async { while transport.receive().await.is_some() {} };
            tokio::time::timeout(Duration::from_secs(10), closed).await.expect("an unused session closes");
            assert!(!sessions.has_session(&id).await.unwrap());
            assert!(!sessions.0.inner.has_session(&id).await.unwrap(), "hang_up={hang_up}");
        }
    }
}
