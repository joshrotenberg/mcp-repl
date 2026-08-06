//! mcp-repl: an interactive MCP client REPL.
//!
//! Connects to any MCP server and turns the server's surface into the
//! command set: every tool becomes a top-level command with schema-coerced
//! `key=value` arguments, prompts and resources get built-ins, tab
//! completion is powered by the server itself where the protocol allows
//! (`completion/complete`), and `list_changed` notifications refresh the
//! command table live mid-session.
//!
//! Usage:
//!
//! ```text
//! # Spawn a stdio server as a child process:
//! cargo run -p mcp-repl -- cargo run --example getting_started
//!
//! # Connect to a streamable HTTP server:
//! cargo run -p mcp-repl -- --http http://127.0.0.1:3001/mcp
//!
//! # Authorize once, then reuse a secure OAuth profile:
//! cargo run -p mcp-repl -- --login work --http https://mcp.example.com/mcp
//! cargo run -p mcp-repl -- --oauth work --http https://mcp.example.com/mcp
//!
//! # Connect to a named profile from ~/.config/mcp-repl/config.toml:
//! cargo run -p mcp-repl -- --server cratesio
//! ```
//!
//! Inside the REPL, `help` lists the built-ins and the server's tools,
//! `alias <name>=<expansion>` gives a frequent command a short name, kept in
//! the same config file as the server profiles, and `bench <tool>` reports the
//! latency distribution over repeated calls.
//! A trailing `&` runs a tool task-augmented (SEP-2663): the call returns a
//! task id immediately; `jobs`, `task <id>`, `wait <id>`, and `cancel <id>`
//! manage it.
//!
//! # Reusable seams
//!
//! The published package keeps its application in this library and its binary
//! target delegates to [`run_cli`]. [`config`], [`import_config`], and
//! [`oauth_profile`] are the deliberately reusable connection-facing seams.
//! Editor, rendering, and command-dispatch modules remain private application
//! details so consumers do not accidentally depend on terminal behavior.

mod alias;
mod bench;
mod command;
pub mod config;
mod editor;
mod elicit;
mod exit_status;
mod find;
pub mod import_config;
mod import_trust;
mod jobs;
pub mod lifecycle;
pub mod oauth_profile;
mod output;
mod sampling;
mod schema_contract;
mod secure_file;
mod session;
mod style;
mod subscribe;
mod surface_subscription;
mod vars;
mod wire;

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use clap::{Parser, ValueEnum};
use nu_ansi_term::{Color, Style};

use tokio::io::{AsyncBufReadExt, BufReader};
use tower_mcp::client::{
    ChannelTransport, HttpClientConfig, HttpClientTransport, McpClient, McpClientBuilder,
    NotificationHandler, OAuthAuthorizationFlow, OAuthAuthorizationStart, OAuthClientError,
    OAuthScopeEscalationConfig, StdioClientTransport,
};
use tower_mcp::protocol::{
    Content, DiscoverResult, Implementation, InitializeResult, LogLevel, PromptDefinition,
    ResourceDefinition, ResourceTemplateDefinition, ServerCapabilities, SubscriptionFilter,
    TaskObject, ToolDefinition,
};
use tower_mcp::{ProtocolSupport, ProtocolSupportError};

use alias::Aliases;
use elicit::ReplClientHandler;
use exit_status::ExitStatus;
use jobs::Jobs;
use output::AsyncOutput;
use session::{Connector, Session, is_not_initialized, is_session_lost};
use style::{json_pretty, paint, sanitize, tag, task_status_style};
use wire::{TracingTransport, wire};

/// Lifecycle selected for this REPL connection.
///
/// The final implementation is compiled into the binary, but stable remains
/// the runtime default so upgrading mcp-repl never silently changes a server's
/// handshake. `final` is accepted as a convenient alias for the dated value.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
enum ProtocolMode {
    #[default]
    Stable,
    #[value(name = "2026-07-28", alias = "final")]
    Final,
}

impl ProtocolMode {
    fn support(self) -> Result<ProtocolSupport, ProtocolSupportError> {
        match self {
            Self::Stable => Ok(ProtocolSupport::stable()),
            Self::Final => ProtocolSupport::try_new(["2026-07-28"]),
        }
    }
}

#[derive(Parser)]
#[command(
    name = "mcp-repl",
    version,
    about = "Interactive MCP client REPL",
    long_about = "\
An interactive terminal REPL for any MCP server. The server's surface is the \
command set: every tool becomes a top-level command with schema-coerced \
key=value arguments, prompts and resources get built-ins, tab completion is \
powered by the server itself where the protocol allows, and the command table \
refreshes when the server's surface changes.

Connects over stdio or streamable HTTP, reads the JSON config files other MCP \
clients use, and keeps named profiles of its own.",
    trailing_var_arg = true,
    // Kept narrow enough that `man` can indent it without reflowing the
    // columns into each other.
    after_help = "\
EXAMPLES:
  mcp-repl --demo                        the bundled demo server
  mcp-repl --http https://example/mcp    a streamable HTTP server
  mcp-repl -- ./my-server --stdio        spawn a stdio server
  mcp-repl .mcp.json:local               an entry from a client config
  mcp-repl --server prod                 a saved profile

  mcp-repl --demo -e 'echo message=hi'   run one command and exit
  mcp-repl --demo --json -e tools | jq   NDJSON for scripts

Inside the REPL, `help` lists the built-ins and `help <command>` explains one."
)]
struct Args {
    /// Protocol lifecycle to use. `stable` uses initialize/initialized;
    /// `2026-07-28` (alias: `final`) uses the sessionless discover lifecycle.
    #[arg(long, value_enum, default_value = "stable")]
    protocol: ProtocolMode,

    /// Connect to a streamable HTTP server at this URL instead of spawning
    /// a stdio child process.
    #[arg(long)]
    http: Option<String>,

    /// Serve the bundled demo router in-process (no external server needed).
    #[arg(long, conflicts_with_all = ["http", "command", "server"])]
    demo: bool,

    /// Connect using a native profile name, or import `PATH.json:ENTRY` from a
    /// standard MCP JSON config. Either selector also works as a lone
    /// positional.
    #[arg(long, value_name = "NAME")]
    server: Option<String>,

    /// Read server profiles from this file instead of
    /// `$XDG_CONFIG_HOME/mcp-repl/config.toml` (or `~/.config/mcp-repl/config.toml`).
    #[arg(long, value_name = "PATH")]
    config: Option<String>,

    /// Print the configured server profiles and exit.
    #[arg(long)]
    list_servers: bool,

    /// Print the servers other MCP clients have configured, as selectors you
    /// can pass straight back, and exit.
    ///
    /// Reads the well-known config files and describes them. It connects to
    /// nothing and runs nothing.
    #[arg(long)]
    scan: bool,

    /// Print a shell completion script for mcp-repl's own flags and exit.
    ///
    /// The REPL completes a server's surface from the moment it connects;
    /// this is the other half, for the invocation that gets you there.
    #[arg(long, value_name = "SHELL")]
    completions: Option<clap_complete::Shell>,

    /// Print this program's man page, in roff, and exit.
    ///
    /// Packagers redirect it: `mcp-repl --man > mcp-repl.1`.
    #[arg(long)]
    man: bool,

    /// When to emit ANSI colors (auto detects tty and NO_COLOR).
    #[arg(long, value_enum, default_value = "auto")]
    color: style::ColorMode,

    /// Bearer token for an authenticated `--http` server (sets
    /// `Authorization: Bearer <token>`). Falls back to the `MCP_BEARER`
    /// environment variable, which is preferred since a command-line token is
    /// visible in `ps` and shell history.
    #[arg(long)]
    bearer: Option<String>,

    /// Extra header for an authenticated `--http` server, as `Name: Value`
    /// (repeatable). Split on the first colon.
    #[arg(long = "header", value_name = "NAME: VALUE")]
    headers: Vec<String>,

    /// Use a named OAuth credential profile for this HTTP connection.
    #[arg(long, value_name = "NAME")]
    oauth: Option<String>,

    /// Authorize and securely save a named OAuth profile, then exit without
    /// opening an MCP session. Supply --http for a new profile.
    #[arg(long, value_name = "NAME", conflicts_with = "logout")]
    login: Option<String>,

    /// Remove a named OAuth profile and its credentials, then exit without
    /// opening an MCP session.
    #[arg(long, value_name = "NAME", conflicts_with = "login")]
    logout: Option<String>,

    /// Initial OAuth scope to request during --login (repeatable). Existing
    /// profile scopes are retained when this is omitted.
    #[arg(long = "oauth-scope", value_name = "SCOPE")]
    oauth_scopes: Vec<String>,

    /// HTTPS Client ID Metadata Document URL to try before Dynamic Client
    /// Registration during --login.
    #[arg(long, value_name = "URL")]
    oauth_client_id_metadata_document: Option<String>,

    /// Exact authorization-server issuer to select when discovery advertises
    /// more than one during --login.
    #[arg(long, value_name = "ISSUER")]
    oauth_authorization_server: Option<String>,

    /// Print the authorization URL instead of launching a browser. The
    /// loopback callback is still used.
    #[arg(long)]
    no_browser: bool,

    /// Run a command and exit instead of starting the interactive prompt.
    /// Repeatable; commands run in order against the same session, including
    /// after a failure. The final status is the most severe command outcome.
    /// Combine with --http/--demo or a stdio child.
    #[arg(short = 'e', long = "exec", value_name = "COMMAND")]
    exec: Vec<String>,

    /// In --exec mode, emit one compact JSON value per command (NDJSON), for
    /// piping to tools like jq.
    #[arg(long)]
    json: bool,

    /// In human --exec mode, still print the startup banner and surface
    /// listing. JSON stdout is always machine-only.
    #[arg(long)]
    verbose: bool,

    /// Validate matching tools and prompts before invocation using this saved
    /// schema snapshot (repeatable).
    #[arg(long = "schema-contract", value_name = "PATH")]
    schema_contracts: Vec<std::path::PathBuf>,

    /// Enforcement used by --schema-contract snapshots.
    #[arg(long, value_enum, default_value = "compatible")]
    schema_mode: schema_contract::ValidationMode,

    /// How to answer a server's `sampling/createMessage` request: `prompt`
    /// shows it and reads the assistant message on stdin, `canned` answers
    /// with a fixed placeholder, `decline` refuses. Defaults to `prompt`
    /// interactively and `decline` under --exec.
    #[arg(long, value_enum, value_name = "STRATEGY")]
    sampling: Option<sampling::SamplingMode>,

    /// How to answer a server's `elicitation/create` request: `prompt` shows
    /// what the server is asking and reads the answers on stdin, `decline`
    /// refuses every request. Defaults to `prompt` interactively and
    /// `decline` under --exec.
    #[arg(long, value_enum, value_name = "STRATEGY")]
    elicitation: Option<elicit::ElicitationMode>,

    /// Spawn a server named by an imported client config without asking
    /// first. The imported file chooses the program, its arguments, and
    /// which of your environment variables it receives, so approval is
    /// interactive by default and remembered per entry.
    #[arg(long)]
    trust_import: bool,

    /// Do not persist command history to ~/.mcp-repl_history.
    #[arg(long)]
    no_history: bool,

    /// Do not transparently re-establish an interrupted HTTP connection
    /// (restart, OOM, or a 502/503 from the edge in front of it).
    /// Connection-loss errors surface as-is instead.
    #[arg(long)]
    no_reconnect: bool,

    /// Print every JSON-RPC frame sent and received, to stderr. Equivalent to
    /// starting with `wire on`; toggle it mid-session with `wire on|off`.
    #[arg(long)]
    trace: bool,

    /// Give up on a request that has produced no response after this many
    /// seconds, and report a transport error. Applies to tool calls, `read`,
    /// `prompt`, `bench` calls, and surface fetches, over both transports.
    /// `0` waits indefinitely. `wait <id>` is exempt, since outliving the
    /// call is what a task is for; give it its own `--timeout`.
    /// Defaults to `[repl] request_timeout` in the config file, or 120.
    #[arg(long, value_name = "SECONDS")]
    timeout: Option<u64>,

    /// Command (and arguments) of a stdio MCP server to spawn.
    command: Vec<String>,
}

/// Set in `--exec` mode by `--json`: render raw JSON instead of pretty output.
static JSON_OUTPUT: AtomicBool = AtomicBool::new(false);

fn json_output() -> bool {
    JSON_OUTPUT.load(Ordering::Relaxed)
}

/// Whether any user command has been dispatched. Only `last` cares: before
/// the first one, the newest recorded frame is the REPL's own startup fetch.
static COMMAND_RAN: AtomicBool = AtomicBool::new(false);

/// What `--timeout` uses when neither the flag nor the config says.
pub(crate) const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 120;

/// `--timeout` in seconds; 0 means wait indefinitely.
static REQUEST_TIMEOUT_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn request_timeout() -> Option<Duration> {
    match REQUEST_TIMEOUT_SECS.load(Ordering::Relaxed) {
        0 => None,
        secs => Some(Duration::from_secs(secs)),
    }
}

/// Run a request under the configured deadline.
///
/// A stdio child that accepts a request and never answers would otherwise
/// hang the REPL forever: unlike HTTP, the framework applies no deadline
/// there. The timeout is reported as a transport error so it classifies and
/// renders like any other failure to get an answer.
async fn with_deadline<T, Fut>(fut: Fut) -> Result<T, tower_mcp::Error>
where
    Fut: Future<Output = Result<T, tower_mcp::Error>>,
{
    let Some(limit) = request_timeout() else {
        return fut.await;
    };
    match tokio::time::timeout(limit, fut).await {
        Ok(result) => result,
        Err(_) => Err(tower_mcp::Error::Transport(format!(
            "no response after {}s (--timeout); the request may still be running on the server",
            limit.as_secs()
        ))),
    }
}

fn note_error(status: ExitStatus) {
    exit_status::record(status);
}

fn automatic_task_updates(one_shot: bool, json: bool) -> bool {
    !one_shot && !json
}

/// Emit one compact JSON value. Repeated `--exec` commands are therefore
/// newline-delimited JSON (NDJSON), with one independently parseable line per
/// command.
fn print_json(value: &serde_json::Value) {
    println!("{value}");
}

/// A stable one-line JSON error object for `--json` mode.
fn error_json(status: ExitStatus, message: &str) -> serde_json::Value {
    serde_json::json!({
        "error": message,
        "kind": status.label(),
        "exitStatus": status.code(),
    })
}

/// Report a failed command.
///
/// Every failure goes through here, so the prefix, the stream, and the
/// recorded status agree no matter which command produced it.
///
/// Under `--json` the envelope is the command's one value and belongs on
/// stdout, which is the documented NDJSON contract. In human mode stdout is
/// the data stream: `mcp-repl -e get_thing > out.txt` should capture the
/// result or nothing, never the words explaining why there is no result.
fn report_error(status: ExitStatus, message: &str) {
    report_error_with_hint(status, message, None);
}

/// A failure plus a suggestion of what to type instead.
fn report_error_with_hint(status: ExitStatus, message: &str, hint: Option<&str>) {
    note_error(status);
    if json_output() {
        let mut value = error_json(status, message);
        if let Some(hint) = hint {
            value["didYouMean"] = serde_json::json!(hint);
        }
        print_json(&value);
        return;
    }
    // Server-originated text reaches here; keep its bytes from
    // reprogramming the terminal.
    let mut line = format!("{}: {}", style::error_prefix(), sanitize(message));
    if let Some(hint) = hint {
        line.push_str(&format!(
            "; did you mean `{}`?",
            paint(Style::new().fg(Color::Green), &sanitize(hint))
        ));
    }
    eprintln!("{line}");
}

fn report_mcp_error(error: &tower_mcp::Error) {
    report_error(
        ExitStatus::from_mcp_error(error),
        collapse_repeated_label(&error.to_string()),
    );
}

/// Build the log subscriber once the CLI's own settings are known.
fn init_tracing(args: &Args) {
    // These records go to stderr, so `auto` follows stderr rather than
    // stdout: piping results to a file should not strip color from messages
    // still headed for a terminal, and redirecting stderr should.
    let ansi = match args.color {
        style::ColorMode::Always => true,
        style::ColorMode::Never => false,
        style::ColorMode::Auto => {
            std::env::var_os("NO_COLOR").is_none()
                && std::io::IsTerminal::is_terminal(&std::io::stderr())
        }
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(ansi)
        .with_env_filter(
            // `tower_mcp::client` narrates failures this REPL already reports
            // in its own words, and warns about responses to requests the
            // REPL cancelled deliberately. Both arrive mid-session at a
            // prompt that owns its rendering, and neither tells the operator
            // anything they are not already being told. `RUST_LOG` replaces
            // this wholesale, so nothing is unreachable.
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,tower_mcp::client=off".into()),
        )
        .init();
}

/// Drop a label the message already carries.
///
/// Every layer that wraps a failure prepends its own label to a string that
/// usually starts with one, so an unreachable server arrives as
/// `Transport error: Transport error: Transport error: HTTP request failed:
/// ...`. Only the innermost sentence is information, and the repetition
/// pushes it off the visible part of the line. One label is kept, so the kind
/// of failure is still named.
fn collapse_repeated_label(message: &str) -> &str {
    let Some((label, _)) = message.split_once(": ") else {
        return message;
    };
    // A label with no text after it is not a label worth collapsing, and
    // stripping on an empty prefix would not terminate.
    if label.is_empty() {
        return message;
    }
    let prefix = format!("{label}: ");
    let mut collapsed = message;
    while let Some(rest) = collapsed.strip_prefix(&prefix) {
        if !rest.starts_with(&prefix) {
            break;
        }
        collapsed = rest;
    }
    collapsed
}

fn exit_with_error(status: ExitStatus, message: &str) -> ! {
    if json_output() {
        print_json(&error_json(status, message));
    } else {
        eprintln!("error: {}", sanitize(message));
    }
    std::process::exit(status.code());
}

/// The server surface the REPL turns into commands. Refreshed on connect
/// and whenever a list_changed notification arrives.
#[derive(Default)]
pub(crate) struct Surface {
    pub tools: Vec<ToolDefinition>,
    pub prompts: Vec<PromptDefinition>,
    pub resources: Vec<ResourceDefinition>,
    pub templates: Vec<ResourceTemplateDefinition>,
    /// Parts whose listing failed, by the name the commands use.
    ///
    /// An empty vector is not the same fact as a failed read, and without
    /// this the two are indistinguishable: a server whose `tools/list` could
    /// not be parsed looked exactly like a server with no tools, so `tools`
    /// printed nothing and the run reported success.
    pub unavailable: Vec<&'static str>,
}

impl Surface {
    /// Whether a named part failed to load rather than coming back empty.
    pub fn is_unavailable(&self, what: &str) -> bool {
        self.unavailable.contains(&what)
    }
}

/// Built-in commands with the short descriptions shown in the completion
/// menu and `help`.
pub(crate) const BUILTINS: &[(&str, &str)] = &[
    ("help", "list built-ins and the server's tools"),
    ("tools", "list tools"),
    ("prompts", "list prompts"),
    ("resources", "list resources"),
    ("templates", "list resource templates"),
    ("find", "search the surface by keyword"),
    ("describe", "show schemas and metadata for a name"),
    ("snapshot", "export a tool or prompt schema contract"),
    ("validate", "compare the surface with a schema snapshot"),
    ("read", "read a resource"),
    ("subscribe", "watch a resource for updates"),
    ("unsubscribe", "stop watching a resource"),
    ("subscriptions", "list active resource subscriptions"),
    ("prompt", "get a prompt"),
    ("call", "call a tool with raw JSON"),
    ("bench", "time repeated calls to a tool"),
    ("jobs", "list background tasks"),
    ("task", "show a background task"),
    ("wait", "wait for background tasks"),
    ("cancel", "cancel a background task"),
    ("alias", "define, list, or show a command alias"),
    ("unalias", "remove a command alias"),
    ("ping", "check the server is answering"),
    ("loglevel", "set the server's log verbosity"),
    ("refresh", "re-fetch the server surface"),
    ("info", "replay the connection banner plus capabilities"),
    ("wire", "toggle raw JSON-RPC frame tracing (on|off)"),
    ("last", "reprint the previous request and response"),
    ("history", "list recent command history"),
    ("vars", "list captured variables"),
    ("unset", "clear a captured variable"),
    ("quit", "exit"),
    ("exit", "exit"),
];

/// Usage and a sentence of explanation per built-in, for `help <name>` and
/// `describe <name>`. Names not listed here fall back to their one-line
/// [`BUILTINS`] description.
const BUILTIN_HELP: &[(&str, &str, &str)] = &[
    (
        "help",
        "help [command]",
        "With no argument, list the built-ins and the server's tools. With one, explain that command.",
    ),
    (
        "tools",
        "tools [--full]",
        "List the server's tools. Every tool is also a command: `<tool> [k=v...]`. \
         A long list is trimmed to the window; `--full` prints all of it.",
    ),
    ("prompts", "prompts [--full]", "List the server's prompts."),
    (
        "resources",
        "resources [--full]",
        "List concrete resources. Parameterized ones are under `templates`.",
    ),
    (
        "templates",
        "templates [--full]",
        "List resource templates: URIs with `{variable}` parts, completed by the server.",
    ),
    (
        "find",
        "find [-E] [-m N] [--case-sensitive] [--tools|--prompts|--resources|--templates|--builtins] <keyword>",
        "Search names and descriptions across the surface and the built-ins. Kind flags narrow \
         it, `-m` caps the results, `-E` treats the keyword as a regular expression, and it \
         exits non-zero when nothing matches, like grep.",
    ),
    (
        "describe",
        "describe <name>",
        "Show a tool's schemas, a prompt's arguments, or a resource's metadata, plus an example invocation.",
    ),
    (
        "snapshot",
        "snapshot <name> [path]",
        "Export a tool or prompt's schema as a versioned contract. Without a path, print it.",
    ),
    (
        "validate",
        "validate <path> [strict|compatible|ignore]",
        "Compare a saved snapshot with the live surface. No request is sent.",
    ),
    (
        "read",
        "read <uri> [--out <path>] [--force]",
        "Read a resource. Tab completes URIs, and template variables via the server. \
         `--out` writes the content to a file, decoding a binary resource, instead of \
         printing it.",
    ),
    (
        "subscribe",
        "subscribe <uri>",
        "Ask the server to report updates to a resource. Updates print inline.",
    ),
    (
        "unsubscribe",
        "unsubscribe <uri>",
        "Stop receiving updates for a resource.",
    ),
    (
        "subscriptions",
        "subscriptions",
        "List the resources the server is currently reporting updates for.",
    ),
    (
        "prompt",
        "prompt <name> [k=v...]",
        "Retrieve a prompt. Argument values tab-complete through the server.",
    ),
    (
        "call",
        "call <tool> <json>",
        "Call a tool with a raw JSON argument object, for when `k=v` coercion is not enough.",
    ),
    (
        "bench",
        "bench <tool> [k=v...] [--n N] [--concurrency C]",
        "Time repeated calls and report the latency distribution. Any failure exits non-zero.",
    ),
    (
        "jobs",
        "jobs",
        "List the background tasks this session started, with their current status.",
    ),
    (
        "task",
        "task <task> [respond]",
        "Show one background task. Takes the short number from `jobs`, `last`, or the server's \
         id. \
         `respond` answers what an `input_required` task is waiting for and lets its handler \
         resume; it needs --protocol 2026-07-28.",
    ),
    (
        "wait",
        "wait [<task>] [--timeout <seconds>]",
        "Block until a task settles. With no task, waits for every task this session started, \
         in start order, which is how an --exec script waits for work whose id it never saw. \
         `last` names the most recent. A task that failed or was cancelled sets a non-zero exit \
         status. Ctrl-C interrupts; the global --timeout does not apply, and this one applies \
         per task.",
    ),
    (
        "cancel",
        "cancel <task>",
        "Ask the server to cancel a task. `last` names the most recent.",
    ),
    (
        "alias",
        "alias [--global] [<name>=<expansion>]",
        "Define, list, or show a command alias. Saved to the config file.",
    ),
    (
        "unalias",
        "unalias [--global] <name>",
        "Remove the alias that is in effect for a name.",
    ),
    (
        "loglevel",
        "loglevel <debug|info|notice|warning|error|critical|alert|emergency>",
        "Ask the server to change how much it logs, via `logging/setLevel`. Levels are the \
         syslog severities the MCP spec uses, least severe first: a level means that one and \
         everything more severe. Needs the server to declare the `logging` capability.",
    ),
    (
        "ping",
        "ping",
        "Send an empty request and report the round trip. Exits non-zero if the server does not answer.",
    ),
    (
        "refresh",
        "refresh",
        "Re-fetch the surface. Usually unnecessary: list_changed notifications refresh it live.",
    ),
    (
        "info",
        "info",
        "Replay the connection banner and show the server's capabilities.",
    ),
    (
        "wire",
        "wire [on|off]",
        "Trace raw JSON-RPC frames to stderr. Bare `wire` reports the current state.",
    ),
    (
        "last",
        "last",
        "Reprint the previous request and response. Frames are recorded whether or not tracing is on.",
    ),
    (
        "history",
        "history [count]",
        "List recent commands from previous sessions. Ctrl-R searches them interactively.",
    ),
    (
        "vars",
        "vars",
        "List captured variables. Capture with `name = <command>`.",
    ),
    ("unset", "unset <name>", "Clear one captured variable."),
    ("quit", "quit", "Close the session and exit."),
    ("exit", "exit", "Close the session and exit."),
];

/// The usage line and explanation for a built-in, if it has one.
fn builtin_help(name: &str) -> Option<(&'static str, &'static str)> {
    BUILTIN_HELP
        .iter()
        .find(|(builtin, _, _)| *builtin == name)
        .map(|(_, usage, detail)| (*usage, *detail))
}

/// Coerce a `key=value` string according to the tool's inputSchema.
fn coerce_arg(schema: &serde_json::Value, key: &str, raw: &str) -> serde_json::Value {
    let ty = schema
        .get("properties")
        .and_then(|p| p.get(key))
        .and_then(|s| s.get("type"))
        .and_then(|t| t.as_str());
    match ty {
        Some("integer") => raw
            .parse::<i64>()
            .map(Into::into)
            .unwrap_or_else(|_| serde_json::Value::String(raw.to_string())),
        Some("number") => raw
            .parse::<f64>()
            .ok()
            .and_then(|n| serde_json::Number::from_f64(n).map(serde_json::Value::Number))
            .unwrap_or_else(|| serde_json::Value::String(raw.to_string())),
        Some("boolean") => raw
            .parse::<bool>()
            .map(serde_json::Value::Bool)
            .unwrap_or_else(|_| serde_json::Value::String(raw.to_string())),
        Some("array") | Some("object") => {
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
        }
        _ => {
            // No schema type: accept JSON literals, fall back to string.
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_string()))
        }
    }
}

fn parse_kv_args(schema: &serde_json::Value, tokens: &[&str]) -> serde_json::Value {
    // A single JSON object literal wins.
    if tokens.len() == 1
        && tokens[0].starts_with('{')
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(tokens[0])
    {
        return v;
    }
    let mut map = serde_json::Map::new();
    for t in tokens {
        if let Some((k, v)) = t.split_once('=') {
            map.insert(k.to_string(), coerce_arg(schema, k, v));
        }
    }
    serde_json::Value::Object(map)
}

fn render_content(content: &[Content]) {
    for c in content {
        match c {
            Content::Text { text, .. } => {
                if style::colors_enabled() && style::looks_like_markdown(text) {
                    println!("{}", style::render_markdown(text));
                } else {
                    println!("{}", sanitize(text));
                }
            }
            other => {
                let v = serde_json::to_value(other).unwrap_or_default();
                let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("content");
                match ty {
                    "image" | "audio" => {
                        let mime = v.get("mimeType").and_then(|m| m.as_str()).unwrap_or("?");
                        let len = v.get("data").and_then(|d| d.as_str()).map_or(0, str::len);
                        println!(
                            "{}",
                            tag(
                                Style::new(),
                                &format!("{ty} {}, {len} base64 chars", sanitize(mime))
                            )
                        );
                    }
                    _ => println!("{}", json_pretty(&v)),
                }
            }
        }
    }
}

fn render_task(task: &TaskObject, label: &str) {
    println!(
        "task {}  status={}  {}",
        paint(Style::new().bold(), &sanitize(label)),
        paint(task_status_style(task.status), &task.status.to_string()),
        sanitize(task.status_message.as_deref().unwrap_or(""))
    );
    if let Some(result) = &task.result {
        // A task whose tool failed still settles as `completed`, so without
        // this the status line reads as success and the error text below it
        // has nothing marking it as an error. Same tag the foreground call
        // path prints, for the same result shape.
        if result.is_error {
            println!("{}", tag(Style::new().fg(Color::Red), "tool error"));
        }
        render_content(&result.content);
    }
    if let Some(err) = &task.error {
        println!(
            "{} {}: {}",
            style::error_prefix(),
            err.code,
            sanitize(&err.message)
        );
    }
}

/// Block until one task settles, honoring `wait`'s own deadline.
async fn wait_for_one(
    client: &McpClient,
    id: &str,
    limit: Option<Duration>,
) -> tower_mcp::Result<TaskObject> {
    match limit {
        None => client.task_wait(id).await,
        Some(limit) => match tokio::time::timeout(limit, client.task_wait(id)).await {
            Ok(result) => result,
            Err(_) => Err(tower_mcp::Error::Transport(format!(
                "task {id} was still running after {}s (--timeout)",
                limit.as_secs()
            ))),
        },
    }
}

/// Record what a settled task means for the process exit status.
///
/// Only `wait` calls this. A script uses `wait` to ask whether the work
/// succeeded, so a task that failed has to be visible in the status rather
/// than only in the rendered output. `task` and `cancel` report state instead
/// of judging it, and a `cancel` that worked is not a failure of `cancel`.
fn note_settled_task(task: &TaskObject) {
    use tower_mcp::protocol::TaskStatus;
    // Three shapes mean the same thing. A handler returning `Err` surfaces as
    // a *completed* task carrying an error rather than a failed one, and a
    // tool-level error result is a third. Judging on status alone would call
    // the most common failure a success.
    if task.error.is_some() || task.result.as_ref().is_some_and(|r| r.is_error) {
        note_error(ExitStatus::Server);
        return;
    }
    match task.status {
        TaskStatus::Failed => note_error(ExitStatus::Server),
        // Not the operator's Ctrl-C, but the same shape of outcome: the work
        // was abandoned and produced no result, and a script that waited for
        // one should not read that as success.
        TaskStatus::Cancelled => note_error(ExitStatus::Cancelled),
        _ => {}
    }
}

/// Wait for every task this session started, oldest first.
///
/// `--timeout` applies per task, matching what it means for `wait <task>`.
async fn wait_for_all(
    client: &McpClient,
    jobs: &Arc<Jobs>,
    limit: Option<Duration>,
    started: std::time::Instant,
) {
    let ids = jobs.all_ids();
    if ids.is_empty() {
        report_error(
            ExitStatus::NoMatch,
            "no tasks in this session to wait for (start one with a trailing `&`)",
        );
        return;
    }
    let mut settled = Vec::new();
    for id in &ids {
        match wait_for_one(client, id, limit).await {
            Ok(task) => {
                jobs.sync(id, task.status, task.status_message.clone());
                note_settled_task(&task);
                if !json_output() {
                    render_task(&task, &jobs.label_for(&task.task_id));
                }
                settled.push(task);
            }
            // Keep going: one unreachable task should not hide the outcome of
            // the others a script is also waiting on.
            Err(e) => report_mcp_error(&e),
        }
    }
    if json_output() {
        // One value per command, so several tasks are one array rather than
        // several NDJSON lines.
        print_json(&serde_json::to_value(&settled).unwrap_or_default());
    } else {
        println!("{}", timing(started.elapsed()));
    }
}

/// Answer the questions an `input_required` task is parked on, then hand the
/// answers back with `tasks/update` so its handler resumes.
///
/// Only the 2026-07-28 lifecycle reports outstanding requests: on the stable
/// lifecycle a server asks by sending `elicitation/create` itself, so there
/// is nothing parked to look up. Answering is a foreground read of stdin,
/// which is safe here because the editor is waiting on this command.
async fn respond_to_task(client: &McpClient, id: &str, label: &str) {
    use tower_mcp::protocol::{InputRequest, InputResponse, InputResponses};

    // Checked here so the refusal explains the protocol rather than surfacing
    // the framework's "task_get_detailed requires ..." transport error, which
    // reads like a bug in the connection.
    if client.selected_protocol_version().await.as_deref()
        != Some(tower_mcp::protocol::PROTOCOL_VERSION_2026_07_28)
    {
        report_error(
            ExitStatus::Usage,
            "`respond` needs --protocol 2026-07-28: only that lifecycle reports what a task is \
             waiting for. On the stable lifecycle a server asks by sending `elicitation/create` \
             itself, which is declined while the editor holds the terminal, so run the tool in \
             the foreground instead of as a task",
        );
        return;
    }
    let detailed = match client.task_get_detailed(id).await {
        Ok(detailed) => detailed,
        Err(e) => {
            report_mcp_error(&e);
            return;
        }
    };
    let Some(outstanding) = detailed.task.input_requests().filter(|r| !r.is_empty()) else {
        report_error(
            ExitStatus::NoMatch,
            &format!(
                "task {label} is not waiting for input (status: {})",
                detailed.task.status()
            ),
        );
        return;
    };

    let server = connection_info(client)
        .await
        .map(|info| info.server_info.name)
        .unwrap_or_default();
    let mut responses = InputResponses::new();
    for (key, request) in outstanding.clone() {
        match request {
            InputRequest::Elicit(params) => {
                let answer = elicit::answer_in_foreground(&server, params).await;
                responses.insert(key, InputResponse::Elicit(answer));
            }
            InputRequest::CreateMessage(params) => {
                match tokio::task::spawn_blocking(move || sampling::prompt(&params)).await {
                    Ok(Ok(result)) => {
                        responses.insert(key, InputResponse::CreateMessage(result));
                    }
                    // Declining is an answer the server can act on; failing
                    // to ask is not, and leaving the key out keeps it
                    // outstanding for another try.
                    Ok(Err(e)) => command_error(&format!(
                        "could not answer `{}`: {}",
                        sanitize(&key),
                        sanitize(&e.message)
                    )),
                    Err(e) => command_error(&format!("could not answer `{}`: {e}", sanitize(&key))),
                }
            }
            // Nothing in this REPL declares roots, so the honest answer is an
            // empty list rather than silence that leaves the task parked.
            InputRequest::ListRoots(_) => {
                println!(
                    "{} answered `{}` with no roots (mcp-repl declares none)",
                    tag(Style::new().fg(Color::Purple), "elicit"),
                    sanitize(&key)
                );
                responses.insert(
                    key,
                    InputResponse::ListRoots(tower_mcp::protocol::ListRootsResult {
                        roots: Vec::new(),
                        meta: None,
                    }),
                );
            }
            other => command_error(&format!(
                "cannot answer `{}`: unsupported request {}",
                sanitize(&key),
                sanitize(other.method_name())
            )),
        }
    }

    if responses.is_empty() {
        report_error(
            ExitStatus::Usage,
            &format!("nothing was answered, so task {label} is still waiting"),
        );
        return;
    }
    if let Err(e) = client.task_update(id, responses).await {
        report_mcp_error(&e);
        return;
    }
    // The handler resumes asynchronously, so report what the task says now
    // rather than claiming it finished.
    match client.task_get(id).await {
        Ok(task) if json_output() => print_json(&serde_json::to_value(&task).unwrap_or_default()),
        Ok(task) => render_task(&task, label),
        Err(e) => report_mcp_error(&e),
    }
}

/// Lifecycle-neutral connection details used by the banner and `info`.
#[derive(Clone, Debug)]
struct ConnectionInfo {
    protocol_version: String,
    capabilities: ServerCapabilities,
    server_info: Implementation,
    instructions: Option<String>,
}

impl From<InitializeResult> for ConnectionInfo {
    fn from(info: InitializeResult) -> Self {
        Self {
            protocol_version: info.protocol_version,
            capabilities: info.capabilities,
            server_info: info.server_info,
            instructions: info.instructions,
        }
    }
}

impl ConnectionInfo {
    fn from_discovery(discovery: DiscoverResult, protocol_version: String) -> Self {
        let server_info = discovery
            .meta
            .as_ref()
            .and_then(|meta| meta.server_info.clone())
            .unwrap_or_else(|| Implementation {
                name: "MCP server".to_string(),
                version: "unknown".to_string(),
                ..Default::default()
            });
        Self {
            protocol_version,
            capabilities: discovery.capabilities,
            server_info,
            instructions: discovery.instructions,
        }
    }
}

async fn connection_info(client: &McpClient) -> Option<ConnectionInfo> {
    if let Some(info) = client.server_info().await {
        return Some(info.into());
    }
    let discovery = client.discovery().await?;
    let protocol_version = client.selected_protocol_version().await?;
    Some(ConnectionInfo::from_discovery(discovery, protocol_version))
}

async fn establish_connection(
    client: &McpClient,
    protocol: ProtocolMode,
) -> tower_mcp::Result<ConnectionInfo> {
    match protocol {
        ProtocolMode::Stable => client
            .initialize("mcp-repl", env!("CARGO_PKG_VERSION"))
            .await
            .map(Into::into),
        ProtocolMode::Final => {
            let discovery: DiscoverResult = client
                .discover("mcp-repl", env!("CARGO_PKG_VERSION"))
                .await?;
            let protocol_version = client
                .selected_protocol_version()
                .await
                .unwrap_or_else(|| "2026-07-28".to_string());
            Ok(ConnectionInfo::from_discovery(discovery, protocol_version))
        }
    }
}

fn client_builder(protocol: ProtocolMode) -> Result<McpClientBuilder, ProtocolSupportError> {
    let builder = McpClient::builder()
        .protocol_support(protocol.support()?)
        .with_elicitation()
        .with_sampling()
        // A server only reports progress when the client asks for it, by
        // carrying a token on the request. Without this the REPL renders
        // progress lines it can never receive.
        .request_progress();
    Ok(match protocol {
        ProtocolMode::Stable => builder,
        ProtocolMode::Final => builder.with_tasks(),
    })
}

/// The connection banner: server identity, negotiated protocol, and any
/// server instructions (markdown-rendered when it looks like markdown).
/// Printed at startup and replayed by the `info` command.
fn print_banner(info: &ConnectionInfo) {
    println!(
        "connected: {} v{} {}",
        paint(Style::new().bold(), &sanitize(&info.server_info.name)),
        sanitize(&info.server_info.version),
        paint(
            Style::new().dimmed(),
            &format!("(protocol {})", sanitize(&info.protocol_version))
        )
    );
    if let Some(instructions) = &info.instructions {
        if style::colors_enabled() && style::looks_like_markdown(instructions) {
            println!("{}", style::render_markdown(instructions));
        } else {
            println!("{}", sanitize(instructions));
        }
    }
}

/// A dimmed `[142ms]` / `[1.23s]` annotation for how long a call took.
/// Printed on its own trailing line after a request-issuing command, so a slow
/// (or timing-out) call is visible without interleaving with streamed output.
pub(crate) fn timing(elapsed: Duration) -> String {
    let body = if elapsed.as_millis() < 1000 {
        format!("[{}ms]", elapsed.as_millis())
    } else {
        format!("[{:.2}s]", elapsed.as_secs_f64())
    };
    paint(Style::new().dimmed(), &body)
}

/// How many rows a listing may print before it truncates, and the tail line
/// to print when it does.
///
/// Sized to the terminal so a listing leaves the command that produced it,
/// and the next prompt, on screen instead of scrolling them away. `None`
/// means print everything: under `--json` or when stdout is not a terminal
/// the output is a data stream, and truncating it would corrupt whatever
/// reads it.
fn listing_limit() -> Option<usize> {
    if json_output() || !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        return None;
    }
    // Room for the command line, the truncation notice, and the next prompt.
    const RESERVED: usize = 4;
    const FALLBACK_ROWS: usize = 24;
    let rows = crossterm::terminal::size()
        .map(|(_, rows)| rows as usize)
        .unwrap_or(FALLBACK_ROWS);
    // Never so small that the listing is useless on a short window.
    Some(rows.saturating_sub(RESERVED).max(5))
}

/// Print at most `limit` of `total` rows, and say what was held back.
///
/// `full` is the flag the user passes to see the rest, named in the notice
/// so the way forward is on screen rather than in the docs.
fn note_truncation(shown: usize, total: usize, full: &str) {
    if shown >= total {
        return;
    }
    println!(
        "{}",
        paint(
            Style::new().dimmed(),
            &format!(
                "... {} more of {total}; `{full}` shows everything",
                total - shown
            )
        )
    );
}

/// A compact tool listing for the startup banner: name and description, capped
/// so a large surface does not flood the screen. The full list is always
/// available via `tools`.
fn print_tool_overview(surface: &Surface) {
    if surface.tools.is_empty() {
        return;
    }
    // Half the window: the banner shares the first screen with the counts,
    // the hint line, and the prompt.
    let cap = listing_limit().map_or(surface.tools.len(), |rows| (rows / 2).max(5));
    for t in surface.tools.iter().take(cap) {
        println!(
            "{} {}{}",
            style::column(Style::new().fg(Color::Green), &sanitize(&t.name), 24),
            sanitize(t.description.as_deref().unwrap_or("")),
            tool_tag_suffix(t)
        );
    }
    if surface.tools.len() > cap {
        println!(
            "{}",
            paint(
                Style::new().dimmed(),
                &format!("... +{} more, type `tools`", surface.tools.len() - cap)
            )
        );
    }
}

/// The `find` built-in's output: matches grouped by kind under the heading
/// of the list command that shows the same entries, best match first within
/// each group.
fn print_find(surface: &Surface, query: &find::Query, output: &vars::Output) {
    let hits = find::search_query(surface, query);
    if !output.is_plain() || json_output() {
        let v: Vec<serde_json::Value> = hits
            .iter()
            .map(|h| {
                serde_json::json!({
                    "kind": h.kind.heading(),
                    "name": h.name,
                    "description": h.description,
                    "score": h.score,
                })
            })
            .collect();
        // An empty result is still a result: grep-style, it exits non-zero
        // but the value is a well-formed empty array.
        if v.is_empty() {
            note_error(ExitStatus::NoMatch);
        }
        emit_value(serde_json::Value::Array(v), output, || {
            unreachable!("plain output handled below")
        });
        return;
    }
    if hits.is_empty() {
        // grep's convention: a search that matched nothing exits non-zero, so
        // `mcp-repl -e "find x"` can be tested in a script.
        report_error(ExitStatus::NoMatch, &format!("no match for {}", query.text));
        return;
    }
    let total = hits.len();
    for (kind, group) in find::grouped(hits) {
        println!("{}:", paint(Style::new().bold(), kind.heading()));
        for hit in group {
            println!(
                "  {} {}",
                style::column(Style::new().fg(Color::Green), &sanitize(&hit.name), 24),
                sanitize(&hit.description)
            );
        }
    }
    println!(
        "{}",
        paint(
            Style::new().dimmed(),
            &format!("{total} match{}", if total == 1 { "" } else { "es" })
        )
    );
}

/// The one-line surface summary.
fn print_counts(surface: &Surface) {
    println!(
        "{}, {}, {}, {}. Type `help`.",
        plural(surface.tools.len(), "tool"),
        plural(surface.prompts.len(), "prompt"),
        plural(surface.resources.len(), "resource"),
        plural(surface.templates.len(), "template")
    );
}

/// The one-line nudge toward the features that are not obvious from a
/// prompt: completion, search, schemas, and backgrounding. Interactive only,
/// since `--exec` output is a data stream.
fn print_first_run_hint() {
    println!(
        "{}",
        paint(
            Style::new().dimmed(),
            "Tab completes  ·  `find <word>` searches  ·  `describe <name>` shows \
             schemas  ·  `&` runs a tool as a task"
        )
    );
}

/// `1 tool`, `2 tools`. Every noun the REPL counts is regular.
fn plural(count: usize, noun: &str) -> String {
    if count == 1 {
        format!("{count} {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

/// Run one request, and if it fails because the server lost the session,
/// rebuild the connection and run it exactly once more.
///
/// The retry is deliberately bounded to a single attempt: a server that is
/// down stays down, and a loop here would turn one dead command into a long
/// unresponsive prompt. On the second failure the original error surfaces
/// with a hint, which is what the user would have seen without reconnection.
///
/// `op` runs against whichever client is current, so it takes the client as
/// an argument rather than closing over one: the second call must use the
/// client the reconnect installed, not the dead one.
async fn with_reconnect<T, F, Fut>(
    session: &Session,
    surface: &Arc<RwLock<Surface>>,
    op: F,
) -> Result<T, tower_mcp::Error>
where
    F: Fn(Arc<McpClient>) -> Fut,
    Fut: Future<Output = Result<T, tower_mcp::Error>>,
{
    let seen = session.generation();
    // Each attempt gets its own deadline: the retry only happens after a
    // reconnect, so it is a fresh request rather than a continuation.
    let err = match with_deadline(op(session.client())).await {
        Ok(value) => return Ok(value),
        Err(e) => e,
    };
    if !session.can_reconnect() || !is_session_lost(&err) {
        return Err(err);
    }
    if let Err(reconnect_err) = session.reconnect(seen).await {
        eprintln!("reconnect failed: {reconnect_err}");
        return Err(err);
    }
    // The surface belongs to the old session: a restarted server may expose a
    // different set of tools, and the completer and command dispatch both read
    // this. Refresh before the retry so the retried command and the next
    // prompt agree on what exists.
    *surface.write().unwrap() = fetch_surface(&session.client()).await;
    // stderr, so the note does not land in the middle of `--json` output
    // being piped somewhere.
    eprintln!("{}", paint(Style::new().dimmed(), "[reconnected]"));

    let retried = with_deadline(op(session.client())).await;
    if let Err(e) = &retried
        && is_session_lost(e)
    {
        eprintln!(
            "still no session after reconnecting. The server is likely down or \
             restart-looping; check its logs, or pass --no-reconnect to see the \
             raw errors."
        );
    }
    retried
}

/// How many pages one surface list may follow, and how many entries it may
/// accept. A server that returns a `next_cursor` on every page describes an
/// infinite surface; following it is an unbounded allocation driven entirely
/// by the other end. These caps are far above any real server (the largest
/// published surfaces are in the hundreds) and exist so a hostile or looping
/// one is reported rather than fatal.
const MAX_SURFACE_PAGES: usize = 100;
const MAX_SURFACE_ITEMS: usize = 10_000;

/// Follow pagination cursors for one list, bounded.
///
/// Stops at the page cap, the item cap, or a repeated cursor (a server that
/// keeps handing back the same cursor is looping), and reports which bound it
/// hit so a truncated surface is never silently presented as complete.
async fn collect_pages<T, F, Fut>(what: &str, mut page: F) -> Result<Vec<T>, tower_mcp::Error>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<(Vec<T>, Option<String>), tower_mcp::Error>>,
{
    let mut all: Vec<T> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for _ in 0..MAX_SURFACE_PAGES {
        let (items, next) = page(cursor).await?;
        all.extend(items);
        if all.len() >= MAX_SURFACE_ITEMS {
            all.truncate(MAX_SURFACE_ITEMS);
            eprintln!(
                "warning: {what} stopped at {MAX_SURFACE_ITEMS} entries; the server offered more"
            );
            return Ok(all);
        }
        match next {
            None => return Ok(all),
            Some(next) if !seen.insert(next.clone()) => {
                eprintln!("warning: {what} paging stopped: the server repeated a cursor");
                return Ok(all);
            }
            Some(next) => cursor = Some(next),
        }
    }
    eprintln!("warning: {what} stopped after {MAX_SURFACE_PAGES} pages; the server offered more");
    Ok(all)
}

/// Fetch the server surface once. Returns the surface plus whether any list
/// call was rejected as not-initialized (the retryable startup condition).
async fn fetch_surface_once(client: &McpClient) -> (Surface, bool) {
    struct Outcome {
        not_initialized: bool,
        unavailable: Vec<&'static str>,
    }

    fn take<T>(
        what: &'static str,
        r: Option<Result<Vec<T>, tower_mcp::Error>>,
        at: &mut Outcome,
    ) -> Vec<T> {
        match r {
            // Not declared by the server, so never asked for. An absent
            // capability is a definite empty, not a failure.
            None => Vec::new(),
            Some(Ok(v)) => v,
            Some(Err(e)) => {
                if is_not_initialized(&e) {
                    at.not_initialized = true;
                } else {
                    eprintln!(
                        "warning: fetching {what} failed: {}",
                        collapse_repeated_label(&e.to_string())
                    );
                    // A part the REPL could not read is a failure of the run,
                    // not an empty result: without this an --exec script that
                    // asked for the tools of an unreadable server exits 0
                    // having printed nothing.
                    note_error(ExitStatus::Transport);
                    at.unavailable.push(what);
                }
                Vec::new()
            }
        }
    }

    // Ask only for what the server said it has. A server that declares tools
    // and nothing else answers `prompts/list` with "Method not found", which
    // is correct of it, and warning about that made a healthy connection look
    // broken. Capabilities we cannot read at all leave every list enabled, so
    // an unreadable capability set hides nothing.
    let declared = connection_info(client).await.map(|info| info.capabilities);
    let has = |pick: fn(&ServerCapabilities) -> bool| declared.as_ref().is_none_or(pick);
    let (want_tools, want_prompts, want_resources) = (
        has(|c| c.tools.is_some()),
        has(|c| c.prompts.is_some()),
        has(|c| c.resources.is_some()),
    );
    // The four list calls are independent reads, so run them concurrently.
    // The McpClient message loop multiplexes requests by id, so this is safe
    // on a single connection, and it means startup costs one round-trip's
    // latency instead of four in series. It also bounds the cost of a slow or
    // unresponsive server: against a server that makes each list time out, the
    // surface fetch now waits one `request_timeout`, not four.
    //
    // Each list pages through `collect_pages` rather than the framework's
    // `list_all_*`, which follows cursors without a bound.
    // The deadline covers each list as a whole, pagination included, so a
    // server that answers every page slowly cannot stretch the fetch by
    // adding pages.
    let (tools, prompts, resources, templates) = tokio::join!(
        maybe(want_tools, async {
            with_deadline(collect_pages("tools", |cursor| async move {
                let page = client.list_tools_with_cursor(cursor).await?;
                Ok((page.tools, page.next_cursor))
            }))
            .await
        }),
        maybe(want_prompts, async {
            with_deadline(collect_pages("prompts", |cursor| async move {
                let page = client.list_prompts_with_cursor(cursor).await?;
                Ok((page.prompts, page.next_cursor))
            }))
            .await
        }),
        maybe(want_resources, async {
            with_deadline(collect_pages("resources", |cursor| async move {
                let page = client.list_resources_with_cursor(cursor).await?;
                Ok((page.resources, page.next_cursor))
            }))
            .await
        }),
        // Templates ride on the resources capability: a server that serves
        // neither declares neither, and there is no separate flag for them.
        maybe(want_resources, async {
            with_deadline(collect_pages("resource templates", |cursor| async move {
                let page = client.list_resource_templates_with_cursor(cursor).await?;
                Ok((page.resource_templates, page.next_cursor))
            }))
            .await
        }),
    );
    let mut at = Outcome {
        not_initialized: false,
        unavailable: Vec::new(),
    };
    let surface = Surface {
        tools: take("tools", tools, &mut at),
        prompts: take("prompts", prompts, &mut at),
        resources: take("resources", resources, &mut at),
        templates: take("resource templates", templates, &mut at),
        unavailable: std::mem::take(&mut at.unavailable),
    };
    (surface, at.not_initialized)
}

/// Run `work` only when the server declared the capability it needs.
async fn maybe<T, F: Future<Output = T>>(wanted: bool, work: F) -> Option<T> {
    if wanted { Some(work.await) } else { None }
}

async fn fetch_surface(client: &McpClient) -> Surface {
    fetch_surface_once(client).await.0
}

/// Re-fetch the surface, reconnecting first if the fetch shows the session is
/// gone. The four list calls swallow their own errors, so not-initialized is
/// the one session-loss signal that survives to here; the typed session
/// errors would have shown up as empty lists with a warning.
async fn refresh_surface(session: &Session) -> Surface {
    let (fresh, not_initialized) = fetch_surface_once(&session.client()).await;
    if !not_initialized || !session.can_reconnect() {
        return fresh;
    }
    let seen = session.generation();
    match session.reconnect(seen).await {
        Ok(()) => {
            eprintln!("{}", paint(Style::new().dimmed(), "[reconnected]"));
            fetch_surface(&session.client()).await
        }
        Err(e) => {
            eprintln!("reconnect failed: {e}");
            fresh
        }
    }
}

/// Startup surface fetch with a bounded retry on the not-initialized
/// condition. Explains the likely cause if it never clears.
async fn fetch_surface_initial(client: &McpClient) -> Surface {
    const ATTEMPTS: usize = 4;
    for attempt in 1..=ATTEMPTS {
        let (surface, not_initialized) = fetch_surface_once(client).await;
        if !not_initialized {
            return surface;
        }
        if attempt == ATTEMPTS {
            eprintln!(
                "warning: the server kept rejecting surface requests as not-initialized \
                 after {ATTEMPTS} attempts. The session the handshake established is not \
                 being recognized on follow-up requests. Two common causes: the server runs \
                 multiple instances without a shared session store, so requests scatter \
                 across instances; or a single instance restarted (crash, OOM, or redeploy) \
                 between requests and lost its in-memory sessions. Try `refresh`. A \
                 persistent session store or the stateless protocol avoids both; if it is a \
                 single instance, check its logs and resources (an OOM-looping machine \
                 flaps like this)."
            );
            return surface;
        }
        tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
    }
    unreachable!()
}

/// Build the HTTP client config from the auth flags and the resolved profile.
/// `--bearer` wins over the profile's token, which wins over the `MCP_BEARER`
/// environment variable; `--header` flags are applied after the profile's
/// headers so a repeated name overrides it. Each `--header "Name: Value"` is
/// split on the first colon (surrounding whitespace trimmed); a header with no
/// colon is a usage error.
fn build_http_config(
    bearer: Option<String>,
    headers: &[String],
    profile_bearer: Option<String>,
    profile_headers: &[(String, String)],
) -> Result<HttpClientConfig, String> {
    build_http_config_with_env(
        bearer,
        headers,
        profile_bearer,
        profile_headers,
        std::env::var("MCP_BEARER").ok(),
    )
}

fn build_http_config_with_env(
    bearer: Option<String>,
    headers: &[String],
    profile_bearer: Option<String>,
    profile_headers: &[(String, String)],
    env_bearer: Option<String>,
) -> Result<HttpClientConfig, String> {
    let mut config = HttpClientConfig::default();
    for (name, value) in profile_headers {
        config = config.header(name.as_str(), value.as_str());
    }
    let selected_has_authorization = profile_headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("authorization"));
    if let Some(token) = bearer.or(profile_bearer).or_else(|| {
        (!selected_has_authorization)
            .then_some(env_bearer)
            .flatten()
    }) {
        config = config.bearer_token(token);
    }
    for raw in headers {
        let (name, value) = raw
            .split_once(':')
            .ok_or_else(|| format!("invalid --header {raw:?}: expected `Name: Value`"))?;
        config = config.header(name.trim(), value.trim());
    }
    // The framework applies its own 30s HTTP request timeout. Leaving it in
    // place would make `--timeout 300` a lie on HTTP and `--timeout 0` still
    // give up at 30s, so the transport is told the same deadline the REPL is
    // using. "Indefinitely" becomes a year, which no interactive session
    // outlives, because the transport requires some duration.
    config.request_timeout = request_timeout().unwrap_or(Duration::from_secs(365 * 24 * 60 * 60));
    Ok(config)
}

fn selected_oauth_profile(
    cli_oauth: Option<&str>,
    profile_oauth: Option<&str>,
    cli_bearer: bool,
    cli_headers: &[String],
) -> Option<String> {
    let explicit_authorization = cli_bearer
        || cli_headers.iter().any(|header| {
            header
                .split_once(':')
                .is_some_and(|(name, _)| name.trim().eq_ignore_ascii_case("authorization"))
        });
    (!explicit_authorization)
        .then(|| cli_oauth.or(profile_oauth).map(str::to_string))
        .flatten()
}

fn demo_router() -> tower_mcp::McpRouter {
    use tower_mcp::context::RequestContext;
    use tower_mcp::extract::{Context, Json, RawArgs};
    use tower_mcp::protocol::ToolAnnotations;
    use tower_mcp::protocol::{
        CompleteResult, CompletionReference, ElicitRequestParams, InputRequest, InputRequests,
        InputRequiredResult, InputResponse, ReadResourceResult, RequestOutcome,
    };
    use tower_mcp::resource::ResourceTemplateBuilder;
    use tower_mcp::{CallToolResult, PromptBuilder, TaskSupportMode, ToolBuilder};

    /// Reads nothing outside this process and changes nothing: true of every
    /// tool here except `sign_in`. Spelled out rather than using
    /// `read_only_safe()`, which leaves `open_world_hint` at the spec default
    /// of true and would tag all six tools identically.
    fn local_read_only() -> ToolAnnotations {
        ToolAnnotations {
            read_only_hint: true,
            idempotent_hint: true,
            destructive_hint: false,
            open_world_hint: false,
            ..Default::default()
        }
    }

    /// One transparent pixel, base64 as it travels on the wire.
    const PIXEL_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    const NOTES: &[(&str, &str)] = &[
        ("groceries", "- eggs\n- coffee"),
        ("ideas", "# Ideas\n\n- a REPL for MCP servers"),
        ("todo", "1. ship it"),
    ];

    tower_mcp::McpRouter::new()
        .server_info("mcp-repl-demo", env!("CARGO_PKG_VERSION"))
        .with_tasks()
        .prompt(
            PromptBuilder::new("greet")
                .description("Generate a greeting (name tab-completes via the server)")
                .required_arg("name", "The person to greet")
                .handler(|args| async move {
                    let name = args.get("name").map(|s| s.as_str()).unwrap_or("World");
                    Ok(tower_mcp::GetPromptResult::user_message(format!(
                        "Please greet {name} warmly."
                    )))
                })
                .build(),
        )
        // A concrete resource, so `read`, `subscribe`, and `unsubscribe` all
        // have something to point at without an external server. Subscribing
        // needs a registered URI: the router rejects a subscription to
        // anything it does not serve.
        .resource(
            tower_mcp::resource::ResourceBuilder::new("note://status")
                .name("Status")
                .description("A one-line status note (subscribe to it)")
                .mime_type("text/plain")
                .handler(|| async {
                    Ok(ReadResourceResult::text(
                        "note://status",
                        "all quiet on the demo server",
                    ))
                })
                .build(),
        )
        // A binary resource, so `read <uri> --out <file>` has something whose
        // bytes matter. One transparent pixel: a real PNG an image viewer
        // opens, small enough to sit in the source.
        .resource(
            tower_mcp::resource::ResourceBuilder::new("img://pixel")
                .name("Pixel")
                .description("A 1x1 transparent PNG (try `read img://pixel --out pixel.png`)")
                .mime_type("image/png")
                .handler(|| async {
                    Ok(ReadResourceResult {
                        contents: vec![tower_mcp::protocol::ResourceContent {
                            uri: "img://pixel".to_string(),
                            mime_type: Some("image/png".to_string()),
                            text: None,
                            blob: Some(PIXEL_PNG.to_string()),
                            meta: None,
                        }],
                        ..Default::default()
                    })
                })
                .build(),
        )
        .resource_template(
            ResourceTemplateBuilder::new("note://{name}")
                .name("Notes")
                .description("Tiny in-memory notes (name tab-completes via the server)")
                .mime_type("text/markdown")
                .handler(
                    |uri: String, vars: std::collections::HashMap<String, String>| async move {
                        let name = vars.get("name").cloned().unwrap_or_default();
                        let text = NOTES
                            .iter()
                            .find(|(n, _)| *n == name)
                            .map(|(_, t)| (*t).to_string())
                            .unwrap_or_else(|| format!("no note named `{name}`"));
                        Ok(ReadResourceResult::text(uri, text))
                    },
                ),
        )
        .completion_handler(|params| async move {
            let partial = params.argument.value;
            let candidates: Vec<String> = match &params.reference {
                CompletionReference::Prompt { name } if name == "greet" => {
                    ["Ada", "Alan", "Grace", "Linus"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect()
                }
                CompletionReference::Resource { uri } if uri == "note://{name}" => {
                    NOTES.iter().map(|(n, _)| n.to_string()).collect()
                }
                _ => Vec::new(),
            };
            Ok(CompleteResult::new(
                candidates
                    .into_iter()
                    .filter(|c| c.starts_with(&partial))
                    .collect::<Vec<_>>(),
            ))
        })
        .tool(
            ToolBuilder::new("echo")
                .description("Echo a message back")
                .annotations(local_read_only())
                .handler(|input: EchoInput| async move {
                    let text = match input.repeat {
                        1 => input.message,
                        n => std::iter::repeat_n(input.message.as_str(), n as usize)
                            .collect::<Vec<_>>()
                            .join(" "),
                    };
                    Ok(CallToolResult::text(text))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("about")
                .description("Notes about this demo server, in markdown")
                .annotations(local_read_only())
                .extractor_handler((), |RawArgs(_): RawArgs| async move {
                    Ok(CallToolResult::text(
                        "# mcp-repl demo\n\n\
                         A tiny in-process router for exploring the REPL.\n\n\
                         - `echo message=hi` echoes back, and `echo <Tab>` completes its arguments\n\
                         - `convert value=100 to=<Tab>` completes the enum values\n\
                         - `slow_add a=2 b=3 &` runs **task-augmented**\n\
                         - `scan steps=5` reports **progress** while it runs\n\
                         - `sign_in` asks *you* for the answers (elicitation)\n\
                         - `describe slow_add` shows the tool's schemas\n",
                    ))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("convert")
                .description("Convert a temperature between scales")
                .annotations(local_read_only())
                .handler(|input: ConvertInput| async move {
                    let celsius = match input.from {
                        Scale::Celsius => input.value,
                        Scale::Fahrenheit => (input.value - 32.0) * 5.0 / 9.0,
                        Scale::Kelvin => input.value - 273.15,
                    };
                    let out = match input.to {
                        Scale::Celsius => celsius,
                        Scale::Fahrenheit => celsius * 9.0 / 5.0 + 32.0,
                        Scale::Kelvin => celsius + 273.15,
                    };
                    Ok(CallToolResult::text(format!("{out:.2}")))
                })
                .build(),
        )
        .tool(
            ToolBuilder::new("slow_add")
                .description("Add two numbers, slowly")
                .task_support(TaskSupportMode::Optional)
                .annotations(local_read_only())
                .handler(|input: AddInput| async move {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    Ok(CallToolResult::text((input.a + input.b).to_string()))
                })
                .build(),
        )
        // Progress notifications need the request context, so this one takes
        // the typed input through an extractor rather than the plain handler.
        .tool(
            ToolBuilder::new("scan")
                .description("Scan slowly, reporting progress")
                .annotations(local_read_only())
                .extractor_handler(
                    (),
                    |ctx: Context, Json(input): Json<ScanInput>| async move {
                        let steps = input.steps.clamp(1, 20);
                        for step in 1..=steps {
                            ctx.report_progress(
                                f64::from(step),
                                Some(f64::from(steps)),
                                Some(&format!("scanned {step} of {steps}")),
                            )
                            .await;
                            tokio::time::sleep(Duration::from_millis(400)).await;
                        }
                        Ok(CallToolResult::text(format!("scanned {steps} items")))
                    },
                )
                .build(),
        )
        // The demo server otherwise always succeeds, which leaves two things
        // with no example: how a tool error renders, and that a failed task
        // fails the script that waited for it. Task-capable so both are
        // reachable from the one tool.
        //
        //   mcp-repl --demo -e fail                 # tool error, exit 3
        //   mcp-repl --demo -e 'fail &' -e wait     # failed task, exit 3
        .tool(
            ToolBuilder::new("fail")
                .description("Always fails (try `fail &` then `wait`)")
                .annotations(local_read_only())
                .task_support(TaskSupportMode::Optional)
                .extractor_handler((), |_ctx: Context, RawArgs(_): RawArgs| async move {
                    // An `isError` result rather than a transport failure:
                    // this is the shape a tool uses to say the work failed,
                    // and the one a server is most likely to return.
                    Ok(CallToolResult::error("the demo `fail` tool always fails"))
                })
                .build(),
        )
        // Elicitation: the server asks the operator for the values instead of
        // taking them as arguments.
        .tool(
            ToolBuilder::new("sign_in")
                .description("Ask you for credentials (elicitation demo)")
                // Task-capable so `sign_in &` parks in `input_required` and
                // `task <id> respond` has something to answer. A task runs
                // detached from the call that started it, so the handler
                // cannot ask the operator directly either way.
                .task_support(TaskSupportMode::Optional)
                // An MRTR handler, because the two lifecycles route a
                // server's question to the operator differently and this
                // tool has to work on both. See `sign_in_form`.
                .mrtr_handler(|ctx: RequestContext, _input: SignInInput| async move {
                    // A 2026-07-28 retry carries the answers the client
                    // collected for the requests returned below.
                    if let Some(responses) = ctx.input_responses() {
                        let answer = responses.values().find_map(|response| match response {
                            InputResponse::Elicit(result) => Some(result.clone()),
                            _ => None,
                        });
                        return Ok(RequestOutcome::Complete(CallToolResult::text(
                            describe_sign_in(answer.as_ref()),
                        )));
                    }
                    if !ctx.can_elicit() {
                        // Two ways to get here, and the answer is the same:
                        // 2026-07-28 has no server-initiated requests at all,
                        // and a task on either lifecycle runs detached from
                        // the call that could have carried one. Either way the
                        // question travels as an input request the client
                        // fulfils before the handler resumes (SEP-2322).
                        let mut requests = InputRequests::new();
                        requests.insert(
                            "credentials".to_string(),
                            InputRequest::Elicit(ElicitRequestParams::Form(sign_in_form())),
                        );
                        return Ok(RequestOutcome::input_required(
                            InputRequiredResult::with_requests(requests),
                        ));
                    }
                    // The stable lifecycle lets the server ask directly.
                    let answer = ctx.elicit_form(sign_in_form()).await?;
                    Ok(RequestOutcome::Complete(CallToolResult::text(
                        describe_sign_in(Some(&answer)),
                    )))
                })
                .build(),
        )
}

/// `sign_in` takes no arguments; the values come from the operator.
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SignInInput {}

/// What the demo asks for, in both lifecycles.
fn sign_in_form() -> tower_mcp::protocol::ElicitFormParams {
    tower_mcp::protocol::ElicitFormParams {
        mode: None,
        message: "The demo server would like to know who you are.".to_string(),
        requested_schema: tower_mcp::protocol::ElicitFormSchema::new()
            .string_field("username", Some("Any name will do"), true)
            .enum_field(
                "environment",
                Some("Which environment to sign in to"),
                vec!["staging".to_string(), "production".to_string()],
                false,
            )
            .boolean_field("remember_me", Some("Stay signed in"), false),
        meta: None,
    }
}

/// Render an elicitation answer the way the operator gave it.
fn describe_sign_in(answer: Option<&tower_mcp::protocol::ElicitResult>) -> String {
    use tower_mcp::protocol::ElicitAction;
    let Some(answer) = answer else {
        return "no answer".to_string();
    };
    match answer.action {
        ElicitAction::Accept => {
            let content = answer.content.clone().unwrap_or_default();
            let username = content
                .get("username")
                .and_then(|v| serde_json::to_value(v).ok())
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_else(|| "(nobody)".to_string());
            format!("signed in as {username}")
        }
        ElicitAction::Decline => "declined".to_string(),
        _ => "cancelled".to_string(),
    }
}

// Every field's doc comment below becomes the description the completion
// menu shows, and a field without a serde default becomes `required`, so a
// missing `message` is refused by the server instead of echoing nothing.
/// Arguments for echoing a message back.
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct EchoInput {
    /// The text to echo back.
    message: String,
    /// How many times to repeat it.
    #[serde(default = "one")]
    repeat: u8,
}

fn one() -> u8 {
    1
}

/// Two numbers to add.
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct AddInput {
    /// The first number.
    a: i64,
    /// The second number.
    b: i64,
}

/// How much scanning to pretend to do.
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ScanInput {
    /// How many steps to take, 1 to 20.
    #[serde(default = "five")]
    steps: u32,
}

fn five() -> u32 {
    5
}

// A unit-only enum becomes a JSON Schema `enum` in `$defs`, referenced from
// each field. That is what a real schema generator emits, and what the
// REPL's completion resolves through.
/// A temperature scale.
#[derive(serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
enum Scale {
    Celsius,
    Fahrenheit,
    Kelvin,
}

/// A temperature and the scales to convert between.
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ConvertInput {
    /// The number to convert.
    value: f64,
    /// The scale it is currently in.
    from: Scale,
    /// The scale to convert it to.
    to: Scale,
}

/// How long the event loop waits after a `list_changed` before refetching.
/// A server that renames a batch of tools emits one notification per change;
/// refetching per notification would issue four list calls each time. The
/// wait folds a burst into one refetch, and caps how much work a server can
/// induce by spamming the notification.
const SURFACE_REFRESH_DEBOUNCE: Duration = Duration::from_millis(250);

/// Signal that the surface changed. A counter over a watch channel rather
/// than a message per notification: only "something changed since we last
/// looked" matters, so any number of notifications arriving before the event
/// loop wakes collapse into a single refetch.
type RefreshSignal = Arc<tokio::sync::watch::Sender<u64>>;

fn note_surface_change(signal: &RefreshSignal) {
    signal.send_modify(|seen| *seen = seen.wrapping_add(1));
}

/// The notification callbacks: log and progress messages print inline,
/// `list_changed` notifications nudge the event loop to refresh the surface.
/// Built per client, since a reconnect installs a new one.
fn notification_handler(
    refresh: RefreshSignal,
    output: AsyncOutput,
    jobs: Arc<Jobs>,
) -> NotificationHandler {
    let t = refresh.clone();
    let r = refresh.clone();
    let p = refresh;
    NotificationHandler::new()
        .on_tools_changed(move || note_surface_change(&t))
        .on_resources_changed(move || note_surface_change(&r))
        .on_prompts_changed(move || note_surface_change(&p))
        .on_task_status_changed({
            let jobs = jobs.clone();
            move |params| jobs.observe_legacy(params)
        })
        .on_final_task_status_changed(move |params| jobs.observe_final(params))
        .on_progress({
            let output = output.clone();
            move |p| {
                let pct = match (p.progress, p.total) {
                    (done, Some(total)) if total > 0.0 => {
                        format!(" {:.0}%", 100.0 * done / total)
                    }
                    _ => String::new(),
                };
                output.line(format!(
                    "{} {}",
                    tag(Style::new().fg(Color::Cyan), &format!("progress{pct}")),
                    sanitize(p.message.as_deref().unwrap_or(""))
                ));
            }
        })
        // A subscribed resource changed. Printed inline like progress and log
        // lines; the content is not re-read, since a `read` may be expensive and
        // the point is to know it moved.
        .on_resource_updated({
            let output = output.clone();
            move |uri| {
                let known = if subscribe::contains(&uri) {
                    String::new()
                } else {
                    format!(" {}", paint(Style::new().dimmed(), "(not subscribed here)"))
                };
                output.line(format!(
                    "{} {}{known}",
                    tag(Style::new().fg(Color::Cyan), "resource updated"),
                    sanitize(&uri)
                ));
            }
        })
        .on_log_message(move |m| {
            output.line(format!(
                "{} {}",
                tag(log_level_style(m.level), &format!("log {}", m.level)),
                sanitize(&m.data.to_string())
            ));
        })
}

/// Forward complete child stderr lines through the prompt-safe output sink.
fn forward_child_stderr(stderr: tokio::process::ChildStderr, output: AsyncOutput) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                // The child is the MCP server; its stderr is as untrusted
                // as its frames.
                Ok(Some(line)) => output.line(sanitize(&line).into_owned()),
                Ok(None) => break,
                Err(error) => {
                    output.line(format!("warning: reading server stderr failed: {error}"));
                    break;
                }
            }
        }
    });
}

/// Watch one task until it reaches a terminal state. Final clients open a
/// task-scoped `subscriptions/listen` stream for immediate notifications; the
/// bounded polling loop remains authoritative for stable servers and for a
/// final notification that is unavailable or dropped.
fn watch_task(session: Arc<Session>, jobs: Arc<Jobs>, task_id: String, poll_interval: Option<u64>) {
    if !jobs.automatic_updates_enabled() || jobs.is_terminal(&task_id) {
        return;
    }
    tokio::spawn(async move {
        let client = session.client();
        let _subscription =
            if client.selected_protocol_version().await.as_deref() == Some("2026-07-28") {
                match client
                    .listen_subscriptions(SubscriptionFilter {
                        task_ids: Some(vec![task_id.clone()]),
                        ..Default::default()
                    })
                    .await
                {
                    Ok(mut handle) => match handle.acknowledged().await {
                        Ok(accepted)
                            if accepted
                                .task_ids
                                .as_ref()
                                .is_some_and(|ids| ids.iter().any(|id| id == &task_id)) =>
                        {
                            Some(handle)
                        }
                        _ => None,
                    },
                    Err(_) => None,
                }
            } else {
                None
            };
        let mut interval_ms = poll_interval.unwrap_or(1000).clamp(50, 30_000);
        let mut consecutive_errors = 0;
        loop {
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
            if jobs.is_terminal(&task_id) {
                break;
            }
            match session.client().task_get(&task_id).await {
                Ok(task) => {
                    consecutive_errors = 0;
                    interval_ms = task.poll_interval.unwrap_or(1000).clamp(50, 30_000);
                    let terminal = task.status.is_terminal();
                    jobs.observe_task(&task);
                    if terminal {
                        break;
                    }
                }
                Err(_) => {
                    consecutive_errors += 1;
                    if consecutive_errors >= 3 {
                        break;
                    }
                }
            }
        }
    });
}

/// The recipe for rebuilding an `--http` connection: a brand new transport
/// (so no dead `Mcp-Session-Id` is carried over), a fresh handler, and the
/// initialize handshake, exactly as at startup. The rebuilt transport is
/// wrapped in `TracingTransport` like the startup one, so `wire` and `last`
/// keep reporting frames after a reconnect, and it declares the same
/// capabilities as the startup client: a reconnect must not quietly leave the
/// session less capable than it began.
#[derive(Clone)]
struct OAuthRuntime {
    flow: OAuthAuthorizationFlow,
    scopes: Vec<String>,
}

fn http_transport(
    url: String,
    config: HttpClientConfig,
    oauth: Option<OAuthRuntime>,
) -> HttpClientTransport {
    let transport = HttpClientTransport::with_config(url, config);
    match oauth {
        Some(oauth) => transport.with_scope_aware_token_provider(
            oauth.flow,
            OAuthScopeEscalationConfig::new(oauth.scopes).max_attempts(2),
        ),
        None => transport,
    }
}

fn http_connector(
    url: String,
    config: HttpClientConfig,
    oauth: Option<OAuthRuntime>,
    make_handler: Arc<dyn Fn() -> ReplClientHandler + Send + Sync>,
    protocol: ProtocolMode,
) -> Connector {
    Box::new(move || {
        let (url, config, oauth, handler) =
            (url.clone(), config.clone(), oauth.clone(), make_handler());
        Box::pin(async move {
            let client = client_builder(protocol)
                .map_err(|error| tower_mcp::Error::Transport(error.to_string()))?
                .connect(
                    TracingTransport::new(http_transport(url, config, oauth)),
                    handler,
                )
                .await?;
            establish_connection(&client, protocol).await?;
            Ok(client)
        })
    })
}

/// Load the profile config, exiting with a usage status on a bad file. A
/// missing file at the default location is not an error: profiles are opt-in.
fn load_config(explicit: Option<&str>) -> config::Config {
    let Some((path, explicit)) = config::config_path(explicit) else {
        return config::Config::default();
    };
    match config::Config::load(&path, explicit) {
        Ok(c) => c,
        Err(e) => {
            exit_with_error(ExitStatus::Usage, &e);
        }
    }
}

async fn handle_oauth_profile_action(
    args: &Args,
    profiles: &config::Config,
    config_file: Option<&std::path::Path>,
) -> bool {
    let Some(name) = args.login.as_deref().or(args.logout.as_deref()) else {
        if !args.oauth_scopes.is_empty()
            || args.oauth_client_id_metadata_document.is_some()
            || args.oauth_authorization_server.is_some()
        {
            exit_with_error(
                ExitStatus::Usage,
                "--oauth-scope, --oauth-client-id-metadata-document, and \
                 --oauth-authorization-server apply only to --login",
            );
        }
        return false;
    };
    oauth_profile::validate_name(name)
        .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error));
    if args.demo
        || !args.command.is_empty()
        || !args.exec.is_empty()
        || args.list_servers
        || args.bearer.is_some()
        || !args.headers.is_empty()
        || args.oauth.is_some()
    {
        exit_with_error(
            ExitStatus::Usage,
            "--login/--logout are standalone credential operations; do not combine them with \
             a command, --demo, --exec, --list-servers, --bearer, --header, or --oauth \
             (--json is allowed, and reports what was created)",
        );
    }
    let path = config_file.unwrap_or_else(|| {
        exit_with_error(
            ExitStatus::Usage,
            "no config file location is available; set HOME/XDG_CONFIG_HOME or pass --config",
        )
    });

    if args.logout.is_some() {
        let store = oauth_profile::CredentialStore::keyring(name)
            .unwrap_or_else(|error| exit_with_error(ExitStatus::Auth, &error));
        store
            .clear()
            .await
            .unwrap_or_else(|error| exit_with_error(ExitStatus::Auth, &error));
        oauth_profile::remove_metadata(path, name)
            .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error));
        if json_output() {
            print_json(&serde_json::json!({
                "profile": name,
                "removed": true,
            }));
        } else {
            println!("removed OAuth profile {name:?} and its stored credentials");
        }
        return true;
    }

    let existing = profiles.oauth.get(name).cloned().unwrap_or_default();
    let server_url = args.server.as_deref().map(|server_name| {
        let profile = profiles
            .profile(server_name)
            .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error));
        match profile.transport() {
            Ok(config::Transport::Http) => profile
                .url
                .clone()
                .or_else(|| {
                    profile
                        .oauth
                        .as_deref()
                        .and_then(|oauth| profiles.oauth.get(oauth))
                        .map(|metadata| metadata.url.clone())
                })
                .unwrap_or_else(|| {
                    exit_with_error(
                        ExitStatus::Usage,
                        &format!("server profile {server_name:?} has no HTTP URL"),
                    )
                }),
            Ok(config::Transport::Stdio) => exit_with_error(
                ExitStatus::Usage,
                &format!("server profile {server_name:?} is stdio; OAuth requires HTTP"),
            ),
            Err(error) => exit_with_error(ExitStatus::Usage, &error),
        }
    });
    let url = args
        .http
        .clone()
        .or(server_url)
        .or_else(|| (!existing.url.is_empty()).then(|| existing.url.clone()))
        .unwrap_or_else(|| {
            exit_with_error(
                ExitStatus::Usage,
                "a new OAuth profile needs --http URL (or --server with an HTTP profile)",
            )
        });
    let scopes = if args.oauth_scopes.is_empty() {
        existing.scopes
    } else {
        args.oauth_scopes
            .iter()
            .flat_map(|scope| scope.split_ascii_whitespace())
            .map(str::to_string)
            .fold(Vec::new(), |mut scopes, scope| {
                if !scope.is_empty() && !scopes.contains(&scope) {
                    scopes.push(scope);
                }
                scopes
            })
    };
    let metadata = config::OAuthProfile {
        url: url.clone(),
        scopes,
        client_id_metadata_document: args
            .oauth_client_id_metadata_document
            .clone()
            .or(existing.client_id_metadata_document),
        authorization_server: args
            .oauth_authorization_server
            .clone()
            .or(existing.authorization_server),
    };
    let (flow, store) = oauth_profile::build_flow(name, &url, &metadata, true, !args.no_browser)
        .unwrap_or_else(|error| exit_with_error(ExitStatus::Auth, &error));
    if let Err(error) = flow.authorize(metadata.scopes.clone()).await {
        if matches!(error, OAuthClientError::TokenRequest(_)) {
            store
                .clear_tokens()
                .await
                .unwrap_or_else(|store_error| exit_with_error(ExitStatus::Auth, &store_error));
            let (retry, _) =
                oauth_profile::build_flow(name, &url, &metadata, true, !args.no_browser)
                    .unwrap_or_else(|build_error| exit_with_error(ExitStatus::Auth, &build_error));
            retry
                .authorize(metadata.scopes.clone())
                .await
                .unwrap_or_else(|retry_error| {
                    exit_with_error(ExitStatus::Auth, &retry_error.to_string())
                });
        } else {
            exit_with_error(ExitStatus::Auth, &error.to_string());
        }
    }
    if let Err(error) = oauth_profile::save_metadata(path, name, &metadata) {
        let _ = store.clear().await;
        exit_with_error(ExitStatus::Usage, &error);
    }
    if json_output() {
        print_json(&saved_profile_json(name, &metadata));
    } else {
        println!(
            "saved OAuth profile {name:?}; credentials are in the operating-system credential store"
        );
    }
    true
}

/// What `--login --json` reports about the profile it saved.
///
/// The scopes are the ones actually recorded rather than the ones asked for,
/// which is the point of asking: a provisioning script needs to know what it
/// ended up with. No secret appears here, and none can: the tokens live in
/// the operating-system credential store and this side never holds them.
fn saved_profile_json(name: &str, metadata: &config::OAuthProfile) -> serde_json::Value {
    serde_json::json!({
        "profile": name,
        "serverUrl": metadata.url,
        "scopes": metadata.scopes,
    })
}

/// The binary name a generated script or man page refers to. Taken from the
/// clap command rather than `argv[0]`, so a script generated through
/// `cargo run` still names the installed binary.
fn program_name() -> String {
    <Args as clap::CommandFactory>::command()
        .get_name()
        .to_string()
}

/// `--completions <shell>`: a completion script on stdout.
fn print_completions(shell: clap_complete::Shell) {
    let mut command = <Args as clap::CommandFactory>::command();
    let name = program_name();
    clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
}

/// `--man`: the man page, in roff, on stdout.
fn print_man() {
    let command = <Args as clap::CommandFactory>::command();
    let mut page = Vec::new();
    if let Err(error) = clap_mangen::Man::new(command).render(&mut page) {
        exit_with_error(
            ExitStatus::Usage,
            &format!("could not render the man page: {error}"),
        );
    }
    use std::io::Write;
    if let Err(error) = std::io::stdout().write_all(&page) {
        exit_with_error(
            ExitStatus::Usage,
            &format!("could not write the man page: {error}"),
        );
    }
}

/// `--scan`: what other MCP clients have configured, as selectors.
///
/// Discovery on purpose stops at describing. Automatic connection would make
/// the source of a session invisible, which is the reason `PATH:ENTRY` is
/// explicit in the first place; this just saves typing the path.
fn print_scan() -> ExitStatus {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let paths = import_config::candidate_paths(&cwd, home.as_deref());
    let scanned = import_config::scan(&paths);

    if json_output() {
        let files: Vec<serde_json::Value> = scanned
            .iter()
            .map(|file| match &file.result {
                Ok(entries) => serde_json::json!({
                    "path": file.path.display().to_string(),
                    "entries": entries.iter().map(|entry| serde_json::json!({
                        "entry": entry.name,
                        "selector": format!("{}:{}", file.path.display(), entry.name),
                        "transport": entry.transport,
                        "summary": entry.summary,
                    })).collect::<Vec<_>>(),
                }),
                Err(error) => serde_json::json!({
                    "path": file.path.display().to_string(),
                    "error": error,
                }),
            })
            .collect();
        let found = scanned
            .iter()
            .filter_map(|file| file.result.as_ref().ok())
            .map(Vec::len)
            .sum::<usize>();
        print_json(&serde_json::Value::Array(files));
        return no_match_when_empty(found);
    }

    if scanned.is_empty() {
        // grep's convention, so a script can test for "nothing configured".
        report_error(
            ExitStatus::NoMatch,
            "no MCP client configs found (looked for .mcp.json, .vscode/mcp.json, \
             .cursor/mcp.json, and the Claude configs under your home directory)",
        );
        return ExitStatus::NoMatch;
    }

    let mut total = 0usize;
    for file in &scanned {
        println!(
            "{}",
            paint(Style::new().bold(), &file.path.display().to_string())
        );
        match &file.result {
            // A file that cannot be read is reported but does not decide the
            // status: what the caller asked is whether anything was found.
            Err(error) => println!("  {} {}", style::error_prefix(), sanitize(error)),
            Ok(entries) if entries.is_empty() => {
                println!("  {}", paint(Style::new().dimmed(), "(no servers)"));
            }
            Ok(entries) => {
                let width = entries.iter().map(|e| e.name.len()).max().unwrap_or(0);
                for entry in entries {
                    total += 1;
                    println!(
                        "  {}  {} {}",
                        style::column(Style::new().fg(Color::Green), &sanitize(&entry.name), width),
                        paint(Style::new().dimmed(), &format!("{:>5}", entry.transport)),
                        sanitize(&entry.summary)
                    );
                }
            }
        }
    }
    if total > 0 {
        println!(
            "{}",
            paint(
                Style::new().dimmed(),
                &format!(
                    "{} in {}. Connect with `mcp-repl <path>:<entry>`.",
                    plural(total, "server"),
                    plural(scanned.len(), "file")
                )
            )
        );
    }
    no_match_when_empty(total)
}

/// Nothing found is a no-match, the way `find` reports one, so a script can
/// branch on it.
fn no_match_when_empty(found: usize) -> ExitStatus {
    if found == 0 {
        ExitStatus::NoMatch
    } else {
        ExitStatus::Success
    }
}

/// `--list-servers`: the configured profiles, one per line.
fn print_servers(config: &config::Config) {
    if config.servers.is_empty() {
        println!("no server profiles configured");
        return;
    }
    let width = config.names().iter().map(|n| n.len()).max().unwrap_or(0);
    for (name, profile) in &config.servers {
        println!(
            "{}  {}",
            style::column(Style::new().fg(Color::Cyan), name, width),
            paint(Style::new().dimmed(), &profile.summary()),
        );
    }
}

/// Resolve the profile the invocation names, if any: `--server <name>`, or a
/// bare single positional that matches a configured profile. A positional that
/// matches nothing stays a stdio command, so spawning a server by bare name
/// still works when no profile shadows it.
fn resolve_profile(args: &Args, config: &config::Config) -> Option<(String, config::Connection)> {
    let name = args
        .server
        .clone()
        .or_else(|| match args.command.as_slice() {
            [only] if config.servers.contains_key(only) => Some(only.clone()),
            _ => None,
        })?;
    let profile = match config.profile(&name) {
        Ok(p) => p,
        Err(e) => {
            exit_with_error(ExitStatus::Usage, &e);
        }
    };
    if profile.bearer.is_some() {
        eprintln!(
            "warning: profile {name:?} stores a literal `bearer` token; prefer \
             `bearer_env = \"VAR\"` so the token is not kept in the config file"
        );
    }
    match config.resolve_profile_with(&name, |var| std::env::var(var).ok()) {
        Ok(connection) => Some((name, connection)),
        Err(e) => {
            exit_with_error(ExitStatus::Usage, &format!("server profile {name:?}: {e}"));
        }
    }
}

/// Resolve an explicit `PATH:ENTRY` selector from `--server` or a lone
/// positional. Imported JSON is intentionally opt-in; ordinary commands and
/// native profile names retain their existing interpretation.
fn resolve_import(args: &Args) -> Option<import_config::ImportedConnection> {
    let candidate = match args.server.as_deref() {
        Some(server) => server,
        None => match args.command.as_slice() {
            [only] => only,
            _ => return None,
        },
    };
    let selector = match import_config::parse_selector(candidate)? {
        Ok(selector) => selector,
        Err(error) => exit_with_error(ExitStatus::Usage, &error),
    };
    Some(
        import_config::load_with(selector, |variable| std::env::var(variable).ok())
            .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error)),
    )
}

/// The levels `logging/setLevel` accepts, in the spec's order of severity.
/// Ordered rather than alphabetical, so the completion menu reads as a scale.
pub(crate) const LOG_LEVELS: &[&str] = &[
    "debug",
    "info",
    "notice",
    "warning",
    "error",
    "critical",
    "alert",
    "emergency",
];

fn parse_log_level(word: &str) -> Option<LogLevel> {
    match word.to_ascii_lowercase().as_str() {
        "debug" => Some(LogLevel::Debug),
        "info" => Some(LogLevel::Info),
        "notice" => Some(LogLevel::Notice),
        "warning" => Some(LogLevel::Warning),
        "error" => Some(LogLevel::Error),
        "critical" => Some(LogLevel::Critical),
        "alert" => Some(LogLevel::Alert),
        "emergency" => Some(LogLevel::Emergency),
        _ => None,
    }
}

fn log_level_style(level: LogLevel) -> Style {
    match level {
        LogLevel::Emergency | LogLevel::Alert | LogLevel::Critical | LogLevel::Error => {
            Style::new().fg(Color::Red)
        }
        LogLevel::Warning => Style::new().fg(Color::Yellow),
        LogLevel::Notice | LogLevel::Info => Style::new().fg(Color::Green),
        _ => Style::new().dimmed(),
    }
}

/// Parse the process arguments and run the published `mcp-repl` CLI.
///
/// The binary target intentionally delegates straight here so the application
/// lifecycle remains testable and the package can be extracted without moving
/// its implementation back into a monolithic executable.
#[tokio::main]
pub async fn run_cli() {
    // Parsed first: the subscriber has to know what --color decided, and
    // building it beforehand meant `--color never` still emitted escape
    // sequences into a stderr a script was told would be plain.
    let args = Args::parse();
    init_tracing(&args);

    // Generators write to stdout and exit. They are checked before anything
    // else so a packaging script never needs a config file, a server, or a
    // terminal to produce them.
    if let Some(shell) = args.completions {
        print_completions(shell);
        return;
    }
    if args.man {
        print_man();
        return;
    }

    style::init(args.color);
    wire::init(args.trace);
    JSON_OUTPUT.store(args.json, Ordering::Relaxed);

    if let Err(error) = run(args).await {
        exit_with_error(
            ExitStatus::from_mcp_error(&error),
            collapse_repeated_label(&error.to_string()),
        );
    }
}

async fn run(args: Args) -> tower_mcp::Result<()> {
    // Server profiles are read up front: both --list-servers and profile
    // resolution need them before anything connects.
    let config_file = config::config_path(args.config.as_deref()).map(|(path, _)| path);
    let profiles = if args.login.is_some() || args.logout.is_some() {
        config_file
            .as_deref()
            .map(|path| {
                config::Config::load(path, false)
                    .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error))
            })
            .unwrap_or_default()
    } else {
        load_config(args.config.as_deref())
    };
    // Resolved here rather than at startup because the config has to be read
    // first. The flag wins, then the config, then the built-in default; the
    // flag stays an `Option` precisely so an explicit `--timeout 0` is
    // distinguishable from not having asked for one.
    REQUEST_TIMEOUT_SECS.store(
        args.timeout
            .or(profiles.repl.request_timeout)
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS),
        Ordering::Relaxed,
    );
    editor::set_completion_timeout(
        profiles
            .repl
            .completion_timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(editor::DEFAULT_COMPLETION_TIMEOUT),
    );

    if handle_oauth_profile_action(&args, &profiles, config_file.as_deref()).await {
        return Ok(());
    }
    if args.list_servers {
        print_servers(&profiles);
        return Ok(());
    }
    if args.scan {
        std::process::exit(print_scan().code());
    }
    let schema_contracts =
        schema_contract::ContractSet::load(&args.schema_contracts, args.schema_mode)
            .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error));
    let imported = resolve_import(&args);
    let profile = if imported.is_none() {
        resolve_profile(&args, &profiles)
    } else {
        None
    };
    // --exec runs commands and exits; suppress the banner and surface listing
    // unless --verbose, so scripted output is only the command results.
    let one_shot = !args.exec.is_empty();
    // JSON stdout is a machine-readable stream. Even `--verbose` must not
    // inject a human banner into it.
    let quiet = one_shot && (!args.verbose || args.json);

    // True while the reedline editor owns the terminal; the elicitation
    // handler declines form requests during that window instead of
    // fighting over raw-mode stdin.
    let at_prompt = Arc::new(AtomicBool::new(false));
    let async_output = AsyncOutput::new(at_prompt.clone(), !one_shot);
    // Automatic task transitions are an interactive convenience. `--exec`
    // and `--json` retain deterministic output; their manual task commands
    // remain authoritative.
    let jobs = Arc::new(Jobs::new(
        async_output.clone(),
        automatic_task_updates(one_shot, args.json),
    ));

    // Notifications print inline and trigger surface refreshes.
    // The sender is held for the life of the process: `changed()` errors once
    // every sender is gone, which in a select loop would spin rather than
    // wait.
    let (refresh_tx, mut refresh_rx) = tokio::sync::watch::channel(0u64);
    let refresh_tx: RefreshSignal = Arc::new(refresh_tx);

    // Filled in once the handshake reports who answered, so an elicitation
    // can say which server is asking. Shared with every handler the factory
    // builds, including the ones a reconnect installs.
    let server_label: elicit::ServerLabel = Arc::new(RwLock::new(String::new()));

    // A reconnect needs a fresh handler for the new client, so build handlers
    // through a factory rather than once.
    let make_handler: Arc<dyn Fn() -> ReplClientHandler + Send + Sync> = {
        let refresh_tx = refresh_tx.clone();
        let at_prompt = at_prompt.clone();
        let async_output = async_output.clone();
        let jobs = jobs.clone();
        let server_label = server_label.clone();
        Arc::new(move || {
            ReplClientHandler::new(
                notification_handler(refresh_tx.clone(), async_output.clone(), jobs.clone()),
                at_prompt.clone(),
                server_label.clone(),
                async_output.clone(),
            )
        })
    };
    // Sampling has no model behind it, so the operator answers. Under --exec
    // there is nobody to ask, so requests are refused unless --sampling says
    // otherwise.
    sampling::init(sampling::resolve(args.sampling, one_shot));
    // Elicitation is the same bargain: a form needs someone at the keyboard,
    // so a script refuses rather than blocking on a read nobody will answer.
    elicit::init(elicit::resolve(args.elicitation, one_shot));

    // Explicit flags override imported or native profile fields: --http
    // retargets the URL while keeping HTTP auth, and --bearer/--header are
    // layered on in build_http_config.
    let (profile_name, import_label, import_selector, connection) = match (imported, profile) {
        (Some(imported), _) => (
            None,
            Some(imported.label()),
            Some(imported.selector),
            Some(imported.connection),
        ),
        (None, Some((name, connection))) => (Some(name), None, None, Some(connection)),
        (None, None) => (None, None, None, None),
    };
    // Spawning an imported entry needs the config location for the approval
    // store, and the alias table takes ownership of it below.
    let trust_store_config = config_file.clone();

    // Aliases come from the same file as the profiles: the global table plus
    // the connected profile's own, which shadows it.
    let aliases = Arc::new(RwLock::new(Aliases::new(
        profiles.aliases.clone(),
        profile_name
            .as_ref()
            .and_then(|name| profiles.servers.get(name))
            .map(|p| p.aliases.clone())
            .unwrap_or_default(),
        profile_name.clone(),
        config_file,
    )));

    let connection = match (args.http.clone(), connection) {
        (
            Some(url),
            Some(config::Connection::Http {
                bearer,
                headers,
                oauth,
                ..
            }),
        ) => Some(config::Connection::Http {
            url,
            bearer,
            headers,
            oauth,
        }),
        (Some(url), _) => Some(config::Connection::Http {
            url,
            bearer: None,
            headers: Vec::new(),
            oauth: None,
        }),
        (None, Some(c)) => Some(c),
        (None, None) if args.command.is_empty() && args.oauth.is_some() => {
            let name = args.oauth.as_deref().expect("guarded above");
            let metadata = profiles.oauth.get(name).unwrap_or_else(|| {
                exit_with_error(
                    ExitStatus::Usage,
                    &format!("no OAuth profile named {name:?}; create it with --login"),
                )
            });
            Some(config::Connection::Http {
                url: metadata.url.clone(),
                bearer: None,
                headers: Vec::new(),
                oauth: Some(name.to_string()),
            })
        }
        (None, None) if !args.command.is_empty() => Some(config::Connection::Stdio {
            command: args.command.clone(),
            env: std::collections::BTreeMap::new(),
            cwd: None,
        }),
        (None, None) => None,
    };

    let over_http = matches!(connection, Some(config::Connection::Http { .. }));
    if !over_http && (args.bearer.is_some() || !args.headers.is_empty()) {
        eprintln!("warning: --bearer/--header apply only to HTTP servers; ignoring them here");
    }
    if !over_http && args.oauth.is_some() {
        exit_with_error(ExitStatus::Usage, "--oauth applies only to HTTP servers");
    }
    if let Some(name) = &profile_name
        && !quiet
    {
        println!(
            "{}",
            tag(Style::new().fg(Color::Cyan), &format!("profile {name}"))
        );
    } else if let Some(label) = &import_label
        && !quiet
    {
        println!(
            "{}",
            tag(Style::new().fg(Color::Cyan), &format!("import {label}"))
        );
    }

    // Every transport is wrapped, whatever `--trace` says: the wrapper is what
    // records the exchange `last` reprints, and tracing can be switched on
    // mid-session with `wire on`.
    // Sampling is advertised whatever the strategy: a client is allowed to
    // refuse an individual request, and a server can only ask when the
    // capability is declared, so `--sampling decline` still exercises the
    // server's rejection path.
    let builder = client_builder(args.protocol)
        .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error.to_string()));
    // Only `--http` can be resurrected. A stdio child that dies takes its
    // stdin and stdout with it (respawning it is a separate concern), and the
    // in-process demo router cannot lose a session at all.
    let mut connector: Option<Connector> = None;
    let client = if args.demo {
        tracing::debug!("connecting to the in-process demo server");
        builder
            .connect(
                TracingTransport::new(ChannelTransport::new(demo_router())),
                make_handler(),
            )
            .await?
    } else {
        match connection {
            Some(config::Connection::Http {
                url,
                bearer,
                headers,
                oauth: profile_oauth,
            }) => {
                let oauth_name = selected_oauth_profile(
                    args.oauth.as_deref(),
                    profile_oauth.as_deref(),
                    args.bearer.is_some(),
                    &args.headers,
                );
                let cli_authorization = oauth_name.is_none()
                    && (args.bearer.is_some()
                        || args.headers.iter().any(|header| {
                            header.split_once(':').is_some_and(|(name, _)| {
                                name.trim().eq_ignore_ascii_case("authorization")
                            })
                        }));
                if cli_authorization && (args.oauth.is_some() || profile_oauth.is_some()) && !quiet
                {
                    eprintln!(
                        "warning: explicit --bearer/--header Authorization takes precedence over OAuth"
                    );
                }
                let profile_headers = if oauth_name.is_some() {
                    headers
                        .into_iter()
                        .filter(|(name, _)| !name.eq_ignore_ascii_case("authorization"))
                        .collect::<Vec<_>>()
                } else {
                    headers
                };
                let config = if oauth_name.is_some() {
                    build_http_config_with_env(
                        args.bearer.clone(),
                        &args.headers,
                        None,
                        &profile_headers,
                        None,
                    )
                } else {
                    build_http_config(args.bearer.clone(), &args.headers, bearer, &profile_headers)
                }
                .unwrap_or_else(|error| exit_with_error(ExitStatus::Usage, &error));
                let oauth = if let Some(name) = oauth_name {
                    let metadata = profiles.oauth.get(&name).unwrap_or_else(|| {
                        exit_with_error(
                            ExitStatus::Usage,
                            &format!(
                                "no OAuth profile named {name:?}; create it with \
                                 `mcp-repl --login {name} --http {url}`"
                            ),
                        )
                    });
                    let interactive = !one_shot && !args.json;
                    let (flow, store) = oauth_profile::build_flow(
                        &name,
                        &url,
                        metadata,
                        interactive,
                        interactive && !args.no_browser,
                    )
                    .unwrap_or_else(|error| exit_with_error(ExitStatus::Auth, &error));
                    if interactive {
                        tracing::debug!(profile = %name, "OAuth: interactive authorization");
                        flow.authorize(metadata.scopes.clone())
                            .await
                            .map_err(|error| {
                                tower_mcp::Error::Transport(format!(
                                    "OAuth authorization failed for profile {name:?}: {error}. \
                                     Run `mcp-repl --login {name} --http {url}` to reauthorize"
                                ))
                            })?;
                    } else {
                        if !store.has_tokens().await.map_err(|error| {
                            tower_mcp::Error::Transport(format!(
                                "OAuth credential restore failed for profile {name:?}: {error}"
                            ))
                        })? {
                            return Err(tower_mcp::Error::Transport(format!(
                                "OAuth login required for profile {name:?}; run \
                                 `mcp-repl --login {name} --http {url}` before using --exec/--json"
                            )));
                        }
                        match flow.begin(metadata.scopes.clone()).await.map_err(|error| {
                            tower_mcp::Error::Transport(format!(
                                "OAuth credential restore failed for profile {name:?}: {error}. \
                                 Run `mcp-repl --login {name} --http {url}` to reauthorize"
                            ))
                        })? {
                            OAuthAuthorizationStart::Authorized { .. } => {
                                tracing::debug!(
                                    profile = %name,
                                    "OAuth: restored a stored credential"
                                );
                            }
                            OAuthAuthorizationStart::Pending(_) => {
                                return Err(tower_mcp::Error::Transport(format!(
                                    "OAuth login required for profile {name:?}; run \
                                     `mcp-repl --login {name} --http {url}` before using --exec/--json"
                                )));
                            }
                            _ => {
                                return Err(tower_mcp::Error::Transport(format!(
                                    "OAuth login required for profile {name:?}; run \
                                     `mcp-repl --login {name} --http {url}` before using --exec/--json"
                                )));
                            }
                        }
                    }
                    Some(OAuthRuntime {
                        flow,
                        scopes: metadata.scopes.clone(),
                    })
                } else {
                    None
                };
                if !args.no_reconnect {
                    connector = Some(http_connector(
                        url.clone(),
                        config.clone(),
                        oauth.clone(),
                        make_handler.clone(),
                        args.protocol,
                    ));
                }
                builder
                    .connect(
                        TracingTransport::new(http_transport(url, config, oauth)),
                        make_handler(),
                    )
                    .await?
            }
            Some(config::Connection::Stdio { command, env, cwd }) => {
                // An imported entry is code from somewhere else: show what it
                // resolved to and get approval before running it. A native
                // profile is the user's own config file, so it is not gated.
                if let Some(selector) = &import_selector {
                    let plan = import_trust::SpawnPlan::new(
                        &selector.path,
                        &selector.entry,
                        &command,
                        cwd.as_deref(),
                        &env,
                    );
                    let interactive =
                        !one_shot && std::io::IsTerminal::is_terminal(&std::io::stdin());
                    match import_trust::authorize(
                        &plan,
                        trust_store_config.as_deref(),
                        args.trust_import,
                        interactive,
                    ) {
                        import_trust::Decision::Approved => {}
                        import_trust::Decision::Refused(reason) => {
                            exit_with_error(ExitStatus::Usage, &reason);
                        }
                    }
                }
                let mut cmd = tokio::process::Command::new(&command[0]);
                cmd.args(&command[1..]);
                cmd.envs(env);
                // The child inherits this process's environment, which is
                // usually what a stdio server wants. MCP_BEARER is the
                // exception: it is an HTTP credential by construction, and
                // nothing reached over stdio has any use for it.
                cmd.env_remove("MCP_BEARER");
                if let Some(cwd) = cwd {
                    cmd.current_dir(cwd);
                }
                cmd.stderr(std::process::Stdio::piped());
                let mut transport = StdioClientTransport::spawn_command(&mut cmd).await?;
                if let Some(stderr) = transport.take_stderr() {
                    forward_child_stderr(stderr, async_output.clone());
                }
                builder
                    .connect(TracingTransport::new(transport), make_handler())
                    .await?
            }
            None => {
                exit_with_error(
                    ExitStatus::Usage,
                    "usage: mcp-repl <server command...> | --http <url> | \
                     --server <name> | --demo",
                );
            }
        }
    };

    let info = establish_connection(&client, args.protocol).await?;
    let server_name = info.server_info.name.clone();
    if let Ok(mut label) = server_label.write() {
        label.clone_from(&server_name);
    }
    if !quiet {
        print_banner(&info);
    }
    let session = Arc::new(Session::new(client, connector));
    let client = session.client();

    let surface = Arc::new(RwLock::new(fetch_surface_initial(&client).await));
    if !quiet {
        let s = surface.read().unwrap();
        print_counts(&s);
        // List the tools at startup so the surface is browsable immediately,
        // unless the server already enumerated them in its instructions (some
        // servers dump the whole surface there); then it would just repeat.
        let instructions_list_tools = info
            .instructions
            .as_deref()
            .is_some_and(|instr| s.tools.first().is_some_and(|t| instr.contains(&t.name)));
        if !instructions_list_tools {
            print_tool_overview(&s);
        }
        // Only for an interactive session: none of it applies to `--exec`,
        // and it would be noise ahead of a data stream.
        if !one_shot {
            print_first_run_hint();
        }
    }

    // One-shot: run each --exec command in order, then exit non-zero if any
    // errored. No editor, no event loop.
    if one_shot {
        for cmd in &args.exec {
            match run_cancellable(
                &session,
                &surface,
                &aliases,
                &jobs,
                &schema_contracts,
                cmd.trim(),
            )
            .await
            {
                Ran::Completed(false) => {}
                // A quit command, or an interrupt: either way the rest of
                // the sequence does not run.
                Ran::Completed(true) | Ran::Cancelled => break,
            }
        }
        let status = exit_status::current().code();
        drop(client);
        // Every command has finished, so nothing should still hold the
        // session. If something does, a background task outlived its command,
        // and skipping the orderly shutdown is a far better answer than
        // panicking after the work already succeeded: the child is killed on
        // drop either way, and the status the commands earned survives.
        match Arc::try_unwrap(session) {
            Ok(session) => session.shutdown().await?,
            Err(_) => eprintln!(
                "warning: a background task outlived its command; exiting without the orderly \
                 shutdown"
            ),
        }
        std::process::exit(status);
    }

    // Final list-change notifications are subscription-scoped. Start the
    // long-lived stream only for an interactive final connection, after the
    // initial surface fetch; stable notifications already arrive directly,
    // and one-shot output must remain deterministic.
    let _surface_subscription = (args.protocol == ProtocolMode::Final).then(|| {
        surface_subscription::SurfaceSubscription::start(session.clone(), async_output.clone())
    });

    // History size is a REPL setting, not a per-server one, so it comes from
    // the config's `[repl]` table rather than a flag.
    let history_capacity = profiles
        .repl
        .history_capacity
        .unwrap_or(editor::DEFAULT_HISTORY_CAPACITY);

    // Readline runs on its own thread; lines cross into async via channels.
    let (line_tx, mut line_rx) = tokio::sync::mpsc::channel::<String>(1);
    let (ack_tx, ack_rx) = std::sync::mpsc::channel::<()>();
    editor::spawn_readline_thread(
        server_name,
        surface.clone(),
        session.clone(),
        aliases.clone(),
        tokio::runtime::Handle::current(),
        line_tx,
        ack_rx,
        at_prompt,
        async_output
            .external_printer()
            .expect("interactive sessions have an external printer"),
        !args.no_history && history_capacity > 0,
        history_capacity,
    );

    loop {
        tokio::select! {
            Ok(()) = refresh_rx.changed() => {
                // Let a burst land before refetching, then mark whatever
                // arrived during the wait as covered by this refetch.
                tokio::time::sleep(SURFACE_REFRESH_DEBOUNCE).await;
                refresh_rx.mark_unchanged();
                tracing::debug!("surface change signalled; re-fetching");
                let fresh = fetch_surface(&session.client()).await;
                async_output.line(format!("{} {}, {}, {}",
                    tag(Style::new().fg(Color::Cyan), "surface changed"),
                    plural(fresh.tools.len(), "tool"),
                    plural(fresh.prompts.len(), "prompt"),
                    plural(fresh.resources.len(), "resource")));
                *surface.write().unwrap() = fresh;
            }
            maybe_line = line_rx.recv() => {
                let Some(line) = maybe_line else { break };
                let ran = run_cancellable(
                    &session,
                    &surface,
                    &aliases,
                    &jobs,
                    &schema_contracts,
                    line.trim(),
                )
                .await;
                // The editor is parked on this ack, so it has to be sent
                // whether the command finished or was interrupted, or the
                // prompt never comes back.
                let _ = ack_tx.send(());
                if matches!(ran, Ran::Completed(true)) {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// What ended a command.
enum Ran {
    /// It finished on its own. The flag is `handle_line`'s quit signal.
    Completed(bool),
    /// The operator pressed Ctrl-C.
    Cancelled,
}

/// Run one command, abandoning it if the operator interrupts.
///
/// Ctrl-C at the prompt never reaches here: reedline holds the terminal in
/// raw mode and takes the keypress itself. It only arrives while a command
/// owns the terminal, which is exactly when there is something to abandon.
/// Installing a handler at all is what keeps SIGINT from killing the process
/// and skipping session shutdown.
///
/// Dropping the command future is the cancellation: the request is no longer
/// awaited and the REPL takes the next line. The server is not told to stop
/// (see the note in the module docs), so a tool with side effects may still
/// be running on the other end.
/// The tool a cancelled line was calling, when it could have been a task.
///
/// Only for the hint after an interrupt. A line that already ends in `&` is
/// a task and returned long ago, and a built-in is not a tool, so neither
/// gets one.
fn backgroundable_tool(surface: &Arc<RwLock<Surface>>, line: &str) -> Option<String> {
    let line = line.trim();
    if line.ends_with('&') {
        return None;
    }
    let word = line.split_whitespace().next()?;
    if BUILTINS.iter().any(|(name, _)| *name == word) {
        return None;
    }
    let surface = surface.read().ok()?;
    let tool = surface.tools.iter().find(|tool| tool.name == word)?;
    tool_tags(tool)
        .contains(&"task-capable")
        .then(|| tool.name.clone())
}

async fn run_cancellable(
    session: &Arc<Session>,
    surface: &Arc<RwLock<Surface>>,
    aliases: &Arc<RwLock<Aliases>>,
    jobs: &Arc<Jobs>,
    schema_contracts: &schema_contract::ContractSet,
    line: &str,
) -> Ran {
    tokio::select! {
        biased;
        quit = handle_line(session, surface, aliases, jobs, schema_contracts, line) => {
            Ran::Completed(quit)
        }
        _ = tokio::signal::ctrl_c() => {
            note_error(ExitStatus::Cancelled);
            if json_output() {
                print_json(&error_json(ExitStatus::Cancelled, "cancelled"));
            } else {
                // stderr: an interrupted command produced no result, and in
                // human --exec mode stdout is the data stream.
                let mut message = format!("{} cancelled", paint(Style::new().dimmed(), "^C"));
                // Interrupting a long call usually means it should have been
                // backgrounded. The REPL cannot retrieve this one, since a
                // plain call leaves no task behind to adopt, but it can say
                // what to type next time.
                if let Some(tool) = backgroundable_tool(surface, line) {
                    message.push_str(&paint(
                        Style::new().dimmed(),
                        &format!("  `{tool} ... &` runs it as a task instead"),
                    ));
                }
                eprintln!("{message}");
            }
            Ran::Cancelled
        }
    }
}

async fn handle_line(
    session: &Arc<Session>,
    surface: &Arc<RwLock<Surface>>,
    aliases: &Arc<RwLock<Aliases>>,
    jobs: &Arc<Jobs>,
    schema_contracts: &schema_contract::ContractSet,
    line: &str,
) -> bool {
    if line.is_empty() {
        if json_output() {
            report_error(ExitStatus::Usage, "empty command");
        }
        return false;
    }
    // Aliases expand before anything else looks at the line, so an expansion
    // can carry arguments, a trailing `&`, or another alias. An alias can
    // never be named after a built-in, so `alias`/`unalias` stay reachable.
    let expanded;
    let line = match aliases.read().unwrap().expand(line) {
        Ok(None) => line,
        Ok(Some(text)) => {
            expanded = text;
            expanded.trim()
        }
        Err(e) => {
            report_error(ExitStatus::Usage, &e);
            return false;
        }
    };
    // Capture (`name = cmd`), pipe (`cmd | path`), and `$var` references make
    // the REPL a small shell: routing and substitution run before dispatch so
    // every command sees resolved arguments. Capture and pipe act on tool
    // results.
    let (output, routed) = vars::route(line);
    let command = match vars::substitute(routed) {
        Ok(c) => c,
        Err(e) => {
            report_error(ExitStatus::Usage, &e);
            return false;
        }
    };
    let line = command.as_str();
    let client = session.client();
    let parsed = match command::parse(line) {
        Ok(parsed) => parsed,
        Err(e) => {
            report_error(ExitStatus::Usage, &e);
            return false;
        }
    };
    let background = parsed.background;
    let tokens: Vec<&str> = parsed.words.iter().map(String::as_str).collect();
    if tokens.is_empty() {
        if json_output() {
            report_error(ExitStatus::Usage, "empty command");
        }
        return false;
    }
    let cmd = tokens[0];
    let rest = &tokens[1..];
    COMMAND_RAN.store(true, Ordering::Relaxed);

    // Routing that cannot be honored is an error, not a silent no-op: in an
    // `-e` chain a dropped capture leaves a later `$name` undefined, and the
    // failure surfaces far from its cause.
    let is_builtin = BUILTINS.iter().any(|(name, _)| *name == cmd);
    if !output.is_plain() && is_builtin && !ROUTABLE_BUILTINS.contains(&cmd) {
        let what = match (&output.capture, &output.filter) {
            (Some(_), _) => "capture",
            _ => "filter",
        };
        report_error(
            ExitStatus::Usage,
            &format!(
                "cannot {what} the result of `{cmd}`: it reports rather than returning a value. \
                 Routable commands: {}",
                ROUTABLE_BUILTINS.join(", ")
            ),
        );
        return false;
    }

    match cmd {
        "quit" | "exit" => {
            if json_output() {
                print_json(&serde_json::json!({ "exit": true }));
            }
            return true;
        }
        "help" => {
            // `help <command>` explains one built-in. Falls through to the
            // full listing when the name is not one.
            if let Some(name) = rest.first()
                && let Some((usage, detail)) = builtin_help(name)
            {
                if json_output() {
                    print_json(&serde_json::json!({
                        "name": name,
                        "usage": usage,
                        "description": detail,
                    }));
                } else {
                    println!("{}", paint(Style::new().bold(), usage));
                    println!("  {detail}");
                }
                return false;
            }
            if let Some(name) = rest.first() {
                report_error_with_hint(
                    ExitStatus::NoMatch,
                    &format!("no built-in named `{name}` (try `help` or `describe {name}`)"),
                    find::did_you_mean(&surface.read().unwrap(), name).as_deref(),
                );
                return false;
            }
            if json_output() {
                let s = surface.read().unwrap();
                print_json(&serde_json::json!({
                    "builtins": BUILTINS
                        .iter()
                        .map(|(name, description)| serde_json::json!({
                            "name": name,
                            "description": description,
                        }))
                        .collect::<Vec<_>>(),
                    "tools": s.tools,
                }));
                return false;
            }
            println!("built-ins:");
            println!("  tools | prompts | resources | templates   list the server surface");
            println!("  find [flags] <keyword>                    search the surface");
            println!("  describe <name>                           schemas and metadata");
            println!("  snapshot <name> [path]                    export a schema contract");
            println!("  validate <path> [mode]                    check a schema contract");
            println!("  read <uri> [--out <path>]                 read a resource");
            println!("  subscribe <uri> | unsubscribe <uri>       watch a resource for updates");
            println!("  subscriptions                             list active subscriptions");
            println!("  prompt <name> [k=v...]                    get a prompt");
            println!("  call <tool> <json>                        call a tool with raw JSON");
            println!("  bench <tool> [k=v...] [--n N] [--concurrency C]  time repeated calls");
            println!("  <tool> [k=v...]                           call a tool (schema-coerced)");
            println!("  <tool> [k=v...] &                         run task-augmented (SEP-2663)");
            println!("  jobs | task <id> | wait <id> | cancel <id>  manage tasks");
            println!("  alias [<name>=<expansion>] | unalias <name>  command aliases");
            println!("  wire [on|off]                             trace raw JSON-RPC frames");
            println!("  last                                      reprint the previous exchange");
            println!(
                "  vars | unset <name>                       list or clear captured variables"
            );
            println!(
                "  name = <cmd> [| <path>]                   capture a result (filter with | path)"
            );
            println!("  $name.path in args                        reference a captured value");
            println!("  ping | refresh | info | quit");
            println!("  help <command>                            explain one built-in");
            let s = surface.read().unwrap();
            if !s.tools.is_empty() {
                println!("tools:");
                for t in &s.tools {
                    println!(
                        "  {} {}",
                        style::column(Style::new().fg(Color::Green), &sanitize(&t.name), 24),
                        sanitize(t.description.as_deref().unwrap_or(""))
                    );
                }
            }
        }
        "tools" | "prompts" | "resources" | "templates" => {
            let s = surface.read().unwrap();
            // Printing an empty list here would state something the REPL does
            // not know. Say the listing failed instead, and keep the status
            // non-zero so a pipeline cannot read it as "this server has none".
            let what = if cmd == "templates" {
                "resource templates"
            } else {
                cmd
            };
            if s.is_unavailable(what) {
                report_error(
                    ExitStatus::Transport,
                    &format!(
                        "the {what} listing is unavailable: it could not be read from this \
                         server (try `refresh`)"
                    ),
                );
                return false;
            }
            if !output.is_plain() || json_output() {
                let v = match cmd {
                    "tools" => serde_json::to_value(&s.tools),
                    "prompts" => serde_json::to_value(&s.prompts),
                    "resources" => serde_json::to_value(&s.resources),
                    _ => serde_json::to_value(&s.templates),
                }
                .unwrap_or_default();
                emit_value(v, &output, || unreachable!("plain output handled below"));
                return false;
            }
            // `<list> --full` prints everything; otherwise a long surface is
            // trimmed to the window so the prompt stays in view.
            let full = rest.contains(&"--full");
            let limit = if full { None } else { listing_limit() };
            match cmd {
                "tools" => {
                    let total = s.tools.len();
                    let shown = limit.unwrap_or(total).min(total);
                    for t in s.tools.iter().take(shown) {
                        println!(
                            "{} {}{}",
                            style::column(Style::new().fg(Color::Green), &sanitize(&t.name), 24),
                            sanitize(t.description.as_deref().unwrap_or("")),
                            tool_tag_suffix(t)
                        );
                    }
                    note_truncation(shown, total, "tools --full");
                }
                "prompts" => {
                    let total = s.prompts.len();
                    let shown = limit.unwrap_or(total).min(total);
                    for p in s.prompts.iter().take(shown) {
                        let args: Vec<String> = p
                            .arguments
                            .iter()
                            .map(|a| {
                                if a.required {
                                    format!("<{}>", sanitize(&a.name))
                                } else {
                                    format!("[{}]", sanitize(&a.name))
                                }
                            })
                            .collect();
                        println!(
                            "{} {} {}",
                            style::column(Style::new().fg(Color::Green), &sanitize(&p.name), 24),
                            paint(Style::new().fg(Color::Cyan), &args.join(" ")),
                            sanitize(p.description.as_deref().unwrap_or(""))
                        );
                    }
                    note_truncation(shown, total, "prompts --full");
                }
                "resources" => {
                    let total = s.resources.len();
                    let shown = limit.unwrap_or(total).min(total);
                    for r in s.resources.iter().take(shown) {
                        println!(
                            "{} {}",
                            style::column(Style::new().fg(Color::Green), &sanitize(&r.uri), 40),
                            sanitize(&r.name)
                        );
                    }
                    note_truncation(shown, total, "resources --full");
                    // Templates (parameterized URIs) are a separate MCP list
                    // and easy to miss; point at them.
                    if !s.templates.is_empty() {
                        println!(
                            "{}",
                            paint(
                                Style::new().dimmed(),
                                &format!(
                                    "(+ {} resource template(s) with variables, see `templates`)",
                                    s.templates.len()
                                )
                            )
                        );
                    }
                }
                _ => {
                    let total = s.templates.len();
                    let shown = limit.unwrap_or(total).min(total);
                    for t in s.templates.iter().take(shown) {
                        println!(
                            "{} {}",
                            style::column(
                                Style::new().fg(Color::Green),
                                &sanitize(&t.uri_template),
                                40
                            ),
                            sanitize(&t.name)
                        );
                    }
                    note_truncation(shown, total, "templates --full");
                    if !s.resources.is_empty() {
                        println!(
                            "{}",
                            paint(
                                Style::new().dimmed(),
                                &format!(
                                    "(+ {} concrete resource(s), see `resources`)",
                                    s.resources.len()
                                )
                            )
                        );
                    }
                }
            }
        }
        "find" => {
            // Everything that is not a flag is the query, so a phrase
            // (`find crate info`) is not silently truncated to its first word.
            match find::parse_query(rest) {
                Ok(query) => print_find(&surface.read().unwrap(), &query, &output),
                Err(message) => {
                    command_error(&message);
                    return false;
                }
            }
        }
        "describe" => {
            let Some(name) = rest.first() else {
                command_error("usage: describe <tool|prompt|resource|template>");
                return false;
            };
            let surface = surface.read().unwrap();
            if !output.is_plain() || json_output() {
                match describe_value(&surface, name) {
                    Some(value) => {
                        emit_value(value, &output, || unreachable!("plain handled below"))
                    }
                    None => report_error_with_hint(
                        ExitStatus::NoMatch,
                        &format!("nothing on the surface named `{name}`"),
                        find::did_you_mean(&surface, name).as_deref(),
                    ),
                }
            } else {
                describe(&surface, name);
            }
        }
        "snapshot" => {
            let Some(name) = rest.first() else {
                command_error("usage: snapshot <tool|prompt> [path]");
                return false;
            };
            if rest.len() > 2 {
                command_error("usage: snapshot <tool|prompt> [path]");
                return false;
            }
            let snapshot = {
                let surface = surface.read().unwrap();
                schema_contract::Snapshot::from_surface(&surface.tools, &surface.prompts, name)
            };
            let snapshot = match snapshot {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    report_error(ExitStatus::Usage, &error);
                    return false;
                }
            };
            let Some(snapshot) = snapshot else {
                report_error(
                    ExitStatus::NoMatch,
                    &format!("no tool or prompt named `{name}`"),
                );
                return false;
            };
            if let Some(path) = rest.get(1) {
                let path = std::path::Path::new(path);
                match snapshot.write(path) {
                    Ok(()) if json_output() => print_json(&serde_json::json!({
                        "kind": snapshot.kind,
                        "name": snapshot.name,
                        "path": path,
                    })),
                    Ok(()) => println!(
                        "saved {} {:?} schema snapshot to {}",
                        snapshot.kind,
                        snapshot.name,
                        path.display()
                    ),
                    Err(error) => report_error(ExitStatus::Usage, &error),
                }
            } else if json_output() {
                print_json(&snapshot.canonical_value());
            } else {
                print!("{}", snapshot.to_pretty_json());
            }
        }
        "validate" => {
            let Some(path) = rest.first() else {
                command_error("usage: validate <snapshot-path> [strict|compatible|ignore]");
                return false;
            };
            if rest.len() > 2 {
                command_error("usage: validate <snapshot-path> [strict|compatible|ignore]");
                return false;
            }
            let mode = match rest.get(1) {
                Some(mode) => match schema_contract::ValidationMode::from_str(mode, true) {
                    Ok(mode) => mode,
                    Err(_) => {
                        command_error(
                            "validation mode must be `strict`, `compatible`, or `ignore`",
                        );
                        return false;
                    }
                },
                None => schema_contracts.mode(),
            };
            let snapshot = match schema_contract::Snapshot::load(std::path::Path::new(path)) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    report_error(ExitStatus::Usage, &error);
                    return false;
                }
            };
            let current = {
                let surface = surface.read().unwrap();
                snapshot.matching_surface(&surface.tools, &surface.prompts)
            };
            let report = schema_contract::validate(&snapshot, current.as_ref(), mode);
            render_validation_report(&report, true);
        }
        "read" => {
            let (destination, force, rest) = match parse_read_flags(rest) {
                Ok(parsed) => parsed,
                Err(message) => {
                    command_error(&message);
                    return false;
                }
            };
            let Some(uri) = rest.first().copied() else {
                command_error("usage: read <uri> [--out <path>] [--force]");
                return false;
            };
            if let Some(path) = &destination
                && !force
                && std::path::Path::new(path).exists()
            {
                command_error(&format!(
                    "{path} already exists; pass --force to overwrite it"
                ));
                return false;
            }
            let started = std::time::Instant::now();
            match with_reconnect(
                session,
                surface,
                |c| async move { c.read_resource(uri).await },
            )
            .await
            {
                // Saving is the whole command when --out is given: the
                // payload is bytes on disk, not something to render.
                Ok(result) if destination.is_some() => {
                    let path = destination.clone().unwrap_or_default();
                    match save_resource(&result, &path) {
                        Ok(written) => {
                            if json_output() {
                                print_json(&serde_json::json!({
                                    "uri": uri,
                                    "path": path,
                                    "bytes": written,
                                }));
                            } else {
                                println!(
                                    "wrote {} to {}",
                                    plural(written, "byte"),
                                    sanitize(&path)
                                );
                            }
                        }
                        Err(message) => report_error(ExitStatus::Usage, &message),
                    }
                }
                Ok(result) if !output.is_plain() => {
                    emit_result(serde_json::to_value(&result).unwrap_or_default(), &output)
                }
                Ok(result) if json_output() => {
                    print_json(&serde_json::to_value(&result).unwrap_or_default())
                }
                Ok(result) => {
                    for c in result.contents {
                        if let Some(text) = c.text {
                            let is_md = c
                                .mime_type
                                .as_deref()
                                .is_some_and(|m| m.contains("markdown"))
                                || style::looks_like_markdown(&text);
                            if style::colors_enabled() && is_md {
                                println!("{}", style::render_markdown(&text));
                            } else {
                                println!("{}", sanitize(&text));
                            }
                        } else if let Some(blob) = c.blob {
                            println!(
                                "{}",
                                tag(Style::new(), &format!("binary {} base64 chars", blob.len()))
                            );
                        }
                    }
                }
                Err(e) => report_mcp_error(&e),
            }
            if !json_output() {
                println!("{}", timing(started.elapsed()));
            }
        }
        "subscribe" | "unsubscribe" => {
            let Some(uri) = rest.first() else {
                command_error(&format!("usage: {cmd} <uri>"));
                return false;
            };
            handle_subscription(&client, cmd, uri).await;
        }
        "subscriptions" => {
            let active = subscribe::list();
            if json_output() {
                print_json(&serde_json::json!(active));
                return false;
            }
            if active.is_empty() {
                println!("no active subscriptions (try `subscribe <uri>`)");
                return false;
            }
            for uri in &active {
                println!("{}", paint(Style::new().fg(Color::Green), &sanitize(uri)));
            }
        }
        "prompt" => {
            let Some(name) = rest.first() else {
                command_error("usage: prompt <name> [k=v...]");
                return false;
            };
            if !enforce_prompt_contract(schema_contracts, surface, name) {
                return false;
            }
            let mut prompt_args = HashMap::new();
            for t in &rest[1..] {
                if let Some((k, v)) = t.split_once('=') {
                    prompt_args.insert(k.to_string(), v.to_string());
                }
            }
            let started = std::time::Instant::now();
            match with_reconnect(session, surface, |c| {
                let prompt_args = prompt_args.clone();
                async move { c.get_prompt(name, Some(prompt_args)).await }
            })
            .await
            {
                Ok(result) if json_output() => {
                    print_json(&serde_json::to_value(&result).unwrap_or_default())
                }
                Ok(result) => {
                    for m in result.messages {
                        let v = serde_json::to_value(&m).unwrap_or_default();
                        let role = v.get("role").and_then(|r| r.as_str()).unwrap_or("?");
                        let text = v
                            .pointer("/content/text")
                            .and_then(|t| t.as_str())
                            .map(str::to_string)
                            .unwrap_or_else(|| {
                                v.get("content").map(|c| c.to_string()).unwrap_or_default()
                            });
                        println!(
                            "{} {}",
                            tag(Style::new().fg(Color::Cyan), &sanitize(role)),
                            sanitize(&text)
                        );
                    }
                }
                Err(e) => report_mcp_error(&e),
            }
            if !json_output() {
                println!("{}", timing(started.elapsed()));
            }
        }
        "call" => {
            let Some(name) = rest.first() else {
                command_error("usage: call <tool> <json>");
                return false;
            };
            let json = rest[1..].join(" ");
            let arguments: serde_json::Value = match serde_json::from_str(&json) {
                Ok(v) => v,
                Err(e) => {
                    report_error(ExitStatus::Usage, &format!("invalid JSON: {e}"));
                    return false;
                }
            };
            run_tool(
                session,
                surface,
                jobs,
                schema_contracts,
                name,
                arguments,
                background,
                &output,
            )
            .await;
        }
        "bench" => {
            handle_bench(&client, surface, schema_contracts, rest, background).await;
        }
        "jobs" => {
            // Every listed job costs a `tasks/get`, so the annotation
            // answers the same question it does for a tool call.
            let started = std::time::Instant::now();
            if json_output() {
                let mut rendered = Vec::new();
                for job in jobs.list() {
                    match client.task_get(&job.task_id).await {
                        Ok(task) => {
                            jobs.sync(&job.task_id, task.status, task.status_message.clone());
                            rendered.push(serde_json::json!({
                                "taskId": job.task_id,
                                "tool": job.tool,
                                "task": task,
                            }));
                        }
                        Err(error) => {
                            let status = ExitStatus::from_mcp_error(&error);
                            note_error(status);
                            rendered.push(serde_json::json!({
                                "taskId": job.task_id,
                                "tool": job.tool,
                                "error": error.to_string(),
                                "kind": status.label(),
                                "exitStatus": status.code(),
                            }));
                        }
                    }
                }
                print_json(&serde_json::Value::Array(rendered));
                return false;
            }
            if jobs.is_empty() {
                println!(
                    "{}",
                    paint(
                        Style::new().dimmed(),
                        "no background tasks (run a task-capable tool with a trailing `&`)"
                    )
                );
            }
            for job in jobs.list() {
                match client.task_get(&job.task_id).await {
                    Ok(task) => {
                        jobs.sync(&job.task_id, task.status, task.status_message.clone());
                        println!(
                            "{}  {}  {}",
                            sanitize(&job.label()),
                            sanitize(&job.tool),
                            paint(task_status_style(task.status), &task.status.to_string())
                        );
                    }
                    Err(error) => {
                        note_error(ExitStatus::from_mcp_error(&error));
                        println!(
                            "{}  {}  (gone)",
                            sanitize(&job.label()),
                            sanitize(&job.tool)
                        );
                    }
                }
            }
            if !json_output() {
                println!("{}", timing(started.elapsed()));
            }
        }
        // Task commands do not reconnect: a task id belongs to the session
        // that created it, so a fresh session would only report it missing.
        // "(gone)" from `jobs` is the honest answer there.
        "task" | "wait" | "cancel" => {
            // `wait` blocks until the task settles, so how long that took is
            // the most useful number the command can report.
            let started = std::time::Instant::now();
            // `wait` takes an optional deadline of its own. The global
            // --timeout deliberately does not apply: a task exists precisely
            // to outlive the call that created it, so the useful default is
            // to keep waiting, interruptible with Ctrl-C.
            let (wait_limit, rest) = match parse_wait_timeout(cmd, rest) {
                Ok(parsed) => parsed,
                Err(message) => {
                    command_error(&message);
                    return false;
                }
            };
            // A bare `wait` waits for every task this session started, in
            // start order. A script cannot name an id the server generated
            // inside an earlier `-e` command, so without this there is no way
            // for one to wait on its own work.
            if cmd == "wait" && rest.is_empty() {
                wait_for_all(&client, jobs, wait_limit, started).await;
                return false;
            }
            let Some(typed) = rest.first() else {
                command_error(&format!("usage: {cmd} <task>"));
                return false;
            };
            // `slow_add a=1 b=2 &` prints a small number; accept that, `last`,
            // the server's full id, or an unambiguous prefix of it.
            let Some(resolved) = jobs.resolve(typed) else {
                report_error(
                    ExitStatus::NoMatch,
                    &format!(
                        "no task `{typed}` in this session (run `jobs`; a task id belongs to \
                         the session that created it)"
                    ),
                );
                return false;
            };
            let id = &resolved.as_str();
            // `task <id> respond` answers what the task is parked on, which
            // is the only way to move an `input_required` task forward: the
            // question was asked while the editor held the terminal, so
            // nothing could answer it at the time.
            if cmd == "task" && rest.get(1).is_some_and(|word| *word == "respond") {
                respond_to_task(&client, id, &jobs.label_for(id)).await;
                if !json_output() {
                    println!("{}", timing(started.elapsed()));
                }
                return false;
            }
            let outcome = match cmd {
                "task" => client.task_get(id).await,
                "wait" => wait_for_one(&client, id, wait_limit).await,
                _ => match client.task_cancel(id, None).await {
                    Ok(()) => {
                        if !json_output() {
                            println!("cancel acknowledged");
                        }
                        client.task_get(id).await
                    }
                    Err(e) => Err(e),
                },
            };
            match outcome {
                Ok(task) if json_output() => {
                    jobs.sync(id, task.status, task.status_message.clone());
                    if cmd == "wait" {
                        note_settled_task(&task);
                    }
                    print_json(&serde_json::to_value(&task).unwrap_or_default());
                }
                Ok(task) => {
                    jobs.sync(id, task.status, task.status_message.clone());
                    if cmd == "wait" {
                        note_settled_task(&task);
                    }
                    render_task(&task, &jobs.label_for(&task.task_id));
                }
                Err(e) => report_mcp_error(&e),
            }
            if !json_output() {
                println!("{}", timing(started.elapsed()));
            }
        }
        "alias" | "unalias" => {
            // Everything after the command word is taken raw: an expansion is
            // a command line, so its spacing and any `=` belong to it.
            let raw = line.strip_prefix(cmd).unwrap_or("").trim();
            handle_alias(aliases, surface, cmd, raw);
        }
        "wire" => {
            match rest.first().copied() {
                Some("on") => wire().set_trace(true),
                Some("off") => wire().set_trace(false),
                None => {}
                Some(other) => {
                    command_error(&format!("usage: wire [on|off] (got `{other}`)"));
                    return false;
                }
            }
            let enabled = wire().trace_enabled();
            if json_output() {
                print_json(&serde_json::json!({ "wire": enabled }));
            } else if enabled {
                println!("wire tracing on (frames print to stderr)");
            } else {
                println!("wire tracing off");
            }
        }
        // Deliberately independent of the trace toggle: frames are recorded
        // either way, so the exchange you did not think to trace is still there.
        "last" => match wire().last_exchange() {
            None => {
                note_error(ExitStatus::NoMatch);
                if json_output() {
                    print_json(&error_json(ExitStatus::NoMatch, "no exchange yet"));
                } else {
                    println!("no request has been sent yet");
                }
            }
            Some((request, response)) => {
                if json_output() {
                    print_json(&serde_json::json!({
                        "request": request.json,
                        "response": response.map(|r| r.json),
                    }));
                } else {
                    // Before the first command, the newest frame is the
                    // REPL's own startup fetch. Showing it is right for a
                    // wire tool; letting it look like something the user ran
                    // is not.
                    if !COMMAND_RAN.load(Ordering::Relaxed) {
                        println!(
                            "{}",
                            paint(
                                Style::new().dimmed(),
                                "(no command has run yet; this is mcp-repl's own startup traffic)"
                            )
                        );
                    }
                    println!("{}", wire::render(wire::Direction::Sent, &request));
                    match response {
                        Some(response) => {
                            println!("{}", wire::render(wire::Direction::Received, &response));
                        }
                        None => println!("(no response recorded for it)"),
                    }
                }
            }
        },
        "ping" => {
            let started = std::time::Instant::now();
            match with_deadline(client.ping()).await {
                Ok(()) => {
                    let elapsed = started.elapsed();
                    if json_output() {
                        print_json(&serde_json::json!({
                            "ok": true,
                            "elapsedMs": elapsed.as_millis(),
                        }));
                    } else {
                        println!(
                            "{} {}",
                            paint(Style::new().fg(Color::Green), "ok"),
                            timing(elapsed)
                        );
                    }
                }
                Err(e) => report_mcp_error(&e),
            }
        }
        "loglevel" => {
            let Some(typed) = rest.first() else {
                command_error(&format!("usage: loglevel <{}>", LOG_LEVELS.join("|")));
                return false;
            };
            let Some(level) = parse_log_level(typed) else {
                // Not `did you mean`: the answer is the whole scale, and the
                // levels are worth seeing in severity order rather than one
                // guess at what was meant.
                report_error(
                    ExitStatus::Usage,
                    &format!(
                        "unknown log level `{}` (levels are {})",
                        sanitize(typed),
                        LOG_LEVELS.join(", ")
                    ),
                );
                return false;
            };
            // The server said whether it has logging at all. Sending anyway
            // would earn a "method not found" the operator would have to
            // interpret; saying so first is the same information, sooner.
            let declared = connection_info(&client)
                .await
                .is_some_and(|info| info.capabilities.logging.is_some());
            if !declared {
                report_error(
                    ExitStatus::Server,
                    "this server does not declare the `logging` capability, so it has no \
                     level to set (any notifications it sends arrive regardless)",
                );
                return false;
            }
            let started = std::time::Instant::now();
            let params = serde_json::json!({ "level": level });
            match with_deadline(client.request::<_, serde_json::Value>("logging/setLevel", &params))
                .await
            {
                Ok(_) => {
                    if json_output() {
                        print_json(&serde_json::json!({ "level": level }));
                    } else {
                        println!(
                            "log level set to {} {}",
                            paint(log_level_style(level), &level.to_string()),
                            timing(started.elapsed())
                        );
                    }
                }
                Err(e) => report_mcp_error(&e),
            }
        }
        "refresh" => {
            let started = std::time::Instant::now();
            let fresh = refresh_surface(session).await;
            if json_output() {
                print_json(&serde_json::json!({
                    "tools": fresh.tools.len(),
                    "prompts": fresh.prompts.len(),
                    "resources": fresh.resources.len(),
                    "templates": fresh.templates.len(),
                }));
            } else {
                println!(
                    "{}, {}, {}, {}",
                    plural(fresh.tools.len(), "tool"),
                    plural(fresh.prompts.len(), "prompt"),
                    plural(fresh.resources.len(), "resource"),
                    plural(fresh.templates.len(), "template")
                );
            }
            if !json_output() {
                println!("{}", timing(started.elapsed()));
            }
            *surface.write().unwrap() = fresh;
        }
        "info" => match connection_info(&client).await {
            Some(info) => {
                if !output.is_plain() || json_output() {
                    emit_value(
                        serde_json::json!({
                            "protocolVersion": info.protocol_version,
                            "serverInfo": info.server_info,
                            "capabilities": info.capabilities,
                            "instructions": info.instructions,
                            "sampling": sampling::mode().as_str(),
                            "elicitation": elicit::mode().as_str(),
                        }),
                        &output,
                        || unreachable!("plain output handled below"),
                    );
                    return false;
                }
                // Replay the full startup banner, then add capabilities.
                print_banner(&info);
                print_counts(&surface.read().unwrap());
                let caps = serde_json::to_value(&info.capabilities).unwrap_or_default();
                println!("capabilities: {}", json_pretty(&caps));
                // What this client does with a request the server sends back.
                println!(
                    "{}",
                    paint(
                        Style::new().dimmed(),
                        &format!(
                            "sampling: {}, elicitation: {}",
                            sampling::mode().as_str(),
                            elicit::mode().as_str()
                        )
                    )
                );
            }
            None => report_error(ExitStatus::Transport, "not initialized"),
        },
        "history" => {
            const DEFAULT_SHOWN: usize = 20;
            let limit = match rest.first() {
                None => DEFAULT_SHOWN,
                Some(raw) => match raw.parse::<usize>() {
                    Ok(n) if n > 0 => n,
                    _ => {
                        command_error(&format!("usage: history [count] (got `{raw}`)"));
                        return false;
                    }
                },
            };
            let entries = editor::recent_history(limit);
            if json_output() {
                print_json(&serde_json::json!(entries));
            } else if entries.is_empty() {
                println!(
                    "{}",
                    paint(
                        Style::new().dimmed(),
                        "no history yet (it persists across sessions unless --no-history)"
                    )
                );
            } else {
                for line in &entries {
                    println!("{}", sanitize(line));
                }
                println!(
                    "{}",
                    paint(
                        Style::new().dimmed(),
                        "Ctrl-R searches history interactively"
                    )
                );
            }
        }
        "vars" => {
            let all = vars::list();
            if json_output() {
                let map: serde_json::Map<String, serde_json::Value> = all.into_iter().collect();
                print_json(&serde_json::Value::Object(map));
            } else if all.is_empty() {
                println!(
                    "{}",
                    paint(
                        Style::new().dimmed(),
                        "no variables (capture one with `name = <command>`)"
                    )
                );
            } else {
                for (name, value) in all {
                    println!(
                        "{} {}",
                        paint(Style::new().fg(Color::Cyan), &format!("${name} =")),
                        value_summary(&value)
                    );
                }
            }
        }
        "unset" => match rest.first() {
            Some(name) => {
                if vars::unset(name) {
                    if json_output() {
                        print_json(&serde_json::json!({ "unset": name }));
                    } else {
                        println!("unset ${name}");
                    }
                } else {
                    command_error(&format!("no such variable `${name}`"));
                }
            }
            None => command_error("usage: unset <name>"),
        },
        tool_name => {
            let schema = {
                let s = surface.read().unwrap();
                s.tools
                    .iter()
                    .find(|t| t.name == tool_name)
                    .map(|t| t.input_schema.clone())
            };
            let Some(schema) = schema else {
                // The command word itself did not resolve, so the line could
                // not be dispatched at all: a usage error, the way a shell
                // treats "command not found". A *name argument* that is
                // absent from the surface is the other case, and reports
                // NoMatch (see `describe` and `bench`).
                let suggestion = find::did_you_mean(&surface.read().unwrap(), tool_name);
                let message = match suggestion {
                    Some(_) => format!("unknown command: {tool_name}"),
                    None => format!("unknown command: {tool_name} (try `help`)"),
                };
                report_error_with_hint(ExitStatus::Usage, &message, suggestion.as_deref());
                return false;
            };
            let arguments = parse_kv_args(&schema, rest);
            run_tool(
                session,
                surface,
                jobs,
                schema_contracts,
                tool_name,
                arguments,
                background,
                &output,
            )
            .await;
        }
    }
    false
}

/// The `bench` built-in: issue one tool call repeatedly and report how long
/// the calls took. Arguments are coerced against the tool's `inputSchema`,
/// exactly as a direct call is, so `bench <tool> a=1` benchmarks the same
/// request `<tool> a=1` would send.
async fn handle_bench(
    client: &Arc<McpClient>,
    surface: &Arc<RwLock<Surface>>,
    schema_contracts: &schema_contract::ContractSet,
    rest: &[&str],
    background: bool,
) {
    // A trailing `&` is stripped before dispatch, so say why it did nothing
    // rather than silently benchmarking the non-task path.
    if background {
        command_error("bench cannot run task-augmented; drop the trailing `&`");
        return;
    }
    let plan = match bench::parse(rest) {
        Ok(plan) => plan,
        Err(e) => {
            command_error(&e);
            return;
        }
    };
    let schema = {
        let s = surface.read().unwrap();
        s.tools
            .iter()
            .find(|t| t.name == plan.tool)
            .map(|t| t.input_schema.clone())
    };
    let Some(schema) = schema else {
        // Same condition as `describe` on an unknown name, so the same
        // status: the invocation was well formed, the name just is not there.
        report_error_with_hint(
            ExitStatus::NoMatch,
            &format!("no tool named `{}` (try `tools`)", plan.tool),
            find::did_you_mean(&surface.read().unwrap(), &plan.tool).as_deref(),
        );
        return;
    };
    if !enforce_tool_contract(schema_contracts, surface, &plan.tool) {
        return;
    }
    let arg_tokens: Vec<&str> = plan.args.iter().map(String::as_str).collect();
    let arguments = parse_kv_args(&schema, &arg_tokens);

    let outcome = bench::run(client, &plan.tool, arguments, plan.n, plan.concurrency).await;
    // A run with failures in it exits non-zero, like any other failing
    // command, so `-e "bench ..."` works as a health check.
    if outcome.errors > 0 {
        note_error(ExitStatus::Server);
    }
    if json_output() {
        print_json(&bench::render_json(&plan, &outcome));
        return;
    }
    println!("{}", bench::render(&plan, &outcome));
    if let Some(message) = &outcome.first_error {
        println!(
            "{} {}",
            tag(Style::new().fg(Color::Red), "first error"),
            sanitize(message)
        );
    }
    println!("{}", timing(outcome.total));
}

/// The `subscribe` and `unsubscribe` built-ins. The local set is only updated
/// once the server has agreed, so `subscriptions` lists what the server is
/// actually sending updates for, not what was asked for.
async fn handle_subscription(client: &Arc<McpClient>, cmd: &str, uri: &str) {
    // A server that does not advertise the capability will reject the call.
    // Saying so first turns a bare protocol error into an explanation.
    if cmd == "subscribe"
        && let Some(info) = connection_info(client).await
        && !subscribe::server_supports(
            &serde_json::to_value(&info.capabilities).unwrap_or_default(),
        )
    {
        eprintln!(
            "warning: {} does not advertise resources.subscribe; the request will \
             probably be rejected",
            info.server_info.name
        );
    }
    let started = std::time::Instant::now();
    let outcome = if cmd == "subscribe" {
        client.subscribe_resource(uri).await
    } else {
        client.unsubscribe_resource(uri).await
    };
    match outcome {
        Ok(()) => {
            let changed = if cmd == "subscribe" {
                subscribe::add(uri)
            } else {
                subscribe::remove(uri)
            };
            if json_output() {
                print_json(&serde_json::json!({
                    cmd: uri,
                    "alreadyInEffect": !changed,
                }));
            } else {
                let note = if changed {
                    String::new()
                } else {
                    format!(" {}", paint(Style::new().dimmed(), "(already in effect)"))
                };
                println!("{cmd}d {}{note}", paint(Style::new().fg(Color::Green), uri));
            }
        }
        Err(e) => report_mcp_error(&e),
    }
    if !json_output() {
        println!("{}", timing(started.elapsed()));
    }
}

/// The `alias` and `unalias` built-ins: define, list, show, and remove
/// command aliases, persisting each change to the config file.
///
/// `raw` is everything after the command word, unsplit: an expansion is a
/// command line of its own, so its spacing is part of it.
fn handle_alias(
    aliases: &Arc<RwLock<Aliases>>,
    surface: &Arc<RwLock<Surface>>,
    cmd: &str,
    raw: &str,
) {
    // A leading `--global` targets the file-level table. Only leading: a
    // trailing one would be ambiguous with an expansion that ends in a flag.
    let (global, rest) = match raw.strip_prefix("--global") {
        Some(r) if r.is_empty() || r.starts_with(char::is_whitespace) => (true, r.trim_start()),
        _ => (false, raw),
    };
    let rest = rest.trim();

    if cmd == "unalias" {
        if rest.is_empty() || rest.contains(char::is_whitespace) {
            command_error("usage: unalias [--global] <name>");
            return;
        }
        match aliases.write().unwrap().remove(rest, global) {
            Ok(applied) => {
                report_alias_warning(applied.warning.as_deref());
                if json_output() {
                    print_json(&serde_json::json!({
                        "removed": rest,
                        "expansion": applied.previous,
                        "scope": applied.scope.label(),
                    }));
                } else {
                    println!(
                        "removed {} {}",
                        paint(Style::new().fg(Color::Cyan), rest),
                        paint(
                            Style::new().dimmed(),
                            &format!("({})", applied.scope.label())
                        )
                    );
                }
            }
            Err(e) => command_error(&e),
        }
        return;
    }

    // `alias` with nothing after it lists what is in effect.
    if rest.is_empty() {
        let aliases = aliases.read().unwrap();
        let entries = aliases.entries();
        if json_output() {
            let rendered: Vec<serde_json::Value> = entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "name": e.name,
                        "expansion": e.expansion,
                        "scope": e.scope.label(),
                    })
                })
                .collect();
            print_json(&serde_json::Value::Array(rendered));
            return;
        }
        if entries.is_empty() {
            println!("no aliases defined (try `alias t=tools`)");
            return;
        }
        let width = entries.iter().map(|e| e.name.len()).max().unwrap_or(0);
        for e in &entries {
            println!(
                "{}  {}  {}",
                style::column(Style::new().fg(Color::Cyan), &e.name, width),
                e.expansion,
                paint(Style::new().dimmed(), &format!("({})", e.scope.label()))
            );
        }
        return;
    }

    // `alias <name>` shows one definition; `alias <name>=<expansion>` defines.
    let Some((name, expansion)) = rest.split_once('=') else {
        let aliases = aliases.read().unwrap();
        match aliases.lookup(rest) {
            Some((expansion, scope)) if json_output() => print_json(&serde_json::json!({
                "name": rest,
                "expansion": expansion,
                "scope": scope.label(),
            })),
            Some((expansion, scope)) => println!(
                "{} = {}  {}",
                paint(Style::new().fg(Color::Cyan), rest),
                expansion,
                paint(Style::new().dimmed(), &format!("({})", scope.label()))
            ),
            None => command_error(&format!(
                "no alias named `{rest}` (define one with `alias {rest}=<expansion>`)"
            )),
        }
        return;
    };
    let name = name.trim();
    match aliases
        .write()
        .unwrap()
        .define(name, expansion.trim(), global)
    {
        Ok(applied) => {
            report_alias_warning(applied.warning.as_deref());
            if json_output() {
                print_json(&serde_json::json!({
                    "name": name,
                    "expansion": expansion.trim(),
                    "scope": applied.scope.label(),
                    "replaced": applied.previous,
                }));
                return;
            }
            println!(
                "{} = {}  {}",
                paint(Style::new().fg(Color::Cyan), name),
                expansion.trim(),
                paint(
                    Style::new().dimmed(),
                    &format!("({})", applied.scope.label())
                )
            );
            // An alias wins over a tool of the same name, since expansion
            // happens before dispatch. Worth saying once, at definition.
            if surface.read().unwrap().tools.iter().any(|t| t.name == name) {
                println!(
                    "{}",
                    paint(
                        Style::new().dimmed(),
                        &format!("note: this shadows the tool `{name}` on this server")
                    )
                );
            }
        }
        Err(e) => command_error(&e),
    }
}

/// A failed write is reported without discarding the alias: it applies to
/// this session, it just did not reach the config file.
fn report_alias_warning(warning: Option<&str>) {
    if let Some(w) = warning {
        eprintln!("warning: {w}");
    }
}

fn command_error(message: &str) {
    report_error(ExitStatus::Usage, message);
}

/// Render a compatibility report. Pre-invocation checks stay silent on
/// success so one JSON command still emits exactly one JSON value.
fn render_validation_report(
    report: &schema_contract::ValidationReport,
    render_success: bool,
) -> bool {
    if report.compatible && !render_success {
        return true;
    }
    if !report.compatible {
        note_error(ExitStatus::NoMatch);
    }
    if json_output() {
        print_json(&serde_json::to_value(report).unwrap_or_default());
    } else if report.compatible {
        println!(
            "{} {:?} is compatible under {} validation",
            report.kind, report.name, report.mode
        );
    } else {
        println!(
            "{} {:?} is incompatible under {} validation:",
            report.kind, report.name, report.mode
        );
        for issue in &report.issues {
            println!("  {} [{}] {}", issue.path, issue.code, issue.message);
        }
    }
    report.compatible
}

fn enforce_tool_contract(
    contracts: &schema_contract::ContractSet,
    surface: &Arc<RwLock<Surface>>,
    name: &str,
) -> bool {
    let report = {
        let surface = surface.read().unwrap();
        surface
            .tools
            .iter()
            .find(|definition| definition.name == name)
            .and_then(|definition| contracts.check_tool(definition))
    };
    report
        .as_ref()
        .is_none_or(|report| render_validation_report(report, false))
}

fn enforce_prompt_contract(
    contracts: &schema_contract::ContractSet,
    surface: &Arc<RwLock<Surface>>,
    name: &str,
) -> bool {
    let report = {
        let surface = surface.read().unwrap();
        surface
            .prompts
            .iter()
            .find(|definition| definition.name == name)
            .and_then(|definition| contracts.check_prompt(definition))
    };
    report
        .as_ref()
        .is_none_or(|report| render_validation_report(report, false))
}

fn describe_value(surface: &Surface, name: &str) -> Option<serde_json::Value> {
    surface
        .tools
        .iter()
        .find(|definition| definition.name == name)
        .map(|definition| {
            serde_json::json!({
                "kind": "tool",
                "definition": definition,
            })
        })
        .or_else(|| {
            surface
                .prompts
                .iter()
                .find(|definition| definition.name == name)
                .map(|definition| {
                    serde_json::json!({
                        "kind": "prompt",
                        "definition": definition,
                    })
                })
        })
        .or_else(|| {
            surface
                .resources
                .iter()
                .find(|definition| definition.name == name || definition.uri == name)
                .map(|definition| {
                    serde_json::json!({
                        "kind": "resource",
                        "definition": definition,
                    })
                })
        })
        .or_else(|| {
            surface
                .templates
                .iter()
                .find(|definition| definition.name == name || definition.uri_template == name)
                .map(|definition| {
                    serde_json::json!({
                        "kind": "resourceTemplate",
                        "definition": definition,
                    })
                })
        })
}

/// Pull an optional `--timeout <secs>` out of a task command's arguments,
/// returning the deadline and the remaining arguments. Only `wait` accepts
/// it; `task` and `cancel` are single round-trips already covered by the
/// global deadline.
/// Pull `--out <path>` and `--force` out of `read`'s arguments.
fn parse_read_flags<'a>(rest: &[&'a str]) -> Result<(Option<String>, bool, Vec<&'a str>), String> {
    let mut destination = None;
    let mut force = false;
    let mut remaining = Vec::new();
    let mut tokens = rest.iter().copied();
    while let Some(token) = tokens.next() {
        match token {
            "--force" => force = true,
            "--out" => {
                let path = tokens
                    .next()
                    .ok_or_else(|| "--out needs a path".to_string())?;
                destination = Some(path.to_string());
            }
            _ => match token.strip_prefix("--out=") {
                Some(path) if !path.is_empty() => destination = Some(path.to_string()),
                Some(_) => return Err("--out needs a path".to_string()),
                None if token.starts_with("--") => {
                    return Err(format!(
                        "unknown option `{token}` (read takes --out and --force)"
                    ));
                }
                None => remaining.push(token),
            },
        }
    }
    Ok((destination, force, remaining))
}

/// Write a resource's content to a file, decoding a blob back to bytes.
///
/// Returns the number of bytes written. A resource that came back as several
/// contents is refused rather than concatenated: joining a set of blobs
/// produces a file that is not any of them.
fn save_resource(
    result: &tower_mcp::protocol::ReadResourceResult,
    path: &str,
) -> Result<usize, String> {
    let mut contents = result.contents.iter();
    let (Some(content), None) = (contents.next(), contents.next()) else {
        return Err(format!(
            "the resource returned {} contents; --out writes a single one",
            result.contents.len()
        ));
    };
    let bytes: Vec<u8> = match (&content.text, &content.blob) {
        (Some(text), _) => text.as_bytes().to_vec(),
        (None, Some(blob)) => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(blob)
                .map_err(|e| format!("the server sent a blob that is not valid base64: {e}"))?
        }
        (None, None) => return Err("the resource returned no content".to_string()),
    };
    // Owner-only: a saved resource is as sensitive as whatever served it.
    crate::secure_file::write_bytes(std::path::Path::new(path), &bytes)
        .map_err(|e| format!("could not write {path}: {e}"))?;
    Ok(bytes.len())
}

fn parse_wait_timeout<'a>(
    cmd: &str,
    rest: &[&'a str],
) -> Result<(Option<Duration>, Vec<&'a str>), String> {
    let mut limit = None;
    let mut remaining = Vec::new();
    let mut tokens = rest.iter().copied();
    while let Some(token) = tokens.next() {
        let value = match token.strip_prefix("--timeout") {
            None => {
                remaining.push(token);
                continue;
            }
            Some("") => tokens
                .next()
                .ok_or_else(|| format!("usage: {cmd} <task-id> [--timeout <seconds>]"))?,
            Some(rest) => rest
                .strip_prefix('=')
                .ok_or_else(|| format!("unknown flag `{token}` for {cmd}"))?,
        };
        if cmd != "wait" {
            return Err(format!(
                "--timeout applies to `wait`, not `{cmd}` (it is a single request, bounded by the global --timeout)"
            ));
        }
        let secs: u64 = value
            .parse()
            .map_err(|_| format!("--timeout expects seconds, got `{value}`"))?;
        limit = (secs > 0).then(|| Duration::from_secs(secs));
    }
    Ok((limit, remaining))
}

/// The short safety tags for a tool: what it says it does to the world, and
/// whether it can run as a task.
///
/// `describe` has shown these all along, but the decision they inform is
/// made while reading a list, before anyone thinks to describe anything.
pub(crate) fn tool_tags(tool: &ToolDefinition) -> Vec<&'static str> {
    let mut tags = Vec::new();
    if let Some(a) = &tool.annotations {
        if a.read_only_hint {
            tags.push("read-only");
        }
        // A destructive read-only tool is a contradiction; trust read-only.
        if a.destructive_hint && !a.read_only_hint {
            tags.push("destructive");
        }
        if a.idempotent_hint {
            tags.push("idempotent");
        }
        if a.open_world_hint {
            tags.push("open-world");
        }
    }
    if let Some(execution) = &tool.execution {
        let v = serde_json::to_value(execution).unwrap_or_default();
        match v.get("taskSupport").and_then(|m| m.as_str()) {
            Some("required") => tags.push("task-only"),
            Some("optional") => tags.push("task-capable"),
            _ => {}
        }
    }
    tags
}

/// The tags as they trail a listing row, or empty when the server declared
/// none.
fn tool_tag_suffix(tool: &ToolDefinition) -> String {
    let tags = tool_tags(tool);
    if tags.is_empty() {
        return String::new();
    }
    format!(
        " {}",
        paint(Style::new().dimmed(), &format!("[{}]", tags.join(" ")))
    )
}

/// A ready-to-run invocation synthesized from a tool's input schema:
/// required arguments first with their types as placeholders, then optional
/// ones in brackets. Long argument lists are trimmed, since the point is to
/// show the shape rather than reproduce the schema.
fn example_invocation(name: &str, schema: &serde_json::Value) -> String {
    const SHOWN: usize = 4;
    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let Some(properties) = schema.get("properties").and_then(|p| p.as_object()) else {
        return sanitize(name).into_owned();
    };
    let placeholder = |key: &str| -> String {
        // A generated schema hoists a named type into `$defs` and leaves a
        // `$ref` on the property, so the type and any enum values live one
        // hop away.
        let target = properties
            .get(key)
            .map(|property| editor::resolve_ref(schema, property));
        let ty = target
            .and_then(|t| t.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("value");
        // An enum tells the user the actual choices, which beats the type.
        let sample = target
            .and_then(|t| t.get("enum"))
            .and_then(|e| e.as_array())
            .and_then(|values| values.first())
            .and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .or_else(|| Some(v.to_string()))
            })
            .unwrap_or_else(|| format!("<{ty}>"));
        format!("{}={}", sanitize(key), sanitize(&sample))
    };
    let mut parts = vec![sanitize(name).into_owned()];
    for key in &required {
        parts.push(placeholder(key));
    }
    let optional: Vec<&String> = properties
        .keys()
        .filter(|key| !required.contains(&key.as_str()))
        .collect();
    for key in optional.iter().take(SHOWN.saturating_sub(required.len())) {
        parts.push(format!("[{}]", placeholder(key)));
    }
    if optional.len() > SHOWN.saturating_sub(required.len()) {
        parts.push("...".to_string());
    }
    parts.join(" ")
}

/// The `describe` built-in: schemas for a tool, the argument table for a
/// prompt, metadata for a resource or template.
fn describe(surface: &Surface, name: &str) {
    // A built-in is a command too, so asking about one should answer rather
    // than report that the server does not offer it.
    if let Some((usage, detail)) = builtin_help(name) {
        println!(
            "built-in {}",
            paint(Style::new().fg(Color::Cyan).bold(), name)
        );
        println!("  usage: {usage}");
        println!("  {detail}");
        return;
    }
    if let Some(t) = surface.tools.iter().find(|t| t.name == name) {
        println!(
            "tool {}  {}",
            paint(Style::new().fg(Color::Green).bold(), &sanitize(&t.name)),
            sanitize(t.description.as_deref().unwrap_or(""))
        );
        if let Some(a) = &t.annotations {
            let mut hints = Vec::new();
            if a.read_only_hint {
                hints.push("read-only");
            }
            if a.idempotent_hint {
                hints.push("idempotent");
            }
            if a.destructive_hint && !a.read_only_hint {
                hints.push("destructive");
            }
            if a.open_world_hint {
                hints.push("open-world");
            }
            if !hints.is_empty() {
                println!("  hints: {}", hints.join(", "));
            }
        }
        if let Some(e) = &t.execution {
            let v = serde_json::to_value(e).unwrap_or_default();
            if let Some(mode) = v.get("taskSupport").and_then(|m| m.as_str()) {
                println!("  task support: {mode}");
            }
        }
        println!("input schema:");
        println!("{}", json_pretty(&t.input_schema));
        if let Some(out) = &t.output_schema {
            println!("output schema:");
            println!("{}", json_pretty(out));
        }
        // A schema answers "what does it take"; the example answers "what do
        // I type", which is the question at a prompt.
        println!(
            "example: {}",
            paint(
                Style::new().dimmed(),
                &example_invocation(&t.name, &t.input_schema)
            )
        );
        return;
    }
    if let Some(p) = surface.prompts.iter().find(|p| p.name == name) {
        println!(
            "prompt {}  {}",
            paint(Style::new().fg(Color::Green).bold(), &sanitize(&p.name)),
            sanitize(p.description.as_deref().unwrap_or(""))
        );
        if p.arguments.is_empty() {
            println!("  (no arguments)");
        } else {
            println!("arguments:");
            for a in &p.arguments {
                println!(
                    "  {} {} {}",
                    style::column(Style::new().fg(Color::Cyan), &sanitize(&a.name), 20),
                    style::column(
                        Style::new(),
                        if a.required { "required" } else { "optional" },
                        10
                    ),
                    sanitize(a.description.as_deref().unwrap_or(""))
                );
            }
        }
        return;
    }
    if let Some(r) = surface
        .resources
        .iter()
        .find(|r| r.uri == name || r.name == name)
    {
        println!(
            "resource {}",
            paint(Style::new().fg(Color::Green).bold(), &sanitize(&r.uri))
        );
        println!("  name: {}", sanitize(&r.name));
        if let Some(t) = &r.title {
            println!("  title: {}", sanitize(t));
        }
        if let Some(d) = &r.description {
            println!("  description: {}", sanitize(d));
        }
        if let Some(m) = &r.mime_type {
            println!("  mimeType: {}", sanitize(m));
        }
        if let Some(s) = r.size {
            println!("  size: {s} bytes");
        }
        return;
    }
    if let Some(t) = surface
        .templates
        .iter()
        .find(|t| t.uri_template == name || t.name == name)
    {
        println!(
            "template {}",
            paint(
                Style::new().fg(Color::Green).bold(),
                &sanitize(&t.uri_template)
            )
        );
        println!("  name: {}", sanitize(&t.name));
        if let Some(d) = &t.description {
            println!("  description: {}", sanitize(d));
        }
        if let Some(m) = &t.mime_type {
            println!("  mimeType: {}", sanitize(m));
        }
        if !t.arguments.is_empty() {
            println!("arguments:");
            for a in &t.arguments {
                println!(
                    "  {} {} {}",
                    style::column(Style::new().fg(Color::Cyan), &sanitize(&a.name), 20),
                    style::column(
                        Style::new(),
                        if a.required { "required" } else { "optional" },
                        10
                    ),
                    sanitize(a.description.as_deref().unwrap_or(""))
                );
            }
        }
        return;
    }
    // A name that is not on the surface is the same condition wherever it is
    // asked about, so it carries the same status everywhere: `describe`,
    // `snapshot`, `bench`, and a bare command word all report NoMatch.
    report_error_with_hint(
        ExitStatus::NoMatch,
        &format!("nothing on the surface named `{name}` (try `tools`, `prompts`, `resources`)"),
        find::did_you_mean(surface, name).as_deref(),
    );
}

#[allow(clippy::too_many_arguments)]
async fn run_tool(
    session: &Arc<Session>,
    surface: &Arc<RwLock<Surface>>,
    jobs: &Arc<Jobs>,
    schema_contracts: &schema_contract::ContractSet,
    name: &str,
    arguments: serde_json::Value,
    background: bool,
    output: &vars::Output,
) {
    if !enforce_tool_contract(schema_contracts, surface, name) {
        return;
    }
    if background {
        match with_reconnect(session, surface, |c| {
            let arguments = arguments.clone();
            async move { c.call_tool_as_task(name, arguments, None).await }
        })
        .await
        {
            Ok(created) => {
                let created_value = serde_json::to_value(&created).unwrap_or_default();
                let task_id = created.task.task_id.clone();
                let poll_interval = created.task.poll_interval;
                // Register before announcing, so the line can name the task
                // by the short number the user will type.
                jobs.register(
                    created.task.task_id.clone(),
                    name.to_string(),
                    created.task.status,
                    created.task.status_message.clone(),
                );
                if !output.is_plain() {
                    // A backgrounded call returns the created task, so
                    // `created = tool ... &` can bind it and a later command
                    // can wait on `$created.task.taskId`.
                    emit_result(created_value, output);
                } else if json_output() {
                    print_json(&created_value);
                } else {
                    println!(
                        "{} started",
                        tag(
                            Style::new().fg(Color::Yellow),
                            &format!("task {}", sanitize(&jobs.label_for(&task_id)))
                        )
                    );
                }
                watch_task(session.clone(), jobs.clone(), task_id, poll_interval);
            }
            Err(e) => report_mcp_error(&e),
        }
        return;
    }
    let started = std::time::Instant::now();
    match with_reconnect(session, surface, |c| {
        let arguments = arguments.clone();
        async move { c.call_tool(name, arguments).await }
    })
    .await
    {
        Ok(result) => {
            if result.is_error {
                note_error(ExitStatus::Server);
            }
            if output.is_plain() {
                if json_output() {
                    print_json(&serde_json::to_value(&result).unwrap_or_default());
                } else {
                    if result.is_error {
                        println!("{}", tag(Style::new().fg(Color::Red), "tool error"));
                    }
                    render_content(&result.content);
                }
            } else {
                emit_result(result_value(&result), output);
            }
        }
        Err(e) => report_mcp_error(&e),
    }
    if !json_output() {
        println!("{}", timing(started.elapsed()));
    }
}

/// Extract a tool result's data value for capture or filtering: the structured
/// content if present, else a lone JSON text block parsed, else the content.
fn result_value(result: &tower_mcp::CallToolResult) -> serde_json::Value {
    if let Some(structured) = &result.structured_content {
        return structured.clone();
    }
    if let [Content::Text { text, .. }] = result.content.as_slice() {
        return serde_json::from_str(text)
            .unwrap_or_else(|_| serde_json::Value::String(text.clone()));
    }
    serde_json::to_value(&result.content).unwrap_or_default()
}

/// Built-ins whose result is a documented value, so `x = <cmd>` and
/// `<cmd> | <path>` mean something. Everything else either has no result
/// (`quit`), or produces a report rather than data (`help`, `alias`, `wire`,
/// `refresh`), and says so rather than dropping the request.
const ROUTABLE_BUILTINS: &[&str] = &[
    "tools",
    "prompts",
    "resources",
    "templates",
    "describe",
    "read",
    "find",
    "info",
    "history",
];

/// Emit a built-in's canonical value: routed when the line asked for a
/// capture or a filter, as one JSON value under `--json`, and otherwise
/// however the command renders for a human.
fn emit_value(value: serde_json::Value, output: &vars::Output, human: impl FnOnce()) {
    if !output.is_plain() {
        emit_result(value, output);
    } else if json_output() {
        print_json(&value);
    } else {
        human();
    }
}

/// Apply a capture/filter [`vars::Output`] to a result value: select a path,
/// bind it to a variable, or print it (bare scalar or pretty JSON).
fn emit_result(mut value: serde_json::Value, output: &vars::Output) {
    if let Some(path) = &output.filter {
        match vars::get_path(&value, path) {
            Some(selected) => value = selected,
            None => {
                command_error(&format!("path `{path}` not found in result"));
                return;
            }
        }
    }
    if let Some(name) = &output.capture {
        vars::set(name, value.clone());
        if json_output() {
            print_json(&value);
        } else {
            println!(
                "{} {}",
                paint(Style::new().fg(Color::Cyan), &format!("${name} =")),
                value_summary(&value)
            );
        }
    } else if json_output() {
        print_json(&value);
    } else {
        render_value(&value);
    }
}

fn value_summary(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => format!("{s:?}"),
        serde_json::Value::Array(a) => format!("[{} items]", a.len()),
        serde_json::Value::Object(o) => format!("{{{} fields}}", o.len()),
        other => other.to_string(),
    }
}

fn render_value(value: &serde_json::Value) {
    match value {
        serde_json::Value::String(s) => println!("{s}"),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            println!("{}", json_pretty(value))
        }
        other => println!("{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use tower_mcp::client::ClientTransport;

    fn surface_with_a_task_capable_tool() -> Arc<RwLock<Surface>> {
        let tool = |name: &str, task: bool| -> ToolDefinition {
            let mut value = serde_json::json!({
                "name": name,
                "description": "",
                "inputSchema": { "type": "object" },
            });
            if task {
                value["execution"] = serde_json::json!({ "taskSupport": "optional" });
            }
            serde_json::from_value(value).expect("tool definition")
        };
        Arc::new(RwLock::new(Surface {
            tools: vec![tool("slow_add", true), tool("echo", false)],
            ..Default::default()
        }))
    }

    /// The hint after an interrupt is only useful where backgrounding is
    /// actually available, and wrong everywhere else.
    #[test]
    fn only_a_task_capable_tool_is_worth_suggesting_backgrounding_for() {
        let surface = surface_with_a_task_capable_tool();
        assert_eq!(
            backgroundable_tool(&surface, "slow_add a=1 b=2").as_deref(),
            Some("slow_add")
        );
        // Not task-capable: `&` would be refused.
        assert_eq!(backgroundable_tool(&surface, "echo message=hi"), None);
        // Already a task, so it returned long ago and this interrupt was
        // something else.
        assert_eq!(backgroundable_tool(&surface, "slow_add a=1 b=2 &"), None);
        // A built-in is not a tool, however long it took.
        assert_eq!(backgroundable_tool(&surface, "wait 1"), None);
        // Nothing the server offers by that name.
        assert_eq!(backgroundable_tool(&surface, "nope"), None);
        assert_eq!(backgroundable_tool(&surface, ""), None);
    }

    #[test]
    fn a_saved_oauth_profile_reports_what_a_script_needs() {
        let metadata = config::OAuthProfile {
            url: "https://mcp.example.com/mcp".to_string(),
            scopes: vec!["openid".to_string(), "offline_access".to_string()],
            client_id_metadata_document: None,
            authorization_server: None,
        };
        let value = saved_profile_json("work", &metadata);
        assert_eq!(value["profile"], "work");
        assert_eq!(value["serverUrl"], "https://mcp.example.com/mcp");
        assert_eq!(value["scopes"][0], "openid");
        assert_eq!(value["scopes"][1], "offline_access");
        // Three keys and no more. A credential must never reach stdout, and
        // the tokens are in the OS store rather than in this struct, so the
        // guard is that nothing new is added here without thought.
        assert_eq!(
            value.as_object().map(|object| object.len()),
            Some(3),
            "{value}"
        );
    }

    #[test]
    fn a_repeated_error_label_is_collapsed_to_one() {
        // The reported shape: an unreachable server, one label per wrapping
        // layer, and the only informative sentence pushed to the end.
        assert_eq!(
            collapse_repeated_label(
                "Transport error: Transport error: Transport error: HTTP request failed: refused"
            ),
            "Transport error: HTTP request failed: refused"
        );
        // One label is kept, so the kind of failure is still named.
        assert_eq!(
            collapse_repeated_label("Transport error: HTTP request failed: refused"),
            "Transport error: HTTP request failed: refused"
        );
    }

    #[test]
    fn collapsing_leaves_ordinary_messages_alone() {
        // Nothing repeats, so nothing is dropped, including messages whose
        // later segments look like labels.
        for message in [
            "unknown command: nope",
            "Server error: tool `x` failed: bad input",
            "no colon here",
            "",
            ": leading colon",
        ] {
            assert_eq!(collapse_repeated_label(message), message, "{message:?}");
        }
    }

    #[test]
    fn only_an_identical_label_collapses() {
        // A different label is a different layer saying a different thing.
        assert_eq!(
            collapse_repeated_label("Transport error: Server error: refused"),
            "Transport error: Server error: refused"
        );
    }

    /// A single-response transport for pinning the final discovery wire shape.
    struct DiscoveryTransport {
        result: serde_json::Value,
        incoming_tx: tokio::sync::mpsc::Sender<String>,
        incoming_rx: tokio::sync::mpsc::Receiver<String>,
        outgoing: Arc<Mutex<Vec<serde_json::Value>>>,
        connected: bool,
    }

    impl DiscoveryTransport {
        fn new(result: serde_json::Value) -> (Self, Arc<Mutex<Vec<serde_json::Value>>>) {
            let (incoming_tx, incoming_rx) = tokio::sync::mpsc::channel(4);
            let outgoing = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    result,
                    incoming_tx,
                    incoming_rx,
                    outgoing: outgoing.clone(),
                    connected: true,
                },
                outgoing,
            )
        }
    }

    #[async_trait]
    impl ClientTransport for DiscoveryTransport {
        async fn send(&mut self, message: &str) -> tower_mcp::Result<()> {
            let request: serde_json::Value = serde_json::from_str(message)?;
            self.outgoing.lock().unwrap().push(request.clone());
            if let Some(id) = request.get("id") {
                self.incoming_tx
                    .send(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "result": self.result,
                        })
                        .to_string(),
                    )
                    .await
                    .map_err(|error| tower_mcp::Error::Transport(error.to_string()))?;
            }
            Ok(())
        }

        async fn recv(&mut self) -> tower_mcp::Result<Option<String>> {
            Ok(self.incoming_rx.recv().await)
        }

        fn is_connected(&self) -> bool {
            self.connected
        }

        async fn close(&mut self) -> tower_mcp::Result<()> {
            self.connected = false;
            Ok(())
        }
    }

    fn jsonrpc(code: i32, message: &str) -> tower_mcp::Error {
        tower_mcp::Error::JsonRpc(tower_mcp::error::JsonRpcError {
            code,
            message: message.to_string(),
            data: None,
        })
    }

    #[test]
    fn protocol_selection_is_stable_by_default_and_final_is_exact() {
        let stable = Args::try_parse_from(["mcp-repl", "--demo"]).unwrap();
        assert_eq!(stable.protocol, ProtocolMode::Stable);
        assert_eq!(
            stable.protocol.support().unwrap().versions(),
            tower_mcp::protocol::SUPPORTED_PROTOCOL_VERSIONS
        );

        for value in ["2026-07-28", "final"] {
            let final_args =
                Args::try_parse_from(["mcp-repl", "--protocol", value, "--demo"]).unwrap();
            assert_eq!(final_args.protocol, ProtocolMode::Final);
            assert_eq!(
                final_args.protocol.support().unwrap().versions(),
                ["2026-07-28"]
            );
        }
    }

    #[test]
    fn oauth_cli_parses_standalone_and_connection_workflows() {
        let login = Args::try_parse_from([
            "mcp-repl",
            "--login",
            "work",
            "--http",
            "https://mcp.example/mcp",
            "--oauth-scope",
            "openid",
            "--oauth-scope",
            "offline_access",
            "--no-browser",
        ])
        .unwrap();
        assert_eq!(login.login.as_deref(), Some("work"));
        assert_eq!(login.oauth_scopes, ["openid", "offline_access"]);
        assert!(login.no_browser);

        let connection = Args::try_parse_from([
            "mcp-repl",
            "--oauth",
            "work",
            "--http",
            "https://mcp.example/mcp",
            "--exec",
            "tools",
            "--json",
        ])
        .unwrap();
        assert_eq!(connection.oauth.as_deref(), Some("work"));
        assert_eq!(connection.exec, ["tools"]);

        assert!(Args::try_parse_from(["mcp-repl", "--login", "work", "--logout", "work"]).is_err());
    }

    #[tokio::test]
    async fn stable_selection_uses_initialize() {
        let client = client_builder(ProtocolMode::Stable)
            .unwrap()
            .connect_simple(ChannelTransport::new(demo_router()))
            .await
            .unwrap();
        let info = establish_connection(&client, ProtocolMode::Stable)
            .await
            .unwrap();

        assert_eq!(info.server_info.name, "mcp-repl-demo");
        assert_eq!(
            info.protocol_version,
            tower_mcp::protocol::LATEST_PROTOCOL_VERSION
        );
        assert!(client.server_info().await.is_some());
        assert!(client.discovery().await.is_none());
    }

    #[tokio::test]
    async fn final_selection_uses_discover_with_required_metadata() {
        let (transport, outgoing) = DiscoveryTransport::new(serde_json::json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"tools": {}},
            "ttlMs": 0,
            "cacheScope": "private",
            "_meta": {
                "io.modelcontextprotocol/serverInfo": {
                    "name": "final-test-server",
                    "version": "1.0.0"
                }
            }
        }));
        let client = client_builder(ProtocolMode::Final)
            .unwrap()
            .connect_simple(transport)
            .await
            .unwrap();
        let info = establish_connection(&client, ProtocolMode::Final)
            .await
            .unwrap();

        assert_eq!(info.server_info.name, "final-test-server");
        assert_eq!(info.protocol_version, "2026-07-28");
        assert!(client.server_info().await.is_none());
        assert!(client.discovery().await.is_some());

        let sent = outgoing.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["method"], "server/discover");
        assert_eq!(
            sent[0]["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"],
            "2026-07-28"
        );
        assert!(
            sent[0]["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"].is_object()
        );
        assert!(
            sent[0]["params"]["_meta"]["io.modelcontextprotocol/clientCapabilities"]["extensions"]
                [tower_mcp::protocol::TASKS_EXTENSION_ID]
                .is_object()
        );
        assert_eq!(
            sent[0]["params"]["_meta"]["io.modelcontextprotocol/clientInfo"]["name"],
            "mcp-repl"
        );
    }

    #[test]
    fn build_http_config_sets_bearer_and_trims_headers() {
        let cfg = build_http_config(
            Some("tok".into()),
            &["X-Api-Key: abc".into(), "X-Trim :  v ".into()],
            None,
            &[],
        )
        .unwrap();
        assert_eq!(
            cfg.headers.get("Authorization").map(String::as_str),
            Some("Bearer tok")
        );
        assert_eq!(
            cfg.headers.get("X-Api-Key").map(String::as_str),
            Some("abc")
        );
        assert_eq!(cfg.headers.get("X-Trim").map(String::as_str), Some("v"));
    }

    #[test]
    fn profile_auth_applies_and_flags_override_it() {
        let profile_headers = [
            ("X-Api-Key".to_string(), "from-profile".to_string()),
            ("X-Kept".to_string(), "profile".to_string()),
        ];
        // No flags: the profile's token and headers are used as-is.
        let cfg =
            build_http_config(None, &[], Some("profile-tok".into()), &profile_headers).unwrap();
        assert_eq!(
            cfg.headers.get("Authorization").map(String::as_str),
            Some("Bearer profile-tok")
        );
        assert_eq!(
            cfg.headers.get("X-Api-Key").map(String::as_str),
            Some("from-profile")
        );

        // Flags win over the profile, header by header.
        let cfg = build_http_config(
            Some("flag-tok".into()),
            &["X-Api-Key: from-flag".into()],
            Some("profile-tok".into()),
            &profile_headers,
        )
        .unwrap();
        assert_eq!(
            cfg.headers.get("Authorization").map(String::as_str),
            Some("Bearer flag-tok")
        );
        assert_eq!(
            cfg.headers.get("X-Api-Key").map(String::as_str),
            Some("from-flag")
        );
        assert_eq!(
            cfg.headers.get("X-Kept").map(String::as_str),
            Some("profile")
        );
    }

    #[test]
    fn oauth_precedence_is_explicit_static_then_cli_then_server_profile() {
        assert_eq!(
            selected_oauth_profile(Some("cli"), Some("server"), false, &[]),
            Some("cli".to_string())
        );
        assert_eq!(
            selected_oauth_profile(None, Some("server"), false, &[]),
            Some("server".to_string())
        );
        assert_eq!(
            selected_oauth_profile(Some("cli"), Some("server"), true, &[]),
            None
        );
        assert_eq!(
            selected_oauth_profile(
                Some("cli"),
                Some("server"),
                false,
                &["authorization: Basic explicit".to_string()],
            ),
            None
        );
        assert_eq!(
            selected_oauth_profile(
                Some("cli"),
                Some("server"),
                false,
                &["X-Tenant: acme".to_string()],
            ),
            Some("cli".to_string())
        );
    }

    #[test]
    fn selected_authorization_header_beats_environment_bearer() {
        let selected_headers = [("authorization".to_string(), "Basic selected".to_string())];
        let cfg = build_http_config_with_env(
            None,
            &[],
            None,
            &selected_headers,
            Some("ambient-token".into()),
        )
        .unwrap();
        assert_eq!(
            cfg.headers.get("authorization").map(String::as_str),
            Some("Basic selected")
        );

        let cfg = build_http_config_with_env(
            Some("explicit-token".into()),
            &[],
            None,
            &selected_headers,
            Some("ambient-token".into()),
        )
        .unwrap();
        assert_eq!(
            cfg.headers.get("Authorization").map(String::as_str),
            Some("Bearer explicit-token")
        );
    }

    #[test]
    fn explicit_oauth_suppresses_profile_and_environment_bearers() {
        let selected = selected_oauth_profile(Some("work"), None, false, &[]);
        assert_eq!(selected.as_deref(), Some("work"));

        let cfg = build_http_config_with_env(
            None,
            &[],
            selected.is_none().then(|| "profile-token".to_string()),
            &[],
            selected.is_none().then(|| "environment-token".to_string()),
        )
        .unwrap();
        assert!(!cfg.headers.contains_key("Authorization"));
    }

    #[test]
    fn build_http_config_rejects_header_without_colon() {
        let err = build_http_config(Some("tok".into()), &["nope".into()], None, &[]).unwrap_err();
        assert!(
            err.contains("nope"),
            "error should name the bad header: {err}"
        );
        assert!(
            err.contains("Name: Value"),
            "error should show the format: {err}"
        );
    }

    #[test]
    fn timing_formats_sub_second_and_seconds() {
        assert!(timing(Duration::from_millis(142)).contains("[142ms]"));
        assert!(timing(Duration::from_millis(2500)).contains("[2.50s]"));
    }

    // Completion and input highlighting both read BUILTINS, and an alias may
    // not shadow a name in it, so listing a command there is what makes it a
    // first-class built-in rather than a hidden one.
    #[test]
    fn bench_is_a_listed_builtin() {
        assert!(BUILTINS.iter().any(|(name, _)| *name == "bench"));
    }

    // Completion and highlighting both read BUILTINS, so membership is what
    // makes `find` completable rather than any code in the editor.
    #[test]
    fn find_is_a_completable_builtin() {
        assert!(BUILTINS.iter().any(|(name, _)| *name == "find"));
    }

    /// Generate into a buffer the way the flag generates onto stdout.
    fn completion_script(shell: clap_complete::Shell) -> String {
        let mut command = <Args as clap::CommandFactory>::command();
        let mut out = Vec::new();
        clap_complete::generate(shell, &mut command, "mcp-repl", &mut out);
        String::from_utf8(out).expect("completion scripts are UTF-8")
    }

    #[test]
    fn every_shell_gets_a_script_naming_the_binary() {
        for shell in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
            clap_complete::Shell::PowerShell,
            clap_complete::Shell::Elvish,
        ] {
            let script = completion_script(shell);
            assert!(!script.is_empty(), "{shell} produced nothing");
            assert!(
                script.contains("mcp-repl"),
                "{shell} does not name the binary"
            );
        }
    }

    #[test]
    fn completion_covers_flags_and_their_values() {
        let bash = completion_script(clap_complete::Shell::Bash);
        // A flag the user reaches for, and one added late: if the generator
        // is wired to a stale command, these are what go missing.
        for flag in [
            "--protocol",
            "--http",
            "--elicitation",
            "--timeout",
            "--man",
        ] {
            assert!(bash.contains(flag), "bash completion is missing {flag}");
        }
        // Enum values matter more than the flag names: they are what a user
        // cannot guess.
        for value in ["stable", "2026-07-28", "decline", "compatible"] {
            assert!(
                bash.contains(value),
                "bash completion is missing value {value}"
            );
        }
    }

    #[test]
    fn the_man_page_renders_with_the_real_sections() {
        let command = <Args as clap::CommandFactory>::command();
        let mut page = Vec::new();
        clap_mangen::Man::new(command)
            .render(&mut page)
            .expect("man page renders");
        let roff = String::from_utf8(page).expect("roff is UTF-8");
        assert!(roff.contains("mcp-repl"));
        for section in [".SH NAME", ".SH SYNOPSIS", ".SH DESCRIPTION", ".SH OPTIONS"] {
            assert!(roff.contains(section), "man page has no {section}");
        }
        // The description is the long one, not the one-liner. The apostrophe
        // arrives as the roff escape clap_mangen emits for it.
        assert!(roff.contains("surface is the command set"));
    }

    #[test]
    fn every_builtin_can_explain_itself() {
        // `help <name>` and `describe <name>` both read this table, so a
        // built-in missing from it answers with nothing useful.
        for (name, _) in BUILTINS {
            assert!(
                builtin_help(name).is_some(),
                "`{name}` has no usage line; add one to BUILTIN_HELP"
            );
        }
        // And nothing in the table names a command that does not exist.
        for (name, _, _) in BUILTIN_HELP {
            assert!(
                BUILTINS.iter().any(|(builtin, _)| builtin == name),
                "BUILTIN_HELP documents `{name}`, which is not a built-in"
            );
        }
    }

    #[test]
    fn an_example_invocation_shows_required_arguments_first() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "b": {"type": "integer"},
                "a": {"type": "integer"},
                "note": {"type": "string"},
            },
            "required": ["a", "b"],
        });
        let example = example_invocation("add", &schema);
        assert!(
            example.starts_with("add a=<integer> b=<integer>"),
            "{example}"
        );
        assert!(example.contains("[note=<string>]"), "{example}");
    }

    #[test]
    fn an_example_invocation_follows_a_ref_into_defs() {
        // What a schema generator actually emits: the named type is hoisted
        // into `$defs` and the property only points at it.
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "to": {"$ref": "#/$defs/Scale"},
                "value": {"type": "number"},
            },
            "required": ["value", "to"],
            "$defs": {
                "Scale": {"type": "string", "enum": ["celsius", "kelvin"]},
            },
        });
        let example = example_invocation("convert", &schema);
        assert!(example.contains("to=celsius"), "{example}");
        assert!(example.contains("value=<number>"), "{example}");
    }

    #[test]
    fn an_example_invocation_prefers_enum_values_to_types() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {"mode": {"type": "string", "enum": ["fast", "slow"]}},
            "required": ["mode"],
        });
        assert_eq!(example_invocation("run", &schema), "run mode=fast");
    }

    #[test]
    fn a_tool_without_properties_still_has_an_example() {
        let schema = serde_json::json!({"type": "object", "additionalProperties": true});
        assert_eq!(example_invocation("about", &schema), "about");
    }

    /// The whole `key=value` path, from typed line to the JSON the server
    /// receives.
    ///
    /// The splitter is tested separately; this covers the part that made #48
    /// worth filing, where a mis-split value still produces a valid call and
    /// the mistake only shows up in the result.
    #[test]
    fn quoted_arguments_reach_the_server_intact() {
        // A schema with the shapes coercion actually branches on.
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "mission": {"type": "string"},
                "count": {"type": "integer"},
                "flag": {"type": "boolean"},
                "untyped": {},
            },
        });
        let arguments = |line: &str| -> serde_json::Value {
            let parsed = command::parse(line).expect("parses");
            let tokens: Vec<&str> = parsed.words[1..].iter().map(String::as_str).collect();
            parse_kv_args(&schema, &tokens)
        };

        assert_eq!(
            arguments(r#"tool mission="two words" count=2"#),
            serde_json::json!({"mission": "two words", "count": 2})
        );
        assert_eq!(
            arguments("tool mission='two words'"),
            serde_json::json!({"mission": "two words"})
        );
        assert_eq!(
            arguments(r"tool mission=two\ words"),
            serde_json::json!({"mission": "two words"})
        );
        assert_eq!(
            arguments(r#"tool mission="say \"hi\"""#),
            serde_json::json!({"mission": "say \"hi\""})
        );
        // An empty value is sent, not dropped.
        assert_eq!(
            arguments(r#"tool mission="""#),
            serde_json::json!({"mission": ""})
        );
        // A quoted value that looks like another argument stays one value.
        assert_eq!(
            arguments(r#"tool mission="count=9""#),
            serde_json::json!({"mission": "count=9"})
        );
        // Typed coercion still applies to the value the quotes produced.
        assert_eq!(
            arguments(r#"tool count="7" flag="true""#),
            serde_json::json!({"count": 7, "flag": true})
        );
        // An untyped property takes a JSON literal, and quoted text that is
        // not JSON stays a string.
        assert_eq!(
            arguments(r#"tool untyped="two words""#),
            serde_json::json!({"untyped": "two words"})
        );
    }

    #[test]
    fn read_flags_are_separated_from_the_uri() {
        let (out, force, rest) =
            parse_read_flags(&["note://status", "--out", "/tmp/x", "--force"]).unwrap();
        assert_eq!(out.as_deref(), Some("/tmp/x"));
        assert!(force);
        assert_eq!(rest, vec!["note://status"]);

        // The attached form, and the plain call.
        let (out, force, rest) = parse_read_flags(&["--out=/tmp/y", "note://status"]).unwrap();
        assert_eq!(out.as_deref(), Some("/tmp/y"));
        assert!(!force);
        assert_eq!(rest, vec!["note://status"]);

        let (out, _, rest) = parse_read_flags(&["note://status"]).unwrap();
        assert_eq!(out, None);
        assert_eq!(rest, vec!["note://status"]);
    }

    #[test]
    fn read_flag_errors_say_what_is_wrong() {
        assert!(parse_read_flags(&["note://x", "--out"]).is_err());
        assert!(parse_read_flags(&["note://x", "--out="]).is_err());
        assert!(parse_read_flags(&["note://x", "--nope"]).is_err());
    }

    #[test]
    fn saving_decodes_a_blob_and_writes_text_as_is() {
        use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
        let dir = tempfile::tempdir().unwrap();

        let content = |text: Option<&str>, blob: Option<&str>| ResourceContent {
            uri: "x://y".to_string(),
            mime_type: None,
            text: text.map(str::to_string),
            blob: blob.map(str::to_string),
            meta: None,
        };

        // Text goes out byte for byte, with no trailing newline added.
        let text_path = dir.path().join("note.txt");
        let result = ReadResourceResult {
            contents: vec![content(
                Some(
                    "hello
world",
                ),
                None,
            )],
            ..Default::default()
        };
        let written = save_resource(&result, text_path.to_str().unwrap()).unwrap();
        assert_eq!(written, 11);
        assert_eq!(std::fs::read_to_string(&text_path).unwrap(), "hello\nworld");

        // A blob is decoded, so the file is the bytes rather than base64.
        let png_path = dir.path().join("pixel.png");
        let result = ReadResourceResult {
            contents: vec![content(None, Some(PIXEL_PNG_FOR_TEST))],
            ..Default::default()
        };
        let written = save_resource(&result, png_path.to_str().unwrap()).unwrap();
        let bytes = std::fs::read(&png_path).unwrap();
        assert_eq!(written, bytes.len());
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a PNG header");
    }

    /// The same pixel the demo serves.
    const PIXEL_PNG_FOR_TEST: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    #[test]
    fn saving_refuses_what_it_cannot_write_faithfully() {
        use tower_mcp::protocol::{ReadResourceResult, ResourceContent};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out");
        let empty = ReadResourceResult::default();
        assert!(save_resource(&empty, path.to_str().unwrap()).is_err());

        // Concatenating several contents would produce a file that is none
        // of them.
        let two = ReadResourceResult {
            contents: vec![
                ResourceContent {
                    uri: "a".into(),
                    mime_type: None,
                    text: Some("one".into()),
                    blob: None,
                    meta: None,
                },
                ResourceContent {
                    uri: "b".into(),
                    mime_type: None,
                    text: Some("two".into()),
                    blob: None,
                    meta: None,
                },
            ],
            ..Default::default()
        };
        assert!(save_resource(&two, path.to_str().unwrap()).is_err());
        // Nothing was written for either refusal.
        assert!(!path.exists());
    }

    #[test]
    fn counted_nouns_agree_with_their_number() {
        assert_eq!(plural(0, "tool"), "0 tools");
        assert_eq!(plural(1, "tool"), "1 tool");
        assert_eq!(plural(2, "template"), "2 templates");
    }

    #[test]
    fn only_commands_with_a_value_accept_capture_and_filter() {
        // The set backing the guard: a command that reports rather than
        // returning data must not silently swallow a capture.
        for routable in ["tools", "describe", "read", "find", "info"] {
            assert!(
                ROUTABLE_BUILTINS.contains(&routable),
                "{routable} returns a documented value"
            );
        }
        for reporting in ["help", "alias", "wire", "refresh", "quit", "unset"] {
            assert!(
                !ROUTABLE_BUILTINS.contains(&reporting),
                "{reporting} has no value to capture"
            );
        }
        // Every routable name is a real built-in, so the guard cannot
        // advertise a command that does not exist.
        for name in ROUTABLE_BUILTINS {
            assert!(
                BUILTINS.iter().any(|(builtin, _)| builtin == name),
                "{name} is not a built-in"
            );
        }
    }

    #[test]
    fn error_json_is_a_valid_object() {
        let v = error_json(ExitStatus::Usage, "boom: it broke");
        assert_eq!(v["error"], "boom: it broke");
        assert_eq!(v["kind"], "usage");
        assert_eq!(v["exitStatus"], 2);
    }

    /// A server that always hands back a fresh cursor describes an endless
    /// surface. The fetch has to end anyway.
    #[tokio::test]
    async fn pagination_stops_at_the_page_cap() {
        let mut pages = 0usize;
        let items: Vec<u32> = collect_pages("tools", |cursor| {
            pages += 1;
            let next = cursor.map_or(0u32, |c| c.parse::<u32>().unwrap_or(0) + 1);
            async move { Ok((vec![next], Some((next + 1).to_string()))) }
        })
        .await
        .unwrap();
        assert_eq!(pages, MAX_SURFACE_PAGES);
        assert_eq!(items.len(), MAX_SURFACE_PAGES);
    }

    #[tokio::test]
    async fn pagination_stops_at_the_item_cap() {
        // 500 entries a page: the item cap is reached well before the page
        // cap would apply.
        let items: Vec<u32> = collect_pages("tools", |cursor| {
            let n = cursor.map_or(0u32, |c| c.parse::<u32>().unwrap_or(0) + 1);
            async move { Ok((vec![n; 500], Some((n + 1).to_string()))) }
        })
        .await
        .unwrap();
        assert_eq!(items.len(), MAX_SURFACE_ITEMS);
    }

    #[tokio::test]
    async fn pagination_stops_when_a_cursor_repeats() {
        let mut pages = 0usize;
        let items: Vec<u32> = collect_pages("prompts", |_cursor| {
            pages += 1;
            async move { Ok((vec![1], Some("same".to_string()))) }
        })
        .await
        .unwrap();
        // First page sets the cursor, second sees it again and stops.
        assert_eq!(pages, 2);
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn pagination_follows_an_ordinary_multi_page_surface() {
        let items: Vec<u32> = collect_pages("tools", |cursor| async move {
            match cursor.as_deref() {
                None => Ok((vec![1, 2], Some("page2".to_string()))),
                Some("page2") => Ok((vec![3], None)),
                other => panic!("unexpected cursor {other:?}"),
            }
        })
        .await
        .unwrap();
        assert_eq!(items, vec![1, 2, 3]);
    }

    #[test]
    fn wait_accepts_an_explicit_deadline() {
        let (limit, rest) = parse_wait_timeout("wait", &["task-1", "--timeout", "30"]).unwrap();
        assert_eq!(limit, Some(Duration::from_secs(30)));
        assert_eq!(rest, vec!["task-1"]);

        let (limit, rest) = parse_wait_timeout("wait", &["--timeout=5", "task-1"]).unwrap();
        assert_eq!(limit, Some(Duration::from_secs(5)));
        assert_eq!(rest, vec!["task-1"]);

        // Zero is the documented way to ask for no deadline at all.
        let (limit, _) = parse_wait_timeout("wait", &["task-1", "--timeout", "0"]).unwrap();
        assert_eq!(limit, None);

        let (limit, rest) = parse_wait_timeout("wait", &["task-1"]).unwrap();
        assert_eq!(limit, None);
        assert_eq!(rest, vec!["task-1"]);
    }

    #[test]
    fn wait_deadline_errors_are_explained() {
        assert!(parse_wait_timeout("wait", &["t", "--timeout"]).is_err());
        assert!(parse_wait_timeout("wait", &["t", "--timeout", "soon"]).is_err());
        // The flag is meaningless on a single round-trip, so say so rather
        // than accepting it silently.
        assert!(parse_wait_timeout("task", &["t", "--timeout", "5"]).is_err());
    }

    #[test]
    fn automatic_task_updates_are_interactive_text_only() {
        assert!(automatic_task_updates(false, false));
        assert!(!automatic_task_updates(true, false));
        assert!(!automatic_task_updates(true, true));
        assert!(!automatic_task_updates(false, true));
    }

    #[test]
    fn quoted_task_arguments_reach_schema_coercion_intact() {
        let parsed = command::parse(
            r#"run.start instruction="Reply with exactly hello" mode=interactive &"#,
        )
        .unwrap();
        let tokens: Vec<&str> = parsed.words[1..].iter().map(String::as_str).collect();
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "instruction": { "type": "string" },
                "mode": { "type": "string" }
            }
        });

        assert!(parsed.background);
        assert_eq!(
            parse_kv_args(&schema, &tokens),
            serde_json::json!({
                "instruction": "Reply with exactly hello",
                "mode": "interactive"
            })
        );
    }

    // The persistent-history path relies on FileBackedHistory buffering saves
    // in memory and only writing on sync() (which sync_history() calls). The
    // REPL exits abruptly without dropping the editor, so it syncs after each
    // accepted line; this pins the assumption that save + sync reaches disk.
    #[test]
    fn file_backed_history_writes_on_sync() {
        use reedline::{FileBackedHistory, History, HistoryItem};
        let path = std::env::temp_dir().join(format!("mcp-repl-hist-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut h = FileBackedHistory::with_file(10, path.clone()).unwrap();
            h.save(HistoryItem::from_command_line("echo persisted"))
                .unwrap();
            h.sync().unwrap();
        }
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("echo persisted"),
            "history was not written to disk: {contents:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// A connected, initialized client over the in-process demo router, so
    /// the reconnect path can be exercised without a socket.
    async fn demo_client() -> McpClient {
        let client = McpClient::builder()
            .connect_simple(ChannelTransport::new(demo_router()))
            .await
            .unwrap();
        client.initialize("mcp-repl-test", "0").await.unwrap();
        client
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundled_slow_task_announces_completion_without_manual_polling() {
        let session = Arc::new(Session::new(demo_client().await, None));
        let surface = Arc::new(RwLock::new(Surface::default()));
        let output = AsyncOutput::new(Arc::new(AtomicBool::new(true)), true);
        let printer = output.external_printer().unwrap();
        let jobs = Arc::new(Jobs::new(output, true));
        let schema_contracts = schema_contract::ContractSet::default();

        run_tool(
            &session,
            &surface,
            &jobs,
            &schema_contracts,
            "slow_add",
            serde_json::json!({ "a": 2, "b": 3 }),
            true,
            &vars::Output::default(),
        )
        .await;

        let line = tokio::time::timeout(Duration::from_secs(6), async {
            loop {
                if let Some(line) = printer.get_line() {
                    break line;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("the task watcher should observe slow_add completion");

        assert!(line.contains("completed"), "{line}");
        assert_eq!(
            jobs.list()[0].status,
            tower_mcp::protocol::TaskStatus::Completed
        );
    }

    /// A session whose connector builds a fresh demo client, counting how
    /// many times it is asked to.
    async fn demo_session() -> (Arc<Session>, Arc<std::sync::atomic::AtomicUsize>) {
        let connects = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = connects.clone();
        let connector: Connector = Box::new(move || {
            let counter = counter.clone();
            Box::pin(async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(demo_client().await)
            })
        });
        (
            Arc::new(Session::new(demo_client().await, Some(connector))),
            connects,
        )
    }

    /// The regression this fixes: the server drops the session mid-command,
    /// so the call fails with not-initialized. The next attempt must succeed
    /// on a rebuilt session rather than leaving a dead prompt.
    #[tokio::test(flavor = "multi_thread")]
    async fn dropped_session_is_rebuilt_and_the_command_retried() {
        let (session, connects) = demo_session().await;
        let surface = Arc::new(RwLock::new(Surface::default()));
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let dead = Arc::as_ptr(&session.client()) as usize;
        let seen: Arc<RwLock<Vec<usize>>> = Arc::new(RwLock::new(Vec::new()));

        let (calls, saw) = (attempts.clone(), seen.clone());
        let result = with_reconnect(&session, &surface, |c| {
            let (calls, saw) = (calls.clone(), saw.clone());
            async move {
                saw.write().unwrap().push(Arc::as_ptr(&c) as usize);
                // First attempt sees the session the server has forgotten.
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(jsonrpc(
                        -32600,
                        "Client must send notifications/initialized before making requests",
                    ));
                }
                c.call_tool("echo", serde_json::json!({ "message": "alive" }))
                    .await
            }
        })
        .await
        .expect("the retried call should succeed on the rebuilt session");

        assert_eq!(attempts.load(Ordering::SeqCst), 2, "one retry, not a loop");
        // The retry has to run against the rebuilt client, not the dead one.
        let seen = seen.read().unwrap();
        assert_eq!(seen[0], dead);
        assert_ne!(seen[1], dead, "the retry reused the dead client");
        assert_eq!(
            connects.load(Ordering::SeqCst),
            1,
            "reconnected exactly once"
        );
        assert_eq!(session.generation(), 1);
        match result.content.first() {
            Some(Content::Text { text, .. }) => assert_eq!(text, "alive"),
            other => panic!("unexpected content: {other:?}"),
        }
        // The surface is re-fetched from the new session, not left stale.
        assert!(
            !surface.read().unwrap().tools.is_empty(),
            "surface should be refreshed after reconnect"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_still_dead_server_surfaces_the_error_after_one_retry() {
        let (session, connects) = demo_session().await;
        let surface = Arc::new(RwLock::new(Surface::default()));
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let calls = attempts.clone();
        let err = with_reconnect(&session, &surface, |_c| {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(tower_mcp::Error::Transport(
                    "HTTP 503 Service Unavailable from server: ".into(),
                ))
            }
        })
        .await
        .unwrap_err();

        assert!(is_session_lost(&err));
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "bounded to one retry");
        assert_eq!(connects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ordinary_errors_do_not_reconnect() {
        let (session, connects) = demo_session().await;
        let surface = Arc::new(RwLock::new(Surface::default()));
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let calls = attempts.clone();
        let err = with_reconnect(&session, &surface, |_c| {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(jsonrpc(-32602, "Invalid params"))
            }
        })
        .await
        .unwrap_err();

        assert!(matches!(err, tower_mcp::Error::JsonRpc(j) if j.code == -32602));
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "no retry");
        assert_eq!(connects.load(Ordering::SeqCst), 0, "no reconnect");
    }

    /// `--no-reconnect`, and the stdio/demo transports, produce a session with
    /// no connector: session-loss errors must pass straight through.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_without_a_connector_never_retries() {
        let session = Arc::new(Session::new(demo_client().await, None));
        let surface = Arc::new(RwLock::new(Surface::default()));
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        assert!(!session.can_reconnect());
        let calls = attempts.clone();
        let err = with_reconnect(&session, &surface, |_c| {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(tower_mcp::Error::SessionExpired)
            }
        })
        .await
        .unwrap_err();

        assert!(matches!(err, tower_mcp::Error::SessionExpired));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }
}
