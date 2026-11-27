//! Deterministic process fixture for `mcp-repl`'s black-box tests.
//!
//! This is intentionally an example target excluded from the published
//! package (see `exclude` in Cargo.toml), rather than another binary in the
//! published `mcp-repl` crate. With no
//! arguments it serves stdio. `--http` binds an ephemeral localhost port and
//! writes the resulting URL to `MCP_REPL_FIXTURE_READY_FILE`.

#[path = "fixtures/plugin_provider.rs"]
mod plugin_provider;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, oneshot};
use tower_mcp::{
    BidirectionalStdioTransport, CallToolResult, HttpTransport, McpRouter, PromptBuilder,
    ProtocolSupport, RequestContext, ResourceBuilder, StdioTransport, TaskSupportMode, ToolBuilder,
    extract::{Context, RawArgs},
    protocol::{
        CreateMessageParams, ElicitAction, ElicitFieldValue, ElicitFormParams, ElicitFormSchema,
        ElicitMode, ElicitRequestParams, ElicitResult, ElicitUrlParams, GetPromptResult,
        InputRequest, InputRequiredResult, InputResponse, LogLevel, LoggingMessageParams,
        RequestOutcome, SamplingMessage,
    },
};

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

static LISTEN_SUBSCRIPTIONS: AtomicUsize = AtomicUsize::new(0);

/// The sensitive form answer the host-input e2e case sends.
const SEEDED_ANSWER: &str = "bridge-private-answer";

fn fixture_router() -> McpRouter {
    let router = McpRouter::new()
        .server_info("mcp-repl-fixture", "1.0.0")
        .with_tasks()
        .tool(
            ToolBuilder::new("plugin")
                .description("A server tool named plugin")
                .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                    Ok(CallToolResult::text("server tool: plugin"))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("add")
                .description("Add two integers")
                .handler(|input: AddInput| async move {
                    Ok(CallToolResult::text((input.a + input.b).to_string()))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("inspect_args")
                .description("Return literal arguments for generated CLI boundary tests")
                .input_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"},
                        "enabled": {"type": "boolean"},
                        "nested": {"type": "object"}
                    }
                }))
                .extractor_handler((), |_ctx: Context, RawArgs(args): RawArgs| async move {
                    Ok(CallToolResult::text(args.to_string()))
                })
                .build(),
        )
        // Deliberately collide with a REPL built-in. The black-box suite uses
        // this to prove that neither namespace can silently win.
        .tool(
            ToolBuilder::new("wait")
                .description("Wait in the fixture server")
                .task_support(TaskSupportMode::Optional)
                .handler(|input: AddInput| async move {
                    Ok(CallToolResult::text(format!(
                        "server wait: {}",
                        input.a + input.b
                    )))
                })
                .build(),
        )
        // The namespace words themselves remain usable as server tool names
        // through `tool tool` and `tool builtin`.
        .tool(
            ToolBuilder::new("tool")
                .description("A server tool named tool")
                .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                    Ok(CallToolResult::text("server tool: tool"))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("builtin")
                .description("A server tool named builtin")
                .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                    Ok(CallToolResult::text("server tool: builtin"))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("slow_add")
                .description("Add two integers in a final-protocol task")
                .task_support(TaskSupportMode::Optional)
                .handler(|input: AddInput| async move {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    Ok(CallToolResult::text((input.a + input.b).to_string()))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("announce")
                .description("Emit a deterministic server log notification")
                .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
                    ctx.send_log(LoggingMessageParams::new(
                        LogLevel::Info,
                        serde_json::json!("fixture announcement"),
                    ));
                    // Let the notification frame reach the client before the
                    // one-shot process receives its terminal tool response.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Ok(CallToolResult::text("announced"))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("fail")
                .description("Return a deterministic MCP tool error")
                .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                    Ok(CallToolResult::error("fixture tool failure"))
                })
                .build(),
        )
        // Task-capable and always fails, so `wait` has something whose
        // terminal state must reach the process exit status.
        .tool(
            ToolBuilder::new("fail_slowly")
                .description("Fail from inside a task")
                .task_support(TaskSupportMode::Optional)
                .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    Err(tower_mcp::Error::Tool(tower_mcp::error::ToolError {
                        tool: Some("fail_slowly".to_string()),
                        message: "fixture task failure".to_string(),
                        source: None,
                    }))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("process_info")
                .description("Report deterministic process environment for import tests")
                .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                    let cwd = std::env::current_dir()
                        .expect("fixture current directory")
                        .to_string_lossy()
                        .into_owned();
                    let imported = std::env::var("MCP_REPL_IMPORTED_VALUE").ok();
                    // The REPL strips this before spawning: it is an HTTP
                    // credential, and a stdio child has no use for it.
                    let bearer = std::env::var("MCP_BEARER").ok();
                    Ok(CallToolResult::text(
                        serde_json::json!({
                            "cwd": cwd,
                            "imported": imported,
                            "bearer": bearer,
                            "argv": std::env::args().skip(1).collect::<Vec<_>>(),
                        })
                        .to_string(),
                    ))
                })
                .build(),
        )
        // Deterministic results of a requested size, for `--max-output`.
        .tool(
            ToolBuilder::new("bulk")
                .description("Return a deterministic result of about the requested size")
                .input_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "kind": {
                            "type": "string",
                            "enum": ["text", "structured", "media", "empty", "error"]
                        },
                        "bytes": {"type": "integer", "minimum": 0},
                        // Shares a name with the list commands' `--brief`, so
                        // a generated `bulk --brief` must reach the tool.
                        "brief": {"type": "boolean"}
                    },
                    "required": ["kind"]
                }))
                .extractor_handler((), |_ctx: Context, RawArgs(args): RawArgs| async move {
                    let kind = args["kind"].as_str().unwrap_or("text").to_string();
                    let bytes = args["bytes"].as_u64().unwrap_or(0) as usize;
                    let mut result = bulk_result(&kind, bytes);
                    if args["brief"] == true
                        && let Some(meta) = result.meta.as_mut()
                    {
                        meta["fixture/brief"] = serde_json::json!(true);
                    }
                    Ok(result)
                })
                .build(),
        )
        .resource(
            ResourceBuilder::new("fixture://guide")
                .name("Fixture Guide")
                .description("A deterministic resource for process tests")
                .mime_type("text/plain")
                .text("fixture resource body"),
        )
        .prompt(
            PromptBuilder::new("greet")
                .description("Generate a deterministic greeting request")
                .required_arg("name", "The person to greet")
                .handler(|args| async move {
                    let name = args.get("name").map(String::as_str).unwrap_or("World");
                    Ok(tower_mcp::GetPromptResult::user_message(format!(
                        "Please greet {name} warmly."
                    )))
                })
                .build(),
        );
    if std::env::var_os("MCP_REPL_FIXTURE_HOST_INPUT").is_some() {
        host_tools(router)
    } else {
        router
    }
}

fn host_tools(router: McpRouter) -> McpRouter {
    router
        .tool(
            ToolBuilder::new("host_form")
                .description("Ask the client for a form answer")
                .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
                    let answer = ctx.elicit_form(host_form()).await?;
                    // A peer can echo a sensitive answer into either diagnostic
                    // channel. Echo the e2e case's seeded value when it arrives.
                    if matches!(
                        answer.content.as_ref().and_then(|values| values.get("apiKey")),
                        Some(ElicitFieldValue::String(value)) if value == SEEDED_ANSWER
                    ) {
                        eprintln!("{SEEDED_ANSWER}");
                        ctx.send_log(LoggingMessageParams::new(
                            LogLevel::Info,
                            serde_json::json!(SEEDED_ANSWER),
                        ));
                    }
                    Ok(form_result(&answer))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("host_url")
                .description("Ask the client to confirm a URL")
                .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
                    let answer = ctx
                        .elicit_url(ElicitUrlParams {
                            mode: Some(ElicitMode::Url),
                            elicitation_id: "fixture-url".into(),
                            message: "Confirm the fixture URL flow".into(),
                            url: "https://example.com/confirm?token=fixture-url-token".into(),
                            meta: None,
                        })
                        .await?;
                    Ok(CallToolResult::text(format!(
                        "url {}",
                        action_name(answer.action)
                    )))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("host_sample")
                .description("Ask the client for a sampling answer")
                .extractor_handler((), |ctx: Context, RawArgs(_): RawArgs| async move {
                    let reply = ctx.sample(host_sample()).await?;
                    Ok(CallToolResult::text(format!("sampled by {}", reply.model)))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("host_rounds")
                .description("Run a state-only round and a multi-input round")
                .mrtr_handler(|ctx: RequestContext, _args: serde_json::Value| async move {
                    match ctx.request_state() {
                        None => Ok(RequestOutcome::input_required(
                            InputRequiredResult::new().with_request_state("fixture-state-only"),
                        )),
                        Some("fixture-state-only") if ctx.input_responses().is_none() => {
                            let inputs = [
                                (
                                    "a-form".into(),
                                    InputRequest::Elicit(ElicitRequestParams::Form(host_form())),
                                ),
                                (
                                    "b-sample".into(),
                                    InputRequest::CreateMessage(host_sample()),
                                ),
                            ]
                            .into_iter()
                            .collect();
                            Ok(RequestOutcome::input_required(
                                InputRequiredResult::with_requests(inputs)
                                    .with_request_state("fixture-answer-round"),
                            ))
                        }
                        Some("fixture-answer-round") => {
                            let responses = ctx.input_responses().expect("answer round responses");
                            let Some(InputResponse::Elicit(form)) = responses.get("a-form") else {
                                return Ok(RequestOutcome::Complete(CallToolResult::error(
                                    "missing form",
                                )));
                            };
                            let Some(InputResponse::CreateMessage(sample)) =
                                responses.get("b-sample")
                            else {
                                return Ok(RequestOutcome::Complete(CallToolResult::error(
                                    "missing sample",
                                )));
                            };
                            if let Some(text) = sample.first_text() {
                                eprintln!("{text}");
                            }
                            Ok(RequestOutcome::Complete(CallToolResult::text(format!(
                                "final {} / {}",
                                action_name(form.action),
                                sample.model,
                            ))))
                        }
                        _ => Ok(RequestOutcome::Complete(CallToolResult::error(
                            "unexpected fixture state",
                        ))),
                    }
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("host_round_limit")
                .description("Request another round forever")
                .mrtr_handler(
                    |_ctx: RequestContext, _args: serde_json::Value| async move {
                        increment_marker("MCP_REPL_FIXTURE_HOST_ROUNDS");
                        Ok(RequestOutcome::input_required(
                            InputRequiredResult::new().with_request_state("fixture-loop"),
                        ))
                    },
                )
                .build(),
        )
}

fn host_form() -> ElicitFormParams {
    ElicitFormParams {
        mode: None,
        message: "Provide a synthetic value".into(),
        requested_schema: ElicitFormSchema::new().string_field("apiKey", None, true),
        meta: None,
    }
}

fn host_sample() -> CreateMessageParams {
    CreateMessageParams::new(vec![SamplingMessage::user("Fixture sample")], 24)
}

fn action_name(action: ElicitAction) -> &'static str {
    match action {
        ElicitAction::Accept => "accept",
        ElicitAction::Decline => "decline",
        ElicitAction::Cancel => "cancel",
        _ => "unknown",
    }
}

fn has_fixture_form_value(answer: &ElicitResult) -> bool {
    matches!(answer.content.as_ref().and_then(|values| values.get("apiKey")),
        Some(ElicitFieldValue::String(value)) if value == SEEDED_ANSWER)
}

fn form_result(answer: &ElicitResult) -> CallToolResult {
    if answer.action == ElicitAction::Accept && !has_fixture_form_value(answer) {
        return CallToolResult::error("fixture did not receive the expected form value");
    }
    CallToolResult::text(format!("form {}", action_name(answer.action)))
}

#[derive(Deserialize, JsonSchema)]
struct WorkerInput {
    text: String,
    fail: bool,
}

#[derive(Deserialize, JsonSchema)]
struct WorkerHandle {
    handle: String,
}

struct Worker {
    handle: String,
    pending: Option<(oneshot::Sender<()>, tokio::task::JoinHandle<Value>)>,
    terminal: Option<Value>,
}

fn with_session_driver_tools(router: McpRouter) -> McpRouter {
    let worker = Arc::new(Mutex::new(None::<Worker>));
    let start = worker.clone();
    let wait = worker.clone();
    router
        .tool(
            ToolBuilder::new("worker_start")
                .handler(move |input: WorkerInput| {
                    let start = start.clone();
                    async move {
                        let mut worker = start.lock().await;
                        if worker.is_some() {
                            return Ok(CallToolResult::error("worker already exists"));
                        }
                        let handle = format!("worker:{} | $owner", std::process::id());
                        let (release, ready) = oneshot::channel();
                        let job = tokio::spawn(async move {
                            ready.await.expect("wait releases this fixture worker");
                            if input.fail {
                                json!({"state": "failed", "error": "fixture application failure"})
                            } else {
                                json!({"state": "completed", "text": input.text})
                            }
                        });
                        *worker = Some(Worker {
                            handle: handle.clone(),
                            pending: Some((release, job)),
                            terminal: None,
                        });
                        Ok(CallToolResult::json(
                            json!({"handle": handle, "state": "working"}),
                        ))
                    }
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("worker_wait")
                .handler(move |input: WorkerHandle| {
                    let wait = wait.clone();
                    async move {
                        let mut worker = wait.lock().await;
                        let Some(worker) = worker.as_mut().filter(|job| job.handle == input.handle)
                        else {
                            return Ok(CallToolResult::error("unknown worker in this process"));
                        };
                        if let Some((release, job)) = worker.pending.take() {
                            release.send(()).expect("fixture worker is still waiting");
                            worker.terminal = Some(job.await.expect("fixture worker settled"));
                        }
                        Ok(CallToolResult::json(worker.terminal.clone().unwrap()))
                    }
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("worker_read")
                .handler(move |input: WorkerHandle| {
                    let worker = worker.clone();
                    async move {
                        let worker = worker.lock().await;
                        let terminal = worker
                            .as_ref()
                            .filter(|job| job.handle == input.handle)
                            .and_then(|job| job.terminal.as_ref())
                            .filter(|result| result["state"] == "completed");
                        match terminal {
                            Some(result) => Ok(CallToolResult::json(result.clone())),
                            None => Ok(CallToolResult::error("no settled result for this worker")),
                        }
                    }
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("worker_disconnect")
                .extractor_handler((), |RawArgs(_): RawArgs| async move {
                    increment_marker("MCP_REPL_FIXTURE_DRIVER_MUTATIONS_FILE");
                    std::process::exit(23)
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("worker_stall")
                .extractor_handler((), |RawArgs(_): RawArgs| async move {
                    write_marker("MCP_REPL_FIXTURE_DRIVER_STALL_FILE", b"started");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    Ok(CallToolResult::text("late fixture result"))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("task_pending")
                .task_support(TaskSupportMode::Required)
                .extractor_handler((), |RawArgs(_): RawArgs| async move {
                    std::future::pending::<Result<CallToolResult, tower_mcp::Error>>().await
                })
                .build(),
        )
}

/// Text of at most `bytes` UTF-8 bytes from a fixed pattern. The pattern has
/// a two-byte character, so a preview cut on a byte boundary would show up.
fn bulk_text(bytes: usize) -> String {
    const LINE: &str = "0123456789 bulk é\n";
    let mut text = String::with_capacity(bytes);
    for c in LINE.chars().cycle() {
        if text.len() + c.len_utf8() > bytes {
            break;
        }
        text.push(c);
    }
    text
}

/// The `bulk` tool's result. Every kind carries an extra `_meta` key, so the
/// saved file can be checked for metadata the REPL does not interpret.
fn bulk_result(kind: &str, bytes: usize) -> CallToolResult {
    use tower_mcp::protocol::Content;

    let mut result = match kind {
        "structured" => {
            let mut items = Vec::new();
            let mut size = 0;
            while size < bytes {
                let item = serde_json::json!({"id": items.len(), "label": "bulk item"});
                size += item.to_string().len() + 1;
                items.push(item);
            }
            let total = items.len();
            CallToolResult {
                content: vec![Content::text(format!("{total} items"))],
                is_error: false,
                structured_content: Some(serde_json::json!({"items": items, "total": total})),
                meta: None,
            }
        }
        "media" => CallToolResult {
            content: vec![
                Content::text("media result"),
                Content::Image {
                    // All zero bytes in base64: valid, and the same every run.
                    data: "A".repeat(bytes / 4 * 4),
                    mime_type: "image/png".to_string(),
                    annotations: None,
                    meta: None,
                },
                Content::Audio {
                    data: "AAAA".to_string(),
                    mime_type: "audio/wav".to_string(),
                    annotations: None,
                    meta: None,
                },
            ],
            is_error: false,
            structured_content: None,
            meta: None,
        },
        "empty" => CallToolResult {
            content: Vec::new(),
            is_error: false,
            structured_content: None,
            meta: None,
        },
        "error" => CallToolResult::error(bulk_text(bytes)),
        _ => CallToolResult::text(bulk_text(bytes)),
    };
    result.meta = Some(serde_json::json!({"fixture/extra": "kept"}));
    result
}

fn protocol_support() -> ProtocolSupport {
    ProtocolSupport::try_new(["2025-11-25", "2026-07-28"]).expect("fixture protocols are compiled")
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn write_marker(name: &str, contents: impl AsRef<[u8]>) {
    if let Some(path) = env_path(name) {
        // Publish markers atomically so the test never observes the empty
        // file between create/truncate and the content write on a busy host.
        let pending = path.with_extension("pending");
        std::fs::write(&pending, contents).expect("write pending fixture marker");
        std::fs::rename(pending, path).expect("publish fixture marker");
    }
}

fn increment_marker(name: &str) {
    let Some(path) = env_path(name) else {
        return;
    };
    let count = std::fs::read_to_string(&path)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0)
        + 1;
    write_marker(name, count.to_string());
}

/// Every `tools/call` attempt that arrived, logged at ingress before any
/// injected fault. One raw tool name per line.
const TOOL_CALL_LOG: &str = "MCP_REPL_FIXTURE_TOOL_CALL_LOG_FILE";
/// Every `tools/call` request that passed the 503 injector, logged just
/// inside it. Over stdio there is no injector, so both logs receive the same
/// calls.
const TOOL_CALL_PAST_503_LOG: &str = "MCP_REPL_FIXTURE_TOOL_CALL_PAST_503_LOG_FILE";

/// Append the raw tool name of every `tools/call` request in one JSON-RPC
/// frame (a message or a batch) to the log named by `log_env`, one name per
/// line. These are the server's own records of what arrived, so a test can
/// prove that a call the client refused was never sent, whatever the client
/// displayed.
fn log_tool_calls(log_env: &str, frame: &[u8]) {
    use std::io::Write as _;
    let Some(path) = env_path(log_env) else {
        return;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(frame) else {
        return;
    };
    let messages = match value {
        serde_json::Value::Array(messages) => messages,
        message => vec![message],
    };
    for message in messages {
        if message.get("method").and_then(serde_json::Value::as_str) != Some("tools/call") {
            continue;
        }
        let name = message
            .pointer("/params/name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open fixture tool-call log");
        writeln!(log, "{name}").expect("append fixture tool-call log");
    }
}

/// Read an HTTP request body once to log its tool calls to `log_env`, then
/// hand the same bytes on. Only active when that log is requested.
async fn log_http_body(log_env: &str, request: Request, next: Next) -> Response {
    if env_path(log_env).is_none() {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, 16 * 1024 * 1024).await else {
        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
    };
    log_tool_calls(log_env, &bytes);
    next.run(Request::from_parts(parts, axum::body::Body::from(bytes)))
        .await
}

/// The ingress log: outside the 503 injector, so a rejected attempt counts.
async fn log_received_tool_calls(request: Request, next: Next) -> Response {
    log_http_body(TOOL_CALL_LOG, request, next).await
}

/// Just inside the 503 injector: requests it let through.
async fn log_tool_calls_past_503(request: Request, next: Next) -> Response {
    log_http_body(TOOL_CALL_PAST_503_LOG, request, next).await
}

/// Serve stdio through a tap that logs each inbound frame's tool calls
/// before the router sees it. Stdin EOF still ends the session.
async fn serve_logged_stdio(
    mut transport: tower_mcp::StdioTransport,
) -> Result<(), tower_mcp::BoxError> {
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
    let (mut tap, reader) = tokio::io::duplex(1 << 20);
    tokio::spawn(async move {
        let mut frames = tokio::io::BufReader::new(tokio::io::stdin()).split(b'\n');
        while let Ok(Some(frame)) = frames.next_segment().await {
            log_tool_calls(TOOL_CALL_LOG, &frame);
            log_tool_calls(TOOL_CALL_PAST_503_LOG, &frame);
            if tap.write_all(&frame).await.is_err() || tap.write_all(b"\n").await.is_err() {
                break;
            }
        }
    });
    transport
        .run_with_streams(reader, tokio::io::stdout())
        .await?;
    Ok(())
}

fn claim_marker(name: &str) -> bool {
    let Some(path) = env_path(name) else {
        return false;
    };
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(error) => panic!("claim {}: {error}", path.display()),
    }
}

/// Write a server-side trace to `MCP_REPL_FIXTURE_TRACE_FILE`, when set.
///
/// The tests discard the fixture's stdout and stderr (stdio mode speaks the
/// protocol on stdout), so a CI failure otherwise carries only the client's
/// view. The file never receives ANSI codes and stdout is never touched.
fn init_trace_file() {
    let Some(path) = env_path("MCP_REPL_FIXTURE_TRACE_FILE") else {
        return;
    };
    let file = std::fs::File::create(&path).expect("create fixture trace file");
    tracing_subscriber::fmt()
        .with_writer(std::sync::Mutex::new(file))
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "tower_mcp=debug,mcp_repl_fixture=debug",
        ))
        .init();
}

/// Log each HTTP request on arrival and again when its response is ready.
///
/// Outermost layer, so the timing covers the fault-injection and bearer
/// layers as well as the MCP transport. The arrival line is what answers
/// whether a request the client timed out on ever reached the fixture.
async fn trace_requests(request: Request, next: Next) -> Response {
    let method = request
        .headers()
        .get("mcp-method")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let session = request.headers().contains_key("mcp-session-id");
    let http_method = request.method().clone();
    tracing::debug!(%http_method, mcp_method = %method, session, "request arrived");
    let started = std::time::Instant::now();
    let response = next.run(request).await;
    tracing::debug!(
        %http_method,
        mcp_method = %method,
        status = response.status().as_u16(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "response ready"
    );
    response
}

async fn observe_subscription(request: Request, next: Next) -> Response {
    let method = request
        .headers()
        .get("mcp-method")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    match method.as_deref() {
        Some("subscriptions/listen") => {
            let count = LISTEN_SUBSCRIPTIONS.fetch_add(1, Ordering::Relaxed) + 1;
            write_marker("MCP_REPL_FIXTURE_SUBSCRIPTION_FILE", count.to_string());
        }
        Some("resources/subscribe") => {
            increment_marker("MCP_REPL_FIXTURE_RESOURCE_SUBSCRIPTIONS_FILE");
        }
        Some("tools/call") => {
            increment_marker("MCP_REPL_FIXTURE_TOOL_CALLS_FILE");
            let fail_once = claim_marker("MCP_REPL_FIXTURE_HTTP_503_ONCE_FILE");
            let fail_always = std::env::var_os("MCP_REPL_FIXTURE_HTTP_503_ALWAYS").is_some();
            if fail_once || fail_always {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
        }
        _ => {}
    }
    next.run(request).await
}

/// Require an exact bearer only in the authentication fixture process.
///
/// The expected value is never rendered: the test process supplies it to the
/// server side solely so a successful MCP handshake proves that the client
/// transported the descriptor's bytes in the Authorization header.
async fn require_bearer(request: Request, next: Next) -> Response {
    let Some(expected) = std::env::var_os("MCP_REPL_FIXTURE_EXPECT_BEARER") else {
        return next.run(request).await;
    };
    let expected = format!("Bearer {}", expected.to_string_lossy());
    let accepted = request
        .headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == expected);
    if !accepted {
        return (StatusCode::UNAUTHORIZED, "missing or invalid bearer").into_response();
    }
    next.run(request).await
}

/// A hand-rolled stdio server that serves tools and rejects everything else.
///
/// Deliberately not built on `McpRouter`. A tower-mcp router answers
/// `prompts/list` with an empty list even when it serves no prompts, so it
/// cannot reproduce what other SDKs do, and a fixture built from the same
/// types as the client under test agrees with our assumptions by
/// construction. GitMCP declares exactly `{"tools":{"listChanged":true}}` and
/// answers `prompts/list` with `-32601 Method not found`, which is correct of
/// it; the REPL used to report that as a failure, so connecting to a healthy
/// server opened with two warnings about nothing.
///
/// Speaking JSON-RPC directly is the point: this is the shape of a server we
/// did not write.
fn serve_raw_tools_only(
    failing_list: bool,
    downgrade: bool,
    strict_cursor: bool,
    ignore_initialize: bool,
    cancel_first_call: bool,
) -> Result<(), tower_mcp::BoxError> {
    use std::io::{BufRead, Write};

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut hold_next_call = cancel_first_call;
    let mut pending_call = None;
    let host_trigger = env_path("MCP_REPL_FIXTURE_HOST_TRIGGER");
    let disconnect = std::env::args().any(|arg| arg == "--disconnect-host-input");
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: serde_json::Value = serde_json::from_str(&line)?;
        let method = request["method"].as_str().unwrap_or_default();
        if ignore_initialize && method == "initialize" {
            continue;
        }
        if method == "notifications/cancelled"
            && pending_call.as_ref() == request["params"].get("requestId")
        {
            write_marker(
                "MCP_REPL_FIXTURE_CANCEL_FILE",
                request["params"]["requestId"].to_string(),
            );
            pending_call = None;
        }
        // A notification carries no id and takes no response.
        let Some(id) = request.get("id").filter(|id| !id.is_null()).cloned() else {
            continue;
        };
        if id == "fixture-callback" && method.is_empty() && host_trigger.is_some() {
            write_marker("MCP_REPL_FIXTURE_UNEXPECTED_HOST_REPLY", b"received");
            continue;
        }
        let response = match method {
            "tools/call" if host_trigger.is_some() => {
                let callback = serde_json::json!({
                    "jsonrpc": "2.0", "id": "fixture-callback", "method": "elicitation/create",
                    "params": host_form(),
                });
                writeln!(stdout, "{callback}")?;
                stdout.flush()?;
                let trigger = host_trigger.as_ref().unwrap();
                let end = std::time::Instant::now() + Duration::from_secs(20);
                while !trigger.exists() {
                    if std::time::Instant::now() >= end {
                        return Err("fixture timed out waiting for host trigger".into());
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                if disconnect {
                    break;
                }
                let cancellation = serde_json::json!({
                    "jsonrpc": "2.0", "method": "notifications/cancelled",
                    "params": {"requestId": "fixture-callback"},
                });
                writeln!(stdout, "{cancellation}")?;
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": {"content":[{"type":"text","text":"server cancelled"}]},
                })
            }
            // Keep the first call pending until the client sends cancellation.
            // This real child deliberately installs no SIGINT handler: terminal
            // interrupts must reach it through MCP, not its process group.
            "tools/call" if hold_next_call => {
                hold_next_call = false;
                pending_call = Some(id.clone());
                write_marker(
                    "MCP_REPL_FIXTURE_CALL_FILE",
                    serde_json::json!({ "id": id, "pid": std::process::id() }).to_string(),
                );
                continue;
            }
            "tools/call" if cancel_first_call => {
                let args = &request["params"]["arguments"];
                let sum =
                    args["a"].as_i64().unwrap_or_default() + args["b"].as_i64().unwrap_or_default();
                write_marker(
                    "MCP_REPL_FIXTURE_FOLLOWUP_FILE",
                    std::process::id().to_string(),
                );
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "content": [{ "type": "text", "text": sum.to_string() }] },
                })
            }
            "initialize" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    // A server is allowed to answer with a version other than
                    // the one asked for, and real ones do: the client either
                    // speaks it or gives up, but must not carry on believing
                    // it got what it requested.
                    "protocolVersion": if downgrade {
                        serde_json::json!("2025-06-18")
                    } else {
                        request["params"]["protocolVersion"].clone()
                    },
                    // The strict mode also serves resources, since the
                    // REPL now asks only for what a server declares and the
                    // null cursor rode on a resource listing.
                    "capabilities": if strict_cursor {
                        serde_json::json!({
                            "tools": { "listChanged": true },
                            "resources": { "listChanged": false, "subscribe": false },
                        })
                    } else {
                        serde_json::json!({ "tools": { "listChanged": true } })
                    },
                    "serverInfo": { "name": "raw-tools-only", "version": "1.0.0" },
                },
            }),
            // Context7 and the Hugging Face server both type `cursor` as
            // `string | undefined` and reject an explicit null, which is
            // correct of them: absent and null are not the same thing. A
            // client that sends `"cursor": null` for a first page cannot
            // list anything from either.
            //
            // Applies to every listing, because which one carries the bug is
            // not the point and has already moved once: the null was on
            // `resources/templates/list` and never on `tools/list`.
            _ if strict_cursor
                && method.ends_with("/list")
                && request["params"].get("cursor") == Some(&serde_json::Value::Null) =>
            {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32603,
                        "message": "Invalid input: expected string, received null at params.cursor",
                    },
                })
            }
            // Only in the strict-cursor mode, and only so the client has a
            // reason to send the listing that carried the null.
            "resources/list" | "resources/templates/list" if strict_cursor => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": { "resources": [], "resourceTemplates": [] },
            }),
            // Declared the capability, then fails to serve it. That is the
            // shape the REPL must not round down to "this server has no
            // tools": the listing was never read, which is a different fact
            // from an empty one.
            "tools/list" if failing_list => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32603, "message": "tool index unavailable" },
            }),
            "tools/list" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": "add",
                        "description": "Add two integers",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "a": { "type": "integer" },
                                "b": { "type": "integer" },
                            },
                            "required": ["a", "b"],
                        },
                    }],
                },
            }),
            // Everything the capabilities did not promise, including the
            // listings the REPL must now know better than to ask for.
            _ => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": "Method not found" },
            }),
        };
        writeln!(stdout, "{response}")?;
        stdout.flush()?;
    }
    Ok(())
}

/// Script stable callbacks on a detached task. The ping round trip puts the
/// background callbacks between task creation and the next foreground call.
/// The first task poll lets a separate script answer the task during `wait`.
fn serve_piped_prompts() -> Result<(), tower_mcp::BoxError> {
    use std::io::{BufRead, Write};

    fn send_frame(stdout: &mut impl Write, frame: Value) -> std::io::Result<()> {
        writeln!(stdout, "{frame}")?;
        stdout.flush()
    }

    fn ask_form(stdout: &mut impl Write, id: &str) -> std::io::Result<()> {
        send_frame(
            stdout,
            json!({
                "jsonrpc": "2.0", "id": id, "method": "elicitation/create", "params": host_form(),
            }),
        )
    }

    let mut stdout = std::io::stdout().lock();
    let mut task_started = false;
    let mut task_asked = false;
    let mut task_result = None;
    let mut pending = Value::Null;
    let working_task = json!({
        "taskId": "background",
        "status": "working",
        "createdAt": "2026-01-01T00:00:00Z",
        "lastUpdatedAt": "2026-01-01T00:00:00Z",
        "ttl": null,
        "pollInterval": 50,
    });
    for line in std::io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line?)?;
        let id = &request["id"];
        let result = match (request["method"].as_str(), id.as_str()) {
            (Some("initialize"), _) => json!({
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": {"tools": {}, "tasks": {"requests": {"tools": {"call": {}}}}},
                "serverInfo": {"name": "piped-prompts", "version": "1.0.0"},
            }),
            (Some("tools/list"), _) => json!({"tools": [{
                "name": "ask",
                "inputSchema": {"type": "object"},
                "execution": {"taskSupport": "optional"},
            }]}),
            (Some("tools/call"), _) if request["params"].get("task").is_some() => {
                task_started = true;
                json!({"task": working_task})
            }
            (Some("ping"), _) if task_started => {
                pending = id.clone();
                ask_form(&mut stdout, "background-form")?;
                continue;
            }
            (None, Some("background-form")) => {
                if request["result"]["action"] != "decline" {
                    return Err("expected the background form during ping to be declined".into());
                }
                send_frame(
                    &mut stdout,
                    json!({
                        "jsonrpc": "2.0", "id": "background-sample", "method": "sampling/createMessage",
                        "params": host_sample(),
                    }),
                )?;
                continue;
            }
            (Some("tools/call"), _) => {
                pending = id.clone();
                ask_form(&mut stdout, "foreground-form")?;
                continue;
            }
            (Some("tasks/get"), _)
                if task_started && request["params"]["taskId"] == "background" =>
            {
                if !task_asked {
                    task_asked = true;
                    ask_form(&mut stdout, "task-form")?;
                }
                let mut task = working_task.clone();
                if let Some(result) = &task_result {
                    task["status"] = json!("completed");
                    task["result"] = serde_json::to_value(result)?;
                }
                task
            }
            (None, Some("task-form")) => {
                let answer = serde_json::from_value(request["result"].clone())?;
                task_result = Some(form_result(&answer));
                continue;
            }
            (None, Some("background-sample")) => {
                if request["error"]["code"] != -32007 {
                    return Err(
                        "expected the background sampling request during ping to be declined"
                            .into(),
                    );
                }
                json!({})
            }
            (None, Some("foreground-form")) => {
                let answer = serde_json::from_value(request["result"].clone())?;
                serde_json::to_value(form_result(&answer))?
            }
            _ if id.is_null() => continue,
            _ => return Err(format!("unexpected piped prompts fixture request: {request}").into()),
        };
        let id = if request["method"].is_null() {
            &pending
        } else {
            id
        };
        send_frame(
            &mut stdout,
            json!({"jsonrpc": "2.0", "id": id, "result": result}),
        )?;
    }
    Ok(())
}

fn with_piped_final_prompt(router: McpRouter) -> McpRouter {
    router.prompt(
        PromptBuilder::new("ask_prompt")
            .mrtr_handler(|ctx: RequestContext, _args| async move {
                let Some(responses) = ctx.input_responses() else {
                    let request = InputRequest::Elicit(ElicitRequestParams::Form(host_form()));
                    return Ok(RequestOutcome::input_required(
                        InputRequiredResult::with_requests(
                            [("form".into(), request)].into_iter().collect(),
                        ),
                    ));
                };
                let accepted = matches!(
                    responses.get("form"),
                    Some(InputResponse::Elicit(answer))
                        if answer.action == ElicitAction::Accept && has_fixture_form_value(answer)
                );
                let message = if accepted {
                    "final form accept"
                } else {
                    "final form unexpected answer"
                };
                Ok(RequestOutcome::Complete(GetPromptResult::user_message(
                    message,
                )))
            })
            .build(),
    )
}

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    if std::env::var_os("MCP_REPL_PLUGIN_FIXTURE").is_some() {
        let executable = std::env::current_exe()?;
        match executable.file_stem().and_then(|stem| stem.to_str()) {
            Some("claude") => return plugin_provider::run(true),
            Some("codex") => return plugin_provider::run(false),
            Some("plugin") => assert_eq!(
                std::env::args().skip(1).collect::<Vec<_>>(),
                ["--help", "literal argument", "--"]
            ),
            _ => {}
        }
    }
    eprintln!("mcp-repl fixture ready");
    increment_marker("MCP_REPL_FIXTURE_DRIVER_SPAWNS_FILE");
    write_marker(
        "MCP_REPL_FIXTURE_DRIVER_PID_FILE",
        std::process::id().to_string(),
    );
    init_trace_file();
    if std::env::args().any(|arg| arg == "--piped-prompts") {
        return serve_piped_prompts();
    }
    // Not a router: this mode speaks JSON-RPC by hand so it can reject the
    // methods it never advertised, the way a server from another SDK does.
    if std::env::args().any(|arg| arg == "--tools-only") {
        serve_raw_tools_only(
            std::env::args().any(|arg| arg == "--failing-list"),
            std::env::args().any(|arg| arg == "--downgrade-protocol"),
            std::env::args().any(|arg| arg == "--strict-cursor"),
            std::env::args().any(|arg| arg == "--ignore-initialize"),
            std::env::args().any(|arg| arg == "--cancel-first-call"),
        )?;
        write_marker("MCP_REPL_FIXTURE_EXIT_FILE", b"clean");
        return Ok(());
    }
    let mut router = fixture_router();
    if std::env::args().any(|arg| arg == "--piped-prompts-final") {
        router = with_piped_final_prompt(router);
    }
    if std::env::args().any(|arg| arg == "--session-driver") {
        router = with_session_driver_tools(router);
    }
    if std::env::args().any(|arg| arg == "--http") {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/", listener.local_addr()?);
        write_marker("MCP_REPL_FIXTURE_READY_FILE", &url);
        let app = HttpTransport::new(router)
            .protocol_support(protocol_support())
            .disable_origin_validation()
            .into_router()
            // The last layer added runs first. The received-calls log sits
            // outside `observe_subscription`, which injects 503s, and the
            // past-503 log just inside it.
            .layer(axum::middleware::from_fn(require_bearer))
            .layer(axum::middleware::from_fn(log_tool_calls_past_503))
            .layer(axum::middleware::from_fn(observe_subscription))
            .layer(axum::middleware::from_fn(log_received_tool_calls))
            .layer(axum::middleware::from_fn(trace_requests));
        axum::serve(listener, app).await?;
    } else if std::env::var_os("MCP_REPL_FIXTURE_HOST_INPUT").is_some() {
        BidirectionalStdioTransport::new(router)
            .protocol_support(protocol_support())
            .run()
            .await?;
        write_marker("MCP_REPL_FIXTURE_EXIT_FILE", b"clean");
    } else {
        let mut transport = StdioTransport::new(router).protocol_support(protocol_support());
        if env_path(TOOL_CALL_LOG).is_some() || env_path(TOOL_CALL_PAST_503_LOG).is_some() {
            serve_logged_stdio(transport).await?;
        } else {
            transport.run().await?;
        }
        write_marker("MCP_REPL_FIXTURE_EXIT_FILE", b"clean");
    }
    Ok(())
}
