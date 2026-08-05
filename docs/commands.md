# The command set

[mcp-repl](../README.md) · [Connecting](connecting.md) · [Commands](commands.md) · [Scripting](scripting.md) · [Debugging](debugging.md)


The server's surface is the command set. This is everything the REPL adds
on top of it.

## What to try

`--demo` needs no server at all, and its tools are typed so the features
that depend on a schema are all reachable:

```text
mcp-repl-demo> help                        # built-ins plus the server's tools
mcp-repl-demo> help wait                   # what one built-in does
mcp-repl-demo> echo <Tab>                  # completes `message=` and `repeat=`, with types
mcp-repl-demo> echo message="hi there"     # args coerced by inputSchema
mcp-repl-demo> echo msg=hi                 # refused: `message` is required
mcp-repl-demo> convert value=100 from=celsius to=<Tab>  # completes the enum values
mcp-repl-demo> slow_add a=2 b=3 &          # runs task-augmented; `jobs`, `wait 1`
mcp-repl-demo> sign_in                     # the server asks *you* (elicitation)
mcp-repl-demo> find note                   # keyword search across the surface
mcp-repl-demo> describe convert            # input/output schemas, colored
mcp-repl-demo> read note://ideas           # a resource template, completed by the server
mcp-repl-demo> prompt greet name=<Tab>     # prompt args complete via completion/complete
mcp-repl-demo> info                        # identity, instructions, counts, capabilities
```

The demo runs the server in this process over an in-memory pipe rather than
a socket or a child process, so it starts instantly and opens nothing. It
speaks the same full-duplex framing a spawned stdio server does, which is
what lets a demo tool call back to ask you a question.

Single or double quotes group whitespace into one argument, and the REPL
removes those grouping quotes before schema coercion. A backslash escapes the
next character outside single quotes. JSON object and array arguments retain
their JSON quotes and spaces exactly, including when passed through `call`:

```text
mcp-repl-demo> echo message="hello world"
mcp-repl-demo> call echo {"message": "hello world"}
```

A line that is merely unfinished keeps the editor reading instead of failing:
pasting a pretty-printed JSON body works, and the `::: ` continuation prompt
shows while the quotes or braces are still open. A delimiter that can never
match (`{1]`) is reported at once rather than waiting for input that would not
help. An unmatched quote, trailing escape, or unclosed JSON argument in
`--exec` is reported locally without calling the server. A quoted or escaped `&` is ordinary input;
only a plain trailing `&` requests task-augmented execution.

Tool listings and the completion menu carry the server's safety annotations,
so what a tool does to the world is visible while choosing it rather than one
`describe` later:

```text
cratesio> tools
get_crate_info           Get detailed crate information [read-only idempotent]
publish_crate            Publish a crate [destructive open-world]
slow_add                 Add two numbers, slowly [task-capable]
```

A server that declares no annotations gets no tags.

`ping` checks the server is answering, which is the smallest possible health
check for a script:

```bash
mcp-repl --http https://example/mcp -e ping
```

A long surface is trimmed to the terminal window rather than scrolling the
prompt away, with the escape hatch named on the last line:

```text
cratesio> tools
...
... 80 more of 100; `tools --full` shows everything
```

`--full` prints every row. Truncation is interactive-only: `--exec` and
`--json` output is a data stream and is never trimmed.

`history` lists recent commands from previous sessions, and Ctrl-R searches
them. History lives at `$XDG_STATE_HOME/mcp-repl/history` (falling back to
`~/.local/state/mcp-repl/history`), owner-readable only, holding the last
1000 lines. Set `[repl] history_capacity` in the config file to change that,
or `--no-history` to keep it in memory. A pre-0.3 `~/.mcp-repl_history` is
moved to the new location once, automatically.

`read <uri> --out <path>` writes the content to a file instead of printing
it, decoding a binary resource back to bytes:

```text
mcp-repl-demo> read img://pixel
[binary 96 base64 chars]
mcp-repl-demo> read img://pixel --out pixel.png
wrote 70 bytes to pixel.png
```

The file is created owner-only, since whatever a server serves is as
sensitive as the server. An existing file is refused unless `--force` is
given, and a resource that came back as several contents is refused rather
than concatenated, because joining them produces a file that is none of
them. Under `--json` the command reports `{"uri", "path", "bytes"}`.

`resources` lists concrete resources and `templates` lists parameterized
(`{variable}`) ones; each points at the other so a server that splits its
resources across the two MCP lists is not confusing.

Task-capable tools support shell-style backgrounding (SEP-2663):

```text
demo> slow_add a=2 b=3 &
[task 1 (105e63bf...)] started
[task 1 (105e63bf...)] completed  run `task 1` for details
demo> wait 1
task 1 (105e63bf...)  status=completed  Task completed
5
```

Task ids are opaque strings chosen by the server, so the REPL also gives each
task a small number for this session. `task`, `wait`, and `cancel` accept the
number, `last` for the most recently started task, the full id, or an
unambiguous prefix of it. `wait` with no argument waits for every task the
session started, oldest first, which is how an `--exec` script waits for work
whose id it never saw; see [scripting](scripting.md).

A task that needs an answer from the operator parks in `input_required`, and
`task <id> respond` is what moves it forward: it asks whatever the task is
waiting for and hands the answers back, after which the handler resumes.

```text
demo> sign_in &
[task 1 (9587890f...)] started
[task 1 (9587890f...)] input_required — Awaiting client input  run `task 1 respond` to answer
demo> task 1 respond
[elicit] server mcp-repl-demo is asking:
  The demo server would like to know who you are.
  username (string, required) Any name will do
  username> ada
[task 1 (9587890f...)] completed — Task completed
signed in as ada
```

This needs `--protocol 2026-07-28`, the only lifecycle where a task reports
what it is waiting for. On the stable lifecycle a server asks by sending
`elicitation/create` itself, which is declined while the editor holds the
terminal, so run such a tool in the foreground rather than as a task.

The REPL tracks only tasks it started and consumes both legacy and final typed
task-status notifications, deduplicating repeated transitions. A final client
opens a task-scoped `subscriptions/listen` stream; a bounded per-task poller
remains authoritative for stable servers and for unavailable or dropped final
notifications. It honors the server's suggested interval, ends at a terminal
state, and gives up after three consecutive read failures. `jobs`, `task`,
`wait`, and `cancel` remain the authoritative manual controls.

Automatic transition lines are interactive-only. `--exec` and `--json`
suppress them for deterministic scripted output; explicit task commands still
return their normal text or JSON results.

Progress and log notifications print inline as they arrive:

```text
mcp-repl-demo> scan steps=4
[progress 25%] scanned 1 of 4
[progress 50%] scanned 2 of 4
[progress 75%] scanned 3 of 4
[progress 100%] scanned 4 of 4
scanned 4 items
```

A server only sends progress when the client asks for it, so mcp-repl
attaches a progress token to every request it issues. `list_changed`
notifications refresh the command table mid-session, so dynamic servers grow
and shrink the REPL's vocabulary live. Stable connections receive those notifications on
their ordinary transport. An interactive final connection opens one
`subscriptions/listen` stream for tool, prompt, and resource list changes
after its initial surface fetch, validates the server's acknowledged subset,
and reopens the stream after a reconnect or unexpected ending with bounded
backoff. `--exec` never opens this background stream, preserving deterministic
one-shot output.

For a spawned stdio server, child diagnostics remain visible but are read from
the child's stderr and passed through reedline's external printer. Logs that
arrive while you are typing therefore appear above a cleanly redrawn prompt
instead of splitting the current input. In `--exec` mode they remain on stderr,
so `--json` stdout contains only command results.

## Completion

Tab opens a columnar menu. What gets completed:

- The command word: built-ins, aliases (shown with what they expand to), and
  every tool, each with its description.
- Tool argument names from the tool's `inputSchema` properties (with type,
  required flag, and description), and enum values after `key=` when the
  property declares an `enum`.
- `read <uri>`: resource URIs and template URI templates. When the partial
  reaches a template's `{variable}`, the server's `completion/complete` is
  asked to complete the variable (2s timeout, best-effort). Try
  `read note://<Tab>` in `--demo`.
- `prompt <name> <arg>=`: argument values via `completion/complete`, and
  argument names from the prompt definition.
- `describe <name>`: everything on the surface, labeled by kind.
- `bench <tool> ...`: tool names in the first position, then that tool's
  argument names, and `--n` / `--concurrency` after a leading `-`.
- `unalias <name>`: the aliases in effect, with their scope.

## find

A server with dozens of tools is not navigable by listing it. `find
<keyword>` searches names and descriptions across tools, prompts, resources,
and templates, grouped by kind:

```text
cratesio> find download
tools:
  get_downloads            Get download statistics
  get_version_downloads    Daily download stats for a specific version
2 matches
```

`find` searches the REPL's own commands too, so `find alias` reaches the
alias built-in rather than reporting that the server has nothing by that name.
`describe <built-in>` explains one the same way `help <built-in>` does.

Flags follow grep's, since the exit status already does:

```text
cratesio> find --tools -m 3 download      # tools only, best three
cratesio> find --case-sensitive Crate     # stop folding case
cratesio> find --builtins alias           # the REPL's own commands
```

`--tools`, `--prompts`, `--resources`, `--templates`, and `--builtins` can be
combined; several narrow to the union of those kinds. `-m N` (also `-mN`,
`--max N`, `--max=N`) caps the results after ranking, so the best survive.

Matching is case-insensitive unless `--case-sensitive` says otherwise. Results rank an exact name match first, then a
name prefix, then a name substring, then a description match, and last a
subsequence (`gvd` reaches `get_version_downloads`) so a loose match never
buries a literal one. The search runs against the cached surface, so it
issues no request.

Under `--json` it prints an array of `{kind, name, description, score}`
objects. A search that matched nothing exits non-zero, following grep.

A mistyped command word gets the nearest built-in, tool, or prompt name by
edit distance:

```text
cratesio> serch_crates query=serde
unknown command: serch_crates; did you mean `search_crates`?
```

The tolerance scales with the length of what you typed, so a short word does
not collect a suggestion from across the surface. When nothing is close
enough, the message points at `help` as before.

## describe

`describe <name>` looks up a tool, prompt, resource, or template by name:

- Tools: behavior hints, task support, the input/output schemas as
  syntax-colored JSON, and an `example:` line showing what to type.
- Prompts: the argument table (name, required/optional, description).
- Resources and templates: URI, name, MIME type, size, and description.

## Aliases

Frequent commands get short names, kept in the same config file as the
profiles:

```text
cratesio> alias dl=get_downloads
dl = get_downloads  (profile cratesio)
cratesio> dl crate=serde
...
cratesio> alias
dl  get_downloads  (profile cratesio)
t   tools          (global)
cratesio> unalias dl
removed dl (profile cratesio)
```

- `alias` lists what is in effect, `alias <name>` shows one, `alias
  <name>=<expansion>` defines, and `unalias <name>` removes.
- Expansion is a literal substitution of the first word with whatever
  followed the alias appended: with `dl = "get_downloads"`, `dl crate=serde`
  runs `get_downloads crate=serde`. An expansion that itself starts with an
  alias expands again; a cycle is reported rather than looped.
- An expansion can end in `&`, so an alias can run its tool task-augmented.
- Scope: an alias defined while connected through a profile belongs to that
  profile; otherwise it is global. `alias --global <name>=<expansion>` forces
  the file-level table. A profile alias shadows a global one of the same
  name, and `unalias` removes the definition that is actually in effect
  (`--global` reaches past a profile alias to the global one).
- Aliases cannot be named after a built-in, since expansion happens before
  dispatch and the built-in would become unreachable. An alias that shadows a
  *tool* is allowed, and says so when defined.
- Every change is written back to the config file through `toml_edit`, so
  comments, key order, and formatting elsewhere in the file survive. Removing
  the last alias leaves the (now empty) table, because a comment above
  `[aliases]` belongs to that table and would go with it.

```toml
[aliases]                       # every server
t = "tools"

[servers.cratesio.aliases]      # only through this profile
dl = "get_downloads"
```

With no config file location at all (no `$HOME`, no `--config`), aliases
still work for the session and the REPL says they were not saved.

## Capture and filtering

The REPL is a small shell. A command's result can be captured into a variable,
referenced in later arguments, or filtered inline.

```text
demo> x = search_crates query=serde
$x = {2 fields}
demo> get_crate_info name=$x.crates[0].name
...
demo> get_crate_info name=serde | crates[0].downloads
11897234
```

- **Capture:** `name = <command>` binds the command's result to `$name`. The
  spaces around `=` distinguish it from a `k=v` argument and from `alias name=...`.
- **Reference:** `$name` and `$name.path[i].field` expand in later command
  arguments before the command runs.
- **Filter:** `<command> | <path>` prints just the selected value. A scalar
  prints bare; an object or array prints as JSON.
- **Paths** are a small selector: `.field`, `[index]`, chained (`crates[0].name`).
  An undefined variable or a missing path is an error, so a typo fails an `-e`
  chain rather than passing silently. JMESPath is a possible future addition.
- **`vars`** lists what is bound; **`unset <name>`** clears one. Variables live
  for the session, so they persist across an `-e` chain:

```sh
mcp-repl -e "x = search_crates query=serde" \
         -e "get_crate_info name=\$x.crates[0].name" <server>
```

Capture and filtering act on tool calls and on the built-ins that return a
documented value: `tools`, `prompts`, `resources`, `templates`, `describe`,
`read`, `find`, and `info`.

```text
demo> t = tools
$t = [3 items]
demo> tools | [0].name
about
```

A command that reports rather than returning a value (`help`, `alias`,
`wire`, `refresh`, ...) refuses the request instead of ignoring it:

```text
demo> x = help
error: cannot capture the result of `help`: it reports rather than returning a value.
```

That matters most in an `-e` chain, where a silently dropped capture would
surface much later as an undefined `$name`.

## Resource subscriptions

A server that supports `resources.subscribe` will push
`notifications/resources/updated` for the resources you ask about. The REPL
prints those inline, the way progress and log lines arrive:

```text
demo> subscribe note://status
subscribed note://status
demo> subscriptions
note://status
[resource updated] note://status
demo> unsubscribe note://status
unsubscribed note://status
```

- `subscribe <uri>` and `unsubscribe <uri>` complete from the surface and
  from what is actually subscribed, respectively.
- The local set is only updated once the server agrees, so `subscriptions`
  lists what the server is sending updates for, not what was asked for.
  Re-subscribing to something already held says so rather than double-counting.
- A server that does not advertise `resources.subscribe` gets a warning before
  the request goes out, so the rejection is explained rather than bare.
- An update for something this session did not subscribe to is still printed,
  tagged `(not subscribed here)`.
- The resource is not re-read on an update: reading may be expensive, and the
  point is to know it moved. Follow with `read <uri>` when you want the content.

## Elicitation

Tools that request user input via `elicitation/create` prompt for each
field at the terminal during a foreground call: the field's type, default,
and description are shown, empty input accepts the default, and EOF
cancels. Fields are asked in the order the server declared them, which is
part of the schema rather than incidental. Try `sign_in` in `--demo`. If a
background task elicits while the editor owns the terminal, the request is
declined rather than fighting the editor for stdin; on the 2026-07-28
lifecycle the question is parked instead, and `task <id> respond` asks it.

Everything in a form comes from the server: the message, the field names,
and their descriptions. The REPL is only the terminal it arrives at, so:

- every request leads with a line naming the server that sent it, keeping
  server-authored text distinct from the REPL's own
- a field whose name looks like a credential (`api_key`, `github_token`,
  `password`) is flagged before the value is typed
- answers are read straight from stdin rather than through the editor, so
  they never enter the command history
- `--elicitation decline` refuses every request. `--exec` defaults to
  `decline`, since a script has nobody to answer a form; pass
  `--elicitation prompt` to answer one anyway

How a server reaches the operator depends on the lifecycle, and mcp-repl
answers both the same way. On the stable lifecycle the server sends
`elicitation/create` directly. The 2026-07-28 lifecycle has no
server-initiated requests at all: the server returns an input-required
result carrying the question, mcp-repl answers it, and the call is retried
automatically (SEP-2322). Either way the prompt looks the same, and
`--elicitation decline` refuses both.

Prompts and their echoes go to stderr, so `--json` stdout stays one value
per command even while you are answering one.

A server can also elicit by asking the operator to visit a URL. Only
`http` and `https` links are shown, and accepting is an explicit `y`:
answering yes tells the server an out-of-band flow was completed, which
is a claim only the operator can make.

## Output rendering

- JSON output (schema dumps, `info` capabilities, non-text content) is
  pretty-printed with a small built-in syntax colorizer.
- Text content that looks like markdown gets a light terminal rendering:
  bold headings, dimmed code fences, styled inline code and bold spans,
  colored bullets.
- Progress, log, and task lines are tagged with dim brackets; task statuses
  are colored (working=yellow, completed=green, failed/cancelled=red).
- Every tool call, `read`, and `prompt` prints a dimmed `[142ms]` / `[1.23s]`
  annotation with the round-trip time, so a slow (or timing-out) call is
  visible at a glance.

All styling degrades to plain text when `NO_COLOR` is set or stdout is not
a terminal. `--color always|never|auto` overrides the detection.
