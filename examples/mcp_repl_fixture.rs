//! Deterministic process fixture for `mcp-repl`'s black-box tests.
//!
//! This is intentionally an example target excluded from the published
//! package (see `exclude` in Cargo.toml), rather than another binary in the
//! published `mcp-repl` crate. With no
//! arguments it serves stdio. `--http` binds an ephemeral localhost port and
//! writes the resulting URL to `MCP_REPL_FIXTURE_READY_FILE`.

use std::path::PathBuf;
use std::time::Duration;

use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use schemars::JsonSchema;
use serde::Deserialize;
use tower_mcp::{
    CallToolResult, HttpTransport, McpRouter, PromptBuilder, ProtocolSupport, ResourceBuilder,
    StdioTransport, TaskSupportMode, ToolBuilder,
    extract::{Context, RawArgs},
    protocol::{LogLevel, LoggingMessageParams},
};

#[derive(Debug, Deserialize, JsonSchema)]
struct AddInput {
    a: i64,
    b: i64,
}

fn fixture_router() -> McpRouter {
    McpRouter::new()
        .server_info("mcp-repl-fixture", "1.0.0")
        .with_tasks()
        .tool(
            ToolBuilder::new("add")
                .description("Add two integers")
                .handler(|input: AddInput| async move {
                    Ok(CallToolResult::text((input.a + input.b).to_string()))
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
                        })
                        .to_string(),
                    ))
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
        )
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

async fn observe_subscription(request: Request, next: Next) -> Response {
    if request
        .headers()
        .get("mcp-method")
        .and_then(|value| value.to_str().ok())
        == Some("subscriptions/listen")
    {
        write_marker("MCP_REPL_FIXTURE_SUBSCRIPTION_FILE", b"seen");
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
) -> Result<(), tower_mcp::BoxError> {
    use std::io::{BufRead, Write};

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let request: serde_json::Value = serde_json::from_str(&line)?;
        let method = request["method"].as_str().unwrap_or_default();
        // A notification carries no id and takes no response.
        let Some(id) = request.get("id").filter(|id| !id.is_null()).cloned() else {
            continue;
        };
        let response = match method {
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

#[tokio::main]
async fn main() -> Result<(), tower_mcp::BoxError> {
    eprintln!("mcp-repl fixture ready");
    // Not a router: this mode speaks JSON-RPC by hand so it can reject the
    // methods it never advertised, the way a server from another SDK does.
    if std::env::args().any(|arg| arg == "--tools-only") {
        serve_raw_tools_only(
            std::env::args().any(|arg| arg == "--failing-list"),
            std::env::args().any(|arg| arg == "--downgrade-protocol"),
            std::env::args().any(|arg| arg == "--strict-cursor"),
        )?;
        write_marker("MCP_REPL_FIXTURE_EXIT_FILE", b"clean");
        return Ok(());
    }
    let router = fixture_router();
    if std::env::args().any(|arg| arg == "--http") {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}/", listener.local_addr()?);
        write_marker("MCP_REPL_FIXTURE_READY_FILE", &url);
        let app = HttpTransport::new(router)
            .protocol_support(protocol_support())
            .disable_origin_validation()
            .into_router()
            .layer(axum::middleware::from_fn(observe_subscription));
        axum::serve(listener, app).await?;
    } else {
        StdioTransport::new(router)
            .protocol_support(protocol_support())
            .run()
            .await?;
        write_marker("MCP_REPL_FIXTURE_EXIT_FILE", b"clean");
    }
    Ok(())
}
