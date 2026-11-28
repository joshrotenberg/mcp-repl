# Scripting

[mcp-repl](../README.md) · [Connecting](connecting.md) · [Commands](commands.md) · [Scripting](scripting.md) · [Debugging](debugging.md) · [Agent skills](agent-skills.md)


Running mcp-repl from a script or a CI job.

## One-shot / scripting

`-e/--exec <COMMAND>` runs a command and exits instead of opening the prompt.
Repeatable; commands run in order against the same session, including after a
failure, so later cleanup or inspection commands still run. The final status
is the highest-severity outcome seen across the sequence.

```bash
# One call, pretty output:
mcp-repl --http https://example/mcp -e "get_crate_info name=serde"

# One JSON result for piping to jq (--json also silences the banner and timings):
mcp-repl --http https://example/mcp -e "search_crates query=serde" --json | jq '.content'

# Several commands in one session; JSON output is NDJSON, one value per line:
mcp-repl --demo --json -e "tools" -e "echo message=hi" | jq -c .

# Human output from several commands:
mcp-repl --demo -e "echo message=hi" -e "about"
```

## Generated commands

Normal builds include a one-command CLI discovered from the live server:

```bash
mcp-repl --demo --json echo --message='hello world'
mcp-repl --server local --json tool add --a -3 --b=5
mcp-repl --trust-import --server path/to/.mcp.json:local tool --help
mcp-repl --http https://example/mcp call raw_tool '{"nested":{"items":[1,null]}}'
```

Use explicit `--demo`, `--server NAME` (native profile or imported entry), or
`--http URL`. With no command, these still open the interactive prompt. Lone
positional profile/import selectors retain their connection meaning; they do
not introduce generated commands. Raw stdio invocations preserve every child
argument after the executable; use `-e` for that form. A lone native-profile or imported positional
selector following `--http URL` retains its existing HTTP retargeting meaning
and authentication, even if its name matches a discovered tool.
Import approval is required before connecting, including for generated help.
With `--demo`, all positional tokens are generated commands; import-looking
tokens do not open files or resolve imported environment variables.

Global flags belong **before the command**; everything after its first word
belongs to the generated parser. `--json` after a tool name is a tool flag only
if the schema declares that property. `tool <name>` and `builtin <name>`
resolve namespace collisions explicitly. A bare name shared by a tool and a
built-in is rejected. The generated built-ins are listings (`tools`, `prompts`,
`resources`, `templates`), `find`, `describe`, `snapshot`, `validate`, `read`,
`prompt`, and `call`. Use `-e` for other built-ins and stateful or multi-command
workflows. Combining a generated command with `-e` is a usage error.

Flags come from top-level schema properties; use raw JSON `call` for schemas
that require other JSON Schema composition. Every schema flag takes one value, including booleans (`--enabled=true` or
`--enabled=false`); bare booleans and repeated flags are usage errors. Required
properties are required flags. Optional properties stay absent when omitted;
schema defaults are left to the server. Empty strings use `--text=`. Values
starting with `-` use `--text=-literal`; negative numbers also accept the
separate form `--count -3`.

Tools use the same coercion as REPL `key=value` arguments: string properties
preserve JSON-looking text, integer/number/boolean properties parse scalars,
and object/array properties parse JSON. A same-document `$ref` is resolved one
level, including references into `$defs`; a `type` beside `$ref` takes
precedence for both coercion and help. Unresolved/remote references, union
and nullable schemas use JSON-literal parsing with a string fallback. For a
nullable string that looks like a number, use `--value='"123"'` to send a string;
`--value=null` sends null. Enums appear in help as guidance; the server validates
their values and other schema constraints. Prompt flag values remain strings.
`call <raw-name> '<JSON object>'` bypasses flag coercion when exact types or
unusual property names are needed. Properties containing `=`, starting with `{`, or with an empty name
cannot use generated tool flags and must use `call`.

Ordinary names (ASCII letters, digits, underscores, internal hyphens) keep
their spelling. Other characters become hyphens and leading/trailing hyphens
are removed. Empty names use a fallback. Collisions get numeric suffixes after
all ordinary names have been reserved, in raw-name sort order. This mapping is
independent of discovery order for a fixed surface; a changed surface can add
collisions, so inspect help or use raw-name `call` in scripts. `--help` and
`--version` are reserved flag names; corresponding properties receive suffixes.
Tool/prompt help shows the raw MCP name when its command spelling changes.

Arguments are literal argv, with no alias expansion, variable substitution,
result routing or background operator. Quotes, backslashes, newlines, dollar
signs and shell-like strings stay data; your invoking shell still applies its
normal quoting rules before starting mcp-repl. Dispatch, output, schema
contracts, HTTP reconnect policy and tool-error status (3) are shared with
`-e`. Generated parser errors use status 2 and stderr even under `--json`,
without issuing a tool call. Help uses status 0 and stdout without invoking a
tool. Discovery itself requires a connection: generated help can start a stdio
child and fetch server metadata, then closes the session. Root `--help`,
`--man`, and `--completions` stay offline. One-shot elicitation and sampling
default to decline on both command paths.

The previous `unstable-dynamic-cli` Cargo feature is a compatibility alias;
all builds, including `--no-default-features`, now include generated commands.

## Result-dependent stdio workflows

The [session driver](../src/plugin_assets/skills/mcp-repl/scripts/session_driver.py)
ships with the mcp-repl plugin skill. Use a binary that supports piped JSON
startup for the session driver. The installed helper checks for at least
the mcp-repl version that bundled it. The source helper checks startup
framing directly. CI uses the runners' default Python 3, and the helper
avoids features newer than Python 3.8. Put the skill's
`scripts` directory on Python's import path. Pass an explicit server argv to
`Session` and use it as a context manager. The driver starts one REPL and
keeps its stdio connection open for subsequent calls. Fixed `-e` batches
continue after errors, so inspect each reply in Python when the next action
depends on it.

This example uses the repository's `mcp_repl_fixture` example binary built
with `cargo build --example mcp_repl_fixture`. Set `fixture_path` to the path
to that executable.

```python
from session_driver import Session

with Session([fixture_path, "--session-driver"]) as session:
    names = {tool["name"] for tool in session.discover().value}
    if not {"worker_start", "worker_wait", "worker_read"} <= names:
        raise RuntimeError("worker operations are unavailable")

    started = session.call("worker_start", {"text": "hello", "fail": False})
    worker = started.value["structuredContent"]
    if worker["state"] != "working" or not isinstance(worker["handle"], str):
        raise RuntimeError("worker did not start")
    owned = {"handle": worker["handle"]}
    terminal = session.call("worker_wait", owned).value["structuredContent"]
    if terminal["state"] != "completed":
        raise RuntimeError(terminal)
    result = session.call("worker_read", owned).value["structuredContent"]
    if result != {"state": "completed", "text": "hello"}:
        raise RuntimeError(result)
```

Each reply has a local `request_id` and a protocol `value`. The driver checks
client error envelopes and MCP `isError`; validate application fields against
the server's contract. For MCP tasks, select the server's supported protocol
and use `start_task(name, args)` followed by `wait_task(ref, timeout=30)`.
The driver tracks one MCP task at a time on that connection.
The wait must return a successful terminal tool result. If the REPL's wait
deadline expires, `TaskPending` leaves the task and connection usable. Wait
again or call `cancel_task(ref)` and inspect the returned task state. A
nonterminal response leaves the task available to wait on. If you can no
longer track a task, `abandon_task(ref)` releases the local reference.
Abandoning leaves server work alone.
An application worker handle goes to the server's documented lifecycle calls.
Use `read(uri)` for an MCP resource. The helper rejects dollar signs in
standalone URIs because the REPL expands variables in those tokens. Pass
dollar signs in tool arguments as ordinary JSON string data.

On Windows, point the driver at an executable. Batch launchers such as
`npx.cmd`, and other `.cmd` or `.bat` files, are rejected to avoid shell
evaluation.

The driver has bounded startup, request, wait, and shutdown deadlines. It
allows two extra seconds for the REPL to report a request or wait timeout.
On session loss or interruption it invalidates the connection and never
replays a request. `SessionLost.sent` means the command was queued for
writing, so its outcome may be uncertain. Before a healthy close, use
documented cleanup operations only for work you own. `close()`
shuts down the owned connection. After a failed operation,
`stderr_tail` contains bounded diagnostics and
`cleanup_error` reports unconfirmed process cleanup.

The driver targets mcp-repl when interrupting a command, with SIGINT on
POSIX or Ctrl-Break when a Windows console is available. Forced POSIX
shutdown kills the owned process group. A forced Windows kill reaches only
mcp-repl. The server may keep running until it handles EOF or exits on
its own.

### Answering elicitation and sampling in Python

On Unix, pass `Session(argv, host_input=handler)` to enable the host input
bridge. The driver opens the socket, keeps reading it while handlers run,
and sends each answer with the question's identifiers. Commands still return
one stdout reply each. Set `host_input_timeout` to an integer from 1 to 120
seconds (default 30). With a handler, `request_timeout` defaults to the
greater of 10 seconds and `host_input_timeout + 2` seconds. Set it
explicitly when the tool and its input rounds need a different command
deadline. On Windows and other non-Unix platforms, constructing a session
with a handler raises `ValueError`.

```python
from session_driver import Session, accept, decline

def handle(request):
    if request.kind == "form" and request.params["schema"] == expected_schema:
        # The host supplies this function to obtain the user's decision.
        approved = ask_user(request.params["message"], request.deadline, request.cancelled)
        if approved and not request.cancelled.is_set():
            return accept({"approve": True})
    return decline()

with Session(server_argv, host_input=handle, request_timeout=60) as session:
    reply = session.call(tool_name, {})
    status = session.next_host_input_status(timeout=1)
```

The handler receives a `HostInputRequest` with `kind` (`form`, `url`, or
`sampling`), opaque `session` and `id` strings, `server` metadata, and
`actions`. Its `params` contains `message` and `schema` for a form,
with optional `credentialFields` listing field names whose input should
be masked. It contains `message` and `url` for a URL or MCP
`CreateMessageParams` for sampling.
Treat those fields as server-supplied data and use an answer helper for the
request kind.

- `accept(content)` with a dictionary of form values.
- `decline()` or `cancel()` for any question.
- `confirm_url()` after the user explicitly confirms completing the
  out-of-band step.
- `sample(result, origin="model")` with an MCP `CreateMessageResult`
  dictionary. For operator-authored content, use `origin="operator"` and
  set the result's model to `mcp-repl/operator`.

Handlers can run concurrently. Use `request.deadline - time.monotonic()`
to bound blocking work, and check the `request.cancelled` event to release
it promptly on withdrawal, timeout, socket EOF, or session shutdown.
Python callbacks must cooperate to exit promptly. The driver sends a
decline for an exception, timeout, invalid answer, or unavailable handler
slot and records a fixed reason in `session.host_input_errors`.
When the REPL's rejection status for a tracked answer arrives with reason
`malformed`, `invalid_payload`, or `wrong_kind`, the driver records
`answer_rejected` and sends one fallback decline. Statuses are best effort,
so if one is dropped the question waits for the REPL's own deadline. When the bridge learns that a question was withdrawn, it discards
the pending handler answer and sends a decline. The REPL can report stale
for those declines and for declines sent at the deadline. Results arriving
after shutdown begins are discarded.

`session.next_host_input_status(timeout=...)` returns a `HostInputStatus`
with `session`, `id`, `status`, and `reason`, or `None` when the wait
expires or the closed bridge has no statuses left. Statuses are best effort.
The status is `accepted` or `rejected`, with reason `duplicate` for a
repeated answer the REPL already received. The driver retains up
to 256 unread statuses and 256 recent `HostInputError` records. Error records
contain only the question identifiers when available and a fixed reason,
such as `handler_exception` or `handler_timeout`. The driver sends answers
over the bridge. Keep them out of host logs and treat ordinary MCP results as
potentially sensitive because the server receives the answers.

## Host-mediated input on Unix

A host can answer elicitation and sampling while an MCP operation is still
running. The Python driver manages the socket when given a
[`host_input` handler](#answering-elicitation-and-sampling-in-python).
To use the bridge directly, pass `--json --host-input-fd FD` before the
command or the raw stdio server argv. Create a connected Unix stream socket
pair, pass only the REPL
endpoint to the child, and close the parent's copy of that endpoint after
spawn. In Python, use `socket.socketpair()` and
`subprocess.Popen(..., pass_fds=(repl_end.fileno(),))`. The descriptor is
owned by mcp-repl and closed on exec of any MCP server subprocess. If using
`--bearer-fd`, supply a separate descriptor.

The socket is full duplex. Stdout keeps its normal command-result NDJSON
framing, and piped stdin still takes REPL commands. The bridge works with
`-e`, generated commands, and a quiet session with piped command stdin.
In bridge mode, a successful `connect` emits one stdout value such as
`{"connected":"example","protocol":"2025-11-25"}`.
`--host-input-timeout SECONDS` sets the per-question timeout (default 30,
range 1 to 120). Set the command's `--timeout` high enough to allow for
the server's work and the expected input rounds. The flag is incompatible
with explicit `--elicitation` and `--sampling` strategies and with plugin
management.

Protocol v1 uses one UTF-8 JSON object followed by LF, with a 1 MiB limit
including the delimiter. `session` is an opaque identifier for the current
connection, and `id` is an opaque string within it. Copy both into every
reply. `server.name` is the peer's claimed name, and `server.protocol` is
the negotiated version. `deadlineUnixMs` is a wall-clock estimate for the
host UI. The REPL enforces the deadline with its monotonic clock.

```json
{"v":1,"type":"needs_input","session":"0123456789abcdef0123456789abcdef-1","id":"1","server":{"name":"example","protocol":"2025-11-25"},"kind":"form","request":{"message":"Proceed?","schema":{"type":"object","properties":{"approve":{"type":"boolean"}},"required":["approve"]}},"actions":["accept","decline","cancel"],"deadlineUnixMs":1800000030000}
{"v":1,"type":"response","session":"0123456789abcdef0123456789abcdef-1","id":"1","action":"accept","content":{"approve":true}}
{"v":1,"type":"response_status","session":"0123456789abcdef0123456789abcdef-1","id":"1","status":"accepted"}
```

- Form requests carry `message`, an MCP form `schema`, and an optional
  `credentialFields` array of field names flagged by a heuristic. Send
  `accept` with typed `content`, or send `decline` or `cancel`.
- URL requests carry `message` and `url`. Use `confirm_url` only after the
  user explicitly confirms completing the out-of-band step. Displaying or
  opening the HTTP(S) link alone does not count. `decline` and `cancel`
  are also available.
- Sampling requests carry MCP `CreateMessageParams`. Send `sample` with
  `origin` set to `model` or `operator` and an MCP `CreateMessageResult`
  in `result`. Set `role` to `assistant`, include the answer in `content`,
  and give the actual model name in `model`. Use `mcp-repl/operator` for
  operator-authored content. Use `decline` or `cancel` to refuse sampling.
- `response_status` is best effort. Hosts must keep working without it.
  Acceptance confirms
  local receipt. Rejection includes a fixed `reason` such as `duplicate`,
  `stale`, `cross_session`, `unknown`, `wrong_kind`, `invalid_payload`,
  `unsupported_version`, or `malformed`. A malformed frame may have no
  identifiers. Re-sending the same answer returns `duplicate` if it was
  consumed and its ID is still remembered. Otherwise the original
  rejection repeats while the request state is the same. For example,
  a reply that was initially invalid can become `stale` after timeout.
- `withdrawn` carries the question identifiers and a fixed `reason`.
  A server cancelling a request gives `server_cancelled`. A lost server
  connection gives `disconnected`. Hosts can answer questions out of order,
  and at most 32 questions can be pending. When `--timeout` or Ctrl-C drops
  a foreground command, stable questions opened since that command began
  are withdrawn with `cancelled`.

Keep draining the control socket while commands run, including while a
user or model is preparing an answer. The session ends with exit status 4
if a socket write makes no progress for `--host-input-timeout` seconds
(30 seconds by default), the channel closes, or an inbound frame is
oversized or truncated. A full 64-frame output queue can refuse new
questions and drop `response_status` and `withdrawn` frames. A dropped
status leaves the answer's disposition unchanged.

Treat server-authored text, schemas, URLs, and sampling messages as data
from that server. Ask for consent in the host before sending answers,
including before invoking a model on the server's behalf. Send sensitive
answers over the socket and keep them out of application logs. Servers
receive answers and can include them in ordinary MCP command results.
History, wire capture, and peer diagnostics are disabled in bridge mode.

Stable server-initiated requests use the bridge directly. For final
`2026-07-28` tool calls, the REPL continues the live operation with the
answers, allowing up to four tool-call attempts including state-only
continuations. Sampling refusal ends a final input round because the
protocol has no sampling error value for a continuation. Automatic HTTP
session recovery and command replay are disabled in bridge mode.

A final server can answer a tool call with a task. When that task parks in
`input_required`, the REPL publishes all of its outstanding requests at
once, then sends the answers with `tasks/update` and keeps waiting. The
same applies to `wait` and to `task <id> respond`. Each wait allows up to
three rounds that ask the host.

The connection keeps one record per task, shared by every command. It holds
the keys the server acknowledged, the keys published to the host or
answered locally, the keys a newer snapshot dropped, and the next poll
slot. Freshness comes from `lastUpdatedAt`: a published key counts as
dropped only when a snapshot whose `lastUpdatedAt` is strictly newer than
the newest one that listed it leaves it out. A `lastUpdatedAt` that is not a
plain RFC 3339 time gives no freshness evidence. That includes a leap
second, a fraction longer than nine digits, a day the month does not have,
and an offset that is not `HH:MM` within range. Acknowledged and dropped
keys are never asked or sent again for that task, so a snapshot that lists
only such keys is polled again. Every read of the task, including `task`,
`jobs`, and `cancel`, starts at least the server's `pollIntervalMs` after
the previous one. A terminal read clears the keys and keeps the poll slot.
A record that holds only a poll slot is removed once the slot passes.
Errors leave the record as it is, and the records end with the connection.

While questions are open, a question whose key is dropped is withdrawn with
`server_cancelled`, and all of them are when the task settles. Before an
update, the task is read again at the next poll slot, and the answers for
the keys that read lists are sent at once. An answer whose key it does not
list is held, without counting the key as dropped. The task is read again
while the deadline allows, and the held answer is sent once a read lists its
key. Answers still held when no slot remains are not sent. Without any such
read before the deadline, nothing is sent and the command reports that the
task is still waiting. A JSON-RPC refusal of the update ends the command, and
its keys can be asked again at once. A transport failure, a `-32000` error
(tower-mcp's form for HTTP failures), or no acknowledgement before
`--timeout` leaves the outcome unknown. The command then ends, names the
task id, and says the update may have been applied. A later `respond` or
`wait` asks again for any key a snapshot still lists. That is the retry, so
check the task before answering twice.

Known limits with a server that misreports task state:

- Without a usable `lastUpdatedAt`, no key is ever counted as dropped, so a
  key the task stopped listing can be asked again. tower-mcp ignores
  answers for keys that are no longer outstanding.
- A server that lists ever more distinct keys grows the task's record until
  the task settles or the connection ends.
- A server whose `lastUpdatedAt` goes backwards, or that reuses a key
  against SEP-2663, can leave a key excluded while the task still waits
  for it. The command then ends at its deadline.

A form or URL `decline` or `cancel` is an answer and is sent. A sampling
refusal, an expired question, a question the full queue could not take, or
a request kind the bridge cannot carry leaves that key outstanding. The
answers that did arrive are still sent, and the command reports an error
that names the remaining keys and the task id. The error has exit status 4,
or 5 when the host refused sampling. The task then stays parked and appears
in `jobs`, so `task <id> respond` can ask again. `--timeout` bounds
`task <id> respond` as one command, including its questions and reads. If
the read after its update would fall past that deadline, the command reports
the snapshot the answers were based on. In bridge mode a bare `wait` reports a
task it could not settle as an entry with `taskId`, `error`, `kind`, and
`exitStatus` in its one array value. Losing the host ends the session
without answering.

A server can copy a submitted value into an error. In bridge mode,
`tasks/get`, `tasks/update`, and `tasks/cancel` errors have their message and
`data` replaced with fixed text. So do the errors of a final tool call after
any attempt carried answers. The same applies to a tool call, `read`, or
`prompt` on either lifecycle once the host answered one of the server's
questions during it. The exit status is decided from the original
error, and the JSON-RPC code is kept. `task`, `jobs`, `cancel`, `wait`, and
`respond` show a task's `error` and the status message of a failed task as
fixed text with the original code, and status messages from task
notifications are not recorded. Completed results are shown as the server
sent them.

Windows IPC and prompt and resource continuations are deferred. The descriptor flag returns a usage error on
non-Unix platforms. In bridge mode, `bench` and background jobs using a
trailing `&` return usage errors. Final-protocol
list-change subscriptions are disabled in bridge mode. Run `refresh` to
update the final protocol's discovered surface. Stable list-change
notifications still refresh the surface automatically.

## Prompts on piped stdin

In a piped REPL session, server elicitation and sampling can read an answer
from stdin only during a foreground tool call, `bench`, `prompt`, `read`, or
`wait`. The REPL declines requests that arrive outside those operations or
are still queued when the last eligible operation ends. `task <id> respond`
reads answers for the task's outstanding questions directly. `-e`, generated
commands, and requests sent to `--host-input-fd` bypass this gate.

The client cannot tell which operation a server callback belongs to. If a
background task asks while a foreground operation can prompt, the callback
may still consume the next input line. Run tools that need answers in the
foreground and avoid overlapping them with background tasks that may ask
for input. If an operation is cancelled or times out after its prompt starts
reading, the prompt still takes the next input line.

## Waiting for background tasks

A tool that requires task-augmented execution has to be started with a
trailing `&`, which returns as soon as the server accepts the work. A script
cannot then name the task: the id is generated by the command that started it,
and the `-e` list is fixed before any of it runs. Exiting there drops the
connection and abandons work the server already began.

`wait` with no argument waits for every task the session started, oldest
first, and `wait last` names the most recent one:

```bash
# Start a task and stay connected until it settles:
mcp-repl -e 'call run.start {"instruction":"..."} &' -e wait -- ./my-server

# Or wait for one specific task without knowing its id:
mcp-repl -e "slow_add a=1 b=2 &" -e "wait last" --demo
```

`last` also works with `task` and `cancel`. A task that failed or was
cancelled sets the process exit status (3 and 6 respectively), so a script can
tell the work failed rather than only that it finished. `--timeout` on `wait`
applies per task.

When that deadline passes while the connection is healthy, the task is still
running: `wait` on it again or `cancel` it. The exit status is 4, as in
earlier releases, but the `--json` envelope has `kind: "timeout"`, the
`taskId`, and `pending: true`, so a script can tell it from a broken
connection without reading the message:

```json
{"error":"task 0123abcd did not settle within 1s (--timeout)","kind":"timeout","exitStatus":4,"taskId":"0123abcd","pending":true}
```

In human mode the same message goes to stderr with a hint:

```console
error: task 0123abcd did not settle within 1s (--timeout)
  hint: the task is still running; `wait` on it again or `cancel` it
```

With `--host-input-fd`, a host can answer a parked task too late: the next
poll slot falls after the deadline, so the answers cannot be checked against
the task and none are sent. `wait` then reports the same `timeout` envelope
with `"answersSent": false`, and the message names `task <id> respond`.

A deadline that interrupts a reconnect after a lost connection is a
connection failure, not a pending task. It has exit status 4 and
`kind: "transport"`, with a message saying the reconnect did not finish
within the `--timeout`.

One case still reports `kind: "transport"` for a healthy connection: in
bridge mode, if the read that checks the host's answers succeeds but finishes
after the deadline, the answers are not sent and the error is a transport
error.

Releases up to 0.5.0 reported every wait deadline as `kind: "transport"` with
a message starting `Transport error:`.

The demo server's `fail` tool is task-capable and always fails, so both halves
of that are visible without a server of your own:

```bash
mcp-repl --demo -e fail                  # tool error, exits 3
mcp-repl --demo -e 'fail &' -e wait      # failed task, also exits 3
```

Note that a failed task still settles as `completed`: the status describes
the task, not the work. What failed is in the result, which is why the exit
status is the reliable thing for a script to check.

In human `--exec` mode the banner and surface listing are suppressed by
default; pass `--verbose` to keep them. With `--json` in one-shot mode or on
piped stdin, startup stays quiet even under `--verbose`. Results are compact
[NDJSON](https://github.com/ndjson/ndjson-spec) values. The driver uses
commands that return one value each. General REPL commands can emit several
values, for example `for` emits results for each item. Timings, tracing,
progress, notifications, reconnect notices, spawned-child diagnostics,
warnings, and the banner, tool policy, and surface summary that a `connect`
prints go to stderr.

Under `--json` a successful `connect` reports its banner, tool policy, and
surface summary on stderr and emits no value on stdout. A failed `connect`
emits the usual error envelope on stdout. This applies with or without a tool
policy. Earlier releases printed the human banner on stdout here, which broke
NDJSON parsing.

Successful protocol operations preserve their MCP result shape: foreground
tool calls return `CallToolResult`, `read` returns `ReadResourceResult`,
`prompt` returns `GetPromptResult`, task commands return `TaskObject`, and a
task-augmented tool call returns its task-creation result. Surface list commands
return convenience arrays of their protocol definitions (without pagination
wrappers). REPL-only commands use documented convenience values or envelopes:
`find` and `subscriptions` return arrays; `describe` returns
`{"kind": ..., "definition": ...}`; `snapshot` returns its canonical contract
or a file acknowledgement; `validate` returns its compatibility report; and
`help`, `bench`, `jobs`, aliases, parameter binds, `wire`, `last`, `refresh`,
`info`, `vars`, `unset`, and `quit` return objects.

JSON errors stay on stdout so they occupy that command's one output line:

```json
{"error":"unknown command: nope","kind":"usage","exitStatus":2}
```

In human mode errors go to **stderr**, because stdout is the data stream:
`mcp-repl -e "get_thing" > out.txt` captures the result or nothing, never the
text explaining why there is no result. Every failure carries the same `error:`
prefix, whichever command produced it, and adds `did you mean ...` when a near
name exists. Process statuses are stable:

| Status | Meaning |
| ---: | --- |
| 0 | success |
| 1 | no-match: the command ran and the thing asked about is not there (`find` matched nothing, `describe`/`bench` named something absent) |
| 2 | local invocation or command usage error, including an unrecognized command word and a tool call refused by the tool policy |
| 3 | server rejection or tool error result |
| 4 | transport or protocol connection failure (including a `--timeout`); also a `wait --timeout` that ran out on a healthy connection, which has `kind: "timeout"` and `pending: true` under `--json` |
| 5 | authentication or authorization failure reported by the server or the transport |
| 6 | the run was interrupted with Ctrl-C |

## Progressive discovery: find, describe, call

A driver with a limited budget, such as an agent's context window, does not
need every schema on a server to make one call. Search first, read the one
definition you chose, then call it:

```bash
mcp-repl --demo --json -e 'find -m 5 convert'   # ranked names and descriptions, no schemas
mcp-repl --demo --json -e 'describe convert'    # the full definition of one tool
mcp-repl --demo --json -e 'convert value=100 from=celsius to=fahrenheit'
```

`find` ranks before `-m` caps the results, so the best matches survive the cap.
When a listing is needed, `tools --brief` (also `prompts`, `resources`,
`templates`) returns only names, titles, and descriptions (URIs for resources
and templates) under `--json` and in captures. `describe <name>` still returns
the full definition. On the demo server:

| Command | stdout bytes |
| --- | ---: |
| `tools` | 5345 |
| `tools --brief` | 1028 |
| `find -m 5 convert` | 101 |
| `describe convert` | 779 |

## Bounded tool output

`--max-output BYTES` caps how much of a tool result is printed. The size is
the byte length of the compact JSON line `--json` would print, without its
newline, in every output mode. A result at or under the limit is printed
unchanged. A larger one is written whole to a new file, and a summary object
is printed in its place:

```bash
mcp-repl --demo --json --max-output 320 --output-dir results -e 'echo message="hello world " repeat=60'
```

```json
{"mcpRepl":"truncated/v1","tool":"echo","isError":false,"bytes":818,"maxOutput":320,"path":"/work/results/0001-echo-ksv08x.json","saveError":null,"content":[{"type":"text","textBytes":779,"preview":"hello world  hello world  hello world  hello world  hello world  hello world  hel"}],"blocksOmitted":0}
```

Without the flag nothing changes. Every summary has these fixed fields:

| Field | Meaning |
| --- | --- |
| `mcpRepl` | `"truncated/v1"`. Test for this key to tell a summary from a `CallToolResult`. |
| `tool` | the tool that was called |
| `isError` | the result's `isError`; a tool error stays an error |
| `bytes` | size of the full result line |
| `maxOutput` | the limit |
| `path` | absolute path of the saved file, or `null` when it could not be written |
| `saveError` | why the file could not be written, or `null` |

Then, only while they fit within the limit, these optional fields, added in
this order:

| Field | Meaning |
| --- | --- |
| `content`, `blocksOmitted` | for a whole tool result: one descriptor per content block, in order, and how many blocks have no descriptor. A descriptor has `type`, plus `textBytes` for text, `mimeType` and `base64Chars` for images and audio, `uri` for resources and links. At most 32 blocks are described. |
| `value` | instead of `content`, for a filtered or captured value: `type` and `bytes`, plus `keyCount` for objects or `length` for arrays |
| `structuredContent`, `meta` | present when the result has them, described like `value` |
| `keys` | inside an object's descriptor: up to 32 of its keys |
| `preview` | inside a text descriptor or a string `value`: the start of the text |

Descriptors stop at the first one that does not fit, so entry `i` always
describes block `i`, and `keyCount` says how many keys there are when
`keys` is shorter. A summary is never larger than the limit when the fixed
fields fit in it. When they do not, the fixed fields alone are printed:
that is the floor, and the only case where a summary exceeds the limit. The
floor is the serialized size of the fixed fields for that result, mostly the
path or the save error.

JSON is never cut: structured content is described, and only text is
previewed, ending on a character boundary. Base64 in a whole result's image,
audio, or blob is never previewed. A filter that selects such a field, for
example `<tool> | [1].data` when the second block is an image, produces a
plain string, and its `value` preview shows the start of the base64 text.

The saved file holds exactly the line that would have been printed, plus a
newline, so reading it is the same as running without the flag. It keeps
every content block, `structuredContent`, `isError`, and `_meta`. Fields the
protocol types do not define are dropped before mcp-repl sees the result, as
they are in default `--json` output.

The limit applies only to foreground tool calls: a tool word, `tool <name>`,
`call`, aliases and `for` bodies that expand to those, and generated
commands. Built-ins, `read`, `prompt`, and `wait` are not limited. A filter
or a capture needs a bare or `tool`-qualified tool name, as it does without
the flag; `call` cannot be filtered or captured. With a path filter,
`<tool> | items`, the selected value is measured and saved, and the summary
describes it under `value`. With a capture, `r = <tool>`, the variable keeps
the whole value and only what is printed is limited, in human mode too. In
human mode the summary prints as a `result truncated:` line, a `[tool error]`
marker when `isError` is true, and the previews.

In one-shot mode (`-e` and generated commands) a tool error still exits 3,
and a result that could not be saved raises the status to at least 2. An
interactive session does not return these statuses when it ends.

Files and permissions:

- `--output-dir DIR` chooses the directory, created owner-only (0700) if
  missing. These are usage errors (exit 2) before connecting: a symbolic link
  (with or without a trailing `/`), a non-directory, a path that is not valid
  UTF-8 (it could not be named in the JSON summary), a directory mcp-repl
  cannot write to, and on unix a directory owned by another account or
  writable by group or others. A directory readable by group or others is
  accepted with a warning on stderr.
- Without `--output-dir`, a new `mcp-repl-out-*` directory (0700) is created
  in the system temp directory when the first result goes over the limit.
- Files are named `<sequence>-<tool>-<random>.json`, with the tool name
  reduced to letters, digits, `_`, and `-`. Each is created new; an existing
  file or symlink at that name is never opened, and another name is tried.
  On unix the file mode is set to 0600, and a created directory to 0700,
  after creation, so the umask does not change them.
- Owner-only modes are a unix guarantee. On Windows, saved files and
  directories inherit the access control list of the directory they are
  created in.
- Threat model: the directory and its parent directories must not be
  replaceable by another account. The ownership and permission checks cover
  the output directory itself, not its parents, so choose a location under a
  directory you control. mcp-repl does not defend against a process running
  as the same user, which could replace the directory between the check and
  a write.
- When a file cannot be written, the summary still prints, with `path: null`
  and `saveError`, and the error goes to stderr. The full result is never
  printed in its place.
- Saved results are server output, unredacted, and can include credentials.
  mcp-repl never deletes them; remove the directory when you are done.

### Tool policy denials

A tool call refused by `allow_tools`, `deny_tools`, `--allow-tool`, or
`--deny-tool` (see [Connecting](connecting.md#restricting-tool-calls)) is
never sent. In one-shot modes (`-e` and generated commands) it sets process
status 2, whichever spelling made it: a generated command, `call`,
`tool <name>`, an alias, `for`, `bench`, or a task-augmented call. Like every
status in the table above, this applies to one-shot runs; at the interactive
prompt the refusal is printed and the session continues. Status 5 stays
reserved for credentials the server or transport rejected, so a script does
not log in again to fix a local policy.

```text
error: tool call `delete_issue` denied by tool policy: in the deny list (profile work); it was not sent
```

Under `--json` the usual envelope gains a `toolPolicy` object. `rule` names
the list that decided (`deny`, or `allow` when an allow list does not name the
tool) and `sources` names where that list came from:

```json
{"error":"tool call `delete_issue` denied by tool policy: in the deny list (profile work); it was not sent","kind":"usage","exitStatus":2,"toolPolicy":{"tool":"delete_issue","rule":"deny","sources":["profile work"],"reason":"in the deny list (profile work)"}}
```

`info` reports the effective policy as `toolPolicy` (`null` when none
applies). `tools` leaves denied tools out of its array and notes how many on
stderr; `tools --all` includes them. `describe` of a denied tool adds a
`toolPolicy` object with `"denied": true`.

## Schema snapshots and compatibility checks

Tool and prompt definitions can be saved as versioned, canonical JSON
contracts. Snapshots intentionally omit descriptions, icons, annotations, and
other presentation metadata: a documentation edit should not break a caller.

```text
demo> snapshot add add.schema.json
saved tool "add" schema snapshot to add.schema.json
demo> validate add.schema.json compatible
tool "add" is compatible under compatible validation
```

Without a path, `snapshot <name>` prints the canonical JSON. Use
`snapshot tool:<name>` or `snapshot prompt:<name>` when both namespaces expose
the same name. `validate <path> [strict|compatible|ignore]` reads a snapshot
and compares it with the advertised surface without invoking anything:

- `strict` requires the entire canonical contract to match.
- `compatible` protects existing callers: removed or retyped inputs, newly
  required inputs, removed or retyped expected outputs, and prompt argument
  breaks fail. Additive optional inputs/arguments and additive outputs pass;
  input widening and output narrowing (for example, integer output replacing
  number output) also pass.
- `ignore` loads the snapshot but deliberately skips enforcement, which is
  useful while rolling out contracts in automation.

Nested object/array schemas and local JSON Schema references such as
`#/$defs/filter` are followed recursively. External references are rejected
because validation is offline and must not fetch code or schemas implicitly.
Changes to complex `anyOf`, `oneOf`, `allOf`, or `not` compositions are treated
conservatively as incompatible.

Repeat `--schema-contract <path>` to enforce snapshots before matching tool
calls, task-augmented calls, benchmarks, or prompt retrievals. The default is
compatible mode; `--schema-mode strict|compatible|ignore` changes it:

```bash
mcp-repl --schema-contract add.schema.json \
  --schema-mode compatible --http https://example/mcp
```

A successful preflight is silent. An incompatible preflight sends no MCP
request, returns status 1, and explains every finding. Under `--json` the
validation report is the command's single NDJSON value, so the scripting
framing contract is preserved.
