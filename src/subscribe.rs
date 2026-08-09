//! The `subscribe` / `unsubscribe` / `subscriptions` built-ins: resource
//! update subscriptions and the set currently held.
//!
//! The protocol side is two RPCs and a notification: `resources/subscribe`
//! asks the server to send `notifications/resources/updated` for a URI, and
//! the REPL prints those inline as they arrive, like progress and log lines.
//! The set of active subscriptions lives here rather than being threaded
//! through `handle_line`, since the notification callback and the command
//! both need it and neither owns the other.

use std::collections::BTreeSet;
use std::future::Future;
use std::sync::{Mutex, OnceLock};

static ACTIVE: OnceLock<Mutex<BTreeSet<String>>> = OnceLock::new();

fn active() -> &'static Mutex<BTreeSet<String>> {
    ACTIVE.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Record a subscription. False when it was already held, which is how the
/// REPL reports a re-subscribe rather than claiming a new one.
pub fn add(uri: &str) -> bool {
    active().lock().unwrap().insert(uri.to_string())
}

/// Drop a subscription. False when it was not held.
pub fn remove(uri: &str) -> bool {
    active().lock().unwrap().remove(uri)
}

/// The active subscriptions, in URI order so repeated listings are stable.
pub fn list() -> Vec<String> {
    active().lock().unwrap().iter().cloned().collect()
}

/// Whether a URI is subscribed. Used to label an update that arrives for
/// something the REPL did not ask for (a shared session, or a server that
/// pushes updates unprompted).
pub fn contains(uri: &str) -> bool {
    active().lock().unwrap().contains(uri)
}

/// Forget subscriptions owned by the server being left.
pub fn clear() -> usize {
    let mut active = active().lock().unwrap();
    let count = active.len();
    active.clear();
    count
}

/// A server's `resources.subscribe` capability, read from the initialize
/// result. `None` when the server was not initialized.
///
/// A server that does not advertise this will reject `resources/subscribe`,
/// so the REPL says so up front rather than letting the error be the answer.
pub fn server_supports(capabilities: &serde_json::Value) -> bool {
    capabilities
        .pointer("/resources/subscribe")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// What happened while replaying subscriptions onto a replacement client.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReplayReport {
    pub restored: usize,
    /// URI and sanitized-at-the-call-site error text for subscriptions the
    /// replacement server rejected without losing the new connection.
    pub failed: Vec<(String, String)>,
}

/// Replay a snapshot of active URIs on a replacement connection.
///
/// A connection-loss error aborts the reconnect and leaves the caller's
/// active set untouched. Ordinary per-resource errors are collected so the
/// caller can warn and drop only those stale entries without making an
/// otherwise healthy replacement session unusable.
pub async fn replay<F, Fut, P>(
    uris: Vec<String>,
    mut request: F,
    connection_lost: P,
) -> Result<ReplayReport, tower_mcp::Error>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<(), tower_mcp::Error>>,
    P: Fn(&tower_mcp::Error) -> bool,
{
    let mut report = ReplayReport::default();
    for uri in uris {
        match request(uri.clone()).await {
            Ok(()) => report.restored += 1,
            Err(error) if connection_lost(&error) => return Err(error),
            Err(error) => report.failed.push((uri, error.to_string())),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    // The registry is process-global, so one test owns it end to end rather
    // than several racing over the same set.
    #[test]
    fn subscriptions_are_tracked_deduplicated_and_ordered() {
        assert!(add("note://b"));
        assert!(add("note://a"));
        // A second subscribe to the same URI is not a new subscription.
        assert!(!add("note://a"));
        assert!(contains("note://a"));
        assert_eq!(list(), ["note://a", "note://b"]);

        assert!(remove("note://a"));
        // Unsubscribing something not held reports that, rather than pretending.
        assert!(!remove("note://a"));
        assert!(!contains("note://a"));
        assert_eq!(list(), ["note://b"]);
        remove("note://b");
        assert!(list().is_empty());
    }

    #[test]
    fn capability_is_read_from_the_initialize_result() {
        let yes = serde_json::json!({ "resources": { "subscribe": true } });
        let no = serde_json::json!({ "resources": { "listChanged": true } });
        assert!(server_supports(&yes));
        assert!(!server_supports(&no));
        // A server with no resources capability at all.
        assert!(!server_supports(&serde_json::json!({ "tools": {} })));
    }

    #[tokio::test]
    async fn replay_is_ordered_and_distinguishes_resource_errors_from_connection_loss() {
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let requests = seen.clone();
        let report = replay(
            vec!["note://a".to_string(), "note://b".to_string()],
            move |uri| {
                requests.lock().unwrap().push(uri.clone());
                async move {
                    if uri.ends_with('b') {
                        Err(tower_mcp::Error::tool("subscription rejected"))
                    } else {
                        Ok(())
                    }
                }
            },
            |_| false,
        )
        .await
        .unwrap();
        assert_eq!(*seen.lock().unwrap(), ["note://a", "note://b"]);
        assert_eq!(report.restored, 1);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, "note://b");

        let lost = replay(
            vec!["note://a".to_string()],
            |_uri| async { Err(tower_mcp::Error::SessionExpired) },
            |_| true,
        )
        .await
        .unwrap_err();
        assert!(matches!(lost, tower_mcp::Error::SessionExpired));
    }
}
