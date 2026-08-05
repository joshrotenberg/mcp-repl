//! Black-box coverage for the published `mcp-repl` process boundary.

use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

// Each process normally finishes in well under a second, but beta and Windows
// runners can be CPU-starved while the all-target workspace job is active.
// Keep hangs bounded without treating scheduler stalls as product failures.
const CASE_TIMEOUT: Duration = Duration::from_secs(60);
const BUILD_TIMEOUT: Duration = Duration::from_secs(180);
const SUITE_TIMEOUT: Duration = Duration::from_secs(600);

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .canonicalize()
        .expect("repository root")
}

async fn run(mut command: Command, label: &str, timeout: Duration) -> Output {
    // Capture through files rather than `Child::wait_with_output`. On Windows,
    // a server grandchild can retain an inherited pipe handle after mcp-repl
    // exits, which makes waiting for pipe EOF look like a hung parent process.
    let mut stdout = tempfile::tempfile().expect("create stdout capture");
    let mut stderr = tempfile::tempfile().expect("create stderr capture");
    command
        .stdin(std::process::Stdio::null())
        .stdout(stdout.try_clone().expect("clone stdout capture"))
        .stderr(stderr.try_clone().expect("clone stderr capture"))
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("spawn {label}: {error}"));
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(result) => result.unwrap_or_else(|error| panic!("wait for {label}: {error}")),
        Err(_) => {
            let _ = child.kill().await;
            let stdout = read_capture(&mut stdout, label, "stdout");
            let stderr = read_capture(&mut stderr, label, "stderr");
            panic!(
                "{label} exceeded {timeout:?}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            );
        }
    };
    Output {
        status,
        stdout: read_capture(&mut stdout, label, "stdout"),
        stderr: read_capture(&mut stderr, label, "stderr"),
    }
}

fn read_capture(file: &mut std::fs::File, label: &str, stream: &str) -> Vec<u8> {
    file.rewind()
        .unwrap_or_else(|error| panic!("rewind {label} {stream}: {error}"));
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .unwrap_or_else(|error| panic!("read {label} {stream}: {error}"));
    bytes
}

fn assert_success(output: &Output, label: &str) {
    assert!(
        output.status.success(),
        "{label} failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn assert_status(output: &Output, expected: i32, label: &str) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "{label} had unexpected status {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn json_lines(output: &Output, label: &str) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line).unwrap_or_else(|error| {
                panic!(
                    "{label} stdout line {} is not JSON: {error}: {line}",
                    index + 1
                )
            })
        })
        .collect()
}

async fn build_fixture() -> PathBuf {
    let mut command = Command::new(env!("CARGO"));
    command.current_dir(repo_root()).args([
        "build",
        "--quiet",
        "--example",
        "mcp_repl_fixture",
        "--message-format=json-render-diagnostics",
    ]);
    // Coverage and beta jobs may need to compile the repository-only fixture
    // with a distinct target configuration. Keep that budget independent of
    // the much tighter timeout used to detect hung mcp-repl processes.
    let output = run(command, "fixture build", BUILD_TIMEOUT).await;
    assert_success(&output, "fixture build");

    // The outer test runner may select a different target directory (notably
    // cargo-llvm-cov). Cargo's artifact record is authoritative; deriving the
    // fixture path from the integration-test executable only works when both
    // Cargo invocations happen to share a target directory.
    let fixture = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|message| {
            (message["reason"] == "compiler-artifact"
                && message["target"]["name"] == "mcp_repl_fixture")
                .then(|| message["executable"].as_str().map(PathBuf::from))
                .flatten()
        })
        .expect("Cargo did not report the mcp_repl_fixture executable");
    assert!(
        fixture.is_file(),
        "fixture was not built at {}",
        fixture.display()
    );
    fixture
}

fn repl_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mcp-repl"));
    command.current_dir(repo_root());
    command
}

async fn wait_for_file(path: &Path, label: &str) -> String {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match std::fs::read_to_string(path) {
                Ok(contents) => break contents,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("read {label}: {error}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
}

async fn run_stdio(fixture: &Path, temp: &TempDir, case: &str, repl_args: &[&str]) -> Output {
    let exit_file = temp.path().join(format!("{case}.exit"));
    let mut command = repl_command();
    command
        .args(repl_args)
        .arg(fixture)
        .env("MCP_REPL_FIXTURE_EXIT_FILE", &exit_file);
    let output = run(command, case, CASE_TIMEOUT).await;
    assert_eq!(
        wait_for_file(&exit_file, "stdio fixture shutdown").await,
        "clean",
        "mcp-repl left its stdio child running"
    );
    output
}

struct HttpFixture {
    child: Option<Child>,
    url: String,
    subscription_file: PathBuf,
}

impl HttpFixture {
    async fn start(fixture: &Path, temp: &TempDir) -> Self {
        let ready_file = temp.path().join("http.ready");
        let subscription_file = temp.path().join("http.subscription");
        let mut command = Command::new(fixture);
        command
            .arg("--http")
            .env("MCP_REPL_FIXTURE_READY_FILE", &ready_file)
            .env("MCP_REPL_FIXTURE_SUBSCRIPTION_FILE", &subscription_file)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let child = command.spawn().expect("spawn HTTP fixture");
        let url = wait_for_file(&ready_file, "HTTP fixture readiness").await;
        Self {
            child: Some(child),
            url,
            subscription_file,
        }
    }

    async fn shutdown(mut self) {
        let mut child = self.child.take().expect("HTTP fixture child");
        child.start_kill().expect("stop HTTP fixture");
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .expect("HTTP fixture did not exit")
            .expect("wait for HTTP fixture");
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

async fn run_http(url: &str, case: &str, repl_args: &[&str]) -> Output {
    let mut command = repl_command();
    command.args(repl_args).args(["--http", url]);
    run(command, case, CASE_TIMEOUT).await
}

async fn auth_failure_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind auth failure server");
    let url = format!(
        "http://{}/",
        listener.local_addr().expect("auth server address")
    );
    let task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0_u8; 8 * 1024];
                let _ = stream.read(&mut request).await;
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 401 Unauthorized\r\n\
                          Content-Length: 0\r\n\
                          WWW-Authenticate: Bearer\r\n\
                          Connection: close\r\n\r\n",
                    )
                    .await;
            });
        }
    });
    (url, task)
}

async fn exercise_json_contract(fixture: &Path, temp: &TempDir) {
    // Keep one round trip after `announce` so the asynchronous notification
    // handler drains before the one-shot process exits, including on Windows.
    let multiple = run_stdio(
        fixture,
        temp,
        "json-multiple",
        &[
            "--json",
            "--verbose",
            "--trace",
            "--exec",
            "tools",
            "--exec",
            "announce",
            "--exec",
            "add a=20 b=22",
        ],
    )
    .await;
    assert_success(&multiple, "multiple JSON commands");
    let values = json_lines(&multiple, "multiple JSON commands");
    assert_eq!(values.len(), 3, "one JSON line must be emitted per command");
    assert!(
        values[0].is_array(),
        "tools returns the raw MCP list: {values:?}"
    );
    assert_eq!(
        values[1].pointer("/content/0/text"),
        Some(&serde_json::json!("announced"))
    );
    assert_eq!(
        values[2].pointer("/content/0/text"),
        Some(&serde_json::json!("42"))
    );
    assert!(
        !String::from_utf8_lossy(&multiple.stdout).contains("connected:"),
        "--verbose must not contaminate JSON stdout"
    );
    let stderr = String::from_utf8_lossy(&multiple.stderr);
    assert!(stderr.contains("fixture announcement"), "{stderr}");
    assert!(
        stderr.contains("tools/list"),
        "wire tracing stayed off: {stderr}"
    );

    let no_match = run_stdio(
        fixture,
        temp,
        "json-no-match",
        &["--json", "--exec", "find definitely-not-on-the-surface"],
    )
    .await;
    assert_status(&no_match, 1, "no-match outcome");
    assert_eq!(
        json_lines(&no_match, "no-match outcome"),
        [serde_json::json!([])]
    );

    let continued = run_stdio(
        fixture,
        temp,
        "json-continued",
        &[
            "--json",
            "--exec",
            "no_such_command",
            "--exec",
            "add a=20 b=22",
        ],
    )
    .await;
    assert_status(&continued, 2, "usage error");
    let values = json_lines(&continued, "continued JSON commands");
    assert_eq!(values.len(), 2, "later commands must run after a failure");
    assert_eq!(values[0]["kind"], "usage");
    assert_eq!(values[0]["exitStatus"], 2);
    assert_eq!(
        values[1].pointer("/content/0/text"),
        Some(&serde_json::json!("42"))
    );

    let server_error = run_stdio(
        fixture,
        temp,
        "json-server-error",
        &["--json", "--exec", "fail"],
    )
    .await;
    assert_status(&server_error, 3, "tool error");
    let values = json_lines(&server_error, "tool error");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["isError"], true);

    let unavailable = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("reserve unavailable endpoint");
    let unavailable_url = format!(
        "http://{}/",
        unavailable.local_addr().expect("unavailable address")
    );
    drop(unavailable);
    let transport_error = run_http(
        &unavailable_url,
        "JSON transport error",
        &["--json", "--exec", "tools"],
    )
    .await;
    assert_status(&transport_error, 4, "transport error");
    let values = json_lines(&transport_error, "transport error");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["kind"], "transport");

    let (auth_url, auth_server) = auth_failure_server().await;
    let auth_error = run_http(&auth_url, "JSON auth error", &["--json", "--exec", "tools"]).await;
    auth_server.abort();
    assert_status(&auth_error, 5, "authentication error");
    let values = json_lines(&auth_error, "authentication error");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["kind"], "auth");
}

async fn exercise_imported_stdio_config(fixture: &Path, temp: &TempDir) {
    let workspace = temp.path().join("import-workspace");
    let cwd = workspace.join("work");
    std::fs::create_dir_all(&cwd).expect("create imported fixture cwd");
    let config = workspace.join(".mcp.json");
    std::fs::write(
        &config,
        serde_json::json!({
            "mcpServers": {
                "fixture": {
                    "command": fixture,
                    "env": {
                        "MCP_REPL_IMPORTED_VALUE": "${env:MCP_REPL_HOST_VALUE}"
                    },
                    "cwd": "${workspaceFolder}/work"
                }
            }
        })
        .to_string(),
    )
    .expect("write imported stdio config");
    let exit_file = temp.path().join("import-stdio.exit");
    let selector = format!("{}:fixture", config.display());

    // An imported entry names a command to execute, so a session with nobody
    // to ask refuses instead of spawning it.
    let mut unapproved = repl_command();
    unapproved
        .args(["--json", "--exec", "process_info", &selector])
        .env("MCP_REPL_HOST_VALUE", "from-host");
    let refused = run(unapproved, "unapproved imported stdio config", CASE_TIMEOUT).await;
    assert_status(&refused, 2, "unapproved imported stdio config");
    let refusal = json_lines(&refused, "unapproved imported stdio config");
    assert_eq!(refusal.len(), 1);
    assert_eq!(refusal[0]["kind"], "usage");
    let message = refusal[0]["error"]
        .as_str()
        .expect("refusal carries a message");
    assert!(
        message.contains("--trust-import"),
        "the refusal must say how to proceed, got: {message}"
    );

    let mut command = repl_command();
    command
        .args([
            "--json",
            "--trust-import",
            "--exec",
            "process_info",
            &selector,
        ])
        .env("MCP_REPL_HOST_VALUE", "from-host")
        // An HTTP credential in the environment must not be handed to a
        // spawned child.
        .env("MCP_BEARER", "http-only-secret")
        .env("MCP_REPL_FIXTURE_EXIT_FILE", &exit_file);
    let output = run(command, "imported stdio config", CASE_TIMEOUT).await;
    assert_success(&output, "imported stdio config");
    assert_eq!(
        wait_for_file(&exit_file, "imported stdio fixture shutdown").await,
        "clean"
    );
    let values = json_lines(&output, "imported stdio config");
    assert_eq!(values.len(), 1);
    let process: serde_json::Value = serde_json::from_str(
        values[0]
            .pointer("/content/0/text")
            .and_then(serde_json::Value::as_str)
            .expect("process_info text result"),
    )
    .expect("process_info JSON");
    assert_eq!(process["imported"], "from-host");
    assert_eq!(
        process["bearer"],
        serde_json::Value::Null,
        "MCP_BEARER must not reach a spawned stdio child"
    );
    assert_eq!(
        PathBuf::from(process["cwd"].as_str().expect("process cwd"))
            .canonicalize()
            .expect("canonical process cwd"),
        cwd.canonicalize().expect("canonical expected cwd")
    );
}

async fn exercise_schema_contracts(fixture: &Path, temp: &TempDir) {
    let snapshot_path = temp.path().join("add.schema.json");
    let snapshot_command = format!("snapshot add '{}'", snapshot_path.display());
    let exported = run_stdio(
        fixture,
        temp,
        "schema-export",
        &["--json", "--exec", &snapshot_command],
    )
    .await;
    assert_success(&exported, "schema snapshot export");
    let snapshot: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&snapshot_path).expect("read exported schema snapshot"),
    )
    .expect("exported schema snapshot JSON");
    assert_eq!(snapshot["formatVersion"], 1);
    assert_eq!(snapshot["kind"], "tool");
    assert_eq!(snapshot["name"], "add");

    let validate_command = format!("validate '{}' strict", snapshot_path.display());
    let validated = run_stdio(
        fixture,
        temp,
        "schema-validate",
        &["--json", "--exec", &validate_command],
    )
    .await;
    assert_success(&validated, "strict schema validation");
    let values = json_lines(&validated, "strict schema validation");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["compatible"], true);
    assert_eq!(values[0]["mode"], "strict");

    let mut incompatible = snapshot;
    incompatible["inputSchema"]["properties"]["a"]["type"] = serde_json::json!("string");
    std::fs::write(
        &snapshot_path,
        serde_json::to_string_pretty(&incompatible).unwrap(),
    )
    .expect("write incompatible schema snapshot");
    let snapshot_path = snapshot_path.to_string_lossy().into_owned();
    let blocked = run_stdio(
        fixture,
        temp,
        "schema-preflight",
        &[
            "--json",
            "--schema-contract",
            &snapshot_path,
            "--exec",
            "add a=20 b=22",
        ],
    )
    .await;
    assert_status(&blocked, 1, "incompatible schema preflight");
    let values = json_lines(&blocked, "incompatible schema preflight");
    assert_eq!(values.len(), 1, "the blocked tool must not emit a result");
    assert_eq!(values[0]["compatible"], false);
    assert!(
        values[0]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["code"] == "schema_retyped")
    );

    let human_validate = format!("validate '{}' compatible", snapshot_path);
    let explained = run_stdio(
        fixture,
        temp,
        "schema-human-report",
        &["--exec", &human_validate],
    )
    .await;
    assert_status(&explained, 1, "human schema validation");
    let stdout = String::from_utf8_lossy(&explained.stdout);
    assert!(stdout.contains("incompatible"), "{stdout}");
    assert!(stdout.contains("schema_retyped"), "{stdout}");
    assert!(
        stdout.contains("$.inputSchema.properties.a.type"),
        "{stdout}"
    );

    let prompt_path = temp.path().join("greet.schema.json");
    let snapshot_prompt = format!("snapshot prompt:greet '{}'", prompt_path.display());
    let exported = run_stdio(
        fixture,
        temp,
        "prompt-schema-export",
        &["--json", "--exec", &snapshot_prompt],
    )
    .await;
    assert_success(&exported, "prompt schema snapshot export");
    let mut prompt_snapshot: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&prompt_path).expect("read prompt schema snapshot"),
    )
    .expect("prompt schema snapshot JSON");
    prompt_snapshot["arguments"][0]["required"] = serde_json::json!(false);
    std::fs::write(
        &prompt_path,
        serde_json::to_string_pretty(&prompt_snapshot).unwrap(),
    )
    .expect("write incompatible prompt snapshot");
    let prompt_path = prompt_path.to_string_lossy().into_owned();
    let blocked = run_stdio(
        fixture,
        temp,
        "prompt-schema-preflight",
        &[
            "--json",
            "--schema-contract",
            &prompt_path,
            "--exec",
            "prompt greet name=Ada",
        ],
    )
    .await;
    assert_status(&blocked, 1, "incompatible prompt schema preflight");
    let values = json_lines(&blocked, "incompatible prompt schema preflight");
    assert_eq!(values.len(), 1, "the blocked prompt must not emit a result");
    assert!(
        values[0]["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["code"] == "argument_newly_required")
    );
}

async fn exercise_imported_http_config(http: &HttpFixture, temp: &TempDir) {
    let config = temp.path().join("vscode-mcp.json");
    std::fs::write(
        &config,
        serde_json::json!({
            "servers": {
                "fixture": {
                    "type": "http",
                    "url": "http://127.0.0.1:1/"
                }
            }
        })
        .to_string(),
    )
    .expect("write imported HTTP config");
    let selector = format!("{}:fixture", config.display());
    let mut command = repl_command();
    command.args([
        "--json",
        "--exec",
        "add a=20 b=22",
        "--http",
        &http.url,
        &selector,
    ]);
    let output = run(command, "imported HTTP config", CASE_TIMEOUT).await;
    assert_success(&output, "imported HTTP config");
    let values = json_lines(&output, "imported HTTP config");
    assert_eq!(values.len(), 1);
    assert_eq!(
        values[0].pointer("/content/0/text"),
        Some(&serde_json::json!("42"))
    );
}

async fn exercise_stdio(fixture: &Path, temp: &TempDir) {
    let stable = run_stdio(
        fixture,
        temp,
        "stable-stdio",
        &[
            "--protocol",
            "stable",
            "--verbose",
            "--exec",
            "add a=20 b=22",
        ],
    )
    .await;
    assert_success(&stable, "stable stdio");
    let stdout = String::from_utf8_lossy(&stable.stdout);
    let stderr = String::from_utf8_lossy(&stable.stderr);
    assert!(stdout.contains("protocol 2025-11-25"), "{stdout}");
    assert!(stdout.contains("42"), "{stdout}");
    assert!(stderr.contains("mcp-repl fixture ready"), "{stderr}");

    let final_ = run_stdio(
        fixture,
        temp,
        "final-stdio",
        &[
            "--protocol",
            "2026-07-28",
            "--verbose",
            "--exec",
            "add a=20 b=22",
            "--exec",
            "prompt greet name=Ada",
            "--exec",
            "read fixture://guide",
        ],
    )
    .await;
    assert_success(&final_, "final stdio");
    let stdout = String::from_utf8_lossy(&final_.stdout);
    assert!(stdout.contains("protocol 2026-07-28"), "{stdout}");
    assert!(stdout.contains("42"), "{stdout}");
    assert!(stdout.contains("Please greet Ada warmly."), "{stdout}");
    assert!(stdout.contains("fixture resource body"), "{stdout}");

    let error = run_stdio(
        fixture,
        temp,
        "json-error",
        &["--json", "--exec", "no_such_command"],
    )
    .await;
    assert!(!error.status.success(), "unknown command should fail");
    let stdout = String::from_utf8_lossy(&error.stdout);
    let stderr = String::from_utf8_lossy(&error.stderr);
    assert!(stdout.contains("\"error\""), "{stdout}");
    assert!(!stdout.contains("fixture ready"), "{stdout}");
    assert!(stderr.contains("mcp-repl fixture ready"), "{stderr}");
}

/// An `--exec` script cannot name a task it started: the id is generated by
/// the command that starts it, and the `-e` list is fixed before any of it
/// runs. Without a way to wait, the process exits and takes the connection
/// with it, abandoning work the server already began.
async fn exercise_exec_waits_for_its_own_tasks(fixture: &Path, temp: &TempDir) {
    let waited = run_stdio(
        fixture,
        temp,
        "exec-wait-all",
        &[
            "--protocol",
            "2026-07-28",
            "--no-history",
            "--color",
            "never",
            "--exec",
            "slow_add a=1 b=2 &",
            "--exec",
            "wait",
        ],
    )
    .await;
    assert_success(&waited, "exec wait all");
    let stdout = String::from_utf8_lossy(&waited.stdout);
    assert!(
        stdout.contains("status=completed"),
        "a bare wait settles the task started earlier in the same run:\n{stdout}"
    );
    assert!(
        stdout.contains("\n3\n"),
        "and reports its result, so the work was not abandoned:\n{stdout}"
    );

    // `last` names it without knowing the id, for a script that wants one
    // specific task rather than all of them.
    let last = run_stdio(
        fixture,
        temp,
        "exec-wait-last",
        &[
            "--protocol",
            "2026-07-28",
            "--no-history",
            "--color",
            "never",
            "--exec",
            "slow_add a=20 b=22 &",
            "--exec",
            "wait last",
        ],
    )
    .await;
    assert_success(&last, "exec wait last");
    assert!(
        String::from_utf8_lossy(&last.stdout).contains("\n42\n"),
        "`last` names the most recently started task:\n{}",
        String::from_utf8_lossy(&last.stdout)
    );

    // The point of waiting is to learn whether the work succeeded, so a
    // failed task has to reach the exit status. The fixture's failure arrives
    // as a *completed* task carrying an error, which is the shape a handler
    // returning `Err` produces, so judging on status alone would call it a
    // success.
    let failed = run_stdio(
        fixture,
        temp,
        "exec-wait-failure",
        &[
            "--protocol",
            "2026-07-28",
            "--no-history",
            "--color",
            "never",
            "--exec",
            "fail_slowly &",
            "--exec",
            "wait",
        ],
    )
    .await;
    assert_status(&failed, 3, "exec wait failure");
    assert!(
        String::from_utf8_lossy(&failed.stdout).contains("fixture task failure"),
        "the failure is reported, not only counted:\n{}",
        String::from_utf8_lossy(&failed.stdout)
    );

    // The other failure shape, and the one a server is most likely to send:
    // an `isError` result rather than a handler error. The demo's `fail` is
    // task-capable so this is reachable without a server of your own, which
    // is the whole point of it being there.
    let error_result = run_demo_answering(
        "",
        "demo task tool error",
        &["--exec", "fail &", "--exec", "wait"],
    )
    .await;
    assert_status(&error_result, 3, "demo task tool error");
    let stdout = String::from_utf8_lossy(&error_result.stdout);
    assert!(
        stdout.contains("tool error"),
        "a failed task settles as `completed`, so the error needs saying:\n{stdout}"
    );

    // Nothing to wait for is a distinct outcome from a task that failed.
    let empty = run_stdio(
        fixture,
        temp,
        "exec-wait-empty",
        &["--no-history", "--color", "never", "--exec", "wait"],
    )
    .await;
    assert_status(&empty, 1, "exec wait with no tasks");
}

/// An unreachable server is the first failure most people meet, so what it
/// prints is worth pinning.
///
/// It used to arrive twice: once as a framework log record and once as the
/// REPL's own error, with `Transport error:` repeated for each wrapping layer
/// and ANSI escapes that `--color never` was supposed to have turned off.
async fn exercise_connection_failure_output() {
    let mut command = repl_command();
    // Port 9 is discard: reserved, and nothing listens on it.
    command.args([
        "--http",
        "http://127.0.0.1:9/",
        "--no-history",
        "--color",
        "never",
        "--timeout",
        "5",
        "--exec",
        "tools",
    ]);
    let output = run(command, "connection failure", CASE_TIMEOUT).await;
    assert_status(&output, 4, "connection failure");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains('\u{1b}'),
        "--color never must reach the log subscriber too:\n{stderr:?}"
    );
    assert_eq!(
        stderr
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count(),
        1,
        "one failure is reported once:\n{stderr}"
    );
    assert_eq!(
        stderr.matches("Transport error:").count(),
        1,
        "the kind of failure is named once, not once per layer:\n{stderr}"
    );
    assert!(
        stderr.contains("HTTP request failed"),
        "and the informative part survives:\n{stderr}"
    );
}

/// Run the demo server, answering elicitation prompts from `stdin`.
async fn run_demo_answering(answers: &str, case: &str, repl_args: &[&str]) -> Output {
    let mut command = repl_command();
    command
        .args(["--demo", "--no-history", "--color", "never"])
        .args(repl_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("spawn {case}: {error}"));
    let mut stdin = child.stdin.take().expect("elicitation stdin");
    stdin
        .write_all(answers.as_bytes())
        .await
        .unwrap_or_else(|error| panic!("write {case} answers: {error}"));
    drop(stdin);
    tokio::time::timeout(CASE_TIMEOUT, child.wait_with_output())
        .await
        .unwrap_or_else(|_| panic!("{case} timed out"))
        .unwrap_or_else(|error| panic!("wait for {case}: {error}"))
}

/// A form's fields must be asked in the order the server declared them.
///
/// The schema is an ordered map and the order is protocol-significant, so
/// sorting the field names alphabetically silently pairs each answer with the
/// wrong field. Answering positionally is what catches it: the fields here are
/// declared username, environment, remember_me, which is not alphabetical, so
/// a re-sorted form signs in as `staging`.
async fn exercise_elicitation_field_order() {
    let output = run_demo_answering(
        "ada\nstaging\ny\n",
        "elicitation order",
        &["--elicitation", "prompt", "--exec", "sign_in"],
    )
    .await;
    assert_success(&output, "elicitation order");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.contains("signed in as ada"),
        "answers must land in declared order\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let username = stderr.find("username").expect("username prompt");
    let environment = stderr.find("environment").expect("environment prompt");
    assert!(
        username < environment,
        "username is declared first, so it is asked first:\n{stderr}"
    );
}

/// `respond` only works where a task can report what it is waiting for, and
/// the refusal has to explain the protocol rather than leak the framework's
/// "task_get_detailed requires ..." transport error.
async fn exercise_respond_needs_the_final_lifecycle() {
    let output = run_demo_answering(
        "",
        "respond on stable",
        &[
            "--protocol",
            "stable",
            // A real task, so the refusal is about the lifecycle rather than
            // an unresolvable id.
            "--exec",
            "slow_add a=1 b=2 &",
            "--exec",
            "task 1 respond",
        ],
    )
    .await;
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("2026-07-28"),
        "the refusal names the lifecycle that supports it:\n{stderr}"
    );
    assert!(
        !stderr.contains("task_get_detailed"),
        "an internal API name is not an explanation:\n{stderr}"
    );
}

/// Interrupting a call has to reach the server, not just free the prompt.
///
/// The framework cancels a request when the caller's future drops, which is
/// what `run_cancellable` does to the losing `select!` branch, so nothing in
/// this crate sends the notification. What is worth pinning is that the
/// notification goes out at all, that it names this call rather than every
/// pending request, and that a numeric id stays numeric on the way out.
#[cfg(unix)]
async fn exercise_cancellation() {
    let mut command = repl_command();
    command
        .args([
            "--demo",
            "--trace",
            "--no-history",
            "--color",
            "never",
            "--exec",
            "slow_add a=1 b=2",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("spawn cancellation case");
    let mut stderr = child.stderr.take().expect("cancellation stderr");
    let pid = child.id().expect("cancellation pid");

    // `slow_add` sleeps three seconds server-side, but interrupt on the trace
    // rather than on a timer: a loaded runner can spend longer than that
    // getting the process up and connected, and a signal that arrives before
    // the call is in flight would test nothing.
    let mut trace = String::new();
    tokio::time::timeout(CASE_TIMEOUT, async {
        let mut chunk = [0u8; 4096];
        loop {
            let read = stderr.read(&mut chunk).await.expect("read wire trace");
            if read == 0 {
                break;
            }
            trace.push_str(&String::from_utf8_lossy(&chunk[..read]));
            if trace.contains("\"slow_add\"") {
                break;
            }
        }
    })
    .await
    .expect("timed out waiting for the call to reach the wire");

    let signalled = Command::new("kill")
        .args(["-INT", &pid.to_string()])
        .status()
        .await
        .expect("send SIGINT");
    assert!(signalled.success(), "kill -INT {pid} failed");

    // The cancellation lands in whatever trace follows the signal.
    stderr
        .read_to_string(&mut trace)
        .await
        .expect("drain wire trace");
    let status = tokio::time::timeout(CASE_TIMEOUT, child.wait())
        .await
        .expect("cancellation case timed out")
        .expect("wait for cancellation case");

    assert_eq!(
        status.code(),
        Some(6),
        "an interrupted command exits cancelled:\n{trace}"
    );
    assert!(
        trace.contains("notifications/cancelled"),
        "the server is never told the call was abandoned:\n{trace}"
    );
    let id = traced_request_id(&trace, "tools/call");
    // Unquoted in the pretty-printed frame, so this also pins the JSON type:
    // a numeric id must not be reported as a string.
    assert!(
        trace.contains(&format!("\"requestId\": {id}")),
        "the cancellation names request {id}:\n{trace}"
    );
}

/// The id of the last traced request for `method`, read back out of the
/// pretty-printed frame that `--trace` writes.
#[cfg(unix)]
fn traced_request_id(trace: &str, method: &str) -> u64 {
    let frame = trace
        .rsplit_once(&format!("\"method\": \"{method}\""))
        .unwrap_or_else(|| panic!("no traced {method} request in:\n{trace}"))
        .0;
    let (_, id) = frame
        .rsplit_once("\"id\": ")
        .unwrap_or_else(|| panic!("traced {method} request carries no id in:\n{trace}"));
    id.trim_start()
        .trim_end_matches([',', '\n', ' '])
        .split(['\n', ','])
        .next()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or_else(|| panic!("unparsable {method} request id in:\n{trace}"))
}

async fn exercise_interactive_final_task(http: &HttpFixture) {
    let mut command = repl_command();
    command
        .args([
            "--protocol",
            "2026-07-28",
            "--no-history",
            "--color",
            "never",
            "--http",
            &http.url,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().expect("spawn interactive final mcp-repl");
    let mut stdin = child.stdin.take().expect("interactive stdin");
    stdin
        .write_all(b"slow_add a=2 b=3 &\n")
        .await
        .expect("write task command");
    wait_for_file(&http.subscription_file, "final subscription").await;
    // The subscription is immediate, while the bounded task poller remains a
    // fallback. The fixture advertises a two-second poll interval, so leave a
    // full extra second for the fallback to observe completion before asking
    // the editor thread to exit.
    tokio::time::sleep(Duration::from_millis(3_000)).await;
    stdin
        .write_all(b"jobs\nquit\n")
        .await
        .expect("write task status and quit commands");
    drop(stdin);
    let output = tokio::time::timeout(CASE_TIMEOUT, child.wait_with_output())
        .await
        .expect("interactive final case timed out")
        .expect("wait for interactive final case");
    assert_success(&output, "interactive final task");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("started"), "{stdout}");
    assert!(stdout.contains("completed"), "{stdout}");
}

async fn exercise_http(fixture: &Path, temp: &TempDir) {
    let http = HttpFixture::start(fixture, temp).await;

    exercise_imported_http_config(&http, temp).await;

    let stable = run_http(
        &http.url,
        "stable HTTP",
        &[
            "--protocol",
            "stable",
            "--verbose",
            "--exec",
            "prompt greet name=Grace",
            "--exec",
            "announce",
        ],
    )
    .await;
    assert_success(&stable, "stable HTTP");
    let stdout = String::from_utf8_lossy(&stable.stdout);
    let stderr = String::from_utf8_lossy(&stable.stderr);
    assert!(stdout.contains("protocol 2025-11-25"), "{stdout}");
    assert!(stdout.contains("Please greet Grace warmly."), "{stdout}");
    assert!(stderr.contains("fixture announcement"), "{stderr}");

    let final_ = run_http(
        &http.url,
        "final HTTP",
        &[
            "--protocol",
            "2026-07-28",
            "--json",
            "--exec",
            "add a=40 b=2",
            "--exec",
            "read fixture://guide",
        ],
    )
    .await;
    assert_success(&final_, "final HTTP");
    let stdout = String::from_utf8_lossy(&final_.stdout);
    assert!(stdout.contains("42"), "{stdout}");
    assert!(stdout.contains("fixture resource body"), "{stdout}");

    exercise_interactive_final_task(&http).await;
    http.shutdown().await;
}

/// The generators run before anything connects, so they need no server, no
/// config file, and no terminal. That is the property a packaging script
/// depends on, and it only holds at the process boundary.
async fn exercise_generators() {
    // Each shell spells a long option its own way: bash and zsh emit
    // `--protocol`, fish emits `-l protocol`.
    for (shell, marker, protocol_flag, demo_flag) in [
        ("bash", "complete -F _mcp-repl", "--protocol", "--demo"),
        ("zsh", "#compdef mcp-repl", "--protocol", "--demo"),
        ("fish", "complete -c mcp-repl", "-l protocol", "-l demo"),
    ] {
        let mut command = repl_command();
        command.args(["--completions", shell]);
        let output = run(command, &format!("completions {shell}"), CASE_TIMEOUT).await;
        assert_success(&output, &format!("completions {shell}"));
        let script = String::from_utf8_lossy(&output.stdout);
        assert!(
            script.contains(marker),
            "{shell} completion does not look like a {shell} script:\n{script}"
        );
        // The flags a user actually reaches for, from the live command
        // definition rather than a snapshot that could drift.
        assert!(
            script.contains(protocol_flag),
            "{shell} completion lost {protocol_flag}"
        );
        assert!(
            script.contains(demo_flag),
            "{shell} completion lost {demo_flag}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).is_empty(),
            "a generator must leave stdout clean and say nothing on stderr"
        );
    }

    let mut command = repl_command();
    command.arg("--man");
    let output = run(command, "man page", CASE_TIMEOUT).await;
    assert_success(&output, "man page");
    let roff = String::from_utf8_lossy(&output.stdout);
    for section in [".SH NAME", ".SH SYNOPSIS", ".SH DESCRIPTION", ".SH OPTIONS"] {
        assert!(roff.contains(section), "man page has no {section}");
    }
    assert!(roff.contains("mcp-repl"));
}

#[tokio::test(flavor = "multi_thread")]
async fn published_cli_covers_transports_and_protocol_lifecycles() {
    tokio::time::timeout(SUITE_TIMEOUT, async {
        let temp = TempDir::new().expect("temporary fixture directory");
        let fixture = build_fixture().await;
        exercise_generators().await;
        exercise_connection_failure_output().await;
        exercise_elicitation_field_order().await;
        exercise_respond_needs_the_final_lifecycle().await;
        #[cfg(unix)]
        exercise_cancellation().await;
        exercise_json_contract(&fixture, &temp).await;
        exercise_exec_waits_for_its_own_tasks(&fixture, &temp).await;
        exercise_schema_contracts(&fixture, &temp).await;
        exercise_imported_stdio_config(&fixture, &temp).await;
        exercise_stdio(&fixture, &temp).await;
        exercise_http(&fixture, &temp).await;
    })
    .await
    .expect("mcp-repl E2E suite exceeded its job-level timeout");
}
