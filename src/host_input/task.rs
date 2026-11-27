//! Final-protocol task input answered through the bridge.
//!
//! A task parked in `input_required` lists its outstanding requests in
//! `tasks/get`. They are published to the host, and the answers go back with
//! `tasks/update`. The foreground wait for a server-created task, `wait`, and
//! `task <id> respond` share this code.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::Instant;
use tower_mcp::client::McpClient;
use tower_mcp::context::CancellationToken;
use tower_mcp::error::JsonRpcError;
use tower_mcp::protocol::{
    ElicitResult, InputRequest, InputRequests, InputResponse, InputResponses, TaskObject,
    TaskStatus,
};
use tower_mcp::tasks::GetTaskResult;

use super::final_protocol::{MAX_ATTEMPTS, collect, open_error};
use super::{
    Answer, Connection, HOST, Host, Question, Withdrawal, elicit_result, failure, peer_error,
    transport_error,
};
use crate::exit_status::ExitStatus;
use crate::jobs::Jobs;

/// Answered input rounds per wait. Four tool-call attempts allow three
/// answered rounds on the automatic path, so a task gets the same number.
const MAX_TASK_ROUNDS: usize = MAX_ATTEMPTS - 1;
const DEFAULT_POLL_MS: u64 = 1000;
const FAILED_TASK_ERROR: &str = "task failed; the server's error text is withheld in bridge mode";
const FAILED_TASK_STATUS: &str = "Task failed";

/// An absolute deadline `limit` from now, or none when it does not fit.
pub(crate) fn deadline_after(limit: Option<Duration>) -> Option<Instant> {
    limit.and_then(|limit| Instant::now().checked_add(limit))
}

/// One `tasks/get` result.
struct Snapshot {
    task: TaskObject,
    /// The parsed `lastUpdatedAt`.
    stamp: Option<Stamp>,
    /// Every outstanding request when the task is `input_required`.
    outstanding: InputRequests,
    poll: Duration,
}

impl Snapshot {
    fn new(result: GetTaskResult) -> tower_mcp::Result<Self> {
        let poll = Duration::from_millis(
            result
                .task
                .metadata()
                .poll_interval_ms
                .unwrap_or(DEFAULT_POLL_MS)
                .clamp(50, 30_000),
        );
        let outstanding = if result.task.status() == TaskStatus::InputRequired {
            result
                .task
                .input_requests()
                .filter(|requests| !requests.is_empty())
                .cloned()
                .ok_or_else(|| {
                    failure(
                        ExitStatus::Transport,
                        "task is input_required without inputRequests",
                    )
                })?
        } else {
            InputRequests::new()
        };
        let stamp = stamp(&result.task.metadata().last_updated_at);
        Ok(Self {
            task: task_object(result)?,
            stamp,
            outstanding,
            poll,
        })
    }
}

/// A `lastUpdatedAt` timestamp as nanoseconds since the Unix epoch.
type Stamp = i128;

/// Parse an RFC 3339 timestamp such as `2026-07-28T00:00:01.5Z`.
///
/// Conservative on purpose: a leap second, a fraction longer than nine
/// digits, a day the month does not have, or an offset that is not `HH:MM`
/// within range is `None`. A snapshot without a usable stamp never proves a
/// key gone.
fn stamp(text: &str) -> Option<Stamp> {
    let digits = |part: &str| -> Option<i64> {
        (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit())).then(|| part.parse().ok())?
    };
    let field = |range: std::ops::Range<usize>| text.get(range).and_then(digits);
    let bytes = text.as_bytes();
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (field(0..4)?, field(5..7)?, field(8..10)?);
    let (hour, minute, second) = (field(11..13)?, field(14..16)?, field(17..19)?);
    let leap_year = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap_year => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=month_days).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let mut rest = &text[19..];
    let mut nanos: i128 = 0;
    if let Some(fraction) = rest.strip_prefix('.') {
        let len = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if !(1..=9).contains(&len) {
            return None;
        }
        nanos = i128::from(digits(&fraction[..len])?) * 10i128.pow(9 - len as u32);
        rest = &fraction[len..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        zone if zone.len() == 6 && zone.is_ascii() && &zone[3..4] == ":" => {
            let sign = match &zone[..1] {
                "+" => 1,
                "-" => -1,
                _ => return None,
            };
            let hours = digits(&zone[1..3])?;
            let minutes = digits(&zone[4..6])?;
            if hours > 23 || minutes > 59 {
                return None;
            }
            sign * (hours * 3600 + minutes * 60)
        }
        _ => return None,
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    Some(i128::from(seconds) * 1_000_000_000 + nanos)
}

/// What the bridge knows about one task on the active connection.
///
/// Owned by the connection, so it ends with it. Every read and every input
/// round of the task, in any command, uses the same record.
#[derive(Debug)]
pub(super) struct TaskRecord {
    /// Keys the server acknowledged through `tasks/update`.
    acknowledged: BTreeSet<String>,
    /// Keys published to the host or answered locally, each with the stamp
    /// of the newest snapshot that listed it.
    asked: BTreeMap<String, Option<Stamp>>,
    /// Asked keys missing from a snapshot strictly newer than the newest one
    /// that listed them. SEP-2663 forbids reusing a key, so these are never
    /// asked or sent again, even if an older snapshot lists them.
    removed: BTreeSet<String>,
    /// No read of the task starts before this.
    next_poll: Option<Instant>,
    interval: Duration,
}

impl Default for TaskRecord {
    fn default() -> Self {
        Self {
            acknowledged: BTreeSet::new(),
            asked: BTreeMap::new(),
            removed: BTreeSet::new(),
            next_poll: None,
            interval: Duration::from_millis(DEFAULT_POLL_MS),
        }
    }
}

impl TaskRecord {
    /// Record a successful read.
    fn observe(&mut self, snapshot: &Snapshot) {
        if snapshot.task.status.is_terminal() {
            // A settled task asks for nothing more. The poll slot stays, so a
            // follow-up read of it is still paced until the slot passes.
            self.acknowledged.clear();
            self.asked.clear();
            self.removed.clear();
            return;
        }
        let now = snapshot.stamp;
        let newer =
            |then: &Option<Stamp>| matches!((now, then), (Some(now), Some(then)) if now > *then);
        for (key, listed) in &mut self.asked {
            if snapshot.outstanding.contains_key(key) {
                if now > *listed {
                    *listed = now;
                }
            } else if newer(listed) {
                self.removed.insert(key.clone());
            }
        }
    }

    fn excluded(&self, key: &str) -> bool {
        self.acknowledged.contains(key) || self.removed.contains(key)
    }

    /// A record that only paces reads can go once its slot has passed.
    pub(super) fn prunable(&self, now: Instant) -> bool {
        self.acknowledged.is_empty()
            && self.asked.is_empty()
            && self.removed.is_empty()
            && self.next_poll.is_none_or(|next| next <= now)
    }
}

/// Reads one task and keeps its record current.
///
/// Every read goes through here: the wait loop, the watch while questions are
/// open, the check before `tasks/update`, the report after it, and plain
/// inspection. Reads of a task start at least `pollIntervalMs` apart across
/// commands.
struct Reader<'a> {
    client: &'a McpClient,
    host: &'a Host,
    connection: Connection,
    id: &'a str,
}

impl<'a> Reader<'a> {
    fn new(client: &'a McpClient, host: &'a Host, id: &'a str) -> tower_mcp::Result<Self> {
        Ok(Self {
            client,
            host,
            connection: active(host)?,
            id,
        })
    }

    /// Run `f` on the task's record. The record exists while this reader's
    /// connection is active; after that, `f` sees an empty one.
    fn record<T>(&self, f: impl FnOnce(&mut TaskRecord) -> T) -> T {
        let mut f = Some(f);
        self.host
            .with_task(self.connection, self.id, |record| {
                (f.take().expect("called once"))(record)
            })
            .unwrap_or_else(|| (f.take().expect("called once"))(&mut TaskRecord::default()))
    }

    /// Read at the task's next poll slot.
    async fn read(&self) -> tower_mcp::Result<Snapshot> {
        if let Some(next) = self.record(|record| record.next_poll) {
            tokio::time::sleep_until(next).await;
        }
        self.read_now().await
    }

    /// Read at the next slot, or `None` when that slot falls after
    /// `deadline`.
    async fn read_by(&self, deadline: Option<Instant>) -> Option<tower_mcp::Result<Snapshot>> {
        let next = self.record(|record| record.next_poll);
        if let (Some(next), Some(deadline)) = (next, deadline)
            && next > deadline
        {
            return None;
        }
        Some(self.read().await)
    }

    async fn read_now(&self) -> tower_mcp::Result<Snapshot> {
        let start = Instant::now();
        // Reserved before sending, so a read dropped in flight still spaces
        // the next one.
        self.record(|record| record.next_poll = Some(start + record.interval));
        let result = self
            .client
            .task_get_detailed(self.id)
            .await
            .map_err(|error| peer_error(error, "tasks/get"))?;
        let snapshot = Snapshot::new(result)?;
        self.record(|record| {
            record.interval = snapshot.poll;
            record.next_poll = Some(start + snapshot.poll);
            record.observe(&snapshot);
        });
        Ok(snapshot)
    }

    fn mark_asked(&self, keys: Vec<String>, listed: Option<Stamp>) {
        self.record(|record| {
            for key in keys {
                let entry = record.asked.entry(key).or_insert(listed);
                if listed > *entry {
                    *entry = listed;
                }
            }
        });
    }

    fn acknowledge(&self, keys: impl IntoIterator<Item = String>) {
        let keys = keys.into_iter().collect::<Vec<_>>();
        self.record(|record| record.acknowledged.extend(keys));
    }

    fn removed(&self) -> BTreeSet<String> {
        self.record(|record| record.removed.clone())
    }

    /// The outstanding requests not acknowledged and not seen removed. A
    /// snapshot that lists only those keys is stale.
    fn fresh(&self, outstanding: &InputRequests) -> InputRequests {
        self.record(|record| {
            outstanding
                .iter()
                .filter(|(key, _)| !record.excluded(key))
                .map(|(key, request)| (key.clone(), request.clone()))
                .collect()
        })
    }
}

/// `task <id>`, `jobs`, and `cancel` over the bridge read the task through
/// the same redacting, paced reader.
pub(crate) async fn inspect_task(client: &McpClient, id: &str) -> tower_mcp::Result<TaskObject> {
    let host = HOST
        .get()
        .ok_or_else(|| transport_error("host input unavailable"))?;
    inspect_with(client, host, id).await
}

async fn inspect_with(client: &McpClient, host: &Host, id: &str) -> tower_mcp::Result<TaskObject> {
    Ok(Reader::new(client, host, id)?.read().await?.task)
}

/// The task object `tasks/get` described, without a second read.
///
/// A task error and the status message of a failed task are server text
/// written after the answers arrived, so both are replaced with fixed text.
/// The JSON-RPC code is kept.
fn task_object(result: GetTaskResult) -> tower_mcp::Result<TaskObject> {
    let task = &result.task;
    let metadata = task.metadata();
    let completed = task
        .result()
        .cloned()
        .map(|result| serde_json::from_value(serde_json::Value::Object(result)))
        .transpose()
        .map_err(|_| failure(ExitStatus::Transport, "completed task result is malformed"))?;
    // tower-mcp writes `Task failed: {message}` into the status message, and
    // a handler error can also arrive on a completed task.
    let failed = task.status() == TaskStatus::Failed || task.error().is_some();
    Ok(TaskObject {
        task_id: metadata.task_id.clone(),
        status: task.status(),
        status_message: if failed {
            Some(FAILED_TASK_STATUS.to_owned())
        } else {
            metadata.status_message.clone()
        },
        created_at: metadata.created_at.clone(),
        last_updated_at: metadata.last_updated_at.clone(),
        ttl: metadata.ttl_ms,
        poll_interval: metadata.poll_interval_ms,
        result: completed,
        error: task.error().map(|error| JsonRpcError {
            code: error.code,
            message: FAILED_TASK_ERROR.to_owned(),
            data: None,
        }),
        meta: result.meta.clone().map(serde_json::Value::Object),
    })
}

/// What the host said about one batch of task input requests.
#[derive(Debug, Default)]
struct TaskInput {
    /// Answers for `tasks/update`, including elicitation declines and cancels.
    responses: InputResponses,
    /// Keys left outstanding, each with the reason. A reason never carries an
    /// answer value.
    unanswered: BTreeMap<String, Unanswered>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unanswered {
    /// A task continuation has no sampling error value, so a refusal leaves
    /// the key outstanding.
    SamplingRefused,
    Expired,
    Cancelled,
    /// The question could not be published.
    Busy,
    /// The task changed while the question was open.
    Superseded,
    Unsupported,
}

impl Unanswered {
    fn reason(self) -> &'static str {
        match self {
            Self::SamplingRefused => "sampling refused by the host",
            Self::Expired => "host reply expired",
            Self::Cancelled => "host input cancelled",
            Self::Busy => "not published to the host, the queue is full or the request too large",
            Self::Superseded => "withdrawn when the task changed",
            Self::Unsupported => "request kind not supported over host input",
        }
    }

    fn from_withdrawal(reason: Withdrawal) -> Self {
        match reason {
            Withdrawal::Expired => Self::Expired,
            Withdrawal::Busy => Self::Busy,
            Withdrawal::ServerCancelled => Self::Superseded,
            _ => Self::Cancelled,
        }
    }
}

impl TaskInput {
    /// The error for keys left outstanding, naming each key and why. The exit
    /// status is fixed here: 5 when the host refused sampling, matching the
    /// automatic path, and 4 otherwise.
    fn unanswered_error(&self, label: &str, id: &str) -> Option<tower_mcp::Error> {
        if self.unanswered.is_empty() {
            return None;
        }
        let keys = self
            .unanswered
            .iter()
            .map(|(key, why)| {
                format!(
                    "`{}` ({})",
                    crate::untrusted::sanitize_inline(key),
                    why.reason()
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let status = if self
            .unanswered
            .values()
            .any(|why| *why == Unanswered::SamplingRefused)
        {
            ExitStatus::Auth
        } else {
            ExitStatus::Transport
        };
        Some(failure(
            status,
            format!(
                "task {label} is still waiting for input: {keys}; {}",
                respond_hint(id)
            ),
        ))
    }
}

/// The full id resolves in `task <id> respond` however many tasks exist.
fn respond_hint(id: &str) -> String {
    format!(
        "answer it with `task {} respond`",
        crate::untrusted::sanitize_inline(id)
    )
}

/// Polls the task while the host prepares answers.
struct Watch<'a> {
    reader: &'a Reader<'a>,
    /// The stamp of the snapshot the requests came from.
    listed: Option<Stamp>,
    /// Set when the task settles while questions are open.
    settled: &'a Mutex<Option<TaskObject>>,
}

/// Publish `requests` to the host and collect the answers.
///
/// Every request is published at once. An elicitation decline or cancel is an
/// answer. A sampling refusal, a timeout, an unsupported request kind, or a
/// question that could not be published leaves that key out. Host loss, a
/// closed MCP connection, or a batch larger than the pending limit is an error.
///
/// With `watch`, the task is polled at its interval while questions are open.
/// A question whose key the task's record shows removed is withdrawn as
/// `server_cancelled`, and all of them are when the task settles.
///
/// The questions carry a cancellation token scoped to this call. It is
/// cancelled when the call returns or is dropped, so the ledger treats any
/// later reply to them as stale.
async fn ask_host(
    client: &McpClient,
    host: &Host,
    connection: Connection,
    requests: InputRequests,
    deadline: Option<Instant>,
    watch: Option<Watch<'_>>,
) -> tower_mcp::Result<TaskInput> {
    struct CancelOnDrop(CancellationToken);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }

    if requests.len() > super::MAX_PENDING {
        return Err(transport_error(
            "host input batch exceeds the pending limit",
        ));
    }
    // Declared before the tickets, so it is dropped after them: each ticket
    // withdraws its question as cancelled before the token fires.
    let scope = CancelOnDrop(CancellationToken::new());
    let mut input = TaskInput::default();
    let mut questions = Vec::new();
    for (key, request) in requests {
        match request {
            InputRequest::Elicit(params) => match Question::elicit(params) {
                Some(question) => questions.push(((key, false), question)),
                None => {
                    input
                        .responses
                        .insert(key, InputResponse::Elicit(ElicitResult::decline()));
                }
            },
            InputRequest::CreateMessage(params) => {
                questions.push(((key, true), Question::Sampling(Box::new(params))));
            }
            _ => {
                input.unanswered.insert(key, Unanswered::Unsupported);
            }
        }
    }
    // Only keys published to the host or answered here enter the task's
    // history, so a refused batch leaves no trace.
    let mark = |keys: Vec<String>| {
        if let Some(watch) = &watch {
            watch.reader.mark_asked(keys, watch.listed);
        }
    };
    let decided = input.responses.keys().cloned().collect::<Vec<_>>();
    if questions.is_empty() {
        mark(decided);
        return Ok(input);
    }
    let keys = questions
        .iter()
        .map(|((key, _), _)| key.clone())
        .collect::<Vec<_>>();
    let tickets = match host.open(connection, questions, Some(&scope.0), deadline) {
        Ok(tickets) => tickets,
        Err(Withdrawal::Busy) => {
            mark(decided);
            input
                .unanswered
                .extend(keys.into_iter().map(|key| (key, Unanswered::Busy)));
            return Ok(input);
        }
        Err(reason) => return Err(open_error(reason)),
    };
    mark(decided.into_iter().chain(keys).collect());
    let open = tickets
        .iter()
        .map(|((key, _), ticket)| (key.clone(), ticket.id))
        .collect::<Vec<_>>();
    let collecting = collect(client, host, tickets, |(key, sampling), answer| {
        let unanswered = match (sampling, answer) {
            (_, Err(reason)) => Some(Unanswered::from_withdrawal(reason)),
            (true, Ok(Answer::Sample { result, .. })) => {
                input
                    .responses
                    .insert(key.clone(), InputResponse::CreateMessage(*result));
                None
            }
            (true, Ok(_)) => Some(Unanswered::SamplingRefused),
            (false, answer) => {
                input
                    .responses
                    .insert(key.clone(), InputResponse::Elicit(elicit_result(answer)));
                None
            }
        };
        if let Some(why) = unanswered {
            input.unanswered.insert(key, why);
        }
        Ok(())
    });
    let Some(watch) = watch else {
        collecting.await?;
        return Ok(input);
    };
    let watching = async {
        loop {
            // A failed read keeps the questions open. The check before
            // submitting reads the task again.
            let Ok(snapshot) = watch.reader.read().await else {
                continue;
            };
            let settled = snapshot.task.status.is_terminal();
            let removed = watch.reader.removed();
            for (key, ticket) in &open {
                if settled || removed.contains(key) {
                    host.withdraw(*ticket, connection, Withdrawal::ServerCancelled);
                }
            }
            if settled {
                *watch.settled.lock().unwrap() = Some(snapshot.task);
                std::future::pending::<()>().await;
            }
        }
    };
    tokio::select! {
        result = collecting => result?,
        () = watching => unreachable!("the watch never finishes"),
    }
    Ok(input)
}

enum Round {
    /// Answers were sent, or nothing asked is outstanding any more.
    Continue,
    /// The task settled while the host was answering. Nothing was sent.
    Settled(Box<TaskObject>),
}

/// Ask the host for `requests`, check what the task still lists, and send the
/// answers for those keys with `tasks/update`.
///
/// Answers go out only after a successful read at the next poll slot shows
/// the task still parked, and only while the command deadline has not
/// passed. A key the task's record shows removed is never sent. If any key
/// stays unanswered, the answers that arrived are still sent and the round
/// ends with an error naming the rest, so the task stays parked.
async fn answer_round(
    reader: &Reader<'_>,
    label: &str,
    requests: InputRequests,
    listed: Option<Stamp>,
    deadline: Option<Instant>,
) -> tower_mcp::Result<Round> {
    let settled = Mutex::new(None);
    let watch = Watch {
        reader,
        listed,
        settled: &settled,
    };
    let mut input = ask_host(
        reader.client,
        reader.host,
        reader.connection,
        requests,
        deadline,
        Some(watch),
    )
    .await?;
    if let Some(task) = settled.into_inner().unwrap() {
        return Ok(Round::Settled(Box::new(task)));
    }
    // Each check sends at once the answers whose keys it lists. An answer
    // whose key it does not list is held, not counted as dropped, and the
    // task is read again at the next slot while the deadline allows. Answers
    // still held when no slot remains are dropped; a later command asks again
    // if the task lists their keys.
    let mut last = None;
    while let Some(check) = reader.read_by(deadline).await {
        let snapshot = check?;
        if snapshot.task.status.is_terminal() {
            return Ok(Round::Settled(Box::new(snapshot.task)));
        }
        let removed = reader.removed();
        let (ready, held): (InputResponses, InputResponses) = std::mem::take(&mut input.responses)
            .into_iter()
            .filter(|(key, _)| !removed.contains(key))
            .partition(|(key, _)| snapshot.outstanding.contains_key(key));
        input.responses = held;
        if !ready.is_empty() {
            send_update(reader, label, ready, deadline).await?;
        }
        last = Some(snapshot);
        if input.responses.is_empty() {
            break;
        }
    }
    if last.is_none() && !input.responses.is_empty() {
        return Err(unchecked(reader, label));
    }
    // An unanswered key is reported only while the task still lists it.
    let removed = reader.removed();
    input.unanswered.retain(|key, _| {
        !removed.contains(key)
            && last
                .as_ref()
                .is_none_or(|snapshot| snapshot.outstanding.contains_key(key))
    });
    match input.unanswered_error(label, reader.id) {
        Some(error) => Err(error),
        None => Ok(Round::Continue),
    }
}

fn unchecked(reader: &Reader<'_>, label: &str) -> tower_mcp::Error {
    failure(
        ExitStatus::Transport,
        format!(
            "the command deadline passed before the answers could be checked against task \
             {label}, so nothing was sent; it is still waiting for input; {}",
            respond_hint(reader.id)
        ),
    )
}

/// Send `responses` with `tasks/update`, unless the deadline has passed.
async fn send_update(
    reader: &Reader<'_>,
    label: &str,
    responses: InputResponses,
    deadline: Option<Instant>,
) -> tower_mcp::Result<()> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Err(unchecked(reader, label));
    }
    let keys = responses.keys().cloned().collect::<Vec<_>>();
    let update = reader.client.task_update(reader.id, responses);
    let outcome = match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, update)
            .await
            .unwrap_or_else(|_| Err(transport_error("no acknowledgement before --timeout"))),
        None => update.await,
    };
    match outcome {
        Ok(()) => {
            reader.acknowledge(keys);
            Ok(())
        }
        // A JSON-RPC error other than `-32000` is the server refusing the
        // update, so its keys can be asked again at once. tower-mcp also
        // reports HTTP transport failures as `-32000`.
        Err(error @ tower_mcp::Error::JsonRpc(_)) if !uncertain(&error) => {
            Err(peer_error(error, "tasks/update"))
        }
        // The update may or may not have reached the server. This command
        // ends here; a later `respond` or `wait` asks again for any key a
        // snapshot still lists, which is the operator's retry.
        Err(error) => Err(failure(
            ExitStatus::from_mcp_error(&error),
            format!(
                "the tasks/update for task {label} was not acknowledged, so it may have been \
                 applied; check the task before answering again; {}",
                respond_hint(reader.id)
            ),
        )),
    }
}

/// A failed update whose outcome is unknown.
fn uncertain(error: &tower_mcp::Error) -> bool {
    match error {
        tower_mcp::Error::JsonRpc(rpc) => rpc.code == -32000,
        _ => true,
    }
}

fn active(host: &Host) -> tower_mcp::Result<Connection> {
    if host.lost.is_cancelled() {
        return Err(super::lost_error());
    }
    host.connection()
        .ok_or_else(|| transport_error("host input connection unavailable"))
}

/// `wait` for a task this session already knows, answering its input
/// requests through the bridge. Without the bridge this is `task_wait`.
pub(crate) async fn wait_task(
    client: &McpClient,
    jobs: &Jobs,
    id: &str,
    deadline: Option<Instant>,
) -> tower_mcp::Result<TaskObject> {
    match HOST.get() {
        Some(host) => wait_for_task(client, host, jobs, None, id, deadline).await,
        None => client.task_wait(id).await,
    }
}

/// Poll a task to a terminal state, answering fresh input requests through
/// the bridge.
///
/// A snapshot that lists only acknowledged keys is stale and is polled again
/// at the task's interval. Only rounds that ask the host count toward the
/// limit. With `tool`, the task is added to `jobs` the first time it asks for
/// input, so `task <id> respond` can find it if the wait leaves it parked.
pub(super) async fn wait_for_task(
    client: &McpClient,
    host: &Host,
    jobs: &Jobs,
    tool: Option<&str>,
    id: &str,
    deadline: Option<Instant>,
) -> tower_mcp::Result<TaskObject> {
    let reader = Reader::new(client, host, id)?;
    let mut rounds = 0;
    let mut registered = false;
    loop {
        active(host)?;
        let snapshot = reader.read().await?;
        if snapshot.task.status.is_terminal() {
            jobs.sync(
                id,
                snapshot.task.status,
                snapshot.task.status_message.clone(),
            );
            return Ok(snapshot.task);
        }
        if let Some(tool) = tool.filter(|_| !registered && !snapshot.outstanding.is_empty()) {
            jobs.register(
                id.to_owned(),
                tool.to_owned(),
                TaskStatus::InputRequired,
                None,
            );
            registered = true;
        }
        // Working, or a stale snapshot of acknowledged keys: the next read
        // waits for the poll slot.
        let requests = reader.fresh(&snapshot.outstanding);
        if requests.is_empty() {
            continue;
        }
        let label = jobs.label_for(id);
        if rounds == MAX_TASK_ROUNDS {
            return Err(failure(
                ExitStatus::Transport,
                format!(
                    "host input round limit exceeded; task {label} is still waiting for input; {}",
                    respond_hint(id)
                ),
            ));
        }
        rounds += 1;
        if let Round::Settled(task) =
            answer_round(&reader, &label, requests, snapshot.stamp, deadline).await?
        {
            jobs.sync(id, task.status, task.status_message.clone());
            return Ok(*task);
        }
    }
}

/// `task <id> respond` over the bridge: ask the host for every request the
/// task lists that has not been acknowledged, then report the task as it is
/// after the update.
///
/// A snapshot that lists only acknowledged keys is polled again at the task's
/// interval until it changes. The caller bounds the whole command with
/// `deadline`, which also bounds each question and each scheduled read.
pub(crate) async fn respond_task(
    client: &McpClient,
    id: &str,
    label: &str,
    deadline: Option<Instant>,
) -> tower_mcp::Result<TaskObject> {
    let host = HOST
        .get()
        .ok_or_else(|| transport_error("host input unavailable"))?;
    respond_with(client, host, id, label, deadline).await
}

async fn respond_with(
    client: &McpClient,
    host: &Host,
    id: &str,
    label: &str,
    deadline: Option<Instant>,
) -> tower_mcp::Result<TaskObject> {
    let not_waiting = |task: &TaskObject| {
        failure(
            ExitStatus::NoMatch,
            format!(
                "task {label} is not waiting for input (status: {})",
                task.status
            ),
        )
    };
    let reader = Reader::new(client, host, id)?;
    loop {
        active(host)?;
        let snapshot = reader.read().await?;
        if snapshot.outstanding.is_empty() {
            return Err(not_waiting(&snapshot.task));
        }
        let requests = reader.fresh(&snapshot.outstanding);
        if requests.is_empty() {
            continue;
        }
        return match answer_round(&reader, label, requests, snapshot.stamp, deadline).await? {
            Round::Settled(task) => Ok(*task),
            // The handler resumes asynchronously, so report what the task
            // says at the next poll slot. When that slot is after the
            // deadline, the snapshot the answers were based on is reported.
            Round::Continue => match reader.read_by(deadline).await {
                Some(after) => Ok(after?.task),
                None => Ok(snapshot.task),
            },
        };
    }
}

#[cfg(test)]
mod tests;
