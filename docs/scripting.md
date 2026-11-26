# Scripting

[mcp-repl](../README.md) · [Connecting](connecting.md) · [Commands](commands.md) · [Scripting](scripting.md) · [Debugging](debugging.md)


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
progress, notifications, reconnect notices, spawned-child diagnostics, and
warnings go to stderr.

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
| 2 | local invocation or command usage error, including an unrecognized command word |
| 3 | server rejection or tool error result |
| 4 | transport or protocol connection failure (including a `--timeout`) |
| 5 | authentication or authorization failure |
| 6 | the run was interrupted with Ctrl-C |

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
