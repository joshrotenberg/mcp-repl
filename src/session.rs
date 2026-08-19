//! Session resurrection: keeping the REPL usable when the server loses the
//! session underneath it.
//!
//! A remote MCP server that restarts, OOMs, or sits behind an edge returning
//! 502/503 leaves the client holding a session id the server no longer knows.
//! Every subsequent request fails, and without recovery the prompt is dead
//! until the user quits and reconnects by hand.
//!
//! [`Session`] holds the live [`McpClient`] behind a swappable slot plus the
//! recipe for building a fresh one. When a command fails with a session-loss
//! error, the REPL rebuilds the connection from scratch (new transport, new
//! session id, fresh handshake) and retries the command once.
//!
//! Rebuilding rather than reusing the transport is deliberate: the existing
//! HTTP transport still carries the dead `Mcp-Session-Id`, so re-initializing
//! on it can fail the same way. A new transport starts from no session at all.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tower_mcp::client::McpClient;

/// Builds a fully connected and initialized client. Called once per
/// reconnect, so it must construct a new transport each time.
pub type Connector = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<McpClient, tower_mcp::Error>> + Send>>
        + Send
        + Sync,
>;

/// The live client plus, when the transport supports it, the means to
/// re-establish it.
pub struct Session {
    client: RwLock<Option<Arc<McpClient>>>,
    connector: RwLock<Option<Connector>>,
    /// Serializes reconnects so two failing commands do not both rebuild.
    reconnecting: tokio::sync::Mutex<()>,
    /// Bumped on every successful reconnect. A caller that saw generation N
    /// and finds N+1 after taking the lock knows someone else already
    /// reconnected and skips its own attempt.
    generation: AtomicU64,
    /// Wakes long-lived work tied to the current client so it can move to the
    /// replacement immediately. Polling the atomic would leave an old final
    /// subscription alive until its transport happened to close.
    generation_tx: tokio::sync::watch::Sender<u64>,
}

impl Session {
    /// A `None` connector means no recovery path, which is the right answer
    /// for stdio children and the in-process demo router: there, a dropped
    /// session means the server itself is gone.
    pub fn new(client: McpClient, connector: Option<Connector>) -> Self {
        let (generation_tx, _) = tokio::sync::watch::channel(0);
        Self {
            client: RwLock::new(Some(Arc::new(client))),
            connector: RwLock::new(connector),
            reconnecting: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
            generation_tx,
        }
    }

    /// A REPL session before its first `connect` command.
    pub fn disconnected() -> Self {
        let (generation_tx, _) = tokio::sync::watch::channel(0);
        Self {
            client: RwLock::new(None),
            connector: RwLock::new(None),
            reconnecting: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
            generation_tx,
        }
    }

    /// The client to issue the next request on. Cloned out rather than
    /// borrowed so a reconnect can swap the slot without waiting on callers.
    pub fn client(&self) -> Arc<McpClient> {
        self.try_client().expect("MCP session is not connected")
    }

    /// The current client, or `None` while the interactive REPL is
    /// disconnected. Completion and long-lived listeners use this rather
    /// than manufacturing requests before the first `connect`.
    pub fn try_client(&self) -> Option<Arc<McpClient>> {
        self.client.read().unwrap().clone()
    }

    pub fn is_connected(&self) -> bool {
        self.client.read().unwrap().is_some()
    }

    pub fn can_reconnect(&self) -> bool {
        self.connector.read().unwrap().is_some()
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Observe successful reconnects. Long-lived requests should reopen on
    /// the client corresponding to each new generation.
    pub fn subscribe_generation(&self) -> tokio::sync::watch::Receiver<u64> {
        self.generation_tx.subscribe()
    }

    /// Close the live client and its transport once the session is no longer
    /// shared by command work. One-shot mode uses this before applying its
    /// process exit status so stdio children see EOF and are reaped cleanly.
    pub async fn shutdown(self) -> Result<(), tower_mcp::Error> {
        let Some(client) = self
            .client
            .into_inner()
            .expect("session client lock poisoned")
        else {
            return Ok(());
        };
        let client = Arc::try_unwrap(client).map_err(|_| {
            tower_mcp::Error::Transport(
                "cannot shut down an MCP session while its client is still in use".to_string(),
            )
        })?;
        client.shutdown().await
    }

    /// Rebuild the connection, unless another caller already did so since
    /// `seen` was read. Returns `Ok(())` either way; the caller should
    /// re-read [`Session::client`] afterwards.
    pub async fn reconnect(&self, seen: u64) -> Result<(), tower_mcp::Error> {
        let Some(connector) = self.connector.read().unwrap().clone() else {
            return Err(tower_mcp::Error::Transport(
                "this transport cannot be reconnected".to_string(),
            ));
        };
        let _guard = self.reconnecting.lock().await;
        if self.generation() != seen {
            tracing::debug!(
                seen,
                current = self.generation(),
                "another command already reconnected; reusing its client"
            );
            return Ok(());
        }
        tracing::debug!(generation = seen, "reconnecting");
        // A restarting server is usually a second or two from ready; a short
        // pause makes the retry land after the bind rather than during it.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let fresh = connector().await?;
        *self.client.write().unwrap() = Some(Arc::new(fresh));
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.generation_tx.send_replace(generation);
        tracing::debug!(generation, "reconnected");
        Ok(())
    }

    /// Publish a fully initialized replacement connection. Callers build and
    /// inspect the candidate first, so a failed switch leaves the current
    /// server usable.
    pub async fn replace(
        &self,
        client: McpClient,
        connector: Option<Connector>,
    ) -> Option<Arc<McpClient>> {
        // Serialize an explicit switch with transparent HTTP recovery. A
        // reconnect already in flight finishes first; a reconnect waiting on
        // this lock observes the bumped generation and reuses this client.
        let _guard = self.reconnecting.lock().await;
        *self.connector.write().unwrap() = connector;
        let previous = self.client.write().unwrap().replace(Arc::new(client));
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.generation_tx.send_replace(generation);
        tracing::debug!(generation, "replaced session connection");
        previous
    }
}

/// True when the server rejected a request because the session is not yet
/// initialized (JSON-RPC `-32600` naming `notifications/initialized`). This
/// is retryable at startup: against a multi-instance server without a shared
/// session store, the initialize handshake and a follow-up request can land
/// on different instances, so a brief retry often lands on a consistent one.
/// Mid-session it means the opposite thing, that the session the handshake
/// established is gone, which is what [`is_session_lost`] keys on.
pub fn is_not_initialized(e: &tower_mcp::Error) -> bool {
    matches!(
        e,
        tower_mcp::Error::JsonRpc(j)
            if j.code == -32600 && j.message.contains("notifications/initialized")
    )
}

/// True when an error means the session the handshake established no longer
/// exists on the server, so a fresh handshake would plausibly succeed.
///
/// The cases:
///
/// - [`tower_mcp::Error::SessionExpired`], the client's own name for HTTP 404
///   against a live session id. The library retries this internally when
///   session recovery is on, so it only reaches here once that has failed.
/// - not-initialized, which mid-session means the server forgot the session
///   (restart, OOM, redeploy, or a request scattered to another instance).
/// - a closed transport, from the client's message loop shutting down.
/// - HTTP 410 Gone, and 502/503 from an edge in front of a restarting server.
///   A request issued after the handshake can arrive as a synthesized
///   JSON-RPC `-32000` error even though its cause is transport-level.
///
/// Everything else, including 4xx auth failures and tool errors, is a real
/// error and must surface unchanged: reconnecting would hide it behind a
/// second identical failure.
pub fn is_session_lost(e: &tower_mcp::Error) -> bool {
    if matches!(e, tower_mcp::Error::SessionExpired)
        || is_not_initialized(e)
        || is_reconnectable_http_error(e)
    {
        return true;
    }
    match e {
        tower_mcp::Error::Transport(msg) => {
            msg.contains("Transport closed") || msg.contains("Connection closed")
        }
        _ => false,
    }
}

/// True for an HTTP status that reached the request waiter as either a direct
/// transport error or tower-mcp's synthesized JSON-RPC transport frame.
///
/// A JSON-RPC 404 can only take this path after a successful handshake. The
/// initial wrong-endpoint 404 remains a direct, actionable `Transport` error
/// and is deliberately not reconnectable. Requiring both `-32000` and the
/// exact generated prefix keeps ordinary protocol/tool errors out even when
/// their own message happens to mention an HTTP status.
pub(crate) fn is_reconnectable_http_error(e: &tower_mcp::Error) -> bool {
    match e {
        tower_mcp::Error::Transport(message) => {
            status_after(message, "HTTP ")
                .is_some_and(|status| matches!(status, "410" | "502" | "503"))
                || is_unsent_request(message)
        }
        tower_mcp::Error::JsonRpc(error) if error.code == -32000 => {
            status_after(&error.message, "server returned HTTP ")
                .is_some_and(|status| matches!(status, "404" | "410" | "502" | "503"))
                || is_unsent_request(&error.message)
        }
        _ => false,
    }
}

/// Whether a transport error means the request never reached the server.
///
/// A pooled keep-alive connection can be closed by the peer, or by anything
/// between, while the client still believes it is usable. The next request
/// committed to that socket fails before it is sent. Any HTTP MCP server
/// behind a load balancer with an idle timeout produces this, and it was
/// previously treated as permanent while a 503 was retried, which is
/// backwards: a 503 is the server saying no, this is the server saying
/// nothing.
///
/// **This condition rather than transport errors generally.** The message
/// means the request was not sent, so the server cannot have acted on it and
/// a retry cannot run a tool twice. A connection that dies while the response
/// is being read reports differently and is deliberately not matched, because
/// there the request may already have been carried out.
///
/// Matching a message is unfortunate. `tower_mcp::Error::Transport` carries a
/// `String` and no cause, so there is nothing else to match on here.
fn is_unsent_request(message: &str) -> bool {
    message.contains("error sending request")
}

fn status_after<'a>(message: &'a str, prefix: &str) -> Option<&'a str> {
    message.strip_prefix(prefix)?.split_whitespace().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jsonrpc(code: i32, message: &str) -> tower_mcp::Error {
        tower_mcp::Error::JsonRpc(tower_mcp::error::JsonRpcError {
            code,
            message: message.to_string(),
            data: None,
        })
    }

    /// A connection dropped before the request was sent is retryable.
    ///
    /// This is the shape a pooled keep-alive connection produces when the
    /// peer, or anything between, closes it while the client still believes
    /// it is usable. Both spellings observed in the wild are covered: the
    /// bare transport failure, and the one wrapped by the handshake's own
    /// context.
    #[test]
    fn a_connection_that_died_before_sending_is_reconnectable() {
        for message in [
            "HTTP request failed: error sending request for url (http://127.0.0.1:8080/)",
            "failed to deliver notifications/initialized: Transport error: HTTP request \
             failed: error sending request for url (http://127.0.0.1:37973/)",
        ] {
            assert!(
                is_reconnectable_http_error(&tower_mcp::Error::Transport(message.to_string())),
                "{message}"
            );
        }
        // The same failure also arrives as a JSON-RPC -32000 from the surface
        // fetch path, and should be treated the same way.
        assert!(is_reconnectable_http_error(&jsonrpc(
            -32000,
            "HTTP request failed: error sending request for url (http://x/)"
        )));
    }

    /// A response that failed while being read is deliberately not retried.
    ///
    /// There the request reached the server, which may already have run a
    /// tool, so retrying risks doing it twice. Only a request that was never
    /// sent is safe to repeat.
    #[test]
    fn a_failure_after_the_request_was_sent_is_not_retried() {
        for message in [
            "error reading response body",
            "connection closed before message completed",
            "Connection closed",
        ] {
            assert!(
                !is_reconnectable_http_error(&tower_mcp::Error::Transport(message.to_string())),
                "{message}"
            );
        }
    }

    #[test]
    fn disconnected_session_has_no_client_or_reconnect_recipe() {
        let session = Session::disconnected();
        assert!(!session.is_connected());
        assert!(session.try_client().is_none());
        assert!(!session.can_reconnect());
        assert_eq!(session.generation(), 0);
    }

    #[test]
    fn session_loss_covers_the_documented_conditions() {
        assert!(is_session_lost(&tower_mcp::Error::SessionExpired));
        assert!(is_session_lost(&jsonrpc(
            -32600,
            "Client must send notifications/initialized before making requests"
        )));
        assert!(is_session_lost(&tower_mcp::Error::Transport(
            "Transport closed".into()
        )));
        assert!(is_session_lost(&tower_mcp::Error::Transport(
            "Connection closed".into()
        )));
        for status in ["HTTP 410 Gone", "HTTP 502 Bad Gateway", "HTTP 503"] {
            assert!(
                is_session_lost(&tower_mcp::Error::Transport(format!(
                    "{status} from server: "
                ))),
                "{status} should count as session loss"
            );
        }
        for status in [
            "404 Not Found",
            "410 Gone",
            "502 Bad Gateway",
            "503 Service Unavailable",
        ] {
            assert!(
                is_session_lost(&jsonrpc(-32000, &format!("server returned HTTP {status}"))),
                "live HTTP {status} should count as session loss"
            );
        }
    }

    #[test]
    fn session_loss_does_not_swallow_real_errors() {
        // Auth and not-found are the server answering, not the session dying.
        assert!(!is_session_lost(&tower_mcp::Error::Transport(
            "HTTP 401 Unauthorized from server: bad token".into()
        )));
        assert!(!is_session_lost(&tower_mcp::Error::Transport(
            "HTTP 404 from http://x/mcp: MCP endpoint not found".into()
        )));
        assert!(!is_session_lost(&jsonrpc(-32602, "Invalid params")));
        assert!(!is_session_lost(&jsonrpc(
            -32000,
            "tool reported HTTP 503 Service Unavailable"
        )));
        assert!(!is_session_lost(&jsonrpc(
            -32603,
            "server returned HTTP 503 Service Unavailable"
        )));
        for status in ["401 Unauthorized", "403 Forbidden"] {
            assert!(!is_session_lost(&jsonrpc(
                -32000,
                &format!("server returned HTTP {status}")
            )));
        }
        assert!(!is_session_lost(&tower_mcp::Error::tool("boom")));
    }

    #[test]
    fn detects_not_initialized_startup_error() {
        assert!(is_not_initialized(&jsonrpc(
            -32600,
            "Client must send notifications/initialized before making requests"
        )));
    }

    #[test]
    fn does_not_match_unrelated_errors() {
        // Same code, different message.
        assert!(!is_not_initialized(&jsonrpc(
            -32600,
            "some other invalid request"
        )));
        // Right message text, different code.
        assert!(!is_not_initialized(&jsonrpc(
            -32602,
            "notifications/initialized"
        )));
        // A transport error is never the not-initialized case.
        assert!(!is_not_initialized(&tower_mcp::Error::Transport(
            "boom".into()
        )));
    }
}
