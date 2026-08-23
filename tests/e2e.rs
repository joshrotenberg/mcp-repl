//! Black-box coverage for the published `mcp-repl` process boundary.

use std::io::{Read, Seek};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[cfg(unix)]
use std::io::Write;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

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

async fn run_with_input(
    mut command: Command,
    input: &str,
    label: &str,
    timeout: Duration,
) -> Output {
    let mut stdout = tempfile::tempfile().expect("create stdout capture");
    let mut stderr = tempfile::tempfile().expect("create stderr capture");
    command
        .stdin(std::process::Stdio::piped())
        .stdout(stdout.try_clone().expect("clone stdout capture"))
        .stderr(stderr.try_clone().expect("clone stderr capture"))
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .unwrap_or_else(|error| panic!("spawn {label}: {error}"));
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())
        .await
        .unwrap_or_else(|error| panic!("write {label} input: {error}"));
    let status = tokio::time::timeout(timeout, child.wait())
        .await
        .unwrap_or_else(|_| panic!("{label} exceeded {timeout:?}"))
        .unwrap_or_else(|error| panic!("wait for {label}: {error}"));
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
    // The suite pins the published process output. A developer's ambient
    // tracing filter must not add framework records to those assertions;
    // individual cases opt back in when logging is what they exercise.
    command.env_remove("RUST_LOG");
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

async fn wait_for_file_count(path: &Path, minimum: usize, label: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match std::fs::read_to_string(path) {
                Ok(contents)
                    if contents
                        .parse::<usize>()
                        .is_ok_and(|count| count >= minimum) =>
                {
                    break;
                }
                Ok(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("read {label}: {error}"),
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label} to reach {minimum}"));
}

async fn wait_for_output(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    captured: &mut Vec<u8>,
    expected: &str,
    label: &str,
) {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut chunk = [0u8; 4096];
        loop {
            if String::from_utf8_lossy(captured).contains(expected) {
                break;
            }
            let read = reader
                .read(&mut chunk)
                .await
                .unwrap_or_else(|error| panic!("read {label}: {error}"));
            assert_ne!(
                read,
                0,
                "{label} reached EOF before {expected:?}:\n{}",
                String::from_utf8_lossy(captured)
            );
            captured.extend_from_slice(&chunk[..read]);
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out waiting for {expected:?} in {label}:\n{}",
            String::from_utf8_lossy(captured)
        )
    });
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
    resource_subscriptions_file: PathBuf,
    tool_calls_file: PathBuf,
}

#[derive(Clone, Copy)]
enum HttpFailure {
    Once,
    Always,
}

impl HttpFixture {
    async fn start(fixture: &Path, temp: &TempDir) -> Self {
        Self::start_configured(fixture, temp, None, None, None).await
    }

    async fn start_for_switching(fixture: &Path, temp: &TempDir) -> Self {
        Self::start_configured(fixture, temp, None, None, Some("http-switching")).await
    }

    #[cfg(unix)]
    async fn start_with_bearer(fixture: &Path, temp: &TempDir, bearer: &str) -> Self {
        Self::start_configured(fixture, temp, Some(bearer), None, None).await
    }

    async fn start_with_reconnect(fixture: &Path, temp: &TempDir) -> Self {
        Self::start_configured(fixture, temp, None, Some(HttpFailure::Once), None).await
    }

    async fn start_always_unavailable(fixture: &Path, temp: &TempDir) -> Self {
        Self::start_configured(fixture, temp, None, Some(HttpFailure::Always), None).await
    }

    async fn start_configured(
        fixture: &Path,
        temp: &TempDir,
        bearer: Option<&str>,
        failure: Option<HttpFailure>,
        label: Option<&str>,
    ) -> Self {
        let label = label.unwrap_or(match (bearer.is_some(), failure) {
            (true, _) => "http-auth",
            (_, Some(HttpFailure::Once)) => "http-reconnect",
            (_, Some(HttpFailure::Always)) => "http-unavailable",
            _ => "http",
        });
        let ready_file = temp.path().join(format!("{label}.ready"));
        let subscription_file = temp.path().join(format!("{label}.subscription"));
        let resource_subscriptions_file =
            temp.path().join(format!("{label}.resource-subscriptions"));
        let tool_calls_file = temp.path().join(format!("{label}.tool-calls"));
        let mut command = Command::new(fixture);
        command
            .arg("--http")
            .env("MCP_REPL_FIXTURE_READY_FILE", &ready_file)
            .env("MCP_REPL_FIXTURE_SUBSCRIPTION_FILE", &subscription_file)
            .env(
                "MCP_REPL_FIXTURE_RESOURCE_SUBSCRIPTIONS_FILE",
                &resource_subscriptions_file,
            )
            .env("MCP_REPL_FIXTURE_TOOL_CALLS_FILE", &tool_calls_file)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(bearer) = bearer {
            command.env("MCP_REPL_FIXTURE_EXPECT_BEARER", bearer);
        }
        match failure {
            Some(HttpFailure::Once) => {
                command.env(
                    "MCP_REPL_FIXTURE_HTTP_503_ONCE_FILE",
                    temp.path().join("http-reconnect.triggered"),
                );
            }
            Some(HttpFailure::Always) => {
                command.env("MCP_REPL_FIXTURE_HTTP_503_ALWAYS", "1");
            }
            None => {}
        }
        let child = command.spawn().expect("spawn HTTP fixture");
        let url = wait_for_file(&ready_file, "HTTP fixture readiness").await;
        Self {
            child: Some(child),
            url,
            subscription_file,
            resource_subscriptions_file,
            tool_calls_file,
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

#[cfg(unix)]
fn inherited_bearer_pipe(contents: &[u8]) -> OwnedFd {
    let mut descriptors = [0; 2];
    // SAFETY: `pipe` initializes both descriptors on success. Each is
    // immediately transferred into one owning standard-library type.
    assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
    let reader = unsafe { OwnedFd::from_raw_fd(descriptors[0]) };
    let mut writer = unsafe { std::fs::File::from_raw_fd(descriptors[1]) };
    writer.write_all(contents).expect("write bearer pipe");
    drop(writer);
    reader
}

#[cfg(unix)]
fn inherited_bearer_file(contents: &[u8]) -> std::fs::File {
    let mut file = tempfile::tempfile().expect("create bearer descriptor file");
    file.write_all(contents)
        .expect("write bearer descriptor file");
    file.rewind().expect("rewind bearer descriptor file");
    let descriptor = file.as_raw_fd();
    let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
    assert!(flags >= 0, "read bearer descriptor flags");
    assert_eq!(
        unsafe { libc::fcntl(descriptor, libc::F_SETFD, flags & !libc::FD_CLOEXEC) },
        0,
        "make bearer descriptor inheritable"
    );
    file
}

#[cfg(unix)]
async fn assert_bearer_fd_usage_error(descriptor: i32, expected: &str, label: &str) {
    let descriptor = descriptor.to_string();
    let mut command = repl_command();
    command
        .args([
            "--json",
            "--bearer-fd",
            &descriptor,
            "--exec",
            "tools",
            "--http",
            "http://127.0.0.1:9/",
        ])
        .env_remove("MCP_BEARER");
    let output = run(command, label, CASE_TIMEOUT).await;
    assert_status(&output, 2, label);
    let values = json_lines(&output, label);
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["kind"], "usage");
    let error = values[0]["error"].as_str().unwrap();
    assert!(error.contains(expected), "{label}: {error}");
    assert!(
        !error.contains("HTTP request failed"),
        "{label} connected: {error}"
    );
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.start_kill();
        }
    }
}

/// Drop the first HTTP request before it reaches the fixture, then proxy every
/// later connection normally. This reproduces a fresh client's first
/// handshake request dying in the transport send path without sleeps or a
/// server-side response.
struct DropFirstRequestProxy {
    url: String,
    accepted: Arc<AtomicUsize>,
    dropped: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

/// An immediate reset is the behavior this fault injector needs. Tokio
/// deprecates linger because a nonzero timeout can block on drop; zero does
/// not wait, and this socket carries test traffic only.
#[allow(deprecated)]
fn reset_on_drop(stream: &tokio::net::TcpStream) {
    stream
        .set_linger(Some(Duration::ZERO))
        .expect("reset first proxy connection");
}

impl DropFirstRequestProxy {
    async fn start(upstream_url: &str) -> Self {
        let upstream = upstream_url
            .strip_prefix("http://")
            .and_then(|address| address.strip_suffix('/'))
            .expect("fixture URL shape")
            .to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind drop-first-request proxy");
        let url = format!(
            "http://{}/",
            listener
                .local_addr()
                .expect("drop-first-request proxy address")
        );
        let accepted = Arc::new(AtomicUsize::new(0));
        let dropped = Arc::new(AtomicUsize::new(0));
        let accepted_task = accepted.clone();
        let dropped_task = dropped.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut downstream, _)) = listener.accept().await else {
                    break;
                };
                let connection = accepted_task.fetch_add(1, Ordering::SeqCst);
                if connection == 0 {
                    // Wait until the HTTP request has reached the proxy, then
                    // reset the socket without opening an upstream connection.
                    let mut byte = [0u8; 1];
                    if downstream.peek(&mut byte).await.is_ok_and(|read| read > 0) {
                        reset_on_drop(&downstream);
                        dropped_task.fetch_add(1, Ordering::SeqCst);
                    }
                    continue;
                }
                let upstream = upstream.clone();
                tokio::spawn(async move {
                    let Ok(mut upstream) = tokio::net::TcpStream::connect(upstream).await else {
                        return;
                    };
                    let _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream).await;
                });
            }
        });
        Self {
            url,
            accepted,
            dropped,
            task,
        }
    }

    async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
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

    // The same selector under `RUST_LOG`. Which file and entry a selector
    // resolved to, and which approval let the spawn happen, are decisions
    // that never become a frame, so `wire on` cannot answer them.
    let mut logged = repl_command();
    logged
        .args([
            "--json",
            "--trust-import",
            "--exec",
            "process_info",
            &selector,
        ])
        .env("MCP_REPL_HOST_VALUE", "from-host")
        .env("RUST_LOG", "mcp_repl=debug")
        .env("MCP_REPL_FIXTURE_EXIT_FILE", &exit_file);
    let logged = run(logged, "imported stdio config with logging", CASE_TIMEOUT).await;
    assert_success(&logged, "imported stdio config with logging");
    let records = String::from_utf8_lossy(&logged.stderr);
    assert!(
        records.contains("resolved a client config entry") && records.contains("entry=fixture"),
        "the records name the entry a selector resolved to:\n{records}"
    );
    assert!(
        records.contains("import approved by --trust-import"),
        "and which approval let the spawn happen:\n{records}"
    );

    // stdout is the data stream either way: records go to stderr, so the
    // NDJSON contract survives having logging on.
    let values = json_lines(&logged, "imported stdio config with logging");
    assert_eq!(values.len(), 1, "one value per command, with logging on");

    // And none of it appears without being asked for.
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("resolved a client config entry"),
        "the default level stays quiet:\n{}",
        String::from_utf8_lossy(&output.stderr)
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
                    "url": "http://127.0.0.1:1/",
                    "headers": {"Authorization": "Bearer ${env:IMPORTED_HTTP_TOKEN}"}
                }
            }
        })
        .to_string(),
    )
    .expect("write imported HTTP config");
    let selector = format!("{}:fixture", config.display());
    let mut command = repl_command();
    command
        .args([
            "--json",
            "--exec",
            "add a=20 b=22",
            "--http",
            &http.url,
            &selector,
        ])
        .env("IMPORTED_HTTP_TOKEN", "do-not-print-this-token");
    let refused = run(command, "unapproved imported HTTP config", CASE_TIMEOUT).await;
    assert_status(&refused, 2, "unapproved imported HTTP config");
    let refusal = json_lines(&refused, "unapproved imported HTTP config");
    assert_eq!(refusal.len(), 1);
    assert_eq!(refusal[0]["kind"], "usage");
    assert!(
        refusal[0]["error"]
            .as_str()
            .is_some_and(|message| message.contains("--trust-import"))
    );
    assert!(!String::from_utf8_lossy(&refused.stderr).contains("do-not-print-this-token"));

    let mut command = repl_command();
    command
        .args([
            "--json",
            "--exec",
            "add a=20 b=22",
            "--trust-import",
            "--http",
            &http.url,
            &selector,
        ])
        .env("IMPORTED_HTTP_TOKEN", "do-not-print-this-token");
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
    let malformed_filter = run_stdio(
        fixture,
        temp,
        "malformed result filter",
        &["--json", "--exec", "add a=20 b=22 | content["],
    )
    .await;
    assert_status(&malformed_filter, 2, "malformed result filter");
    let values = json_lines(&malformed_filter, "malformed result filter");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["kind"], "usage");
    assert!(
        values[0]["error"]
            .as_str()
            .is_some_and(|message| message.contains("invalid path"))
    );

    let malformed = run_stdio(
        fixture,
        temp,
        "malformed direct tool arguments",
        &["--json", "--exec", "add forgot-the-equals"],
    )
    .await;
    assert_status(&malformed, 2, "malformed direct tool arguments");
    let values = json_lines(&malformed, "malformed direct tool arguments");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["kind"], "usage");
    assert!(
        values[0]["error"]
            .as_str()
            .is_some_and(|message| message.contains("key=value"))
    );

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

async fn exercise_colliding_tool_names(fixture: &Path, temp: &TempDir) {
    let ambiguous = run_stdio(
        fixture,
        temp,
        "ambiguous-tool-name",
        &["--json", "--exec", "wait a=20 b=22"],
    )
    .await;
    assert_status(&ambiguous, 2, "ambiguous tool name");
    let values = json_lines(&ambiguous, "ambiguous tool name");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["kind"], "usage");
    let message = values[0]["error"].as_str().unwrap_or_default();
    assert!(message.contains("tool wait"), "{message}");
    assert!(message.contains("builtin wait"), "{message}");
    assert!(!message.contains("no tasks"), "the built-in ran: {message}");

    let explicit_tools = run_stdio(
        fixture,
        temp,
        "explicit-tool-names",
        &[
            "--json",
            "--exec",
            "tool wait a=20 b=22",
            "--exec",
            "tool tool",
            "--exec",
            "tool builtin",
        ],
    )
    .await;
    assert_success(&explicit_tools, "explicit tool names");
    let values = json_lines(&explicit_tools, "explicit tool names");
    assert_eq!(
        values[0].pointer("/content/0/text"),
        Some(&serde_json::json!("server wait: 42"))
    );
    assert_eq!(
        values[1].pointer("/content/0/text"),
        Some(&serde_json::json!("server tool: tool"))
    );
    assert_eq!(
        values[2].pointer("/content/0/text"),
        Some(&serde_json::json!("server tool: builtin"))
    );

    let wrong_namespaces = run_stdio(
        fixture,
        temp,
        "wrong-command-namespaces",
        &[
            "--json",
            "--exec",
            "tool jobs",
            "--exec",
            "builtin add a=20 b=22",
        ],
    )
    .await;
    assert_status(&wrong_namespaces, 1, "wrong command namespaces");
    let values = json_lines(&wrong_namespaces, "wrong command namespaces");
    assert!(values[0]["error"].as_str().unwrap().contains("server tool"));
    assert!(values[1]["error"].as_str().unwrap().contains("built-in"));

    let captured = run_stdio(
        fixture,
        temp,
        "capture-explicit-tool",
        &[
            "--json",
            "--exec",
            "answer = tool wait a=19 b=23",
            "--exec",
            "vars",
        ],
    )
    .await;
    assert_success(&captured, "capture explicit tool");
    let values = json_lines(&captured, "capture explicit tool");
    assert_eq!(values[0], "server wait: 42");
    assert_eq!(values[1]["answer"], "server wait: 42");

    let alias_config = temp.path().join("collision-alias.toml");
    std::fs::write(&alias_config, "[aliases]\nw = \"tool wait\"\n")
        .expect("write collision alias config");
    let alias_config = alias_config.to_string_lossy();
    let aliased = run_stdio(
        fixture,
        temp,
        "alias-explicit-tool",
        &["--json", "--config", &alias_config, "--exec", "w a=40 b=2"],
    )
    .await;
    assert_success(&aliased, "alias explicit tool");
    assert_eq!(
        json_lines(&aliased, "alias explicit tool")[0].pointer("/content/0/text"),
        Some(&serde_json::json!("server wait: 42"))
    );

    let defined_alias = run_stdio(
        fixture,
        temp,
        "qualified-alias-builtin",
        &[
            "--json",
            "--config",
            &alias_config,
            "--exec",
            "builtin alias x=tool wait",
            "--exec",
            "x a=21 b=21",
        ],
    )
    .await;
    assert_success(&defined_alias, "qualified alias built-in");
    let values = json_lines(&defined_alias, "qualified alias built-in");
    assert_eq!(
        values.last().unwrap().pointer("/content/0/text"),
        Some(&serde_json::json!("server wait: 42"))
    );

    let backgrounded = run_stdio(
        fixture,
        temp,
        "background-explicit-tool",
        &[
            "--protocol",
            "2026-07-28",
            "--color",
            "never",
            "--exec",
            "tool wait a=20 b=22 &",
            "--exec",
            "builtin wait",
        ],
    )
    .await;
    assert_success(&backgrounded, "background explicit tool");
    assert!(
        String::from_utf8_lossy(&backgrounded.stdout).contains("server wait: 42"),
        "{}",
        String::from_utf8_lossy(&backgrounded.stdout)
    );

    // Connecting to a server whose tools shadow built-ins should say so at
    // connect, in the banner that already prints the surface counts, rather
    // than waiting for the mid-task ambiguous-command error above.
    let banner = run_stdio(
        fixture,
        temp,
        "collision-banner",
        &["--no-history", "--verbose", "--exec", "quit"],
    )
    .await;
    assert_success(&banner, "collision banner");
    let stdout = String::from_utf8_lossy(&banner.stdout);
    let note_lines: Vec<&str> = stdout
        .lines()
        .filter(|line| line.contains("also a built-in") || line.contains("also built-ins"))
        .collect();
    assert_eq!(
        note_lines.len(),
        1,
        "exactly one collision note, however many names collide:\n{stdout}"
    );
    let note = note_lines[0];
    for shadowed in ["tool", "builtin", "wait"] {
        assert!(note.contains(shadowed), "{note}");
    }
    assert!(note.contains("`tool <name>`"), "{note}");

    // A server whose tools do not shadow anything gets no note at all: the
    // raw-tools-only fixture mode serves exactly one tool, `add`.
    let exit_file = temp.path().join("no-collision.exit");
    let mut command = repl_command();
    command
        .args([
            "--no-history",
            "--color",
            "never",
            "--verbose",
            "--exec",
            "quit",
        ])
        .arg(fixture)
        .args(["--tools-only"])
        .env("MCP_REPL_FIXTURE_EXIT_FILE", &exit_file);
    let no_collision = run(command, "no collision banner", CASE_TIMEOUT).await;
    assert_success(&no_collision, "no collision banner");
    let stdout = String::from_utf8_lossy(&no_collision.stdout);
    assert!(
        !stdout.contains("also a built-in") && !stdout.contains("also built-ins"),
        "a server with no shadowed names gets no note:\n{stdout}"
    );
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
            "builtin wait",
        ],
    )
    .await;
    assert_success(&waited, "exec wait all");
    let stdout = String::from_utf8_lossy(&waited.stdout);
    assert!(
        stdout.contains("status=completed"),
        "the explicit wait built-in settles the task started earlier in the same run:\n{stdout}"
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
            "builtin wait last",
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
            "builtin wait",
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
        &["--no-history", "--color", "never", "--exec", "builtin wait"],
    )
    .await;
    assert_status(&empty, 1, "exec wait with no tasks");
}

/// A bare human REPL starts disconnected, while machine-oriented modes still
/// require an initial target.
async fn exercise_no_target(temp: &TempDir) {
    let mut command = repl_command();
    command.current_dir(temp.path());
    let output = run(command, "no target", CASE_TIMEOUT).await;
    assert_success(&output, "disconnected prompt");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("not connected") && stdout.contains("connect demo"),
        "a bare invocation explains the disconnected prompt:\n{stdout}"
    );
    // For many people this is the first thing mcp-repl says, so it has to
    // name more than one command: where the rest of them are, where the
    // startup flags are, and how to leave.
    for pointer in ["help", "-h", "quit", "--demo"] {
        assert!(
            stdout.contains(pointer),
            "the disconnected prompt must point at `{pointer}`:\n{stdout}"
        );
    }
    assert!(
        !stdout.contains('\u{2014}'),
        "user-facing output uses no em dashes:\n{stdout}"
    );

    // Under --json it is the standard envelope, on stdout, like every other
    // usage failure.
    let mut as_json = repl_command();
    as_json.arg("--json").current_dir(temp.path());
    let as_json = run(as_json, "no target json", CASE_TIMEOUT).await;
    assert_status(&as_json, 2, "no target json");
    let values = json_lines(&as_json, "no target json");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["kind"], "usage");

    let mut exec = repl_command();
    exec.args(["--exec", "help"]);
    let exec = run(exec, "no target exec", CASE_TIMEOUT).await;
    assert_status(&exec, 2, "no target exec");

    let mut final_repl = repl_command();
    final_repl.args([
        "--protocol",
        "2026-07-28",
        "--no-history",
        "--color",
        "never",
    ]);
    let final_repl = run_with_input(
        final_repl,
        "connect demo\necho message=final-connect\nquit\n",
        "disconnected final connect",
        CASE_TIMEOUT,
    )
    .await;
    assert_success(&final_repl, "disconnected final connect");
    let stdout = String::from_utf8_lossy(&final_repl.stdout);
    assert!(stdout.contains("protocol 2026-07-28"), "{stdout}");
    assert!(stdout.contains("final-connect"), "{stdout}");
}

async fn exercise_multiline_piped_input() {
    let mut command = repl_command();
    command.args(["--demo", "--no-history", "--color", "never"]);
    let output = run_with_input(
        command,
        "call echo {\n  \"message\": \"hello from a pipe\"\n}\nquit\n",
        "multiline piped input",
        CASE_TIMEOUT,
    )
    .await;
    assert_success(&output, "multiline piped input");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello from a pipe"), "{stdout}");
    assert!(!stdout.contains("unknown command"), "{stdout}");
}

async fn exercise_in_repl_connect(fixture: &Path, http_url: &str, temp: &TempDir) {
    let fixture_string = fixture.display().to_string();
    let fixture_literal = serde_json::to_string(&fixture_string).expect("quote fixture path");
    let config = temp.path().join("connect.toml");
    std::fs::write(
        &config,
        format!(
            "[servers.profiled]\ntransport = \"stdio\"\ncommand = [{fixture_literal}]\n\
             [servers.profiled.aliases]\nprofile_add = \"add\"\n"
        ),
    )
    .expect("write connect profile");
    let imported = temp.path().join("connect-import.json");
    std::fs::write(
        &imported,
        serde_json::to_vec_pretty(&serde_json::json!({
            "mcpServers": {
                "imported": { "command": fixture_string }
            }
        }))
        .expect("serialize imported config"),
    )
    .expect("write imported config");
    let selector = format!("{}:imported", imported.display());
    let selector_literal = serde_json::to_string(&selector).expect("quote import selector");
    let exit_file = temp.path().join("in-repl-connect.exit");
    let missing = temp.path().join("missing-mcp-server");
    let missing_literal =
        serde_json::to_string(&missing.display().to_string()).expect("quote missing server path");
    let input = format!(
        "help connect\n\
         alias h=help connect\n\
         connect demo\n\
         saved = echo message=hello\n\
         connect -- {missing_literal}\n\
         vars\n\
         echo message=still-connected\n\
         connect -- {fixture_literal}\n\
         add a=20 b=22\n\
         connect profiled\n\
         profile_add a=19 b=23\n\
         connect {selector_literal}\n\
         add a=18 b=24\n\
         connect {http_url}\n\
         add a=17 b=25\n\
         connect demo\n\
         vars\n\
         h\n\
         echo message=switched\n\
         quit\n",
    );
    let mut command = repl_command();
    command
        .args([
            "--no-history",
            "--color",
            "never",
            "--trust-import",
            "--config",
        ])
        .arg(&config)
        .env("MCP_REPL_FIXTURE_EXIT_FILE", &exit_file);
    let output = run_with_input(command, &input, "in-REPL connect", CASE_TIMEOUT).await;
    assert_success(&output, "in-REPL connect");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stdout.contains("not connected"), "{stdout}");
    assert!(
        stdout.contains("hello")
            && stdout.contains("still-connected")
            && stdout.contains("switched"),
        "{stdout}"
    );
    assert!(
        stdout.matches("42").count() >= 4,
        "stdio, profile, import, and HTTP targets all answer:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("profile profiled"), "{stdout}");
    assert!(stdout.contains("import "), "{stdout}");
    assert!(
        stdout.contains("server-scoped state cleared: 1 captured variable"),
        "{stdout}"
    );
    assert!(
        stdout.matches("connect <url|profile").count() >= 2,
        "global aliases and help survive switches:\n{stdout}"
    );
    assert!(
        stdout.contains("A candidate is initialized")
            && stdout.contains("connect -- ./my-server --stdio"),
        "rich help carries explanation and examples:\n{stdout}"
    );
    assert!(stdout.contains("no variables"), "{stdout}");
    assert!(
        stderr.contains("missing-mcp-server"),
        "the failed candidate is reported:\n{stderr}"
    );
    assert_eq!(
        wait_for_file(&exit_file, "switched stdio fixture shutdown").await,
        "clean",
        "switching servers did not orderly close the old stdio child"
    );
}

/// `--login`/`--logout` under `--json` speak the same NDJSON contract.
///
/// The success path of `--login` needs a real authorization server, so what
/// is reachable here is everything around it: that `--json` is accepted at
/// all, that a failure is the standard envelope rather than prose, and that
/// `--logout` reports what it removed. The shape of a saved profile is
/// covered by a unit test.
async fn exercise_login_json(temp: &TempDir) {
    let config = temp.path().join("login.toml");
    std::fs::write(&config, "").expect("write empty config");
    let config = config.display().to_string();

    // A usage failure is the same envelope every other command emits, on
    // stdout, so it occupies that invocation's one output line.
    let mut missing_url = repl_command();
    missing_url.args(["--login", "work", "--json", "--config", &config]);
    let missing_url = run(missing_url, "login without a url", CASE_TIMEOUT).await;
    assert_status(&missing_url, 2, "login without a url");
    let values = json_lines(&missing_url, "login without a url");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["kind"], "usage");
    assert_eq!(values[0]["exitStatus"], 2);

    // `--json` is now allowed, but the genuinely incompatible combinations
    // are still refused.
    let mut with_exec = repl_command();
    with_exec.args([
        "--login", "work", "--json", "--exec", "tools", "--config", &config,
    ]);
    let with_exec = run(with_exec, "login with exec", CASE_TIMEOUT).await;
    assert_status(&with_exec, 2, "login with exec");
    assert!(
        String::from_utf8_lossy(&with_exec.stdout).contains("standalone credential"),
        "{}",
        String::from_utf8_lossy(&with_exec.stdout)
    );

    // Removing a profile touches the operating-system credential store, which
    // a headless runner does not have. Both outcomes are correct, and the
    // contract is what this pins: exactly one parseable value either way,
    // the success shape or the standard auth envelope. The failure branch is
    // worth covering in its own right, since "a script gets an envelope
    // rather than prose when it fails" is half the point of the flag.
    let mut logout = repl_command();
    logout.args(["--logout", "work", "--json", "--config", &config]);
    let logout = run(logout, "logout json", CASE_TIMEOUT).await;
    let values = json_lines(&logout, "logout json");
    assert_eq!(values.len(), 1, "one value per invocation");
    match logout.status.code() {
        Some(0) => {
            assert_eq!(values[0]["profile"], "work");
            assert_eq!(values[0]["removed"], true);
        }
        Some(5) => {
            assert_eq!(values[0]["kind"], "auth");
            assert_eq!(values[0]["exitStatus"], 5);
        }
        other => panic!("unexpected logout status {other:?}: {}", values[0]),
    }

    // Without `--json`, the human wording is unchanged. Same split: the
    // message goes to stdout on success and stderr on failure, because
    // stdout is the data stream.
    let mut human = repl_command();
    human.args(["--logout", "work", "--config", &config]);
    let human = run(human, "logout human", CASE_TIMEOUT).await;
    let stdout = String::from_utf8_lossy(&human.stdout);
    let stderr = String::from_utf8_lossy(&human.stderr);
    if human.status.success() {
        assert!(stdout.contains("removed OAuth profile"), "{stdout}");
    } else {
        assert!(
            stderr.contains("credential store"),
            "a failure explains itself on stderr:\n{stderr}"
        );
        assert!(
            stdout.is_empty(),
            "and stdout stays the data stream: {stdout}"
        );
    }
}

/// `[repl] request_timeout` supplies the default `--timeout` uses.
///
/// The precedence is what matters and what a unit test cannot see: the flag
/// beats the config, the config beats the built-in default, and an explicit
/// `--timeout 0` still means "wait indefinitely" rather than "unset".
async fn exercise_repl_config(temp: &TempDir) {
    let config = temp.path().join("repl-config.toml");
    std::fs::write(&config, "[repl]\nrequest_timeout = 1\n").expect("write repl config");
    let config = config.display().to_string();

    // `slow_add` sleeps three seconds, so a one-second budget must expire.
    let mut timed_out = repl_command();
    timed_out.args([
        "--demo",
        "--no-history",
        "--color",
        "never",
        "--config",
        &config,
        "--exec",
        "slow_add a=1 b=2",
    ]);
    let timed_out = run(timed_out, "config timeout", CASE_TIMEOUT).await;
    assert_status(&timed_out, 4, "config timeout");

    // The flag overrides it.
    let mut flag_wins = repl_command();
    flag_wins.args([
        "--demo",
        "--no-history",
        "--color",
        "never",
        "--config",
        &config,
        "--timeout",
        "30",
        "--exec",
        "slow_add a=1 b=2",
    ]);
    let flag_wins = run(flag_wins, "flag over config", CASE_TIMEOUT).await;
    assert_success(&flag_wins, "flag over config");
    assert!(
        String::from_utf8_lossy(&flag_wins.stdout).contains('3'),
        "the call completes when the flag allows it"
    );

    // A typo is refused rather than ignored, since a setting that appears to
    // apply and does not is worse than one that fails loudly.
    let typo = temp.path().join("repl-typo.toml");
    std::fs::write(&typo, "[repl]\nrequest_timeoutt = 1\n").expect("write typo config");
    let mut refused = repl_command();
    refused.args([
        "--demo",
        "--no-history",
        "--color",
        "never",
        "--config",
        &typo.display().to_string(),
        "--exec",
        "echo message=hi",
    ]);
    let refused = run(refused, "config typo", CASE_TIMEOUT).await;
    assert_status(&refused, 2, "config typo");
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("request_timeout"),
        "the refusal names the key that was meant:\n{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

/// `loglevel` must actually change what the server sends.
///
/// The fixture's `announce` emits one Info log. Asserting only that the
/// request went out would pass against a server that ignored it, so this
/// checks the observable consequence: the same call, quiet afterwards.
async fn exercise_loglevel(fixture: &Path, temp: &TempDir) {
    let before = run_stdio(
        fixture,
        temp,
        "loglevel-default",
        &["--no-history", "--color", "never", "--exec", "announce"],
    )
    .await;
    assert_success(&before, "loglevel default");
    // Notifications go to stderr: stdout is the data stream, and a log line
    // arriving mid-command is not part of any command's result.
    assert!(
        String::from_utf8_lossy(&before.stderr).contains("log info"),
        "the fixture logs at info by default:\n{}",
        String::from_utf8_lossy(&before.stderr)
    );

    let after = run_stdio(
        fixture,
        temp,
        "loglevel-raised",
        &[
            "--no-history",
            "--color",
            "never",
            "--exec",
            "loglevel emergency",
            "--exec",
            "announce",
        ],
    )
    .await;
    assert_success(&after, "loglevel raised");
    let stdout = String::from_utf8_lossy(&after.stdout);
    let stderr = String::from_utf8_lossy(&after.stderr);
    assert!(
        stdout.contains("announced"),
        "the tool still runs:\n{stdout}"
    );
    assert!(
        !stderr.contains("log info"),
        "and its log is below the level that was set:\n{stderr}"
    );

    // Final MCP removed the session-wide RPC. The same built-in now updates
    // the metadata on later typed requests: one call opts into Info and the
    // next threshold suppresses it, without replacing `call_tool` with a raw
    // request that would lose task/MRTR/schema-retry behavior.
    let final_output = run_stdio(
        fixture,
        temp,
        "loglevel-final",
        &[
            "--protocol",
            "2026-07-28",
            "--no-history",
            "--color",
            "never",
            "--exec",
            "loglevel debug",
            "--exec",
            "announce",
            "--exec",
            "loglevel emergency",
            "--exec",
            "announce",
        ],
    )
    .await;
    assert_success(&final_output, "final loglevel");
    let stdout = String::from_utf8_lossy(&final_output.stdout);
    let stderr = String::from_utf8_lossy(&final_output.stderr);
    assert_eq!(
        stdout.matches("log level set to").count(),
        2,
        "both final thresholds are accepted:\n{stdout}"
    );
    assert_eq!(
        stdout.matches("announced").count(),
        2,
        "both typed tool calls still complete:\n{stdout}"
    );
    assert_eq!(
        stderr.matches("log info").count(),
        1,
        "only the request prepared with the debug threshold receives Info:\n{stderr}"
    );

    // A server that never declared the capability is told so, rather than
    // being sent a request it will only reject.
    let mut undeclared = repl_command();
    undeclared
        .args([
            "--no-history",
            "--color",
            "never",
            "--exec",
            "loglevel debug",
        ])
        .arg(fixture)
        .arg("--tools-only")
        .env(
            "MCP_REPL_FIXTURE_EXIT_FILE",
            temp.path().join("loglevel-undeclared.exit"),
        );
    let undeclared = run(undeclared, "loglevel undeclared", CASE_TIMEOUT).await;
    assert_status(&undeclared, 3, "loglevel undeclared");
    assert!(
        String::from_utf8_lossy(&undeclared.stderr).contains("does not declare"),
        "{}",
        String::from_utf8_lossy(&undeclared.stderr)
    );
}

/// Sampling is a feature the README leads with, so something must exercise it.
///
/// `canned` rather than `prompt`, so the case is deterministic and needs no
/// stdin: what is being checked is that the request reaches the client and
/// the answer reaches the tool, not how a human types.
async fn exercise_sampling() {
    let answered = run_demo_answering(
        "",
        "sampling canned",
        &[
            "--sampling",
            "canned",
            "--exec",
            "summarize text=\"the quick brown fox\"",
        ],
    )
    .await;
    assert_success(&answered, "sampling canned");
    let stdout = String::from_utf8_lossy(&answered.stdout);
    assert!(
        stdout.contains("summary (mcp-repl/canned)"),
        "the tool receives what the client answered:\n{stdout}"
    );

    // Declining is an answer too, and the tool has to cope with it rather
    // than the REPL pretending it succeeded.
    let declined = run_demo_answering(
        "",
        "sampling declined",
        &[
            "--sampling",
            "decline",
            "--exec",
            "summarize text=\"the quick brown fox\"",
        ],
    )
    .await;
    assert_ne!(
        declined.status.code(),
        Some(0),
        "a declined request is not a success:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&declined.stdout),
        String::from_utf8_lossy(&declined.stderr)
    );
}

/// A server that serves only tools must not be greeted with warnings.
///
/// Real servers declare exactly this: GitMCP's `initialize` result is
/// `{"tools":{"listChanged":true}}`. The REPL used to ask for prompts and
/// resources anyway, and report the correct "Method not found" it got back as
/// a failure, so connecting to a healthy server opened with two warnings
/// about nothing.
async fn exercise_tools_only_server(fixture: &Path, temp: &TempDir) {
    let exit_file = temp.path().join("tools-only.exit");
    let mut command = repl_command();
    command
        .args(["--no-history", "--color", "never", "--exec", "tools"])
        .arg(fixture)
        .arg("--tools-only")
        .env("MCP_REPL_FIXTURE_EXIT_FILE", &exit_file);
    let output = run(command, "tools only", CASE_TIMEOUT).await;
    assert_success(&output, "tools only");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("add"), "the tools still list:\n{stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("warning:"),
        "a server that declares only tools is not broken:\n{stderr}"
    );
}

/// The protocol version reported is the one the server returned.
///
/// A server may answer `initialize` with a version other than the one asked
/// for, and real ones do. Echoing the request back would be the easy mistake
/// and would tell the operator something false: the banner is the only place
/// the negotiated version appears, so it has to be the negotiated one.
async fn exercise_downgraded_protocol(fixture: &Path, temp: &TempDir) {
    let exit_file = temp.path().join("downgrade.exit");
    let mut command = repl_command();
    command
        .args([
            "--no-history",
            "--color",
            "never",
            "--verbose",
            "--exec",
            "quit",
        ])
        .arg(fixture)
        .args(["--tools-only", "--downgrade-protocol"])
        .env("MCP_REPL_FIXTURE_EXIT_FILE", &exit_file);
    let output = run(command, "downgraded protocol", CASE_TIMEOUT).await;
    assert_success(&output, "downgraded protocol");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("protocol 2025-06-18"),
        "the version the server chose is the one reported:\n{stdout}"
    );
    assert!(
        !stdout.contains("2025-11-25"),
        "and the version mcp-repl asked for is not:\n{stdout}"
    );
}

/// An absent pagination cursor must be absent, not null.
///
/// `cursor` is optional in the schema, and a server that generates its
/// validators from that schema types it `string | undefined`. Context7 and
/// the Hugging Face server both reject an explicit null, correctly: absent
/// and null are not the same thing. Sending one made every listing they
/// serve fail.
///
/// The fixture rejects a null on any listing rather than a chosen one,
/// because which method carries it is not the point and has already moved:
/// the null rode on `resources/templates/list` and never on `tools/list`.
async fn exercise_absent_cursor(fixture: &Path, temp: &TempDir) {
    let mut command = repl_command();
    command
        .args(["--no-history", "--color", "never", "--exec", "tools"])
        .arg(fixture)
        .args(["--tools-only", "--strict-cursor"])
        .env(
            "MCP_REPL_FIXTURE_EXIT_FILE",
            temp.path().join("strict-cursor.exit"),
        );
    let output = run(command, "strict cursor", CASE_TIMEOUT).await;
    assert_success(&output, "strict cursor");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("add"),
        "the listing survives a server that types cursor as string|undefined:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("cursor"),
        "and no listing is refused over one:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A listing that could not be read must not be reported as an empty one.
///
/// The same raw server, now declaring the tools capability and then failing
/// to serve it. Found against DeepWiki, where the listing failed to
/// deserialize and `--json -e tools | jq length` read 0 for a server with 18
/// tools. The original fixture reproduced that exact cause, a `_meta`
/// key tower-mcp refused; upstream now drops such keys instead, so the cause
/// had to become one that does not depend on someone else's bug. What is
/// being pinned is the REPL's response to an unreadable listing, not the
/// reason it was unreadable.
async fn exercise_unreadable_listing(fixture: &Path, temp: &TempDir) {
    let exit_file = temp.path().join("failing-list.exit");
    let mut command = repl_command();
    command
        .args(["--no-history", "--color", "never", "--exec", "tools"])
        .arg(fixture)
        .args(["--tools-only", "--failing-list"])
        .env("MCP_REPL_FIXTURE_EXIT_FILE", &exit_file);
    let output = run(command, "unreadable listing", CASE_TIMEOUT).await;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_ne!(
        output.status.code(),
        Some(0),
        "a listing that failed to load is not a success:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    // The server's rejection is a sentence and a code, not a struct dump.
    assert!(
        stderr.contains("tool index unavailable (code -32603)"),
        "a server's error reads as a sentence:\n{stderr}"
    );
    assert!(
        !stderr.contains("JsonRpcError {"),
        "and not as Rust debug output:\n{stderr}"
    );
    assert!(
        stdout.trim().is_empty(),
        "stdout is the data stream, and there is no data:\n{stdout}"
    );
    assert!(
        stderr.contains("unavailable"),
        "the operator is told the listing failed, not shown an empty one:\n{stderr}"
    );
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

/// The request deadline includes startup: a child that reads `initialize`
/// but never answers cannot hang a script before its first command (#220).
async fn exercise_initialization_timeout(fixture: &Path, temp: &TempDir) {
    let exit_file = temp.path().join("ignored-initialize.exit");
    let mut command = repl_command();
    command
        .args([
            "--timeout",
            "1",
            "--no-history",
            "--color",
            "never",
            "--exec",
            "tools",
        ])
        .arg(fixture)
        .args(["--tools-only", "--ignore-initialize"])
        .env("MCP_REPL_FIXTURE_EXIT_FILE", &exit_file);
    let output = run(command, "ignored initialization", CASE_TIMEOUT).await;
    assert_status(&output, 4, "ignored initialization");
    assert!(
        output.stdout.is_empty(),
        "a failed handshake has no command output: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no response after 1s (--timeout)"),
        "the configured deadline is reported: {stderr}"
    );
    assert_eq!(
        wait_for_file(&exit_file, "ignored-initialize fixture shutdown").await,
        "clean",
        "timing out the handshake did not orderly close the stdio child"
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
    //
    // Wait for the call frame, not for the tool's name: the name is already
    // on the wire in the `tools/list` response fetched at connect, so waiting
    // on it released the signal while the REPL was still setting up.
    let mut trace = String::new();
    tokio::time::timeout(CASE_TIMEOUT, async {
        let mut chunk = [0u8; 4096];
        loop {
            let read = stderr.read(&mut chunk).await.expect("read wire trace");
            if read == 0 {
                break;
            }
            trace.push_str(&String::from_utf8_lossy(&chunk[..read]));
            if trace.contains("\"method\": \"tools/call\"") {
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
    let mut stdout = child.stdout.take().expect("interactive stdout");
    let mut stderr = child.stderr.take().expect("interactive stderr");
    let stderr_task = tokio::spawn(async move {
        let mut captured = Vec::new();
        stderr
            .read_to_end(&mut captured)
            .await
            .expect("read interactive stderr");
        captured
    });

    // Prove the piped editor is accepting and acknowledging commands before
    // starting the task. Writing immediately after spawn made the old test
    // depend on startup scheduling, and its subscription marker was already
    // satisfied by the final protocol's surface subscription.
    stdin
        .write_all(b"jobs\n")
        .await
        .expect("write readiness command");
    let mut stdout_capture = Vec::new();
    wait_for_output(
        &mut stdout,
        &mut stdout_capture,
        "no background tasks",
        "interactive final readiness",
    )
    .await;

    stdin
        .write_all(b"slow_add a=2 b=3 &\n")
        .await
        .expect("write task command");
    wait_for_output(
        &mut stdout,
        &mut stdout_capture,
        "started",
        "interactive final task start",
    )
    .await;
    // One long-lived subscription follows surface changes and a second,
    // task-scoped subscription follows this task. Counting them prevents the
    // surface stream from impersonating proof that the task watcher opened.
    wait_for_file_count(&http.subscription_file, 2, "final task subscription").await;
    stdin
        .write_all(b"builtin wait 1\n")
        .await
        .expect("write task wait command");
    wait_for_output(
        &mut stdout,
        &mut stdout_capture,
        "completed",
        "interactive final task completion",
    )
    .await;

    stdin
        .write_all(b"jobs\nquit\n")
        .await
        .expect("write task status and quit commands");
    drop(stdin);
    let status = tokio::time::timeout(CASE_TIMEOUT, child.wait())
        .await
        .expect("interactive final case timed out")
        .expect("wait for interactive final case");
    stdout
        .read_to_end(&mut stdout_capture)
        .await
        .expect("drain interactive stdout");
    let stderr = tokio::time::timeout(CASE_TIMEOUT, stderr_task)
        .await
        .expect("interactive stderr capture timed out")
        .expect("join interactive stderr capture");
    let output = Output {
        status,
        stdout: stdout_capture,
        stderr,
    };
    assert_success(&output, "interactive final task");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("started"), "{stdout}");
    assert!(stdout.contains("completed"), "{stdout}");
}

async fn exercise_http(fixture: &Path, temp: &TempDir) {
    let http = HttpFixture::start(fixture, temp).await;
    let first_request = DropFirstRequestProxy::start(&http.url).await;
    let recovered = run_http(
        &first_request.url,
        "first HTTP handshake request dropped",
        &["--json", "--exec", "add a=20 b=22"],
    )
    .await;
    assert_success(&recovered, "first HTTP handshake request dropped");
    let values = json_lines(&recovered, "first HTTP handshake request dropped");
    assert_eq!(values.len(), 1);
    assert_eq!(
        values[0].pointer("/content/0/text"),
        Some(&serde_json::json!("42"))
    );
    assert_eq!(first_request.dropped.load(Ordering::SeqCst), 1);
    assert!(
        first_request.accepted.load(Ordering::SeqCst) >= 2,
        "recovery must open a fresh TCP connection"
    );
    assert_eq!(
        std::fs::read_to_string(&http.tool_calls_file)
            .expect("read first-handshake tool-call count"),
        "1",
        "the recovered command must reach the server exactly once"
    );
    first_request.shutdown().await;

    let reconnecting = HttpFixture::start_with_reconnect(fixture, temp).await;
    let restored = run_http(
        &reconnecting.url,
        "resource subscription reconnect",
        &[
            "--json",
            "--exec",
            "subscribe fixture://guide",
            "--exec",
            "add a=20 b=22",
        ],
    )
    .await;
    assert_success(&restored, "resource subscription reconnect");
    let values = json_lines(&restored, "resource subscription reconnect");
    assert_eq!(values.len(), 2);
    assert_eq!(values[0]["subscribe"], "fixture://guide");
    assert_eq!(
        values[1].pointer("/content/0/text"),
        Some(&serde_json::json!("42"))
    );
    assert_eq!(
        std::fs::read_to_string(&reconnecting.resource_subscriptions_file)
            .expect("read resource subscription count"),
        "2",
        "the active resource subscription must be sent once initially and once on reconnect"
    );
    assert!(
        String::from_utf8_lossy(&restored.stderr).contains("[reconnected]"),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&reconnecting.tool_calls_file)
            .expect("read transient HTTP tool-call count"),
        "2",
        "one failed HTTP request and one retry should reach the server"
    );
    reconnecting.shutdown().await;

    let unavailable = HttpFixture::start_always_unavailable(fixture, temp).await;
    let failed = run_http(
        &unavailable.url,
        "persistent HTTP 503",
        &["--json", "--exec", "add a=20 b=22"],
    )
    .await;
    assert_status(&failed, 4, "persistent HTTP 503");
    let values = json_lines(&failed, "persistent HTTP 503");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["kind"], "transport");
    assert_eq!(
        std::fs::read_to_string(&unavailable.tool_calls_file)
            .expect("read persistent HTTP tool-call count"),
        "2",
        "the reconnect path must retry exactly once"
    );
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(stderr.contains("[reconnected]"), "{stderr}");
    assert!(
        stderr.contains("still no session after reconnecting"),
        "{stderr}"
    );
    unavailable.shutdown().await;

    let switching = HttpFixture::start_for_switching(fixture, temp).await;
    exercise_in_repl_connect(fixture, &switching.url, temp).await;
    switching.shutdown().await;

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

#[cfg(unix)]
async fn exercise_bearer_fd(fixture: &Path, temp: &TempDir) {
    const SECRET: &str = "fd-only-secret-88";
    let http = HttpFixture::start_with_bearer(fixture, temp, SECRET).await;
    let config = temp.path().join("bearer-fd.toml");
    std::fs::write(&config, "").expect("write empty bearer-fd config");
    let config = config.display().to_string();

    // `libc::pipe` leaves the read side inheritable. The secret is pipe data,
    // never an argument or an environment value in the mcp-repl process.
    let inherited = inherited_bearer_pipe(format!("{SECRET}\r\n").as_bytes());
    let descriptor = inherited.as_raw_fd().to_string();
    let mut command = repl_command();
    command
        .args([
            "--json",
            "--trace",
            "--no-history",
            "--config",
            &config,
            "--bearer-fd",
            &descriptor,
            "--exec",
            "tools",
            "--http",
            &http.url,
        ])
        .env_remove("MCP_BEARER")
        .env("RUST_LOG", "debug");
    let output = run(command, "bearer from inherited descriptor", CASE_TIMEOUT).await;
    drop(inherited);
    assert_success(&output, "bearer from inherited descriptor");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stdout.contains(SECRET), "secret reached stdout: {stdout}");
    assert!(!stderr.contains(SECRET), "secret reached stderr: {stderr}");
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "",
        "the ephemeral bearer must not be saved to the profile file"
    );
    http.shutdown().await;

    // CLI conflicts are rejected before reading the descriptor, and no
    // network connection is attempted when another source is present.
    let inherited = inherited_bearer_pipe(b"fd-secret");
    let descriptor = inherited.as_raw_fd().to_string();
    let mut conflict = repl_command();
    conflict.args([
        "--json",
        "--bearer-fd",
        &descriptor,
        "--bearer",
        "other-secret",
        "--exec",
        "tools",
        "--http",
        "http://127.0.0.1:9/",
    ]);
    let conflict = run(conflict, "conflicting bearer sources", CASE_TIMEOUT).await;
    drop(inherited);
    assert_status(&conflict, 2, "conflicting bearer sources");
    let values = json_lines(&conflict, "conflicting bearer sources");
    assert_eq!(values[0]["kind"], "usage");
    let error = values[0]["error"].as_str().unwrap();
    assert!(error.contains("--bearer-fd") && error.contains("--bearer"));
    assert!(!error.contains("fd-secret") && !error.contains("other-secret"));

    // Every malformed descriptor shape is a local usage failure. Regular
    // files keep the oversized case deterministic even on systems whose pipe
    // buffer is smaller than the test input.
    assert_bearer_fd_usage_error(0, "reserved", "stdio-reserved bearer descriptor").await;
    assert_bearer_fd_usage_error(i32::MAX, "not open", "closed bearer descriptor").await;
    let invalid_utf8 = inherited_bearer_file(&[0xff, 0xfe]);
    assert_bearer_fd_usage_error(
        invalid_utf8.as_raw_fd(),
        "UTF-8",
        "non-UTF-8 bearer descriptor",
    )
    .await;
    drop(invalid_utf8);
    let oversized = inherited_bearer_file(&vec![b'x'; 16 * 1024 + 1]);
    assert_bearer_fd_usage_error(
        oversized.as_raw_fd(),
        "input limit",
        "oversized bearer descriptor",
    )
    .await;
    drop(oversized);

    // A native bearer_env declaration is a conflict even when it is unset:
    // resolution must not try to load a credential that cannot be selected.
    let profile = temp.path().join("bearer-fd-profile.toml");
    std::fs::write(
        &profile,
        "[servers.secure]\nurl = \"http://127.0.0.1:9/\"\nbearer_env = \"PROFILE_TOKEN\"\n",
    )
    .unwrap();
    let inherited = inherited_bearer_pipe(b"fd-secret");
    let descriptor = inherited.as_raw_fd().to_string();
    let profile = profile.display().to_string();
    let mut profile_conflict = repl_command();
    profile_conflict
        .args([
            "--json",
            "--config",
            &profile,
            "--bearer-fd",
            &descriptor,
            "--exec",
            "tools",
            "--server",
            "secure",
        ])
        .env_remove("PROFILE_TOKEN")
        .env_remove("MCP_BEARER");
    let profile_conflict = run(profile_conflict, "profile bearer conflict", CASE_TIMEOUT).await;
    drop(inherited);
    assert_status(&profile_conflict, 2, "profile bearer conflict");
    let values = json_lines(&profile_conflict, "profile bearer conflict");
    let error = values[0]["error"].as_str().unwrap();
    assert!(error.contains("profile `bearer_env`"), "{error}");
    assert!(!error.contains("fd-secret"));
}

/// The generators run before anything connects, so they need no server, no
/// config file, and no terminal. That is the property a packaging script
/// depends on, and it only holds at the process boundary.
async fn exercise_generators() {
    // Each shell spells a long option its own way: bash and zsh emit
    // `--protocol`, fish emits `-l protocol`.
    //
    // The bash marker is the `complete -F` builtin rather than the generated
    // function name. clap_complete renamed that function from `_mcp-repl` to
    // `_mcp__repl` in 4.6.9, which broke this case while the script itself
    // stayed correct: it defines the function and registers it under the same
    // name. Pinning the name here tests clap_complete's spelling rather than
    // the script's shape.
    for (shell, marker, protocol_flag, demo_flag) in [
        ("bash", "complete -F", "--protocol", "--demo"),
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
    for section in [
        ".SH NAME",
        ".SH SYNOPSIS",
        ".SH DESCRIPTION",
        ".SH OPTIONS",
        ".SH \"REPL BUILT-INS\"",
    ] {
        assert!(roff.contains(section), "man page has no {section}");
    }
    assert!(roff.contains("mcp-repl"));
    assert!(roff.contains("connect demo"));
    assert!(roff.contains("bench get_downloads"));

    let mut command = repl_command();
    command.args(["--demo", "--json", "--exec", "help wait"]);
    let output = run(command, "JSON built-in help", CASE_TIMEOUT).await;
    assert_success(&output, "JSON built-in help");
    let values = json_lines(&output, "JSON built-in help");
    assert_eq!(values.len(), 1);
    assert_eq!(values[0]["name"], "wait");
    assert!(
        values[0]["details"]
            .as_array()
            .is_some_and(|details| !details.is_empty())
    );
    assert_eq!(values[0]["examples"][1], "wait --timeout 30");
}

/// `bind`/`binds`/`unbind`: a default value for a tool parameter, applied
/// when a call omits it and typed from the schema rather than sent as text,
/// an explicit argument always wins over it, and everything clears when
/// `connect` switches servers (#161).
async fn exercise_bind(fixture: &Path, temp: &TempDir) {
    let mut command = repl_command();
    command.args(["--demo", "--json", "--exec", "binds"]);
    let output = run(command, "empty binds", CASE_TIMEOUT).await;
    assert_success(&output, "empty binds");
    assert_eq!(json_lines(&output, "empty binds"), [serde_json::json!({})]);

    // `repeat` on the demo's `echo` tool is a u8: a bind sent as the text
    // "3" rather than the JSON number 3 would fail deserialization
    // server-side instead of repeating three times, so a successful repeat
    // proves the coercion, not just that the bind applied.
    let mut command = repl_command();
    command.args([
        "--demo",
        "--json",
        "--exec",
        "bind repeat=3",
        "--exec",
        "echo message=hi",
        "--exec",
        "echo message=hi repeat=1",
        "--exec",
        "unbind repeat",
        "--exec",
        "echo message=hi",
    ]);
    let output = run(command, "bind fills a gap and explicit wins", CASE_TIMEOUT).await;
    assert_success(&output, "bind fills a gap and explicit wins");
    let values = json_lines(&output, "bind fills a gap and explicit wins");
    assert_eq!(values[0]["name"], "repeat");
    assert_eq!(values[0]["value"], "3");
    assert_eq!(
        values[1].pointer("/content/0/text"),
        Some(&serde_json::json!("hi hi hi")),
        "the bind filled repeat, coerced to an integer: {:?}",
        values[1]
    );
    assert_eq!(
        values[2].pointer("/content/0/text"),
        Some(&serde_json::json!("hi")),
        "an explicit repeat=1 wins over the bind: {:?}",
        values[2]
    );
    assert_eq!(values[3]["removed"], "repeat");
    assert_eq!(
        values[4].pointer("/content/0/text"),
        Some(&serde_json::json!("hi")),
        "unbind removed the default, so echo's own default (one repeat) applies: {:?}",
        values[4]
    );

    // `bench` promises the same prepared request as a direct call. Fill a
    // required numeric argument from a bind, then make that bind invalid and
    // prove an explicit benchmark argument still wins over it (#222).
    let mut command = repl_command();
    command.args([
        "--demo",
        "--json",
        "--exec",
        "bind value=100",
        "--exec",
        "bench convert from=celsius to=fahrenheit --n 1",
        "--exec",
        "bind value=not-a-number",
        "--exec",
        "bench convert value=100 from=celsius to=fahrenheit --n 1",
    ]);
    let output = run(command, "bench honors binds", CASE_TIMEOUT).await;
    assert_success(&output, "bench honors binds");
    let values = json_lines(&output, "bench honors binds");
    assert_eq!(
        values.len(),
        4,
        "two bind reports and two benchmark results"
    );
    for value in [&values[1], &values[3]] {
        assert_eq!(value["calls"], 1, "{value:?}");
        assert_eq!(value["ok"], 1, "{value:?}");
        assert_eq!(value["errors"], 0, "{value:?}");
    }

    // A bind for a parameter no tool declares warns at bind time rather than
    // failing silently at call time, and still succeeds.
    let mut command = repl_command();
    command.args([
        "--demo",
        "--json",
        "--exec",
        "bind nope=1",
        "--exec",
        "binds",
    ]);
    let output = run(
        command,
        "bind warns on an undeclared parameter",
        CASE_TIMEOUT,
    )
    .await;
    assert_success(&output, "bind warns on an undeclared parameter");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no tool") && stderr.contains("nope"),
        "{stderr}"
    );
    let values = json_lines(&output, "bind warns on an undeclared parameter");
    assert_eq!(values[1]["nope"], "1");

    // Binds are per connection: connecting to a different server clears
    // them, the same way captured variables and background tasks already
    // do, so a value bound for one server cannot leak into a same-named
    // parameter on the next.
    let fixture_string = fixture.display().to_string();
    let fixture_literal = serde_json::to_string(&fixture_string).expect("quote fixture path");
    let exit_file = temp.path().join("bind-reconnect.exit");
    let input = format!(
        "bind repeat=3\n\
         binds\n\
         connect -- {fixture_literal}\n\
         binds\n\
         quit\n"
    );
    let mut command = repl_command();
    command
        .args(["--demo", "--no-history", "--color", "never"])
        .env("MCP_REPL_FIXTURE_EXIT_FILE", &exit_file);
    let output = run_with_input(command, &input, "binds clear on reconnect", CASE_TIMEOUT).await;
    assert_success(&output, "binds clear on reconnect");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("repeat"), "{stdout}");
    assert!(
        stdout.contains("server-scoped state cleared") && stdout.contains("1 bind"),
        "{stdout}"
    );
    assert!(stdout.contains("no binds set"), "{stdout}");
    assert_eq!(
        wait_for_file(&exit_file, "bind reconnect fixture shutdown").await,
        "clean",
        "switching servers did not orderly close the stdio child"
    );
}

/// Values that happen to be valid JSON literals still remain text when the
/// tool schema declares a string (#221).
async fn exercise_string_tool_arguments() {
    let mut command = repl_command();
    command.args([
        "--demo",
        "--json",
        "--exec",
        "echo message=true",
        "--exec",
        "echo message=123",
        "--exec",
        "echo message=null",
        "--exec",
        "echo message=[1,2]",
    ]);
    let output = run(command, "string tool arguments", CASE_TIMEOUT).await;
    assert_success(&output, "string tool arguments");
    let values = json_lines(&output, "string tool arguments");
    assert_eq!(values.len(), 4, "one result per string tool argument");
    for (value, expected) in values.iter().zip(["true", "123", "null", "[1,2]"]) {
        assert_eq!(
            value.pointer("/content/0/text"),
            Some(&serde_json::json!(expected)),
            "the schema-declared string changed JSON type: {value:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn published_cli_covers_transports_and_protocol_lifecycles() {
    tokio::time::timeout(SUITE_TIMEOUT, async {
        let temp = TempDir::new().expect("temporary fixture directory");
        let fixture = build_fixture().await;
        exercise_generators().await;
        exercise_connection_failure_output().await;
        exercise_initialization_timeout(&fixture, &temp).await;
        exercise_elicitation_field_order().await;
        exercise_respond_needs_the_final_lifecycle().await;
        #[cfg(unix)]
        exercise_cancellation().await;
        exercise_json_contract(&fixture, &temp).await;
        exercise_colliding_tool_names(&fixture, &temp).await;
        exercise_exec_waits_for_its_own_tasks(&fixture, &temp).await;
        exercise_tools_only_server(&fixture, &temp).await;
        exercise_loglevel(&fixture, &temp).await;
        exercise_sampling().await;
        exercise_repl_config(&temp).await;
        exercise_login_json(&temp).await;
        exercise_no_target(&temp).await;
        exercise_multiline_piped_input().await;
        exercise_unreadable_listing(&fixture, &temp).await;
        exercise_absent_cursor(&fixture, &temp).await;
        exercise_downgraded_protocol(&fixture, &temp).await;
        exercise_schema_contracts(&fixture, &temp).await;
        exercise_imported_stdio_config(&fixture, &temp).await;
        exercise_stdio(&fixture, &temp).await;
        #[cfg(unix)]
        exercise_bearer_fd(&fixture, &temp).await;
        exercise_http(&fixture, &temp).await;
        exercise_bind(&fixture, &temp).await;
        exercise_string_tool_arguments().await;
    })
    .await
    .expect("mcp-repl E2E suite exceeded its job-level timeout");
}
