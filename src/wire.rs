//! Wire tracing: the raw JSON-RPC frames, redacted, plus the last exchange.
//!
//! Half of any "is it the client, the server, or the network?" question is
//! answered by seeing the frames themselves. [`TracingTransport`] wraps any
//! [`ClientTransport`] and reports every frame that crosses it to a [`Wire`],
//! which records the last request/response pair and, when tracing is on,
//! renders the frame for printing.
//!
//! Recording happens whether or not tracing is on, so `last` can reprint an
//! exchange the user did not know they would want. The cost is one JSON parse
//! per frame, which is nothing next to the round trip that produced it.
//!
//! Rendered frames are meant for stderr: `--json` output goes to stdout, and
//! a trace interleaved with it would break whatever is parsing it downstream.
//!
//! Secrets are masked before a frame is stored, so a redacted frame is the
//! only form that exists past this module.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use nu_ansi_term::Style;
use serde_json::Value;
use tower_mcp::client::ClientTransport;
use tower_mcp::error::Result;

use crate::style::{paint, tag};
use crate::timing;

/// Which way a frame went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Client to server.
    Sent,
    /// Server to client.
    Received,
}

impl Direction {
    fn label(self) -> &'static str {
        match self {
            Direction::Sent => "wire ->",
            Direction::Received => "wire <-",
        }
    }
}

/// One frame: the parsed JSON with secrets already masked, how far into the
/// session it crossed the wire, and, for a response, how long its request had
/// been outstanding.
#[derive(Clone, Debug)]
pub struct Frame {
    pub json: Value,
    pub at: Duration,
    pub elapsed: Option<Duration>,
}

/// The frame recorder. One per process in normal use (see [`wire`]); tests
/// build their own so they do not share state.
pub struct Wire {
    trace: AtomicBool,
    started: Instant,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Outstanding request ids and when they were sent, for the elapsed
    /// annotation on the matching response.
    pending: HashMap<String, Instant>,
    last_request: Option<Frame>,
    last_response: Option<Frame>,
}

/// A server that never answers would otherwise grow `pending` without bound.
/// Well past any realistic number of concurrent requests from a REPL.
const PENDING_CAP: usize = 256;

/// Above this, a frame is summarized rather than kept.
///
/// Every frame is parsed and redacted whether or not tracing is on, and the
/// last exchange is held until the next one replaces it, so a server that
/// returns a large resource would otherwise keep several times its size
/// resident for as long as the session lasts. Nobody reads a megabyte of
/// JSON in a terminal either, so the same cap bounds what `--trace` prints.
const MAX_FRAME_BYTES: usize = 1 << 20;

/// Keep what identifies a frame, drop what makes it large.
///
/// The id and method are what `last` and the elapsed-time pairing need, and
/// they are small by construction.
fn summarize(raw_len: usize, json: &Value) -> Value {
    let mut summary = serde_json::Map::new();
    for key in ["jsonrpc", "id", "method"] {
        if let Some(value) = json.get(key) {
            summary.insert(key.to_string(), value.clone());
        }
    }
    summary.insert(
        "mcp-repl/truncated".to_string(),
        Value::String(format!(
            "{raw_len} bytes, over the {MAX_FRAME_BYTES} byte cap; body not retained"
        )),
    );
    Value::Object(summary)
}

impl Wire {
    pub fn new(trace: bool) -> Self {
        Self {
            trace: AtomicBool::new(trace),
            started: Instant::now(),
            state: Mutex::new(State::default()),
        }
    }

    pub fn set_trace(&self, on: bool) {
        self.trace.store(on, Ordering::Relaxed);
    }

    pub fn trace_enabled(&self) -> bool {
        self.trace.load(Ordering::Relaxed)
    }

    /// Record an outgoing frame. Returns the rendered trace block when
    /// tracing is on.
    pub fn sent(&self, raw: &str) -> Option<String> {
        let frame = self.record(Direction::Sent, raw);
        self.trace_enabled()
            .then(|| render(Direction::Sent, &frame))
    }

    /// Record an incoming frame. Returns the rendered trace block when
    /// tracing is on.
    pub fn received(&self, raw: &str) -> Option<String> {
        let frame = self.record(Direction::Received, raw);
        self.trace_enabled()
            .then(|| render(Direction::Received, &frame))
    }

    /// The most recent request and, if it has arrived, its response.
    pub fn last_exchange(&self) -> Option<(Frame, Option<Frame>)> {
        let state = self.state.lock().unwrap();
        let request = state.last_request.clone()?;
        Some((request, state.last_response.clone()))
    }

    fn record(&self, dir: Direction, raw: &str) -> Frame {
        let now = Instant::now();
        let json = redact(&parse(raw));
        let id = frame_id(&json);
        // Parsing a large frame is transient; keeping it is not. Summarize
        // before anything stores or renders it.
        let json = if raw.len() > MAX_FRAME_BYTES {
            summarize(raw.len(), &json)
        } else {
            json
        };
        // A frame carrying a `method` is a request or a notification, whichever
        // side sent it. That is what separates our request from our response to
        // a server-initiated one, and a server's response from its own request.
        let has_method = json.get("method").is_some();

        let mut state = self.state.lock().unwrap();
        let mut elapsed = None;
        if dir == Direction::Received
            && !has_method
            && let Some(id) = &id
        {
            elapsed = state
                .pending
                .remove(id)
                .map(|sent| now.saturating_duration_since(sent));
        }
        let frame = Frame {
            json,
            at: now.saturating_duration_since(self.started),
            elapsed,
        };
        match dir {
            Direction::Sent => {
                if has_method && let Some(id) = id {
                    if state.pending.len() >= PENDING_CAP {
                        state.pending.clear();
                    }
                    state.pending.insert(id, now);
                    // A new request is a new exchange: the previous one stops
                    // being "last" the moment this goes out.
                    state.last_request = Some(frame.clone());
                    state.last_response = None;
                }
            }
            Direction::Received => {
                if !has_method
                    && id.is_some()
                    && state.last_request.as_ref().and_then(|f| frame_id(&f.json)) == id
                {
                    state.last_response = Some(frame.clone());
                }
            }
        }
        frame
    }
}

/// The process-wide recorder. Created by [`init`] at startup; the lazy
/// fallback keeps the accessor total for any path that runs before it.
static WIRE: OnceLock<Wire> = OnceLock::new();

pub fn init(trace: bool) {
    let _ = WIRE.set(Wire::new(trace));
}

pub fn wire() -> &'static Wire {
    WIRE.get_or_init(|| Wire::new(false))
}

/// A frame as it prints: a dim header with direction, session-relative
/// timestamp, and (for a response) the round-trip time, then the pretty JSON.
pub fn render(dir: Direction, frame: &Frame) -> String {
    let mut header = format!(
        "{} {}",
        tag(Style::new().dimmed(), dir.label()),
        paint(
            Style::new().dimmed(),
            &format!("+{:.3}s", frame.at.as_secs_f64())
        )
    );
    if let Some(elapsed) = frame.elapsed {
        header.push(' ');
        header.push_str(&timing(elapsed));
    }
    let body = serde_json::to_string_pretty(&frame.json).unwrap_or_else(|_| frame.json.to_string());
    format!("{header}\n{}", paint(Style::new().dimmed(), &body))
}

/// A frame that is not valid JSON still deserves to be seen: it is exactly
/// the case where the trace is the answer. Scrub its raw shape first: the
/// structured redactor cannot see keys inside a string fallback.
fn parse(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::String(scrub_malformed(raw)))
}

/// The JSON-RPC id as a lookup key. Numbers and strings both appear in the
/// wild, and `null` means there is no correlation to make.
fn frame_id(json: &Value) -> Option<String> {
    match json.get("id")? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Redaction
// ---------------------------------------------------------------------------

const REDACTED: &str = "<redacted>";

/// Keys whose values never print, in normalized form (see [`normalize_key`]).
/// Matching is exact after normalization, so a `taskToken` argument is not
/// caught by `token`.
const SECRET_KEYS: &[&str] = &[
    "authorization",
    "proxyauthorization",
    "wwwauthenticate",
    "bearer",
    "bearertoken",
    "token",
    "accesstoken",
    "refreshtoken",
    "idtoken",
    "sessiontoken",
    "apitoken",
    "authtoken",
    "apikey",
    "xapikey",
    "apisecret",
    "accesskey",
    "accesskeyid",
    "secretaccesskey",
    "privatekey",
    "secret",
    "clientsecret",
    "clientassertion",
    "assertion",
    "password",
    "passwd",
    "passphrase",
    "credential",
    "credentials",
    "cookie",
    "setcookie",
    "signature",
];

/// Lowercase and drop separators, so `X-Api-Key`, `x_api_key`, and `apiKey`
/// all compare equal to the same entry.
fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Names that end like a credential but are not one, so the exact list and
/// the suffix rule below both have to let them through. A tool argument
/// called `taskToken` is an identifier, and blanking it would make a trace
/// harder to read for no gain.
const NOT_SECRETS: &[&str] = &[
    "tasktoken",
    "progresstoken",
    "requesttoken",
    "continuationtoken",
    "pagetoken",
    "nexttoken",
    "publickey",
    "keys",
    "key",
];

/// Endings that mean a credential on their own. `githubToken` and
/// `stripeSecret` are not enumerable, so anything ending this way is masked
/// unless [`NOT_SECRETS`] says otherwise. The asymmetry is deliberate: a
/// masked correlation id costs a little readability in a trace, a leaked
/// token costs the credential, and traces get pasted into issues.
const STRONG_ENDINGS: &[&str] = &["token", "secret", "password", "passphrase", "credential"];

/// `key` is too common a suffix to mask on its own: `sortKey`,
/// `partitionKey`, and `idempotencyKey` are all ordinary data. It counts
/// only next to a qualifier.
const QUALIFIED_ENDINGS: &[&str] = &["key"];

/// Qualifiers that turn `key` into a credential.
const SECRET_QUALIFIERS: &[&str] = &[
    "api",
    "auth",
    "access",
    "private",
    "client",
    "session",
    "signing",
    "encryption",
    "secret",
];

fn is_secret_key(key: &str) -> bool {
    let normalized = normalize_key(key);
    if NOT_SECRETS.contains(&normalized.as_str()) {
        return false;
    }
    if SECRET_KEYS.contains(&normalized.as_str()) {
        return true;
    }
    if STRONG_ENDINGS
        .iter()
        .any(|ending| normalized.ends_with(ending))
    {
        return true;
    }
    QUALIFIED_ENDINGS.iter().any(|ending| {
        normalized.ends_with(ending)
            && SECRET_QUALIFIERS
                .iter()
                .any(|qualifier| normalized.contains(qualifier))
    })
}

/// Substrings that make a name look like it carries a credential.
const CREDENTIAL_SHAPES: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "credential",
    "apikey",
    "privatekey",
    "authorization",
];

/// Whether a field or variable name looks like it carries a credential.
///
/// Deliberately broader than [`is_secret_key`], which drives redaction and
/// matches exactly so an innocent `taskToken` is not blanked out. This one
/// drives warnings, where the costs run the other way: flagging `taskToken`
/// is a shrug, missing `github_token` is the failure. It matches on
/// substrings, so `api_token` and `awsSecretAccessKey` are caught too.
pub(crate) fn looks_like_credential(name: &str) -> bool {
    let normalized = normalize_key(name);
    CREDENTIAL_SHAPES
        .iter()
        .any(|shape| normalized.contains(shape))
}

/// Mask secrets before a frame goes anywhere. Recursive: a token nested in a
/// tool's arguments is as sensitive as one in a header map.
fn redact(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, val)| {
                    if is_secret_key(key) {
                        (key.clone(), Value::String(REDACTED.to_string()))
                    } else {
                        (key.clone(), redact(val))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        Value::String(s) => Value::String(mask_auth_schemes(s)),
        other => other.clone(),
    }
}

/// Scrub credential-shaped fragments without assuming the frame is valid
/// JSON. Each pass is deliberately conservative and linear: recognizable
/// JSON key/value pairs first, then plain HTTP header lines, then auth schemes
/// embedded anywhere in the remaining diagnostic text.
fn scrub_malformed(raw: &str) -> String {
    let keyed = scrub_json_like_secrets(raw);
    let headers = scrub_header_lines(&keyed);
    mask_auth_schemes(&headers)
}

/// Mask values following quoted, credential-shaped JSON keys even when the
/// surrounding object or array is truncated. The scanner understands string
/// escapes and balanced containers only far enough to find the value's end;
/// an unterminated secret value consumes the rest of the malformed frame.
fn scrub_json_like_secrets(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut output = String::with_capacity(raw.len());
    let mut copied = 0usize;
    let mut cursor = 0usize;

    while cursor < bytes.len() {
        if bytes[cursor] != b'"' {
            cursor += 1;
            continue;
        }
        let Some(key_end) = quoted_end(bytes, cursor) else {
            break;
        };
        let mut colon = key_end + 1;
        while colon < bytes.len() && bytes[colon].is_ascii_whitespace() {
            colon += 1;
        }
        if bytes.get(colon) != Some(&b':') {
            cursor = key_end + 1;
            continue;
        }

        let key = serde_json::from_str::<String>(&raw[cursor..=key_end]).ok();
        if !key.as_deref().is_some_and(is_secret_key) {
            cursor = key_end + 1;
            continue;
        }

        let mut value_start = colon + 1;
        while value_start < bytes.len() && bytes[value_start].is_ascii_whitespace() {
            value_start += 1;
        }
        let value_end = malformed_value_end(bytes, value_start);
        if value_end > value_start {
            output.push_str(&raw[copied..value_start]);
            output.push('"');
            output.push_str(REDACTED);
            output.push('"');
            copied = value_end;
            cursor = value_end;
        } else {
            cursor = value_start.max(key_end + 1);
        }
    }

    if copied == 0 {
        raw.to_string()
    } else {
        output.push_str(&raw[copied..]);
        output
    }
}

fn quoted_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut escaped = false;
    for (offset, byte) in bytes[start + 1..].iter().enumerate() {
        if escaped {
            escaped = false;
        } else if *byte == b'\\' {
            escaped = true;
        } else if *byte == b'"' {
            return Some(start + 1 + offset);
        }
    }
    None
}

fn malformed_value_end(bytes: &[u8], start: usize) -> usize {
    let Some(first) = bytes.get(start) else {
        return start;
    };
    match first {
        b'"' => quoted_end(bytes, start).map_or(bytes.len(), |end| end + 1),
        b'{' | b'[' => balanced_end(bytes, start).unwrap_or(bytes.len()),
        _ => {
            let mut end = start;
            while end < bytes.len() && !matches!(bytes[end], b',' | b'}' | b']' | b'\r' | b'\n') {
                end += 1;
            }
            while end > start && bytes[end - 1].is_ascii_whitespace() {
                end -= 1;
            }
            end
        }
    }
}

fn balanced_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut stack = vec![bytes[start]];
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'"' => cursor = quoted_end(bytes, cursor)? + 1,
            b'{' | b'[' => {
                stack.push(bytes[cursor]);
                cursor += 1;
            }
            b'}' if stack.last() == Some(&b'{') => {
                stack.pop();
                cursor += 1;
                if stack.is_empty() {
                    return Some(cursor);
                }
            }
            b']' if stack.last() == Some(&b'[') => {
                stack.pop();
                cursor += 1;
                if stack.is_empty() {
                    return Some(cursor);
                }
            }
            _ => cursor += 1,
        }
    }
    None
}

/// Mask a plain `Header-Name: value` line when the name itself is secret.
/// Line boundaries keep one malformed header from hiding the diagnostics and
/// additional secrets that follow it.
fn scrub_header_lines(raw: &str) -> String {
    let mut output = String::with_capacity(raw.len());
    for segment in raw.split_inclusive('\n') {
        let line_end = segment.trim_end_matches(['\r', '\n']);
        let leading = line_end.len() - line_end.trim_start_matches([' ', '\t']).len();
        let line = &line_end[leading..];
        let Some(colon) = line.find(':') else {
            output.push_str(segment);
            continue;
        };
        let name = line[..colon].trim_end();
        let header_shaped = !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
        if !header_shaped || !is_secret_key(name) {
            output.push_str(segment);
            continue;
        }

        let value_offset = leading + colon + 1;
        let whitespace = segment[value_offset..]
            .bytes()
            .take_while(u8::is_ascii_whitespace)
            .take_while(|byte| !matches!(byte, b'\r' | b'\n'))
            .count();
        let value_start = value_offset + whitespace;
        output.push_str(&segment[..value_start]);
        output.push_str(REDACTED);
        if segment.ends_with("\r\n") {
            output.push_str("\r\n");
        } else if segment.ends_with('\n') {
            output.push('\n');
        }
    }
    output
}

/// A header line echoed inside a string value (`"Authorization: Bearer abc"`
/// in an error message, say) carries a live token where the key-name rule
/// cannot see it.
/// HTTP authentication schemes whose credential follows the scheme name.
/// `Basic` carries `user:password`, and `token` is what several APIs use
/// where others say `Bearer`.
const AUTH_SCHEMES: &[&str] = &["bearer ", "basic ", "digest ", "token "];

/// Mask a credential embedded in a string value.
///
/// A key-based rule cannot catch `"Authorization: Bearer abc"` arriving as
/// one string. Bearer, Basic, and token credentials end at header/token
/// delimiters; Digest parameters consume the rest of their line. Scanning
/// continues afterward so multiple credentials are masked without hiding
/// unrelated diagnostic context.
fn mask_auth_schemes(s: &str) -> String {
    // ASCII-only lowercasing leaves byte offsets aligned with the original.
    let lowered = s.to_ascii_lowercase();
    let mut output = String::with_capacity(s.len());
    let mut copied = 0usize;
    let mut cursor = 0usize;

    while cursor < s.len() {
        let Some((scheme_start, scheme)) = next_auth_scheme(lowered.as_bytes(), cursor) else {
            break;
        };
        let credential_start = scheme_start + scheme.len();
        let credential_end = if scheme == "digest " {
            s[credential_start..]
                .find(['\r', '\n'])
                .map_or(s.len(), |offset| credential_start + offset)
        } else {
            s[credential_start..]
                .find(|character: char| {
                    character.is_ascii_whitespace()
                        || matches!(character, '"' | '\'' | ',' | ';' | '}' | ']' | ')' | '&')
                })
                .map_or(s.len(), |offset| credential_start + offset)
        };
        if credential_end == credential_start {
            cursor = credential_start;
            continue;
        }
        output.push_str(&s[copied..credential_start]);
        output.push_str(REDACTED);
        copied = credential_end;
        cursor = credential_end;
    }

    if copied == 0 {
        s.to_string()
    } else {
        output.push_str(&s[copied..]);
        output
    }
}

fn next_auth_scheme(lowered: &[u8], start: usize) -> Option<(usize, &'static str)> {
    (start..lowered.len()).find_map(|at| {
        AUTH_SCHEMES
            .iter()
            .find(|scheme| lowered[at..].starts_with(scheme.as_bytes()))
            .map(|scheme| (at, *scheme))
    })
}

// ---------------------------------------------------------------------------
// Transport wrapper
// ---------------------------------------------------------------------------

/// Wraps any client transport and reports each frame to a [`Wire`].
///
/// Every method delegates, including `supports_session_recovery`: the wrapper
/// must be invisible to the client's own session handling.
pub struct TracingTransport<T> {
    inner: T,
    wire: &'static Wire,
}

impl<T: ClientTransport> TracingTransport<T> {
    pub fn new(inner: T) -> Self {
        Self::with_wire(inner, wire())
    }

    pub fn with_wire(inner: T, wire: &'static Wire) -> Self {
        Self { inner, wire }
    }
}

#[async_trait]
impl<T: ClientTransport> ClientTransport for TracingTransport<T> {
    async fn send(&mut self, message: &str) -> Result<()> {
        if let Some(block) = self.wire.sent(message) {
            eprintln!("{block}");
        }
        self.inner.send(message).await
    }

    async fn recv(&mut self) -> Result<Option<String>> {
        let message = self.inner.recv().await?;
        if let Some(raw) = &message
            && let Some(block) = self.wire.received(raw)
        {
            eprintln!("{block}");
        }
        Ok(message)
    }

    fn is_connected(&self) -> bool {
        self.inner.is_connected()
    }

    async fn close(&mut self) -> Result<()> {
        self.inner.close().await
    }

    async fn reset_session(&mut self) {
        self.inner.reset_session().await;
    }

    fn supports_session_recovery(&self) -> bool {
        self.inner.supports_session_recovery()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(id: u32, method: &str) -> String {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {}}).to_string()
    }

    fn response(id: u32) -> String {
        serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {"ok": true}}).to_string()
    }

    /// The last exchange is held until the next one replaces it, so a server
    /// that returns a large resource would otherwise keep it resident for the
    /// rest of the session, several times over.
    #[test]
    fn an_oversized_frame_is_summarized_rather_than_kept() {
        let wire = Wire::new(true);
        let body = "x".repeat(4 * MAX_FRAME_BYTES);
        let huge =
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": {"text": body}}).to_string();
        wire.sent(&request(1, "resources/read"));
        wire.received(&huge);

        let (_, response) = wire.last_exchange().unwrap();
        let response = response.expect("the response is still paired with its request");
        // What identifies the frame survives, so `last` still shows which
        // exchange this was.
        assert_eq!(response.json["id"], 1);
        // The body does not.
        assert!(response.json.get("result").is_none(), "{:?}", response.json);
        let note = response.json["mcp-repl/truncated"]
            .as_str()
            .expect("the truncation is explained rather than silent");
        assert!(note.contains(&huge.len().to_string()), "{note}");
        // Cheap proxy for "not retained": the whole rendered frame is now far
        // smaller than the payload it stood in for.
        assert!(
            serde_json::to_string(&response.json).unwrap().len() < 1024,
            "the summary is small"
        );

        let malformed = format!(
            "{{\"password\":\"do-not-retain{}",
            "x".repeat(MAX_FRAME_BYTES)
        );
        let frame = wire.record(Direction::Received, &malformed);
        let rendered = render(Direction::Received, &frame);
        assert!(frame.json.get("mcp-repl/truncated").is_some());
        assert!(!rendered.contains("do-not-retain"), "{rendered}");
        assert!(
            rendered.len() < 1024,
            "malformed oversize frame was retained"
        );
    }

    /// The cap must not touch ordinary traffic.
    #[test]
    fn a_normal_frame_is_kept_whole() {
        let wire = Wire::new(true);
        wire.sent(&request(1, "tools/call"));
        wire.received(&response(1));
        let (_, response) = wire.last_exchange().unwrap();
        assert_eq!(response.unwrap().json["result"]["ok"], true);
    }

    #[test]
    fn a_response_is_paired_with_the_request_it_answers() {
        let wire = Wire::new(true);
        wire.sent(&request(1, "tools/call"));
        wire.received(&response(1));

        let (req, resp) = wire.last_exchange().expect("an exchange was recorded");
        assert_eq!(req.json["method"], "tools/call");
        let resp = resp.expect("the response was paired");
        assert_eq!(resp.json["result"]["ok"], true);
        assert!(
            resp.elapsed.is_some(),
            "a paired response carries its round-trip time"
        );
    }

    #[test]
    fn a_new_request_clears_the_previous_response() {
        let wire = Wire::new(false);
        wire.sent(&request(1, "tools/list"));
        wire.received(&response(1));
        wire.sent(&request(2, "tools/call"));

        let (req, resp) = wire.last_exchange().unwrap();
        assert_eq!(req.json["method"], "tools/call");
        assert!(resp.is_none(), "the new request has not been answered yet");
    }

    #[test]
    fn notifications_are_not_exchanges() {
        let wire = Wire::new(false);
        wire.sent(
            &serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
                .to_string(),
        );
        assert!(wire.last_exchange().is_none());
    }

    #[test]
    fn a_server_initiated_request_does_not_answer_ours() {
        let wire = Wire::new(false);
        wire.sent(&request(1, "tools/call"));
        // The server asks us something mid-call (sampling, elicitation). It
        // carries an id, but it is not our response.
        wire.received(&request(7, "sampling/createMessage"));

        let (_, resp) = wire.last_exchange().unwrap();
        assert!(resp.is_none());
    }

    #[test]
    fn a_mismatched_response_is_not_the_last_response() {
        let wire = Wire::new(false);
        wire.sent(&request(1, "tools/list"));
        wire.sent(&request(2, "tools/call"));
        // Answers the older request, which is no longer the tracked exchange.
        wire.received(&response(1));

        let (req, resp) = wire.last_exchange().unwrap();
        assert_eq!(req.json["id"], 2);
        assert!(resp.is_none());
    }

    #[test]
    fn recording_happens_with_tracing_off_but_nothing_renders() {
        let wire = Wire::new(false);
        assert!(wire.sent(&request(1, "tools/list")).is_none());
        assert!(wire.received(&response(1)).is_none());
        assert!(
            wire.last_exchange().is_some(),
            "`last` works without --trace"
        );

        wire.set_trace(true);
        assert!(wire.sent(&request(2, "tools/list")).is_some());
    }

    #[test]
    fn a_rendered_frame_shows_direction_timestamp_and_elapsed() {
        let wire = Wire::new(true);
        let sent = wire.sent(&request(1, "tools/call")).unwrap();
        assert!(sent.contains("wire ->"), "{sent}");
        assert!(sent.contains("+0."), "a session-relative timestamp: {sent}");
        assert!(sent.contains("tools/call"), "{sent}");
        assert!(!sent.contains("elapsed"));

        let received = wire.received(&response(1)).unwrap();
        assert!(received.contains("wire <-"), "{received}");
        assert!(
            received.contains("ms]") || received.contains("s]"),
            "a response carries its round-trip time: {received}"
        );
    }

    #[test]
    fn an_unparseable_frame_still_traces() {
        let wire = Wire::new(true);
        let rendered = wire.received("<html>502 Bad Gateway</html>").unwrap();
        assert!(rendered.contains("502 Bad Gateway"), "{rendered}");
    }

    #[test]
    fn malformed_objects_and_arrays_scrub_secret_keys_but_keep_context() {
        for raw in [
            r#"{"params":{"password":"hunter2","taskToken":"visible"}"#,
            r#"[{"api_token":"ghp_one"},{"clientSecret":"stripe_two"},{"note":"keep me"}"#,
        ] {
            let scrubbed = parse(raw);
            let scrubbed = scrubbed.as_str().expect("malformed frames stay strings");
            for secret in ["hunter2", "ghp_one", "stripe_two"] {
                assert!(!scrubbed.contains(secret), "{secret} leaked: {scrubbed}");
            }
            assert!(scrubbed.contains(REDACTED), "{scrubbed}");
            if raw.contains("taskToken") {
                assert!(scrubbed.contains("\"taskToken\":\"visible\""), "{scrubbed}");
            }
        }

        let object =
            scrub_malformed("{\"password\":\"line one\nline two\",\"note\":\"still visible\"");
        assert!(!object.contains("line one"), "{object}");
        assert!(!object.contains("line two"), "{object}");
        assert!(object.contains("still visible"), "{object}");
    }

    #[test]
    fn malformed_header_lines_scrub_multiple_secrets_and_preserve_other_lines() {
        let raw = "Authorization: Bearer first\nX-Api-Key: second\r\nCookie: third\nContent-Type: application/json\n<broken>";
        let scrubbed = scrub_malformed(raw);
        for secret in ["first", "second", "third"] {
            assert!(!scrubbed.contains(secret), "{secret} leaked: {scrubbed}");
        }
        for context in [
            "Authorization:",
            "X-Api-Key:",
            "Cookie:",
            "Content-Type: application/json",
            "<broken>",
        ] {
            assert!(
                scrubbed.contains(context),
                "missing {context:?}: {scrubbed}"
            );
        }
        assert_eq!(scrubbed.matches(REDACTED).count(), 3, "{scrubbed}");
    }

    #[test]
    fn ordinary_malformed_text_is_unchanged() {
        let raw = "<html>502 Bad Gateway</html>\nupstream reset {";
        assert_eq!(scrub_malformed(raw), raw);
    }

    #[test]
    fn secrets_are_masked_by_key_name() {
        let frame = redact(&serde_json::json!({
            "params": {
                "headers": {"Authorization": "Bearer sk-live-123", "X-Api-Key": "k1"},
                "arguments": {"apiKey": "k2", "password": "hunter2", "nested": [{"token": "t"}]},
            }
        }));
        let rendered = frame.to_string();
        for secret in ["sk-live-123", "k1", "k2", "hunter2", "\"t\""] {
            assert!(!rendered.contains(secret), "{secret} leaked: {rendered}");
        }
        assert_eq!(frame["params"]["headers"]["Authorization"], REDACTED);
        assert_eq!(frame["params"]["arguments"]["nested"][0]["token"], REDACTED);
    }

    #[test]
    fn a_bearer_token_inside_a_string_is_masked() {
        let frame = redact(&serde_json::json!({
            "error": {"message": "rejected Authorization: Bearer sk-live-123"}
        }));
        let message = frame["error"]["message"].as_str().unwrap();
        assert!(!message.contains("sk-live-123"), "{message}");
        assert!(message.starts_with("rejected Authorization: Bearer "));
    }

    #[test]
    fn ordinary_values_are_left_alone() {
        let original = serde_json::json!({
            "params": {"name": "add", "arguments": {"a": 2, "b": 3, "taskToken": "visible"}},
            "flags": [true, null, 1.5],
        });
        assert_eq!(redact(&original), original);
    }

    #[test]
    fn credential_headers_and_fields_are_masked() {
        // Every name here has shown up in a real MCP server's traffic. A
        // trace is printed and often pasted into an issue, so a miss is a
        // leak.
        for key in [
            "Cookie",
            "Set-Cookie",
            "api_token",
            "auth_token",
            "x-api-token",
            "accessKey",
            "secretAccessKey",
            "AWS_SECRET_ACCESS_KEY",
            "private_key",
            "client_assertion",
            "signature",
            "githubToken",
            "session_key",
            "signingSecret",
            "WWW-Authenticate",
        ] {
            let frame = serde_json::json!({ key.to_string(): "s3cret" });
            let redacted = redact(&frame);
            assert_eq!(redacted[key], REDACTED, "{key} leaked: {redacted}");
        }
    }

    #[test]
    fn identifiers_that_merely_end_like_secrets_stay_readable() {
        // Over-redaction makes a trace useless in its own way: these are
        // correlation ids and pagination cursors, not credentials.
        for key in [
            "taskToken",
            "progressToken",
            "nextToken",
            "continuationToken",
            "pageToken",
            "publicKey",
            "sortKey",
            "partitionKey",
            "idempotencyKey",
            "name",
            "uri",
        ] {
            let frame = serde_json::json!({ key.to_string(): "visible" });
            assert_eq!(redact(&frame)[key], "visible", "{key} was over-redacted");
        }
    }

    #[test]
    fn every_auth_scheme_is_masked_inside_a_string() {
        // A header arriving as one string cannot be caught by key, so the
        // scheme is found in the value.
        for (value, kept) in [
            ("Bearer abc.def.ghi", "Bearer "),
            ("bearer abc", "bearer "),
            ("Basic dXNlcjpwYXNz", "Basic "),
            ("Digest username=\"u\", response=\"r\"", "Digest "),
            ("token ghp_xxx", "token "),
            ("Authorization: Bearer abc", "Authorization: Bearer "),
        ] {
            let masked = mask_auth_schemes(value);
            assert_eq!(masked, format!("{kept}{REDACTED}"), "{value:?}");
        }
    }

    #[test]
    fn multiple_auth_schemes_are_masked_without_hiding_context() {
        let masked = mask_auth_schemes("Bearer aaa and Basic bbb");
        assert!(!masked.contains("aaa"), "{masked}");
        assert!(!masked.contains("bbb"), "{masked}");
        assert_eq!(masked, "Bearer <redacted> and Basic <redacted>");
    }

    #[test]
    fn a_value_without_a_scheme_is_untouched() {
        assert_eq!(mask_auth_schemes("just a sentence"), "just a sentence");
        // The word alone, with no credential after it, is not a match.
        assert_eq!(mask_auth_schemes("bearer"), "bearer");
    }

    // -- the transport wrapper ------------------------------------------------

    struct FakeTransport {
        sent: Vec<String>,
        incoming: Vec<String>,
    }

    #[async_trait]
    impl ClientTransport for FakeTransport {
        async fn send(&mut self, message: &str) -> Result<()> {
            self.sent.push(message.to_string());
            Ok(())
        }

        async fn recv(&mut self) -> Result<Option<String>> {
            Ok(if self.incoming.is_empty() {
                None
            } else {
                Some(self.incoming.remove(0))
            })
        }

        fn is_connected(&self) -> bool {
            true
        }

        async fn close(&mut self) -> Result<()> {
            Ok(())
        }

        fn supports_session_recovery(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn the_wrapper_records_both_directions_and_delegates() {
        // Leaked rather than global, so this test cannot collide with another.
        let wire: &'static Wire = Box::leak(Box::new(Wire::new(false)));
        let mut transport = TracingTransport::with_wire(
            FakeTransport {
                sent: Vec::new(),
                incoming: vec![response(1)],
            },
            wire,
        );

        transport.send(&request(1, "tools/call")).await.unwrap();
        let received = transport.recv().await.unwrap();

        assert_eq!(received.as_deref(), Some(response(1).as_str()));
        assert_eq!(
            transport.inner.sent.len(),
            1,
            "the frame reached the inner transport"
        );
        assert!(
            transport.supports_session_recovery(),
            "the wrapper must not change how the client handles sessions"
        );
        let (req, resp) = wire.last_exchange().unwrap();
        assert_eq!(req.json["method"], "tools/call");
        assert!(resp.is_some());
    }
}
