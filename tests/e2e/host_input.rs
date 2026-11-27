use super::{
    CASE_TIMEOUT, Child, Command, Output, Path, TempDir, assert_status, assert_success, json_lines,
    repl_command, run, wait_for_file,
};
use serde_json::{Value, json};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};

struct HostControl {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl HostControl {
    fn spawn(mut command: Command, generated: Option<&str>) -> (Child, Self) {
        let (host, socket) = UnixStream::pair().expect("host input socketpair");
        host.set_nonblocking(true).expect("nonblocking host socket");
        let fd = socket.as_raw_fd();
        command
            .args(["--json", "--host-input-fd", &fd.to_string(), "--trace"])
            .args(generated)
            .env("MCP_REPL_FIXTURE_HOST_INPUT", "1")
            .env("RUST_LOG", "trace")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // SAFETY: The child only calls fcntl on the inherited socket before exec.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().expect("spawn bridged REPL");
        drop(socket);
        let (reader, writer) = tokio::net::UnixStream::from_std(host)
            .expect("register host socket")
            .into_split();
        (
            child,
            Self {
                reader: BufReader::new(reader),
                writer,
            },
        )
    }

    async fn next(&mut self, frame_type: &str) -> Value {
        tokio::time::timeout(CASE_TIMEOUT, async {
            let mut frame = String::new();
            loop {
                frame.clear();
                let bytes = self
                    .reader
                    .read_line(&mut frame)
                    .await
                    .expect("read host frame");
                assert!(bytes > 0, "socket closed while waiting for {frame_type}");
                let value: Value = serde_json::from_str(&frame).expect("host JSON frame");
                assert_eq!(value["v"], 1);
                if value["type"] == "response_status" && frame_type != "response_status" {
                    continue;
                }
                assert_eq!(value["type"], frame_type, "{value}");
                return value;
            }
        })
        .await
        .expect("host frame timeout")
    }

    async fn answer(&mut self, question: &Value, payload: Value) {
        let mut frame = json!({
            "v": 1, "type": "response", "session": question["session"], "id": question["id"],
        });
        frame
            .as_object_mut()
            .unwrap()
            .extend(payload.as_object().unwrap().clone());
        self.writer
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .expect("send host answer");
    }
}

async fn host_output(child: Child) -> Output {
    tokio::time::timeout(CASE_TIMEOUT, child.wait_with_output())
        .await
        .expect("bridged REPL did not exit")
        .expect("bridged REPL output")
}

fn fixture_command(config: &Path, argv: Value) -> Command {
    std::fs::write(
        config,
        format!("[servers.fixture]\ntransport = \"stdio\"\ncommand = {argv}\n"),
    )
    .unwrap();
    let mut command = repl_command();
    command
        .arg("--config")
        .arg(config)
        .args(["--server", "fixture"]);
    command
}

fn sample_response() -> Value {
    json!({"action":"sample","origin":"model","result":{
        "model":"fixture-model","role":"assistant","content":{"type":"text","text":"bridge-private-sample"}
    }})
}

pub(super) async fn exercise(fixture: &Path, temp: &TempDir) {
    exercise_usage_errors().await;
    let config = temp.path().join("host-input.toml");
    let mut command = fixture_command(&config, json!([fixture]));
    command.args([
        "-e",
        "host_form",
        "-e",
        "host_url",
        "-e",
        "host_sample",
        "-e",
        "host_form",
        "-e",
        "host_form",
        "-e",
        "host_form",
        "--host-input-timeout",
        "1",
    ]);
    let (child, mut host) = HostControl::spawn(command, None);
    let form = host.next("needs_input").await;
    assert_eq!(form["server"]["name"], "mcp-repl-fixture");
    assert_eq!(form["server"]["protocol"], "2025-11-25");
    assert_eq!(form["kind"], "form");
    assert_eq!(form["request"]["credentialFields"], json!(["apiKey"]));
    assert_eq!(form["request"]["schema"]["required"], json!(["apiKey"]));
    assert!(form["deadlineUnixMs"].as_u64().is_some());
    host.answer(
        &form,
        json!({"action":"accept","content":{"apiKey":"bridge-private-answer"}}),
    )
    .await;
    let url = host.next("needs_input").await;
    assert_eq!(url["kind"], "url");
    assert_eq!(url["actions"], json!(["confirm_url", "decline", "cancel"]));
    host.answer(&url, json!({"action":"confirm_url"})).await;
    let sample = host.next("needs_input").await;
    assert_eq!(sample["kind"], "sampling");
    assert_eq!(sample["request"]["maxTokens"], 24);
    host.answer(&sample, sample_response()).await;
    for action in ["decline", "cancel"] {
        let form = host.next("needs_input").await;
        host.answer(&form, json!({"action":action})).await;
    }
    let timeout = host.next("needs_input").await;
    let withdrawn = host.next("withdrawn").await;
    assert_eq!(withdrawn["id"], timeout["id"]);
    assert_eq!(withdrawn["reason"], "expired");
    let output = host_output(child).await;
    assert_success(&output, "stable host input");
    let frames = json_lines(&output, "stable host input");
    assert_eq!(frames.len(), 6);
    for (frame, expected) in frames.iter().zip([
        "form accept",
        "url accept",
        "sampled by fixture-model",
        "form decline",
        "form cancel",
        "form cancel",
    ]) {
        assert_eq!(frame["content"][0]["text"], expected);
    }
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    for secret in [
        "bridge-private-answer",
        "bridge-private-sample",
        "fixture-url-token",
    ] {
        assert!(!diagnostics.contains(secret), "{diagnostics}");
    }

    exercise_final(fixture, temp, &config).await;
    exercise_final_task(fixture, &config).await;
    exercise_command_timeout(fixture, &config).await;
    exercise_stable_error_is_withheld(fixture, temp).await;
    exercise_server_end(fixture, temp, false).await;
    exercise_server_end(fixture, temp, true).await;

    let exit = temp.path().join("host-loss-fixture-exit");
    let mut command = fixture_command(&config, json!([fixture]));
    command
        .args(["--timeout", "0"])
        .env("MCP_REPL_FIXTURE_EXIT_FILE", &exit);
    let (mut child, mut host) = HostControl::spawn(command, None);
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"host_form\n").await.unwrap();
    assert_eq!(host.next("needs_input").await["kind"], "form");
    drop(host);
    // Hold command stdin open until the REPL exits.
    let output = host_output(child).await;
    drop(stdin);
    assert_status(&output, 4, "host disappearance");
    assert_eq!(json_lines(&output, "host disappearance").len(), 1);
    assert_eq!(
        wait_for_file(&exit, "host loss fixture shutdown").await,
        "clean"
    );

    let command = fixture_command(&config, json!([fixture]));
    let (mut child, host) = HostControl::spawn(command, None);
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    stdin.write_all(b"connect fixture\n").await.unwrap();
    let mut line = String::new();
    tokio::time::timeout(CASE_TIMEOUT, stdout.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap(),
        json!({"connected":"mcp-repl-fixture","protocol":"2025-11-25"})
    );
    drop(host);
    let output = host_output(child).await;
    drop(stdin);
    assert_status(&output, 4, "idle host disappearance");
    line.clear();
    assert_eq!(stdout.read_line(&mut line).await.unwrap(), 0);
}

async fn exercise_command_timeout(fixture: &Path, config: &Path) {
    let mut command = fixture_command(config, json!([fixture]));
    command.args(["--timeout", "1"]);
    let (mut child, mut host) = HostControl::spawn(command, None);
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"host_form\n").await.unwrap();
    let question = host.next("needs_input").await;
    let withdrawn = host.next("withdrawn").await;
    assert_eq!(withdrawn["id"], question["id"]);
    assert_eq!(withdrawn["reason"], "cancelled");
    host.answer(&question, json!({"action":"decline"})).await;
    let status = host.next("response_status").await;
    assert_eq!(status["id"], question["id"]);
    assert_eq!(status["reason"], "stale");
    drop(stdin);
    let output = host_output(child).await;
    let frames = json_lines(&output, "command timeout during host input");
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[0]["exitStatus"], 4);
    assert!(frames[0]["error"].as_str().unwrap().contains("--timeout"));
    assert_eq!(frames[1], json!({"exit":true}));
}

async fn exercise_usage_errors() {
    let fd = i32::MAX.to_string();
    let mut command = repl_command();
    command.args(["--json", "--host-input-fd", &fd, "--demo", "-e", "tools"]);
    let output = run(command, "closed host input descriptor", CASE_TIMEOUT).await;
    assert_status(&output, 2, "closed host input descriptor");
    let frames = json_lines(&output, "closed host input descriptor");
    assert_eq!(frames.len(), 1);
    assert!(frames[0]["error"].as_str().unwrap().contains("not open"));

    for flag in ["--elicitation", "--sampling"] {
        let mut command = repl_command();
        command.args([
            "--json",
            "--host-input-fd",
            &fd,
            flag,
            "decline",
            "--demo",
            "-e",
            "tools",
        ]);
        let output = run(command, flag, CASE_TIMEOUT).await;
        assert_status(&output, 2, flag);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("--host-input-fd") && stderr.contains(flag),
            "{stderr}"
        );
    }

    let mut command = repl_command();
    command.args(["--json", "--host-input-fd", &fd, "plugin", "status"]);
    let output = run(command, "host input with plugin management", CASE_TIMEOUT).await;
    assert_status(&output, 2, "host input with plugin management");
    assert!(String::from_utf8_lossy(&output.stderr).contains("--host-input-fd is not applicable"));
}

async fn exercise_final(fixture: &Path, temp: &TempDir, config: &Path) {
    let rounds = temp.path().join("host-rounds");
    for generated in [false, true] {
        let mut command = fixture_command(config, json!([fixture]));
        command.args(["--protocol", "final"]);
        if !generated {
            command
                .args([
                    "-e",
                    "host_rounds",
                    "-e",
                    "slow_add a=19 b=23",
                    "-e",
                    "host_round_limit",
                    "-e",
                    "bench slow_add",
                    "-e",
                    "slow_add a=19 b=23 &",
                ])
                .env("MCP_REPL_FIXTURE_HOST_ROUNDS", &rounds);
        }
        let (child, mut host) = HostControl::spawn(command, generated.then_some("host_rounds"));
        let form = host.next("needs_input").await;
        let sample = host.next("needs_input").await;
        assert_eq!(form["server"]["protocol"], "2026-07-28");
        assert_eq!(form["kind"], "form");
        assert_eq!(sample["kind"], "sampling");
        assert_eq!(form["session"], sample["session"]);
        host.answer(&sample, sample_response()).await;
        host.answer(&form, json!({"action":"decline"})).await;
        let output = host_output(child).await;
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("bridge-private-sample"), "{stderr}");
        let frames = json_lines(&output, "final host input");
        assert_eq!(
            frames[0]["content"][0]["text"],
            "final decline / fixture-model"
        );
        if generated {
            assert_success(&output, "generated final host input");
            assert_eq!(frames.len(), 1);
        } else {
            assert_status(&output, 4, "final round limit");
            assert_eq!(frames.len(), 5);
            assert_eq!(frames[1]["content"][0]["text"], "42");
            assert!(frames[2]["error"].as_str().unwrap().contains("round limit"));
            assert_eq!(frames[3]["exitStatus"], 2);
            assert_eq!(frames[4]["exitStatus"], 2);
            assert!(
                frames[4]["error"]
                    .as_str()
                    .unwrap()
                    .contains("background jobs")
            );
            assert_eq!(std::fs::read_to_string(&rounds).unwrap(), "4");
        }
    }

    let mut command = fixture_command(config, json!([fixture]));
    command.args(["--protocol", "final", "-e", "host_rounds"]);
    let (child, mut host) = HostControl::spawn(command, None);
    let form = host.next("needs_input").await;
    let sample = host.next("needs_input").await;
    host.answer(&sample, json!({"action":"decline"})).await;
    let withdrawn = host.next("withdrawn").await;
    assert_eq!(withdrawn["id"], form["id"]);
    assert_eq!(withdrawn["reason"], "cancelled");
    let output = host_output(child).await;
    assert_status(&output, 5, "final sampling refusal");
    assert_eq!(json_lines(&output, "final sampling refusal").len(), 1);
}

/// A server-created final task that parks in `input_required` gets its
/// requests through the bridge, both while the call waits on it and from
/// `task <id> respond`.
async fn exercise_final_task(fixture: &Path, config: &Path) {
    let mut command = fixture_command(config, json!([fixture]));
    command.args(["--protocol", "final", "-e", "host_task"]);
    let (child, mut host) = HostControl::spawn(command, None);
    let form = host.next("needs_input").await;
    let sample = host.next("needs_input").await;
    assert_eq!(form["server"]["protocol"], "2026-07-28");
    assert_eq!(form["kind"], "form");
    assert_eq!(sample["kind"], "sampling");
    host.answer(
        &form,
        json!({"action":"accept","content":{"apiKey":"bridge-private-answer"}}),
    )
    .await;
    host.answer(&sample, sample_response()).await;
    let output = host_output(child).await;
    assert_success(&output, "final task host input");
    let frames = json_lines(&output, "final task host input");
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(
        frames[0]["content"][0]["text"],
        "task accept / fixture-model"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for secret in ["bridge-private-answer", "bridge-private-sample"] {
        assert!(!stderr.contains(secret), "{stderr}");
    }

    // The host declines the form and lets the sampling request expire. The
    // decline is sent, the expired key stays outstanding, and the call
    // reports the parked task. `task last respond` asks only for that key.
    let mut command = fixture_command(config, json!([fixture]));
    command.args([
        "--protocol",
        "final",
        "--host-input-timeout",
        "2",
        "-e",
        "host_task",
        "-e",
        "task last",
        "-e",
        "task last respond",
        "-e",
        "builtin wait last",
    ]);
    let (child, mut host) = HostControl::spawn(command, None);
    let form = host.next("needs_input").await;
    let sample = host.next("needs_input").await;
    host.answer(&form, json!({"action":"decline"})).await;
    // The decline reached the REPL before the shared deadline.
    let status = host.next("response_status").await;
    assert_eq!(status["id"], form["id"]);
    assert_eq!(status["status"], "accepted");
    let withdrawn = host.next("withdrawn").await;
    assert_eq!(withdrawn["id"], sample["id"]);
    assert_eq!(withdrawn["reason"], "expired");
    let again = host.next("needs_input").await;
    assert_eq!(again["kind"], "sampling");
    assert_eq!(again["session"], sample["session"]);
    assert_ne!(again["id"], sample["id"]);
    host.answer(&again, sample_response()).await;
    let output = host_output(child).await;
    assert_status(&output, 4, "final task host timeout");
    let frames = json_lines(&output, "final task host timeout");
    assert_eq!(frames.len(), 4, "{frames:?}");
    assert_eq!(frames[0]["exitStatus"], 4);
    let error = frames[0]["error"].as_str().unwrap();
    assert!(
        error.contains("still waiting for input")
            && error.contains("`b-sample` (host reply expired)")
            && error.contains("respond"),
        "{error}"
    );
    assert!(!error.contains("a-form"), "{error}");
    assert_eq!(frames[1]["status"], "input_required");
    let task_id = frames[1]["taskId"].as_str().unwrap();
    assert!(error.contains(task_id), "{error}");
    assert_eq!(frames[2]["taskId"], task_id);
    assert!(frames[2].get("error").is_none(), "{:?}", frames[2]);
    assert_eq!(frames[3]["status"], "completed");
    assert_eq!(
        frames[3]["result"]["content"][0]["text"],
        "task decline / fixture-model"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("bridge-private-sample"), "{stderr}");

    exercise_wait_answers_a_parked_task(fixture, config).await;
    exercise_bare_wait_is_one_value(fixture, config).await;
    exercise_respond_honors_the_command_timeout(fixture, config).await;
    exercise_failed_task_text_is_withheld(fixture, config).await;

    // Losing the host while the call waits on the task ends the session.
    let mut command = fixture_command(config, json!([fixture]));
    command.args(["--protocol", "final", "--timeout", "0"]);
    let (mut child, mut host) = HostControl::spawn(command, None);
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"host_task\n").await.unwrap();
    assert_eq!(host.next("needs_input").await["kind"], "form");
    assert_eq!(host.next("needs_input").await["kind"], "sampling");
    drop(host);
    let output = host_output(child).await;
    drop(stdin);
    assert_status(&output, 4, "host loss during final task");
    let frames = json_lines(&output, "host loss during final task");
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_eq!(frames[0]["exitStatus"], 4);
}

/// Both questions expire during the call, so nothing is sent and the task
/// stays parked. The first `wait` asks both again; the host answers the
/// sampling request and lets the form expire, so only the sample is sent and
/// the form stays outstanding instead of going back as `cancel`. The second
/// `wait` asks only for the form and the task completes.
async fn exercise_wait_answers_a_parked_task(fixture: &Path, config: &Path) {
    let mut command = fixture_command(config, json!([fixture]));
    command.args([
        "--protocol",
        "final",
        "--host-input-timeout",
        "2",
        "-e",
        "host_task",
        "-e",
        "builtin wait last",
        "-e",
        "builtin wait last",
    ]);
    let (child, mut host) = HostControl::spawn(command, None);
    for _ in 0..2 {
        host.next("needs_input").await;
    }
    for _ in 0..2 {
        assert_eq!(host.next("withdrawn").await["reason"], "expired");
    }
    let form = host.next("needs_input").await;
    let sample = host.next("needs_input").await;
    assert_eq!(form["kind"], "form");
    assert_eq!(sample["kind"], "sampling");
    host.answer(&sample, sample_response()).await;
    let status = host.next("response_status").await;
    assert_eq!(status["id"], sample["id"]);
    assert_eq!(status["status"], "accepted");
    let withdrawn = host.next("withdrawn").await;
    assert_eq!(withdrawn["id"], form["id"]);
    assert_eq!(withdrawn["reason"], "expired");
    let again = host.next("needs_input").await;
    assert_eq!(again["kind"], "form");
    host.answer(
        &again,
        json!({"action":"accept","content":{"apiKey":"bridge-private-answer"}}),
    )
    .await;
    let output = host_output(child).await;
    assert_status(&output, 4, "wait answers a parked task");
    let frames = json_lines(&output, "wait answers a parked task");
    assert_eq!(frames.len(), 3, "{frames:?}");
    let error = frames[0]["error"].as_str().unwrap();
    assert!(
        error.contains("`a-form` (host reply expired)")
            && error.contains("`b-sample` (host reply expired)"),
        "{error}"
    );
    let error = frames[1]["error"]
        .as_str()
        .expect("the form stayed outstanding");
    assert!(
        error.contains("`a-form` (host reply expired)") && !error.contains("b-sample"),
        "{error}"
    );
    assert_eq!(frames[2]["status"], "completed");
    assert_eq!(
        frames[2]["result"]["content"][0]["text"],
        "task accept / fixture-model"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for secret in ["bridge-private-answer", "bridge-private-sample"] {
        assert!(!stderr.contains(secret), "{stderr}");
    }
}

/// A bare `wait` whose task is left parked again reports that task's error
/// inside its one array value.
async fn exercise_bare_wait_is_one_value(fixture: &Path, config: &Path) {
    let mut command = fixture_command(config, json!([fixture]));
    command.args([
        "--protocol",
        "final",
        "--host-input-timeout",
        "1",
        "-e",
        "host_task",
        "-e",
        "builtin wait",
    ]);
    let (child, mut host) = HostControl::spawn(command, None);
    for _ in 0..2 {
        host.next("needs_input").await;
        host.next("needs_input").await;
        host.next("withdrawn").await;
        host.next("withdrawn").await;
    }
    let output = host_output(child).await;
    assert_status(&output, 4, "bare wait with bridge input");
    let frames = json_lines(&output, "bare wait with bridge input");
    assert_eq!(frames.len(), 2, "{frames:?}");
    let outcomes = frames[1].as_array().expect("one array value");
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert_eq!(outcomes[0]["exitStatus"], 4);
    assert!(outcomes[0]["taskId"].is_string());
    assert!(
        outcomes[0]["error"]
            .as_str()
            .unwrap()
            .contains("still waiting for input")
    );
}

/// `--timeout` bounds `task <id> respond`, including the host's questions,
/// well inside the 30 second host timeout. The host-input fixture advertises
/// a 100 ms poll interval, so the call parks the task within its own one
/// second deadline; only the `respond` that follows is measured.
async fn exercise_respond_honors_the_command_timeout(fixture: &Path, config: &Path) {
    let mut command = fixture_command(config, json!([fixture]));
    command.args([
        "--protocol",
        "final",
        "--timeout",
        "1",
        "--host-input-timeout",
        "30",
        "-e",
        "host_task",
        "-e",
        "task last respond",
    ]);
    let (child, mut host) = HostControl::spawn(command, None);
    // The call parks the task and gives up at its deadline.
    for _ in 0..2 {
        host.next("needs_input").await;
    }
    for _ in 0..2 {
        host.next("withdrawn").await;
    }
    let first = host.next("needs_input").await;
    let published = std::time::Instant::now();
    assert_eq!(first["kind"], "form");
    host.next("needs_input").await;
    for _ in 0..2 {
        let withdrawn = host.next("withdrawn").await;
        assert!(
            ["expired", "cancelled"].contains(&withdrawn["reason"].as_str().unwrap()),
            "{withdrawn}"
        );
    }
    assert!(
        published.elapsed() < std::time::Duration::from_secs(10),
        "respond questions outlived --timeout"
    );
    let output = host_output(child).await;
    assert_status(&output, 4, "respond under --timeout");
    let frames = json_lines(&output, "respond under --timeout");
    assert_eq!(frames.len(), 2, "{frames:?}");
    for frame in &frames {
        assert_eq!(frame["exitStatus"], 4, "{frame}");
    }
}

/// A task that fails with the submitted value in its error message and data
/// does not reveal it through the call, `task`, `jobs`, or `cancel`.
async fn exercise_failed_task_text_is_withheld(fixture: &Path, config: &Path) {
    let mut command = fixture_command(config, json!([fixture]));
    command.args([
        "--protocol",
        "final",
        "-e",
        "host_task_fail",
        "-e",
        "task last",
        "-e",
        "jobs",
        "-e",
        "cancel last",
    ]);
    let (child, mut host) = HostControl::spawn(command, None);
    let form = host.next("needs_input").await;
    host.answer(
        &form,
        json!({"action":"accept","content":{"apiKey":"bridge-private-answer"}}),
    )
    .await;
    let output = host_output(child).await;
    assert_status(&output, 3, "failed task text");
    for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains("bridge-private-answer"), "{stream}: {text}");
    }
    let frames = json_lines(&output, "failed task text");
    assert_eq!(frames.len(), 4, "{frames:?}");
    assert_eq!(frames[0]["exitStatus"], 3);
    assert!(
        frames[0]["error"]
            .as_str()
            .unwrap()
            .contains("(code -32602)")
    );
    assert_eq!(frames[1]["status"], "failed");
    assert_eq!(frames[1]["statusMessage"], "Task failed");
    assert_eq!(frames[1]["error"]["code"], -32602);
    assert!(frames[1]["error"].get("data").is_none());
    assert_eq!(frames[2][0]["task"]["status"], "failed");
    assert_eq!(frames[3]["status"], "failed");
}

/// A stable-protocol server that rejects a tool call, resource read, or
/// prompt request with the host's form answer in its JSON-RPC error has that
/// text withheld.
async fn exercise_stable_error_is_withheld(fixture: &Path, temp: &TempDir) {
    let config = temp.path().join("host-echo.toml");
    let mut command = fixture_command(
        &config,
        json!([fixture, "--tools-only", "--echo-host-answer"]),
    );
    let commands = ["add a=1 b=2", "read note://status", "prompt greet name=x"];
    for line in commands {
        command.args(["-e", line]);
    }
    let (child, mut host) = HostControl::spawn(command, None);
    for _ in commands {
        let form = host.next("needs_input").await;
        assert_eq!(form["server"]["protocol"], "2025-11-25");
        host.answer(
            &form,
            json!({"action":"accept","content":{"apiKey":"bridge-private-answer"}}),
        )
        .await;
    }
    let output = host_output(child).await;
    assert_status(&output, 3, "stable error text");
    for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
        let text = String::from_utf8_lossy(bytes);
        assert!(!text.contains("bridge-private-answer"), "{stream}: {text}");
    }
    let frames = json_lines(&output, "stable error text");
    assert_eq!(frames.len(), commands.len(), "{frames:?}");
    for frame in &frames {
        assert!(
            frame["error"]
                .as_str()
                .unwrap()
                .contains("withheld in bridge mode (code -32602)"),
            "{frame}"
        );
    }
}

async fn exercise_server_end(fixture: &Path, temp: &TempDir, disconnect: bool) {
    let tag = if disconnect {
        "disconnect"
    } else {
        "server-cancel"
    };
    let trigger = temp.path().join(format!("host-trigger-{tag}"));
    let reply = temp.path().join(format!("unexpected-host-reply-{tag}"));
    let config = temp.path().join(format!("host-{tag}.toml"));
    let mut argv = vec![fixture.display().to_string(), "--tools-only".to_owned()];
    if disconnect {
        argv.push("--disconnect-host-input".to_owned());
    }
    let mut command = fixture_command(&config, json!(argv));
    command
        .env("MCP_REPL_FIXTURE_HOST_TRIGGER", &trigger)
        .env("MCP_REPL_FIXTURE_UNEXPECTED_HOST_REPLY", &reply);
    let (mut child, mut host) = HostControl::spawn(command, None);
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"add a=1 b=2\n").await.unwrap();
    let question = host.next("needs_input").await;
    std::fs::write(trigger, b"go").unwrap();
    let withdrawn = host.next("withdrawn").await;
    assert_eq!(withdrawn["id"], question["id"]);
    let expected = if disconnect {
        "disconnected"
    } else {
        "server_cancelled"
    };
    assert_eq!(withdrawn["reason"], expected);
    host.answer(&question, json!({"action":"decline"})).await;
    let status = host.next("response_status").await;
    assert_eq!(status["reason"], "stale");
    drop(stdin);
    let output = host_output(child).await;
    let frames = json_lines(&output, tag);
    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_eq!(frames[1], json!({"exit":true}));
    if disconnect {
        assert_eq!(frames[0]["exitStatus"], 4);
    } else {
        assert_eq!(frames[0]["content"][0]["text"], "server cancelled");
    }
    assert!(!reply.exists());
}
