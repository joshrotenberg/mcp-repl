//! The versioned host control channel and local input lifetimes.

mod final_protocol;
#[cfg(unix)]
mod socket;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tower_mcp::client::McpClient;
use tower_mcp::context::CancellationToken;
use tower_mcp::error::JsonRpcError;
use tower_mcp::protocol::{
    ContentRole, CreateMessageParams, CreateMessageResult, ElicitAction, ElicitFieldValue,
    ElicitFormSchema, ElicitRequestParams, ElicitResult, PrimitiveSchemaDefinition,
};

pub(crate) use final_protocol::call_tool;

const VERSION: u32 = 1;
const MAX_FRAME: usize = 1024 * 1024;
const MAX_PENDING: usize = 32;
const MAX_RETIRED: usize = 256;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const OPERATOR_MODEL: &str = "mcp-repl/operator";
pub(crate) const CHANNEL_CLOSED: &str = "host input channel closed";

pub(crate) struct Endpoint {
    #[cfg(unix)]
    stream: std::os::unix::net::UnixStream,
}

impl Endpoint {
    /// Called before Tokio or any child is started.
    pub(crate) fn claim(fd: i32) -> Result<Self, String> {
        #[cfg(unix)]
        {
            socket::claim(fd).map(|stream| Self { stream })
        }
        #[cfg(not(unix))]
        {
            let _ = fd;
            Err("--host-input-fd is supported only on Unix".into())
        }
    }

    pub(crate) async fn run<T>(
        self,
        answer_timeout: Duration,
        run: impl Future<Output = tower_mcp::Result<T>>,
    ) -> tower_mcp::Result<T> {
        #[cfg(unix)]
        {
            socket::run(self.stream, answer_timeout, run).await
        }
        #[cfg(not(unix))]
        {
            let _ = (self, answer_timeout, run);
            Err(transport_error("--host-input-fd is supported only on Unix"))
        }
    }
}

static HOST: OnceLock<Host> = OnceLock::new();

pub(crate) fn enabled() -> bool {
    HOST.get().is_some()
}

pub(crate) async fn lost() {
    match HOST.get() {
        Some(host) => host.lost.cancelled().await,
        None => std::future::pending().await,
    }
}

/// Withdraw only the stable questions opened while this operation was live.
/// Final continuations already own their tickets.
pub(crate) async fn command_request<T>(request: impl Future<Output = T>) -> T {
    struct WithdrawOnDrop<'a> {
        host: &'a Host,
        after: Option<u64>,
    }

    impl Drop for WithdrawOnDrop<'_> {
        fn drop(&mut self) {
            if let Some(after) = self.after {
                self.host.withdraw_stable_after(after);
            }
        }
    }

    let mut guard = HOST.get().map(|host| WithdrawOnDrop {
        host,
        after: Some(host.ledger.lock().unwrap().next_id),
    });
    let result = request.await;
    if let Some(guard) = &mut guard {
        guard.after = None;
    }
    result
}

pub(crate) fn lost_error() -> tower_mcp::Error {
    transport_error(CHANNEL_CLOSED)
}

pub(crate) async fn finish() {
    let Some(host) = HOST.get() else { return };
    {
        let mut ledger = host.ledger.lock().unwrap();
        host.withdraw_pending(&mut ledger, Withdrawal::Shutdown);
        ledger.source = None;
    }
    host.flush().await;
}

fn transport_error(message: &'static str) -> tower_mcp::Error {
    tower_mcp::Error::Transport(message.into())
}

#[derive(Clone, Copy)]
pub(crate) struct Connection(u64);

impl Connection {
    pub(crate) fn candidate() -> Option<Self> {
        let host = HOST.get()?;
        let mut ledger = host.ledger.lock().unwrap();
        ledger.next_connection += 1;
        Some(Self(ledger.next_connection))
    }

    pub(crate) fn activate(self, info: &crate::ConnectionInfo, client: &Arc<McpClient>) {
        let host = HOST.get().expect("bridge installed");
        host.activate(
            self,
            &info.server_info.name,
            &info.protocol_version,
            Some(Arc::downgrade(client)),
        );
    }

    pub(crate) async fn elicit(
        self,
        params: ElicitRequestParams,
        token: CancellationToken,
    ) -> ElicitResult {
        let Some(question) = Question::elicit(params) else {
            return ElicitResult::decline();
        };
        let result = HOST
            .get()
            .expect("bridge installed")
            .ask(self, question, token)
            .await;
        elicit_result(result)
    }

    pub(crate) async fn sample(
        self,
        params: CreateMessageParams,
        token: CancellationToken,
    ) -> Result<CreateMessageResult, JsonRpcError> {
        let result = HOST
            .get()
            .expect("bridge installed")
            .ask(self, Question::Sampling(Box::new(params)), token)
            .await;
        sample_result(result)
    }
}

#[derive(Serialize)]
struct Source {
    #[serde(skip)]
    connection: u64,
    #[serde(skip)]
    client: Option<Weak<McpClient>>,
    name: String,
    protocol: String,
}

impl Source {
    fn disconnected(&self) -> bool {
        self.client
            .as_ref()
            .is_some_and(|client| client.upgrade().is_none_or(|client| !client.is_connected()))
    }
}

#[derive(Serialize)]
#[serde(tag = "kind", content = "request", rename_all = "snake_case")]
enum Question {
    Form {
        message: String,
        schema: ElicitFormSchema,
        #[serde(rename = "credentialFields", skip_serializing_if = "Vec::is_empty")]
        credential_fields: Vec<String>,
    },
    Url {
        message: String,
        url: String,
    },
    Sampling(Box<CreateMessageParams>),
}

impl Question {
    fn elicit(params: ElicitRequestParams) -> Option<Self> {
        match params {
            ElicitRequestParams::Form(form) => {
                let schema = form.requested_schema;
                let credential_fields = schema
                    .properties
                    .keys()
                    .filter(|name| crate::wire::looks_like_credential(name))
                    .cloned()
                    .collect();
                Some(Self::Form {
                    message: form.message,
                    schema,
                    credential_fields,
                })
            }
            ElicitRequestParams::Url(params) => Self::url(params.message, params.url),
            ElicitRequestParams::FinalUrl(params) => Self::url(params.message, params.url),
            _ => None,
        }
    }

    fn url(message: String, url: String) -> Option<Self> {
        (crate::elicit::is_web_url(&url) && url::Url::parse(&url).is_ok())
            .then_some(Self::Url { message, url })
    }

    fn actions(&self) -> &'static [&'static str] {
        match self {
            Self::Form { .. } => &["accept", "decline", "cancel"],
            Self::Url { .. } => &["confirm_url", "decline", "cancel"],
            Self::Sampling(_) => &["sample", "decline", "cancel"],
        }
    }

    fn expected(self) -> ExpectedAnswer {
        match self {
            Self::Form { schema, .. } => ExpectedAnswer::Form(schema),
            Self::Url { .. } => ExpectedAnswer::Url,
            Self::Sampling(_) => ExpectedAnswer::Sampling,
        }
    }
}

enum ExpectedAnswer {
    Form(ElicitFormSchema),
    Url,
    Sampling,
}

impl ExpectedAnswer {
    fn validate(&self, answer: &Answer) -> Result<(), &'static str> {
        let valid = match (self, answer) {
            (_, Answer::Decline {} | Answer::Cancel {}) | (Self::Url, Answer::ConfirmUrl {}) => {
                true
            }
            (Self::Form(schema), Answer::Accept { content }) => {
                schema.required.iter().all(|key| content.contains_key(key))
                    && content.iter().all(|(key, value)| {
                        schema
                            .properties
                            .get(key)
                            .is_some_and(|field| value.fits(field))
                    })
            }
            (Self::Sampling, Answer::Sample { origin, result }) => {
                result.role == ContentRole::Assistant
                    && !result.content_items().is_empty()
                    && match origin {
                        SamplingOrigin::Operator => result.model == OPERATOR_MODEL,
                        SamplingOrigin::Model => {
                            !result.model.trim().is_empty()
                                && !matches!(
                                    result.model.as_str(),
                                    OPERATOR_MODEL | "mcp-repl/canned"
                                )
                        }
                    }
            }
            _ => return Err("wrong_kind"),
        };
        valid.then_some(()).ok_or("invalid_payload")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum SamplingOrigin {
    Operator,
    Model,
}

// Prefer the integer variant during deserialization to keep full i64
// precision before converting to the tower-mcp field type.
#[derive(Deserialize)]
#[serde(untagged)]
enum FormValue {
    Integer(i64),
    Number(f64),
    String(String),
    Boolean(bool),
    StringArray(Vec<String>),
}

impl FormValue {
    fn fits(&self, field: &PrimitiveSchemaDefinition) -> bool {
        let raw = crate::elicit::field_json(field);
        let kind = raw.get("type").and_then(Value::as_str);
        let matches_type = matches!(
            (kind, self),
            (Some("integer" | "number"), Self::Integer(_))
                | (Some("number"), Self::Number(_))
                | (Some("string") | None, Self::String(_))
                | (Some("boolean"), Self::Boolean(_))
                | (Some("array"), Self::StringArray(_))
        );
        matches_type
            && crate::elicit::accepted_values_from_json(&raw)
                .or_else(|| titled_values(&raw))
                .is_none_or(|(choices, multi)| match (self, multi) {
                    (Self::StringArray(values), true) => {
                        values.iter().all(|value| choices.contains(value))
                    }
                    (Self::String(value), false) => choices.contains(value),
                    (Self::Integer(value), false) => choices.contains(&value.to_string()),
                    (Self::Number(value), false) => choices.contains(&json!(value).to_string()),
                    (Self::Boolean(value), false) => choices.contains(&value.to_string()),
                    _ => false,
                })
    }
}

fn titled_values(raw: &Value) -> Option<(Vec<String>, bool)> {
    let multi = raw.get("type").and_then(Value::as_str) == Some("array");
    let titled = raw.pointer(if multi { "/items/anyOf" } else { "/oneOf" })?;
    let values = titled
        .as_array()?
        .iter()
        .map(|option| option.get("const")?.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()?;
    Some((values, multi))
}

impl From<FormValue> for ElicitFieldValue {
    fn from(value: FormValue) -> Self {
        match value {
            FormValue::Integer(n) => Self::Integer(n),
            FormValue::Number(n) => Self::Number(n),
            FormValue::String(s) => Self::String(s),
            FormValue::Boolean(b) => Self::Boolean(b),
            FormValue::StringArray(values) => Self::StringArray(values),
        }
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Answer {
    Accept {
        content: HashMap<String, FormValue>,
    },
    ConfirmUrl {},
    Sample {
        origin: SamplingOrigin,
        result: Box<CreateMessageResult>,
    },
    Decline {},
    Cancel {},
}

#[derive(Serialize)]
struct NeedsInput<'a> {
    v: u32,
    #[serde(rename = "type")]
    frame_type: &'static str,
    session: &'a str,
    id: String,
    server: &'a Source,
    #[serde(flatten)]
    question: &'a Question,
    actions: &'static [&'static str],
    #[serde(rename = "deadlineUnixMs")]
    deadline: u128,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Withdrawal {
    Expired,
    ServerCancelled,
    Cancelled,
    Disconnected,
    Shutdown,
    HostGone,
    Busy,
}

impl Withdrawal {
    fn code(self) -> &'static str {
        match self {
            Self::Expired => "expired",
            Self::ServerCancelled => "server_cancelled",
            Self::Cancelled => "cancelled",
            Self::Disconnected => "disconnected",
            Self::Shutdown => "shutdown",
            Self::HostGone => "host_gone",
            Self::Busy => "busy",
        }
    }
}

type AnswerResult = Result<Answer, Withdrawal>;

fn elicit_result(result: AnswerResult) -> ElicitResult {
    match result {
        Ok(Answer::Accept { content }) => ElicitResult::accept(
            content
                .into_iter()
                .map(|(key, value)| (key, value.into()))
                .collect(),
        ),
        Ok(Answer::ConfirmUrl {}) => ElicitResult {
            action: ElicitAction::Accept,
            content: None,
            meta: None,
        },
        Ok(Answer::Decline {}) => ElicitResult::decline(),
        _ => ElicitResult::cancel(),
    }
}

fn sample_result(result: AnswerResult) -> Result<CreateMessageResult, JsonRpcError> {
    match result {
        Ok(Answer::Sample { result, .. }) => Ok(*result),
        Ok(_) => Err(crate::sampling::declined("host refused")),
        Err(Withdrawal::Expired) => Err(JsonRpcError::request_timeout("host reply expired")),
        Err(_) => Err(crate::sampling::declined("host input unavailable")),
    }
}

enum Outgoing {
    Frame(Vec<u8>),
    Flush(oneshot::Sender<()>),
}

struct Pending {
    expected: ExpectedAnswer,
    reply: oneshot::Sender<AnswerResult>,
    deadline: Instant,
    server_stop: Option<CancellationToken>,
}

#[derive(Default)]
struct Ledger {
    next_connection: u64,
    next_id: u64,
    source: Option<Source>,
    pending: HashMap<u64, Pending>,
    retired: VecDeque<(u64, &'static str)>,
}

impl Ledger {
    fn retire(&mut self, id: u64, code: &'static str) {
        if self.retired.len() == MAX_RETIRED {
            self.retired.pop_front();
        }
        self.retired.push_back((id, code));
    }
}

struct Host {
    nonce: String,
    timeout: Duration,
    outgoing: mpsc::Sender<Outgoing>,
    lost: CancellationToken,
    ledger: Mutex<Ledger>,
}

impl Host {
    fn session(&self, connection: u64) -> String {
        format!("{}-{connection}", self.nonce)
    }

    fn activate(
        &self,
        connection: Connection,
        name: &str,
        protocol: &str,
        client: Option<Weak<McpClient>>,
    ) {
        let mut ledger = self.ledger.lock().unwrap();
        self.withdraw_pending(&mut ledger, Withdrawal::Disconnected);
        ledger.retired.clear();
        ledger.source = Some(Source {
            connection: connection.0,
            client,
            name: name.into(),
            protocol: protocol.into(),
        });
    }

    fn connection(&self) -> Option<Connection> {
        self.ledger
            .lock()
            .unwrap()
            .source
            .as_ref()
            .map(|source| Connection(source.connection))
    }

    fn open<K>(
        &self,
        connection: Connection,
        questions: Vec<(K, Question)>,
        server_stop: Option<&CancellationToken>,
        operation_deadline: Option<Instant>,
    ) -> Result<Vec<(K, Ticket<'_>)>, Withdrawal> {
        let mut ledger = self.ledger.lock().unwrap();
        if self.lost.is_cancelled() {
            return Err(Withdrawal::HostGone);
        }
        let Some(source) = ledger
            .source
            .as_ref()
            .filter(|s| s.connection == connection.0 && !s.disconnected())
        else {
            return Err(Withdrawal::Disconnected);
        };
        if server_stop.is_some_and(CancellationToken::is_cancelled) {
            return Err(Withdrawal::ServerCancelled);
        }
        if questions.len() > MAX_PENDING - ledger.pending.len() {
            return Err(Withdrawal::Busy);
        }
        let permits = self
            .outgoing
            .try_reserve_many(questions.len())
            .map_err(|_| Withdrawal::Busy)?;
        let now = Instant::now();
        let deadline =
            operation_deadline.map_or(now + self.timeout, |end| end.min(now + self.timeout));
        if deadline <= now {
            return Err(Withdrawal::Expired);
        }
        let deadline_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .saturating_add(deadline - now)
            .as_millis();
        let session = self.session(connection.0);
        let first_id = ledger.next_id + 1;
        // Serialize every frame before publishing any. An oversized question
        // must not leave a partly announced batch.
        let frames = questions
            .iter()
            .zip(first_id..)
            .map(|((_, question), id)| {
                encode(&NeedsInput {
                    v: VERSION,
                    frame_type: "needs_input",
                    session: &session,
                    id: id.to_string(),
                    server: source,
                    question,
                    actions: question.actions(),
                    deadline: deadline_unix,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut tickets = Vec::with_capacity(questions.len());
        for ((((key, question), id), frame), permit) in questions
            .into_iter()
            .zip(first_id..)
            .zip(frames)
            .zip(permits)
        {
            let (reply, receiver) = oneshot::channel();
            ledger.pending.insert(
                id,
                Pending {
                    expected: question.expected(),
                    reply,
                    deadline,
                    server_stop: server_stop.cloned(),
                },
            );
            ledger.next_id = id;
            tickets.push((
                key,
                Ticket {
                    host: self,
                    id,
                    connection,
                    receiver,
                    deadline,
                },
            ));
            permit.send(Outgoing::Frame(frame));
        }
        Ok(tickets)
    }

    async fn ask(
        &self,
        connection: Connection,
        question: Question,
        token: CancellationToken,
    ) -> AnswerResult {
        let mut tickets = self.open(connection, vec![((), question)], Some(&token), None)?;
        let (_, mut ticket) = tickets.pop().expect("one question");
        let outcome = tokio::select! {
            biased;
            () = token.cancelled() => Withdrawal::ServerCancelled,
            () = tokio::time::sleep_until(ticket.deadline) => Withdrawal::Expired,
            answer = &mut ticket.receiver => return answer.unwrap_or(Err(Withdrawal::Cancelled)),
        };
        self.withdraw(ticket.id, ticket.connection, outcome);
        ticket.receiver.try_recv().unwrap_or(Err(outcome))
    }

    fn submit(
        &self,
        ledger: &mut Ledger,
        session: &str,
        id: u64,
        answer: Answer,
    ) -> Result<(), &'static str> {
        let source = ledger.source.as_ref().ok_or("cross_session")?;
        let connection = source.connection;
        if self.session(connection) != session {
            return Err("cross_session");
        }
        let Some(pending) = ledger.pending.get(&id) else {
            return Err(ledger
                .retired
                .iter()
                .find(|(known, _)| *known == id)
                .map_or("unknown", |(_, code)| *code));
        };
        let reason = if self.lost.is_cancelled() {
            Some(Withdrawal::HostGone)
        } else if source.disconnected() {
            Some(Withdrawal::Disconnected)
        } else if pending
            .server_stop
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
        {
            Some(Withdrawal::ServerCancelled)
        } else if Instant::now() >= pending.deadline {
            Some(Withdrawal::Expired)
        } else {
            None
        };
        if let Some(reason) = reason {
            self.withdraw_locked(ledger, connection, id, reason);
            return Err("stale");
        }
        pending.expected.validate(&answer)?;
        let pending = ledger.pending.remove(&id).expect("pending under lock");
        ledger.retire(id, "duplicate");
        let _ = pending.reply.send(Ok(answer));
        Ok(())
    }

    fn withdraw(&self, id: u64, connection: Connection, reason: Withdrawal) {
        let mut ledger = self.ledger.lock().unwrap();
        self.withdraw_locked(&mut ledger, connection.0, id, reason);
    }

    fn withdraw_locked(&self, ledger: &mut Ledger, connection: u64, id: u64, reason: Withdrawal) {
        if let Some(pending) = ledger.pending.remove(&id) {
            // tower-mcp also cancels the request tokens when its message
            // loop closes, after marking the client disconnected.
            let reason = if reason == Withdrawal::ServerCancelled
                && ledger
                    .source
                    .as_ref()
                    .is_none_or(|source| source.connection != connection || source.disconnected())
            {
                Withdrawal::Disconnected
            } else {
                reason
            };
            ledger.retire(id, "stale");
            // A full queue can drop a withdrawal. The ledger still rejects
            // all further responses for the question.
            let _ = self.send(&json!({
                "v": VERSION, "type": "withdrawn", "session": self.session(connection),
                "id": id.to_string(), "reason": reason.code()
            }));
            let _ = pending.reply.send(Err(reason));
        }
    }

    fn withdraw_all(&self, reason: Withdrawal) {
        self.withdraw_pending(&mut self.ledger.lock().unwrap(), reason);
    }

    fn withdraw_stable_after(&self, after: u64) {
        let mut ledger = self.ledger.lock().unwrap();
        let Some(source) = &ledger.source else { return };
        let connection = source.connection;
        let ids: Vec<_> = ledger
            .pending
            .iter()
            .filter_map(|(&id, pending)| {
                (id > after && pending.server_stop.is_some()).then_some(id)
            })
            .collect();
        for id in ids {
            self.withdraw_locked(&mut ledger, connection, id, Withdrawal::Cancelled);
        }
    }

    fn withdraw_pending(&self, ledger: &mut Ledger, reason: Withdrawal) {
        if let Some(source) = &ledger.source {
            let connection = source.connection;
            let ids = ledger.pending.keys().copied().collect::<Vec<_>>();
            for id in ids {
                self.withdraw_locked(ledger, connection, id, reason);
            }
        }
    }

    fn send(&self, message: &impl Serialize) -> Result<(), Withdrawal> {
        self.outgoing
            .try_send(Outgoing::Frame(encode(message)?))
            .map_err(|_| Withdrawal::Busy)
    }

    async fn flush(&self) {
        let (done, flushed) = oneshot::channel();
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, async {
            if self.outgoing.send(Outgoing::Flush(done)).await.is_ok() {
                let _ = flushed.await;
            }
        })
        .await;
    }
}

struct Ticket<'a> {
    host: &'a Host,
    id: u64,
    connection: Connection,
    receiver: oneshot::Receiver<AnswerResult>,
    deadline: Instant,
}

impl Drop for Ticket<'_> {
    fn drop(&mut self) {
        self.host
            .withdraw(self.id, self.connection, Withdrawal::Cancelled);
    }
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>, Withdrawal> {
    struct Frame(Vec<u8>);
    impl std::io::Write for Frame {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() >= MAX_FRAME - self.0.len() {
                return Err(std::io::ErrorKind::InvalidData.into());
            }
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut frame = Frame(Vec::new());
    serde_json::to_writer(&mut frame, value).map_err(|_| Withdrawal::Busy)?;
    frame.0.push(b'\n');
    Ok(frame.0)
}

#[cfg(any(unix, test))]
fn response_parts(bytes: &[u8]) -> Option<(u64, String, u64, Value)> {
    let Value::Object(mut fields) = serde_json::from_slice(bytes).ok()? else {
        return None;
    };
    if fields.remove("type")?.as_str()? != "response" {
        return None;
    }
    let version = fields.remove("v")?.as_u64()?;
    let Value::String(session) = fields.remove("session")? else {
        return None;
    };
    let id = fields.remove("id")?;
    let id = id.as_str()?;
    let parsed_id = id.parse::<u64>().ok().filter(|n| n.to_string() == id)?;
    let valid_session = session.split_once('-').is_some_and(|(nonce, generation)| {
        nonce.len() == 32
            && nonce.bytes().all(|b| b.is_ascii_hexdigit())
            && generation
                .parse::<u64>()
                .is_ok_and(|n| n.to_string() == generation)
    });
    valid_session.then_some((version, session, parsed_id, Value::Object(fields)))
}

#[cfg(any(unix, test))]
fn receive(host: &Host, bytes: &[u8]) {
    let Some((version, session, id, payload)) = response_parts(bytes) else {
        let _ = host.send(&json!({
            "v": VERSION, "type": "response_status", "status": "rejected", "reason": "malformed"
        }));
        return;
    };
    let answer = if version == u64::from(VERSION) {
        serde_json::from_value(payload).map_err(|_| "malformed")
    } else {
        Err("unsupported_version")
    };
    // Queue the acknowledgement before letting shutdown acquire the ledger
    // lock and flush the socket. The answer can immediately finish a tool,
    // and an already full queue can drop its status.
    let mut ledger = host.ledger.lock().unwrap();
    let result = answer.and_then(|answer| host.submit(&mut ledger, &session, id, answer));
    let mut reply = json!({
        "v": VERSION, "type": "response_status", "session": session,
        "id": id.to_string(), "status": "accepted"
    });
    if let Err(reason) = result {
        reply["status"] = "rejected".into();
        reply["reason"] = reason.into();
    }
    let _ = host.send(&reply);
}
