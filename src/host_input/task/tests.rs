use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tower_mcp::client::ClientTransport;
use tower_mcp::protocol::{ElicitAction, ElicitFormSchema, ListRootsParams};
use tower_mcp::{ChannelTransport, McpRouter, ProtocolSupport};

use super::super::tests::{host, response};
use super::super::{Outgoing, Question};
use super::*;

const SECRET: &str = "synthetic-credential-7f3a";
const STEP: Duration = Duration::from_secs(10);

/// Bound a whole exchange, so an unexpected request fails the test instead of
/// hanging it.
async fn within<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(STEP, future)
        .await
        .expect("test exchange timed out")
}

struct FrameTransport {
    incoming: mpsc::UnboundedReceiver<String>,
    outgoing: Option<mpsc::UnboundedSender<Value>>,
}

#[async_trait]
impl ClientTransport for FrameTransport {
    async fn send(&mut self, message: &str) -> tower_mcp::Result<()> {
        if let Some(outgoing) = &self.outgoing {
            let _ = outgoing.send(serde_json::from_str(message).expect("valid outgoing JSON"));
        }
        Ok(())
    }

    async fn recv(&mut self) -> tower_mcp::Result<Option<String>> {
        Ok(self.incoming.recv().await)
    }

    fn is_connected(&self) -> bool {
        self.outgoing.is_some()
    }

    async fn close(&mut self) -> tower_mcp::Result<()> {
        self.outgoing.take();
        Ok(())
    }
}

/// A final-protocol server. Every `tasks/get` is answered with the current
/// snapshot and reported on `served` with its arrival time; every other
/// request is handed to the test.
struct Server {
    incoming: mpsc::UnboundedSender<String>,
    snapshot: Arc<Mutex<Value>>,
    /// How long each later `tasks/get` takes to answer.
    delay: Arc<Mutex<Duration>>,
    served: mpsc::UnboundedReceiver<(Instant, Value)>,
    requests: mpsc::UnboundedReceiver<Value>,
}

impl Server {
    /// Serve `snapshot` to every later `tasks/get`. A value with an `error`
    /// member and no `taskId` is sent as a JSON-RPC error instead.
    fn set(&self, snapshot: Value) {
        *self.snapshot.lock().unwrap() = snapshot;
    }

    fn delay_reads(&self, delay: Duration) {
        *self.delay.lock().unwrap() = delay;
    }

    /// The next request that is not `tasks/get`, skipping notifications such
    /// as the cancellation of an abandoned request.
    async fn request(&mut self, method: &str) -> Value {
        let frame = tokio::time::timeout(STEP, async {
            loop {
                let frame = self.requests.recv().await.expect("client closed");
                if frame.get("id").is_some() {
                    return frame;
                }
            }
        })
        .await
        .expect("timed out waiting for a request");
        assert_eq!(frame["method"], method, "{frame}");
        frame
    }

    fn respond(&self, request: &Value, result: Value) {
        let frame = json!({"jsonrpc": "2.0", "id": request["id"], "result": result});
        self.incoming.send(frame.to_string()).unwrap();
    }

    fn reject(&self, request: &Value, error: Value) {
        let frame = json!({"jsonrpc": "2.0", "id": request["id"], "error": error});
        self.incoming.send(frame.to_string()).unwrap();
    }

    /// Wait until a read is served a snapshot matching `served`.
    async fn served(&mut self, served: impl Fn(&Value) -> bool) {
        tokio::time::timeout(STEP, async {
            while let Some((_, snapshot)) = self.served.recv().await {
                if served(&snapshot) {
                    return;
                }
            }
            panic!("client closed");
        })
        .await
        .expect("timed out waiting for a read");
    }

    /// Arrival times of every read served since the last drain.
    fn drain_reads(&mut self) -> Vec<Instant> {
        std::iter::from_fn(|| self.served.try_recv().ok())
            .map(|(at, _)| at)
            .collect()
    }

    fn assert_no_request(&mut self) {
        while let Ok(frame) = self.requests.try_recv() {
            assert!(frame.get("id").is_none(), "unexpected request: {frame}");
        }
    }
}

async fn connect() -> (McpClient, Server) {
    let (incoming_tx, incoming) = mpsc::unbounded_channel();
    let (outgoing, mut outgoing_rx) = mpsc::unbounded_channel::<Value>();
    let (served_tx, served) = mpsc::unbounded_channel();
    let (requests_tx, requests) = mpsc::unbounded_channel();
    let snapshot = Arc::new(Mutex::new(Value::Null));
    let delay = Arc::new(Mutex::new(Duration::ZERO));
    {
        let snapshot = snapshot.clone();
        let delay = delay.clone();
        let incoming = incoming_tx.clone();
        tokio::spawn(async move {
            while let Some(frame) = outgoing_rx.recv().await {
                let reply = match frame["method"].as_str() {
                    Some("server/discover") => json!({"result": {
                        "resultType": "complete",
                        "supportedVersions": ["2026-07-28"],
                        "capabilities": {},
                        "ttlMs": 0,
                        "cacheScope": "private"
                    }}),
                    Some("tasks/get") => {
                        let current = snapshot.lock().unwrap().clone();
                        let _ = served_tx.send((Instant::now(), current.clone()));
                        let wait = *delay.lock().unwrap();
                        tokio::time::sleep(wait).await;
                        if current.get("taskId").is_none() && current.get("error").is_some() {
                            json!({"error": current["error"]})
                        } else {
                            json!({"result": current})
                        }
                    }
                    _ => {
                        let _ = requests_tx.send(frame);
                        continue;
                    }
                };
                let mut reply = reply;
                reply["jsonrpc"] = json!("2.0");
                reply["id"] = frame["id"].clone();
                let _ = incoming.send(reply.to_string());
            }
        });
    }
    let client = McpClient::builder()
        .protocol_support(ProtocolSupport::try_new(["2026-07-28"]).unwrap())
        .disable_response_cache()
        .with_elicitation()
        .with_sampling()
        .with_tasks()
        .connect(
            FrameTransport {
                incoming,
                outgoing: Some(outgoing),
            },
            (),
        )
        .await
        .unwrap();
    within(client.discover("test-client", "1.0.0"))
        .await
        .unwrap();
    let server = Server {
        incoming: incoming_tx,
        snapshot,
        delay,
        served,
        requests,
    };
    (client, server)
}

fn jobs() -> Jobs {
    Jobs::new(
        crate::output::AsyncOutput::new(Arc::new(AtomicBool::new(false)), false),
        false,
    )
}

fn task(id: &str, status: &str, poll_ms: u64) -> Value {
    json!({
        "resultType": "complete",
        "taskId": id,
        "status": status,
        "createdAt": "2026-07-28T00:00:00Z",
        "lastUpdatedAt": "2026-07-28T00:00:01Z",
        "ttlMs": null,
        "pollIntervalMs": poll_ms
    })
}

fn completed(id: &str) -> Value {
    let mut done = task(id, "completed", 50);
    done["result"] = json!({"content": [{"type": "text", "text": "done"}]});
    done
}

fn form_json() -> Value {
    json!({
        "method": "elicitation/create",
        "params": {
            "mode": "form",
            "message": "Provide a value",
            "requestedSchema": {
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"]
            }
        }
    })
}

fn parked(id: &str, keys: &[&str], poll_ms: u64) -> Value {
    let mut value = task(id, "input_required", poll_ms);
    value["inputRequests"] = keys.iter().map(|key| (*key, form_json())).collect();
    value
}

/// `snapshot` as of `second` seconds past the fixed base time.
fn at(mut snapshot: Value, second: u32) -> Value {
    snapshot["lastUpdatedAt"] = json!(format!("2026-07-28T00:00:{second:02}Z"));
    snapshot
}

fn lists(snapshot: &Value, key: &str) -> bool {
    snapshot["inputRequests"].get(key).is_some()
}

fn requests(value: Value) -> InputRequests {
    serde_json::from_value(value).unwrap()
}

fn accept() -> Value {
    json!({"action":"accept","content":{"value":"v"}})
}

/// The next frame of `kind` the host receives, skipping the others.
async fn next(frames: &mut mpsc::Receiver<Outgoing>, kind: &str) -> Value {
    tokio::time::timeout(STEP, async {
        loop {
            let Some(Outgoing::Frame(bytes)) = frames.recv().await else {
                panic!("host channel closed");
            };
            let frame: Value = serde_json::from_slice(&bytes).unwrap();
            if frame["type"] == kind {
                return frame;
            }
        }
    })
    .await
    .expect("timed out waiting for a host frame")
}

fn no_more(frames: &mut mpsc::Receiver<Outgoing>, kind: &str) {
    while let Ok(Outgoing::Frame(bytes)) = frames.try_recv() {
        let frame: Value = serde_json::from_slice(&bytes).unwrap();
        assert_ne!(frame["type"], kind, "{frame}");
    }
}

fn frame_id(frame: &Value) -> u64 {
    frame["id"].as_str().unwrap().parse().unwrap()
}

fn record<T>(
    host: &Host,
    connection: Connection,
    id: &str,
    f: impl FnOnce(&mut TaskRecord) -> T,
) -> T {
    host.with_task(connection, id, f)
        .expect("active connection")
}

#[tokio::test]
async fn task_input_sends_declines_and_leaves_failed_keys_outstanding() {
    let (host, mut frames, connection) = host();
    let client = McpClient::connect(ChannelTransport::new(McpRouter::new()))
        .await
        .unwrap();
    let mut batch = requests(json!({
        "a-form": form_json(),
        "b-sample": {
            "method": "sampling/createMessage",
            "params": {"messages": [{"role": "user", "content": {"type": "text", "text": "hi"}}], "maxTokens": 8}
        },
        "d-form": form_json(),
    }));
    batch.insert(
        "c-roots".into(),
        InputRequest::ListRoots(ListRootsParams::default()),
    );
    let deadline = Some(Instant::now() + Duration::from_millis(500));
    let session = host.session(connection.0);
    let answering = async {
        let first = next(&mut frames, "needs_input").await;
        let second = next(&mut frames, "needs_input").await;
        let third = next(&mut frames, "needs_input").await;
        assert_eq!(
            [frame_id(&first), frame_id(&second), frame_id(&third)],
            [1, 2, 3]
        );
        for id in [1, 2] {
            response(&host, &session, id, json!({"action":"decline"}));
        }
        let withdrawn = next(&mut frames, "withdrawn").await;
        assert_eq!(withdrawn["id"], "3");
        assert_eq!(withdrawn["reason"], "expired");
    };
    let (input, ()) = within(async {
        tokio::join!(
            ask_host(&client, &host, connection, batch, deadline, None),
            answering
        )
    })
    .await;
    let input = input.unwrap();
    assert_eq!(input.responses.len(), 1);
    assert!(matches!(
        input.responses.get("a-form"),
        Some(InputResponse::Elicit(result)) if result.action == ElicitAction::Decline
    ));
    assert_eq!(
        input.unanswered.into_iter().collect::<Vec<_>>(),
        [
            ("b-sample".to_owned(), Unanswered::SamplingRefused),
            ("c-roots".to_owned(), Unanswered::Unsupported),
            ("d-form".to_owned(), Unanswered::Expired),
        ]
    );
    client.shutdown().await.unwrap();
}

/// A stale `input_required` snapshot after an acknowledged update does not
/// publish the same questions again or use up rounds.
#[tokio::test]
async fn a_stale_snapshot_after_an_update_is_polled_not_asked_again() {
    let (host, mut frames, connection) = host();
    let (client, mut server) = connect().await;
    let jobs = jobs();
    let session = host.session(connection.0);
    let id = "stale-task";
    server.set(parked(id, &["a"], 50));
    let script = async {
        let question = next(&mut frames, "needs_input").await;
        response(&host, &session, frame_id(&question), accept());
        let update = server.request("tasks/update").await;
        assert_eq!(update["params"]["taskId"], id);
        assert!(update["params"]["inputResponses"]["a"].is_object());
        server.drain_reads();
        server.respond(&update, json!({"resultType": "complete"}));
        // More stale reads than the round limit allows answered rounds.
        for _ in 0..=MAX_TASK_ROUNDS {
            server.served(|snapshot| lists(snapshot, "a")).await;
        }
        server.set(completed(id));
    };
    let (result, ()) = within(async {
        tokio::join!(wait_for_task(&client, &host, &jobs, None, id, None), script)
    })
    .await;
    assert_eq!(result.unwrap().status, TaskStatus::Completed);
    no_more(&mut frames, "needs_input");
    client.shutdown().await.unwrap();
}

/// Keys an earlier command saw acknowledged or removed are not asked again by
/// `task <id> respond`.
#[tokio::test]
async fn respond_skips_keys_an_earlier_command_acknowledged() {
    let (host, mut frames, connection) = host();
    let (client, mut server) = connect().await;
    let session = host.session(connection.0);
    let id = "respond-acknowledged";
    record(&host, connection, id, |record| {
        record.acknowledged.insert("a".into());
        record.removed.insert("c".into());
    });
    server.set(parked(id, &["a", "b", "c"], 50));
    let script = async {
        let question = next(&mut frames, "needs_input").await;
        response(
            &host,
            &session,
            frame_id(&question),
            json!({"action":"decline"}),
        );
        let update = server.request("tasks/update").await;
        let keys = update["params"]["inputResponses"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(keys, ["b"]);
        server.set(task(id, "working", 50));
        server.respond(&update, json!({"resultType": "complete"}));
    };
    let (result, ()) =
        within(async { tokio::join!(respond_with(&client, &host, id, "1", None), script) }).await;
    assert_eq!(result.unwrap().status, TaskStatus::Working);
    no_more(&mut frames, "needs_input");
    client.shutdown().await.unwrap();
}

/// A task cancelled while the host prepares its answer withdraws the open
/// question and reports the cancelled task, without an update.
#[tokio::test]
async fn a_task_cancelled_while_the_host_prepares_withdraws_its_questions() {
    let (host, mut frames, _) = host();
    let (client, mut server) = connect().await;
    let jobs = jobs();
    let id = "cancelled-task";
    server.set(parked(id, &["a"], 50));
    let script = async {
        let question = next(&mut frames, "needs_input").await;
        server.set(task(id, "cancelled", 50));
        let withdrawn = next(&mut frames, "withdrawn").await;
        assert_eq!(withdrawn["id"], question["id"]);
        assert_eq!(withdrawn["reason"], "server_cancelled");
    };
    let (result, ()) = within(async {
        tokio::join!(wait_for_task(&client, &host, &jobs, None, id, None), script)
    })
    .await;
    assert_eq!(result.unwrap().status, TaskStatus::Cancelled);
    server.assert_no_request();
    client.shutdown().await.unwrap();
}

/// A question whose key the task replaced is withdrawn, and an answer for a
/// key the task no longer lists is not sent.
#[tokio::test]
async fn replaced_requests_are_withdrawn_and_not_submitted() {
    let (host, mut frames, connection) = host();
    let (client, mut server) = connect().await;
    let session = host.session(connection.0);
    let id = "replaced-task";
    server.set(at(parked(id, &["a", "b"], 50), 1));
    let script = async {
        let first = next(&mut frames, "needs_input").await;
        let second = next(&mut frames, "needs_input").await;
        response(&host, &session, frame_id(&first), accept());
        server.set(at(parked(id, &["c"], 50), 2));
        let withdrawn = next(&mut frames, "withdrawn").await;
        assert_eq!(withdrawn["id"], second["id"]);
        assert_eq!(withdrawn["reason"], "server_cancelled");
    };
    let (result, ()) =
        within(async { tokio::join!(respond_with(&client, &host, id, "1", None), script) }).await;
    assert_eq!(result.unwrap().status, TaskStatus::InputRequired);
    server.assert_no_request();
    client.shutdown().await.unwrap();
}

/// Review finding 3: a key the watch saw disappear is not sent, even when
/// the read before the update returns an older snapshot that lists it again.
#[tokio::test]
async fn a_key_seen_removed_is_not_sent_when_an_older_snapshot_relists_it() {
    let (host, mut frames, connection) = host();
    let (client, mut server) = connect().await;
    let session = host.session(connection.0);
    let id = "relisted-task";
    server.set(at(parked(id, &["a", "b"], 200), 1));
    let script = async {
        let first = next(&mut frames, "needs_input").await;
        let second = next(&mut frames, "needs_input").await;
        response(&host, &session, frame_id(&first), accept());
        // The newer snapshot drops both keys; the watch withdraws `b`.
        server.set(at(parked(id, &["c"], 200), 2));
        let withdrawn = next(&mut frames, "withdrawn").await;
        assert_eq!(withdrawn["id"], second["id"]);
        // The read before the update, a poll interval later, is older.
        server.set(at(parked(id, &["a", "b"], 200), 1));
    };
    let (result, ()) =
        within(async { tokio::join!(respond_with(&client, &host, id, "1", None), script) }).await;
    assert_eq!(result.unwrap().status, TaskStatus::InputRequired);
    server.assert_no_request();
    client.shutdown().await.unwrap();
}

/// Review finding 4: every read of a task, including the one before the
/// update and the one after it, starts at least `pollIntervalMs` after the
/// previous read.
#[tokio::test]
async fn reads_respect_the_poll_interval_around_an_update() {
    let (host, mut frames, connection) = host();
    let (client, mut server) = connect().await;
    let session = host.session(connection.0);
    let id = "spaced-task";
    server.set(parked(id, &["a"], 200));
    let script = async {
        let question = next(&mut frames, "needs_input").await;
        response(&host, &session, frame_id(&question), accept());
        let update = server.request("tasks/update").await;
        server.set(task(id, "working", 200));
        server.respond(&update, json!({"resultType": "complete"}));
    };
    let (result, ()) =
        within(async { tokio::join!(respond_with(&client, &host, id, "1", None), script) }).await;
    assert_eq!(result.unwrap().status, TaskStatus::Working);
    let reads = server.drain_reads();
    assert_eq!(reads.len(), 3, "{reads:?}");
    for pair in reads.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(gap >= Duration::from_millis(180), "reads {gap:?} apart");
    }
    client.shutdown().await.unwrap();
}

/// Review finding 1: when the check before the update cannot run before the
/// command deadline, nothing is sent.
#[tokio::test]
async fn no_update_is_sent_without_a_check_before_the_deadline() {
    let (host, mut frames, connection) = host();
    let (client, mut server) = connect().await;
    let session = host.session(connection.0);
    let id = "unchecked-task";
    server.set(parked(id, &["a"], 20_000));
    let deadline = Some(Instant::now() + Duration::from_secs(5));
    let script = async {
        let question = next(&mut frames, "needs_input").await;
        response(&host, &session, frame_id(&question), accept());
    };
    let (result, ()) =
        within(async { tokio::join!(respond_with(&client, &host, id, "1", deadline), script) })
            .await;
    let error = result.unwrap_err();
    let failure = super::super::explicit_failure(&error).unwrap();
    assert_eq!(failure.status, ExitStatus::Transport);
    assert!(
        failure.message.contains("nothing was sent"),
        "{}",
        failure.message
    );
    assert_eq!(server.drain_reads().len(), 1);
    server.assert_no_request();
    client.shutdown().await.unwrap();
}

/// #390: when `wait` drops host answers because the next poll slot falls
/// after its deadline, the connection is healthy and the task is still
/// parked, so it is a pending wait (which the session driver keeps) rather
/// than a transport failure (which ends the session). A foreground call
/// keeps the transport failure.
#[tokio::test]
async fn an_unchecked_answer_during_wait_is_a_pending_wait_deadline() {
    for tool in [None, Some("probe")] {
        let (host, mut frames, connection) = host();
        let (client, mut server) = connect().await;
        let jobs = jobs();
        let session = host.session(connection.0);
        let id = "unchecked-wait-task";
        server.set(parked(id, &["a"], 20_000));
        let deadline = Some(Instant::now() + Duration::from_secs(5));
        let script = async {
            let question = next(&mut frames, "needs_input").await;
            response(&host, &session, frame_id(&question), accept());
        };
        let (result, ()) = within(async {
            tokio::join!(
                wait_for_task(&client, &host, &jobs, tool, id, deadline),
                script
            )
        })
        .await;
        let error = result.unwrap_err();
        assert_eq!(ExitStatus::from_mcp_error(&error), ExitStatus::Transport);
        match tool {
            None => {
                let (marker, message) =
                    crate::jobs::wait_deadline(&error).expect("a pending wait deadline");
                assert_eq!(marker.task_id, id);
                assert!(marker.answers_unsent);
                assert!(message.contains("answers were not sent"), "{message}");
                assert!(message.contains("respond"), "{message}");
                assert!(super::super::explicit_failure(&error).is_none());
            }
            Some(_) => {
                assert!(crate::jobs::wait_deadline(&error).is_none());
                let failure = super::super::explicit_failure(&error).unwrap();
                assert!(
                    failure.message.contains("nothing was sent"),
                    "{}",
                    failure.message
                );
            }
        }
        assert_eq!(server.drain_reads().len(), 1);
        server.assert_no_request();
        client.shutdown().await.unwrap();
    }
}

/// Review finding 1: a check that finishes after the command deadline does
/// not lead to an update.
#[tokio::test]
async fn no_update_is_sent_when_the_deadline_passes_during_the_check() {
    let (host, mut frames, connection) = host();
    let (client, mut server) = connect().await;
    let session = host.session(connection.0);
    let id = "late-check-task";
    server.set(parked(id, &["a"], 50));
    let deadline = Some(Instant::now() + Duration::from_millis(600));
    let script = async {
        let question = next(&mut frames, "needs_input").await;
        server.delay_reads(Duration::from_secs(1));
        response(&host, &session, frame_id(&question), accept());
    };
    let (result, ()) =
        within(async { tokio::join!(respond_with(&client, &host, id, "1", deadline), script) })
            .await;
    let error = result.unwrap_err();
    assert!(
        super::super::explicit_failure(&error)
            .unwrap()
            .message
            .contains("nothing was sent")
    );
    server.assert_no_request();
    client.shutdown().await.unwrap();
}

/// Review finding 3: a question withdrawn because its key disappeared is not
/// asked again in a later round, even when stale reads list it again.
#[tokio::test]
async fn a_withdrawn_key_is_not_asked_again_in_a_later_round() {
    let (host, mut frames, _) = host();
    let (client, mut server) = connect().await;
    let jobs = jobs();
    let id = "withdrawn-across-rounds";
    server.set(at(parked(id, &["a"], 50), 1));
    let script = async {
        let question = next(&mut frames, "needs_input").await;
        server.set(at(parked(id, &["c"], 50), 2));
        let withdrawn = next(&mut frames, "withdrawn").await;
        assert_eq!(withdrawn["id"], question["id"]);
        // Two stale reads list `a` again; `c` is never shown to a read that
        // would ask it.
        server.set(at(parked(id, &["a"], 50), 1));
        server.drain_reads();
        for _ in 0..2 {
            server.served(|snapshot| lists(snapshot, "a")).await;
        }
        server.set(completed(id));
    };
    let (result, ()) = within(async {
        tokio::join!(wait_for_task(&client, &host, &jobs, None, id, None), script)
    })
    .await;
    assert_eq!(result.unwrap().status, TaskStatus::Completed);
    no_more(&mut frames, "needs_input");
    server.assert_no_request();
    client.shutdown().await.unwrap();
}

/// Review finding 4: the poll slot belongs to the task, so a follow-up
/// command's read of a task the previous command just read waits for it.
#[tokio::test]
async fn pacing_carries_over_to_the_next_command() {
    let (host, _frames, _) = host();
    let (client, mut server) = connect().await;
    let jobs = jobs();
    let id = "paced-across-commands";
    let mut done = completed(id);
    done["pollIntervalMs"] = json!(300);
    server.set(done);
    within(wait_for_task(&client, &host, &jobs, None, id, None))
        .await
        .unwrap();
    let task = within(inspect_with(&client, &host, id)).await.unwrap();
    assert_eq!(task.status, TaskStatus::Completed);
    let reads = server.drain_reads();
    assert_eq!(reads.len(), 2, "{reads:?}");
    assert!(reads[1] - reads[0] >= Duration::from_millis(280));
    client.shutdown().await.unwrap();
}

/// The snapshot that reports a terminal state is the result. A second read
/// could fail after the task expired.
#[tokio::test]
async fn a_completed_snapshot_is_the_result_without_a_second_read() {
    let (host, _frames, _) = host();
    let (client, mut server) = connect().await;
    let jobs = jobs();
    let id = "completed-task";
    server.set(completed(id));
    let task = within(wait_for_task(&client, &host, &jobs, None, id, None))
        .await
        .unwrap();
    assert_eq!(task.status, TaskStatus::Completed);
    assert!(task.result.is_some());
    assert_eq!(server.drain_reads().len(), 1);
    client.shutdown().await.unwrap();
}

/// A failed task's error and status message are server text, so the bridge
/// replaces both and keeps the code.
#[tokio::test]
async fn a_failed_task_payload_is_withheld() {
    let (host, _frames, _) = host();
    let (client, server) = connect().await;
    let jobs = jobs();
    let id = "failed-task";
    let mut failed = task(id, "failed", 50);
    failed["statusMessage"] = json!(format!("Task failed: rejected {SECRET}"));
    failed["error"] =
        json!({"code": -32602, "message": format!("rejected {SECRET}"), "data": {"value": SECRET}});
    server.set(failed);
    let task = within(wait_for_task(&client, &host, &jobs, None, id, None))
        .await
        .unwrap();
    assert_eq!(task.status, TaskStatus::Failed);
    let error = task.error.as_ref().unwrap();
    assert_eq!(error.code, -32602);
    assert!(error.data.is_none());
    assert!(!serde_json::to_string(&task).unwrap().contains(SECRET));
    client.shutdown().await.unwrap();
}

/// A rejected `tasks/update` can echo the submitted value. The rendered
/// error keeps the code and exit status and withholds the text.
#[tokio::test]
async fn a_rejected_update_withholds_the_server_text() {
    let (host, mut frames, connection) = host();
    let (client, mut server) = connect().await;
    let session = host.session(connection.0);
    let id = "rejected-update";
    server.set(parked(id, &["a"], 50));
    let script = async {
        let question = next(&mut frames, "needs_input").await;
        response(
            &host,
            &session,
            frame_id(&question),
            json!({"action":"accept","content":{"value":SECRET}}),
        );
        let update = server.request("tasks/update").await;
        server.reject(
            &update,
            json!({"code": -32602, "message": format!("bad value {SECRET}"), "data": {"value": SECRET}}),
        );
    };
    let (result, ()) =
        within(async { tokio::join!(respond_with(&client, &host, id, "1", None), script) }).await;
    let error = result.unwrap_err();
    assert!(!format!("{error:?}").contains(SECRET), "{error:?}");
    let failure = super::super::explicit_failure(&error).unwrap();
    assert_eq!(failure.code, Some(-32602));
    assert_eq!(failure.status, ExitStatus::Server);
    client.shutdown().await.unwrap();
}

/// Review finding 2: a host answer accepted by one attempt, then a state-only
/// continuation the server rejects, still has its error text withheld.
#[tokio::test]
async fn a_rejected_state_only_continuation_after_an_answer_is_withheld() {
    let (host, mut frames, connection) = host();
    let (client, mut server) = connect().await;
    let jobs = jobs();
    let session = host.session(connection.0);
    let script = async {
        let first = server.request("tools/call").await;
        server.respond(
            &first,
            json!({
                "resultType": "input_required",
                "inputRequests": {"a": form_json()},
                "requestState": "s1"
            }),
        );
        let question = next(&mut frames, "needs_input").await;
        response(
            &host,
            &session,
            frame_id(&question),
            json!({"action":"accept","content":{"value":SECRET}}),
        );
        let second = server.request("tools/call").await;
        assert!(second["params"]["inputResponses"]["a"].is_object());
        server.respond(
            &second,
            json!({"resultType": "input_required", "requestState": "s2"}),
        );
        let third = server.request("tools/call").await;
        assert!(third["params"].get("inputResponses").is_none());
        // Not -32602, which tower-mcp answers with a schema refresh.
        server.reject(
            &third,
            json!({"code": -32603, "message": format!("bad value {SECRET}"), "data": SECRET}),
        );
    };
    let (result, ()) = within(async {
        tokio::join!(
            super::super::final_protocol::call_tool_with(
                &client,
                &host,
                &jobs,
                "work",
                json!({}),
                None
            ),
            script
        )
    })
    .await;
    let error = result.unwrap_err();
    assert!(!format!("{error:?}").contains(SECRET), "{error:?}");
    assert_eq!(
        super::super::explicit_failure(&error).unwrap().code,
        Some(-32603)
    );
    client.shutdown().await.unwrap();
}

/// Review finding 6: an HTTP 503 delivered as JSON-RPC `-32000` keeps the
/// transport status it had before its text was withheld.
#[test]
fn redaction_keeps_the_status_the_original_error_had() {
    let original = tower_mcp::Error::JsonRpc(JsonRpcError {
        code: -32000,
        message: format!("server returned HTTP 503 Service Unavailable {SECRET}"),
        data: Some(json!(SECRET)),
    });
    assert_eq!(ExitStatus::from_mcp_error(&original), ExitStatus::Transport);
    let error = peer_error(original, "tasks/update");
    assert_eq!(ExitStatus::from_mcp_error(&error), ExitStatus::Transport);
    let failure = super::super::explicit_failure(&error).unwrap();
    assert_eq!(failure.code, Some(-32000));
    assert!(!format!("{error:?}").contains(SECRET));
}

#[test]
fn transport_errors_from_the_peer_keep_their_status_and_lose_their_text() {
    let error = peer_error(
        tower_mcp::Error::Transport(format!("HTTP 401 Unauthorized: {SECRET}")),
        "tasks/update",
    );
    assert_eq!(ExitStatus::from_mcp_error(&error), ExitStatus::Auth);
    assert!(!format!("{error:?}").contains(SECRET));
    let lost = peer_error(super::super::lost_error(), "tasks/update");
    assert!(matches!(lost, tower_mcp::Error::Transport(message)
        if message == super::super::CHANNEL_CLOSED));
}

/// Review findings 2 and 5: a task's record belongs to the active connection.
/// A terminal read clears its keys and keeps its poll slot, an error,
/// including `-32602`, keeps everything, and a new connection starts without
/// any record.
#[tokio::test]
async fn task_records_follow_the_task_and_the_connection() {
    let (host, _frames, connection) = host();
    let (client, server) = connect().await;
    let seed = |id: &str| {
        record(&host, connection, id, |record| {
            record.acknowledged.insert("a".into());
            record.removed.insert("b".into());
        });
    };

    seed("settled");
    server.set(completed("settled"));
    within(Reader::new(&client, &host, "settled").unwrap().read_now())
        .await
        .unwrap();
    let (acknowledged, removed, next) = record(&host, connection, "settled", |record| {
        (
            record.acknowledged.len(),
            record.removed.len(),
            record.next_poll,
        )
    });
    assert_eq!((acknowledged, removed), (0, 0));
    assert!(next.is_some());

    for (id, code) in [("ambiguous", -32602), ("busy", -32603)] {
        seed(id);
        server.set(json!({"error": {"code": code, "message": "rejected"}}));
        let reader = Reader::new(&client, &host, id).unwrap();
        assert!(within(reader.read_now()).await.is_err());
        let kept = record(&host, connection, id, |record| {
            record.acknowledged.contains("a") && record.removed.contains("b")
        });
        assert!(kept, "{id}");
    }

    host.activate(Connection(2), "other", "2026-07-28", None);
    assert!(host.with_task(connection, "busy", |_| ()).is_none());
    let fresh = record(&host, Connection(2), "busy", |record| {
        record.acknowledged.is_empty() && record.next_poll.is_none()
    });
    assert!(fresh);
    client.shutdown().await.unwrap();
}

/// Review round 5, finding 1: an older snapshot that omits a published key
/// does not remove it, and the answer goes out once a check lists the key.
#[tokio::test]
async fn an_older_snapshot_does_not_remove_a_live_key() {
    let (host, mut frames, connection) = host();
    let (client, mut server) = connect().await;
    let session = host.session(connection.0);
    let id = "older-working";
    server.set(at(parked(id, &["a"], 50), 2));
    let script = async {
        let question = next(&mut frames, "needs_input").await;
        server.set(at(task(id, "working", 50), 1));
        response(&host, &session, frame_id(&question), accept());
        // The older snapshot does not list `a`, so nothing is sent yet; the
        // next check lists it again.
        server.drain_reads();
        server
            .served(|snapshot| snapshot["status"] == "working")
            .await;
        server.set(at(parked(id, &["a"], 50), 2));
        let update = server.request("tasks/update").await;
        assert!(update["params"]["inputResponses"]["a"].is_object());
        server.set(at(task(id, "working", 50), 3));
        server.respond(&update, json!({"resultType": "complete"}));
    };
    let (result, ()) =
        within(async { tokio::join!(respond_with(&client, &host, id, "1", None), script) }).await;
    // The update above carried `a`, so the older snapshot did not remove it.
    assert_eq!(result.unwrap().status, TaskStatus::Working);
    client.shutdown().await.unwrap();
}

/// Final review, findings 3 and 4: an update whose acknowledgement never
/// arrives ends the command with the task id and a warning that it may have
/// been applied. The next `respond` is the retry: it asks again for a key
/// the task still lists, even from the same snapshot.
#[tokio::test]
async fn an_unacknowledged_update_warns_and_the_next_respond_retries() {
    let (host, mut frames, connection) = host();
    let (client, mut server) = connect().await;
    let session = host.session(connection.0);
    let id = "unacknowledged";
    server.set(at(parked(id, &["a"], 50), 1));
    let deadline = Some(Instant::now() + Duration::from_millis(800));
    let script = async {
        let question = next(&mut frames, "needs_input").await;
        response(&host, &session, frame_id(&question), accept());
        // Received and never acknowledged.
        server.request("tasks/update").await;
    };
    let (result, ()) =
        within(async { tokio::join!(respond_with(&client, &host, id, "1", deadline), script) })
            .await;
    let error = result.unwrap_err();
    let message = super::super::explicit_failure(&error).unwrap().message;
    assert!(message.contains("may have been applied"), "{message}");
    assert!(message.contains(id), "{message}");

    let script = async {
        let question = next(&mut frames, "needs_input").await;
        response(&host, &session, frame_id(&question), accept());
        let update = server.request("tasks/update").await;
        server.set(at(task(id, "working", 50), 2));
        server.respond(&update, json!({"resultType": "complete"}));
    };
    let (result, ()) =
        within(async { tokio::join!(respond_with(&client, &host, id, "1", None), script) }).await;
    assert_eq!(result.unwrap().status, TaskStatus::Working);
    client.shutdown().await.unwrap();
}

/// Final review, finding 3: tower-mcp reports HTTP transport failures as
/// JSON-RPC `-32000`, so that code is an uncertain outcome. Any other
/// JSON-RPC error is a refusal and carries no such warning.
#[tokio::test]
async fn a_minus_32000_update_failure_is_uncertain_and_other_codes_are_refusals() {
    for (code, uncertain) in [(-32000, true), (-32602, false)] {
        let (host, mut frames, connection) = host();
        let (client, mut server) = connect().await;
        let session = host.session(connection.0);
        let id = "refused-update";
        server.set(parked(id, &["a"], 50));
        let script = async {
            let question = next(&mut frames, "needs_input").await;
            response(&host, &session, frame_id(&question), accept());
            let update = server.request("tasks/update").await;
            server.reject(
                &update,
                json!({"code": code, "message": "server returned HTTP 502 Bad Gateway"}),
            );
        };
        let (result, ()) =
            within(async { tokio::join!(respond_with(&client, &host, id, "1", None), script) })
                .await;
        let error = result.unwrap_err();
        let message = super::super::explicit_failure(&error).unwrap().message;
        assert_eq!(
            message.contains("may have been applied"),
            uncertain,
            "{code}: {message}"
        );
        client.shutdown().await.unwrap();
    }
}

/// Final review, finding 2: a check whose snapshot no longer lists the
/// answered key sends nothing, even with an unchanged `lastUpdatedAt`, and
/// the task is read again until it settles.
#[tokio::test]
async fn an_answer_is_not_sent_when_the_check_does_not_list_its_key() {
    let (host, mut frames, connection) = host();
    let (client, mut server) = connect().await;
    let session = host.session(connection.0);
    let id = "answered-elsewhere";
    server.set(at(parked(id, &["a"], 50), 1));
    let script = async {
        let question = next(&mut frames, "needs_input").await;
        // Another actor answered; the same second, now working.
        server.set(at(task(id, "working", 50), 1));
        response(&host, &session, frame_id(&question), accept());
        server.drain_reads();
        server
            .served(|snapshot| snapshot["status"] == "working")
            .await;
        server.set(completed(id));
    };
    let (result, ()) =
        within(async { tokio::join!(respond_with(&client, &host, id, "1", None), script) }).await;
    assert_eq!(result.unwrap().status, TaskStatus::Completed);
    server.assert_no_request();
    let removed = record(&host, connection, id, |record| record.removed.clone());
    assert!(removed.is_empty(), "{removed:?}");
    client.shutdown().await.unwrap();
}

/// Final review, finding 2: when the deadline leaves no slot for another
/// check, only the keys the last check listed are sent.
#[tokio::test]
async fn only_keys_the_check_lists_are_sent() {
    let (host, mut frames, connection) = host();
    let (client, mut server) = connect().await;
    let session = host.session(connection.0);
    let id = "partly-listed";
    server.set(at(parked(id, &["a", "b"], 300), 1));
    let deadline = Some(Instant::now() + Duration::from_millis(500));
    let script = async {
        let first = next(&mut frames, "needs_input").await;
        let second = next(&mut frames, "needs_input").await;
        server.set(at(parked(id, &["a"], 300), 1));
        response(&host, &session, frame_id(&first), accept());
        response(&host, &session, frame_id(&second), accept());
        let update = server.request("tasks/update").await;
        let keys = update["params"]["inputResponses"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(keys, ["a"]);
        server.respond(&update, json!({"resultType": "complete"}));
    };
    let (result, ()) =
        within(async { tokio::join!(respond_with(&client, &host, id, "1", deadline), script) })
            .await;
    result.unwrap();
    client.shutdown().await.unwrap();
}

/// Scoped check, finding 2: a key the check does not list holds back only its
/// own answer. The listed answers go out right after the check, and the held
/// one follows once a later check lists its key.
#[tokio::test]
async fn listed_answers_go_out_at_once_and_held_ones_follow() {
    let (host, mut frames, connection) = host();
    let (client, mut server) = connect().await;
    let session = host.session(connection.0);
    let id = "held-key";
    server.set(at(parked(id, &["a", "b"], 100), 1));
    let deadline = Some(Instant::now() + Duration::from_secs(5));
    let script = async {
        let first = next(&mut frames, "needs_input").await;
        let second = next(&mut frames, "needs_input").await;
        // `b` was answered elsewhere within the same second.
        server.set(at(parked(id, &["a"], 100), 1));
        server.drain_reads();
        response(&host, &session, frame_id(&first), accept());
        response(&host, &session, frame_id(&second), accept());
        let update = server.request("tasks/update").await;
        let keys = |update: &Value| {
            update["params"]["inputResponses"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(&update), ["a"]);
        let reads = server.drain_reads().len();
        assert!(reads <= 2, "`a` waited for {reads} reads");
        server.respond(&update, json!({"resultType": "complete"}));
        // The task lists `b` again, so its held answer is sent.
        server.set(at(parked(id, &["b"], 100), 1));
        let update = server.request("tasks/update").await;
        assert_eq!(keys(&update), ["b"]);
        server.set(at(task(id, "working", 100), 2));
        server.respond(&update, json!({"resultType": "complete"}));
    };
    let (result, ()) =
        within(async { tokio::join!(respond_with(&client, &host, id, "1", deadline), script) })
            .await;
    // `b` was sent, so holding it did not count it as dropped.
    assert_eq!(result.unwrap().status, TaskStatus::Working);
    no_more(&mut frames, "needs_input");
    client.shutdown().await.unwrap();
}

/// Final review, finding 1: the answered counter tells a stable call whether
/// the host answered during it, and only then is its error text withheld.
#[test]
fn stable_errors_are_withheld_once_the_host_answered() {
    let (host, mut frames, connection) = host();
    let error = || {
        tower_mcp::Error::JsonRpc(JsonRpcError {
            code: -32602,
            message: format!("rejected {SECRET}"),
            data: Some(json!(SECRET)),
        })
    };
    let before = host.answered();
    let raw = super::super::redact_if_answered(&host, before, error());
    assert!(format!("{raw:?}").contains(SECRET));

    let question = Question::Form {
        message: "m".into(),
        schema: ElicitFormSchema::new().string_field("value", None, true),
        credential_fields: Vec::new(),
    };
    let _tickets = host
        .open(connection, vec![((), question)], None, None)
        .unwrap();
    let asked = frames.try_recv();
    assert!(asked.is_ok());
    response(&host, &host.session(connection.0), 1, accept());
    let redacted = super::super::redact_if_answered(&host, before, error());
    assert!(!format!("{redacted:?}").contains(SECRET));
    assert_eq!(
        super::super::explicit_failure(&redacted).unwrap().code,
        Some(-32602)
    );
}

/// Review round 5, finding 3: a record that only paces reads is pruned once
/// its slot passes. One with key history stays.
#[test]
fn pacing_only_records_are_pruned_after_their_slot() {
    let (host, _frames, connection) = host();
    let past = Instant::now() - Duration::from_millis(1);
    record(&host, connection, "paced", |record| {
        record.next_poll = Some(past)
    });
    record(&host, connection, "pending", |record| {
        record.next_poll = Some(Instant::now() + Duration::from_secs(60));
    });
    record(&host, connection, "history", |record| {
        record.next_poll = Some(past);
        record.acknowledged.insert("a".into());
    });
    record(&host, connection, "other", |_| ());
    let ledger = host.ledger.lock().unwrap();
    let mut kept = ledger
        .source
        .as_ref()
        .unwrap()
        .tasks
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    kept.sort();
    assert_eq!(kept, ["history", "other", "pending"]);
}

/// Review round 5, finding 4: keys enter a task's history only when they are
/// published or answered locally, not when the batch is refused.
#[tokio::test]
async fn refused_batches_leave_no_key_history() {
    let (host, _frames, connection) = host();
    let client = McpClient::connect(ChannelTransport::new(McpRouter::new()))
        .await
        .unwrap();
    let id = "refused-batches";
    let reader = Reader::new(&client, &host, id).unwrap();
    let settled = Mutex::new(None);
    let watch = || Watch {
        reader: &reader,
        listed: None,
        settled: &settled,
    };
    let oversized = (0..=super::super::MAX_PENDING)
        .map(|n| (format!("k{n}"), form_json()))
        .collect::<serde_json::Map<_, _>>();
    assert!(
        ask_host(
            &client,
            &host,
            connection,
            requests(Value::Object(oversized)),
            None,
            Some(watch()),
        )
        .await
        .is_err()
    );
    assert!(record(&host, connection, id, |record| record
        .asked
        .is_empty()));

    let filler = (0..super::super::MAX_PENDING)
        .map(|_| {
            (
                (),
                Question::Form {
                    message: "filler".into(),
                    schema: ElicitFormSchema::new().string_field("value", None, true),
                    credential_fields: Vec::new(),
                },
            )
        })
        .collect();
    let _held = host.open(connection, filler, None, None).unwrap();
    let batch = requests(json!({
        "a-url": {
            "method": "elicitation/create",
            "params": {"mode": "url", "message": "open", "url": "file:///etc/passwd"}
        },
        "b-form": form_json(),
    }));
    ask_host(&client, &host, connection, batch, None, Some(watch()))
        .await
        .unwrap();
    let asked = record(&host, connection, id, |record| {
        record.asked.keys().cloned().collect::<Vec<_>>()
    });
    assert_eq!(asked, ["a-url"]);
    client.shutdown().await.unwrap();
}

/// A full queue gives keyed busy outcomes and keeps the decline already
/// decided for an unsupported URL. An oversized batch is an error.
#[tokio::test]
async fn queue_saturation_returns_keyed_busy_outcomes() {
    let (host, _frames, connection) = host();
    let client = McpClient::connect(ChannelTransport::new(McpRouter::new()))
        .await
        .unwrap();
    let filler = (0..super::super::MAX_PENDING)
        .map(|_| {
            (
                (),
                Question::Form {
                    message: "filler".into(),
                    schema: ElicitFormSchema::new().string_field("value", None, true),
                    credential_fields: Vec::new(),
                },
            )
        })
        .collect();
    let _held = host.open(connection, filler, None, None).unwrap();
    let batch = requests(json!({
        "a-url": {
            "method": "elicitation/create",
            "params": {"mode": "url", "message": "open", "url": "file:///etc/passwd"}
        },
        "b-form": form_json(),
    }));
    let input = ask_host(&client, &host, connection, batch, None, None)
        .await
        .unwrap();
    assert!(matches!(
        input.responses.get("a-url"),
        Some(InputResponse::Elicit(result)) if result.action == ElicitAction::Decline
    ));
    assert_eq!(
        input.unanswered.into_iter().collect::<Vec<_>>(),
        [("b-form".to_owned(), Unanswered::Busy)]
    );

    let oversized = (0..=super::super::MAX_PENDING)
        .map(|n| (format!("k{n}"), form_json()))
        .collect::<serde_json::Map<_, _>>();
    let error = ask_host(
        &client,
        &host,
        connection,
        requests(Value::Object(oversized)),
        None,
        None,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, tower_mcp::Error::Transport(message)
        if message.contains("pending limit")));
    client.shutdown().await.unwrap();
}

/// A server-chosen key such as `authorization` cannot move a local failure
/// into the auth exit status.
#[test]
fn local_failures_keep_their_status_whatever_the_key() {
    let mut input = TaskInput::default();
    input
        .unanswered
        .insert("authorization".into(), Unanswered::Expired);
    let error = input.unanswered_error("1 (abc)", "abc").unwrap();
    assert_eq!(ExitStatus::from_mcp_error(&error), ExitStatus::Transport);
    let message = super::super::explicit_failure(&error).unwrap().message;
    assert!(message.contains("`authorization` (host reply expired)"));
    assert!(message.contains("`task abc respond`"));

    input
        .unanswered
        .insert("b-sample".into(), Unanswered::SamplingRefused);
    let error = input.unanswered_error("1 (abc)", "abc").unwrap();
    assert_eq!(ExitStatus::from_mcp_error(&error), ExitStatus::Auth);
}

/// A `wait --timeout` too large for the clock has no deadline rather than a
/// panic.
#[test]
fn an_unrepresentable_deadline_is_none() {
    assert!(deadline_after(Some(Duration::MAX)).is_none());
    assert!(deadline_after(None).is_none());
    assert!(deadline_after(Some(Duration::from_secs(1))).is_some());
}
