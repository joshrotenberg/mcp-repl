# mcp-repl

[![crates.io](https://img.shields.io/crates/v/mcp-repl.svg)](https://crates.io/crates/mcp-repl)
[![docs.rs](https://img.shields.io/docsrs/mcp-repl)](https://docs.rs/mcp-repl)
[![CI](https://github.com/joshrotenberg/mcp-repl/actions/workflows/ci.yml/badge.svg)](https://github.com/joshrotenberg/mcp-repl/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/mcp-repl.svg)](#license)

An interactive terminal REPL for any [MCP](https://modelcontextprotocol.io)
server. The server's surface *is* the command set: every tool becomes a
top-level command with schema-coerced `key=value` arguments, prompts and
resources get built-ins, tab completion is powered by the server itself where
the protocol allows, and the command table refreshes live when the server's
surface changes.

It also runs a single command and exits, so the same binary is a scriptable
MCP client for a shell script, a CI job, or an agent.

![mcp-repl starting disconnected, connecting to the demo server, completing schema-driven arguments, reporting progress, and running a tool as a background task](https://raw.githubusercontent.com/joshrotenberg/mcp-repl/main/docs/media/hero.gif)

## Install

On macOS or Linux, install the latest release with Homebrew:

```bash
brew install joshrotenberg/brew/mcp-repl
```

Or, with Rust 1.90.0 or newer, install from crates.io on any supported
platform:

```bash
cargo install --locked mcp-repl
```

Prebuilt macOS, Linux, and Windows archives are also attached to the
[latest release](https://github.com/joshrotenberg/mcp-repl/releases/latest).
Download the archive for your platform, extract it, and place `mcp-repl` (or
`mcp-repl.exe`) on `PATH`.

Or run it without installing anything:

```bash
docker run --rm -it ghcr.io/joshrotenberg/mcp-repl --demo
```

The image suits `--http` and `--demo`; a stdio server would have to live
inside it, and OAuth has no credential store there. See
[the connecting guide](https://github.com/joshrotenberg/mcp-repl/blob/main/docs/connecting.md#in-a-container)
for what differs.

Completions and a man page come from the binary itself. The man page includes
both startup options and the same REPL built-in reference shown by
`help <command>`:

```bash
mcp-repl --completions zsh > ~/.zfunc/_mcp-repl
mcp-repl --man > /usr/local/share/man/man1/mcp-repl.1
```

Every release also publishes SHA-256 checksums, an SBOM, GitHub artifact
attestations, and BuildKit provenance and SBOM attestations for the container.
See [Verify a downloaded release](https://github.com/joshrotenberg/mcp-repl/blob/main/docs/releases.md#verify-a-downloaded-release)
for verification commands.

## Start here

Run the bundled demo first. It needs no config, credentials, network, or
separate MCP server:

```bash
mcp-repl --demo
```

Then take a short tour of the live command set:

```text
mcp-repl-demo> help
mcp-repl-demo> echo message="hello"
mcp-repl-demo> convert value=100 from=celsius to=fahrenheit
mcp-repl-demo> describe convert
mcp-repl-demo> read note://ideas
```

`help` lists every built-in and server tool. `help <command>` gives usage and
runnable examples, while `find <word>` searches tools, prompts, resources,
templates, and built-ins. Tab completes command names, schema-derived argument
names, and enum values.

A bare invocation starts disconnected if you would rather choose or switch
servers from inside the prompt:

```text
$ mcp-repl
mcp-repl> connect demo
```

`connect` also accepts an HTTP URL, saved profile, imported
`path.json:entry`, or stdio command. Run it again to switch servers without
losing command history or global aliases. Captured variables, parameter
defaults, background task ids, resource subscriptions, and profile-scoped
aliases are cleared because they belong to the server being left.

Then point it at something real. mcp-repl speaks stdio and streamable HTTP,
reads the `.mcp.json` files other clients use, and can keep named profiles of
its own:

```bash
mcp-repl --http https://cratesio-mcp.fly.dev/   # a public server to try
mcp-repl -- ./my-server --stdio                 # spawn a stdio server
mcp-repl --scan                                 # what other clients have configured
mcp-repl .mcp.json:local                        # one entry from a client config
mcp-repl --server prod                          # a saved profile
```

If a tool and built-in share a name, the bare spelling is rejected as
ambiguous; `tool <name>` selects the server tool and `builtin <name>` selects
the REPL command.

See [the connecting guide](https://github.com/joshrotenberg/mcp-repl/blob/main/docs/connecting.md)
for auth, OAuth, profiles, and config imports.

## One call, no prompt

The prompt is one of two ways to use this. `-e` runs a command and exits:

```console
$ mcp-repl --demo -e 'convert value=100 from=celsius to=fahrenheit'
212.00
[0ms]
```

`--json` makes stdout [NDJSON](https://github.com/ndjson/ndjson-spec), one
value per command, and silences the banner and the timing line. Each value is
the tool result as the protocol returns it:

```console
$ mcp-repl --demo --json -e 'convert value=100 from=celsius to=fahrenheit'
{"content":[{"type":"text","text":"212.00"}]}

$ mcp-repl --demo --json -e 'convert value=100 from=celsius to=fahrenheit' \
    | jq -r '.content[0].text'
212.00
```

### Experimental generated CLI

The `unstable-dynamic-cli` Cargo feature tries a more conventional one-shot
form while its command grammar is evaluated. It is off by default:

```bash
cargo install --git https://github.com/joshrotenberg/mcp-repl \
  --features unstable-dynamic-cli
```

This feature has not reached a crates.io release yet, so it currently needs to
be installed from Git.

With the feature enabled and an explicit connection selector, mcp-repl
discovers the server first and projects the REPL's one-shot vocabulary into a
CLI. Tool and prompt schemas become flags; resources keep the same
`read <uri>` shape they have at the prompt:

```console
$ mcp-repl --demo echo --help
Echo a message back

Usage: mcp-repl <connection options> echo [OPTIONS] --message <STRING>

Options:
      --message <STRING>  The text to echo back.
      --repeat <INTEGER>  How many times to repeat it.
  -h, --help              Print help

$ mcp-repl --demo convert --value=100 --from=celsius --to=fahrenheit
212.00
[0ms]

$ mcp-repl --demo read note://status
all quiet on the demo server
[0ms]
```

`mcp-repl --demo tool --help` lists the generated tools; `tool <name>` and
`builtin <name>` explicitly select the same namespaces they do in the REPL.
The shorter bare tool name works whenever it does not collide with a built-in.
Listings, `read`, `prompt`, `call`, `find`, `describe`, `snapshot`, and
`validate` are also available directly.

This form works with `--demo`, `--http`, and `--server`. A raw stdio child has
no reliable boundary between its own arguments and a following command, so use
`-e` there. `-e` also remains the way to run several commands or stateful REPL
workflows against one session.

**A one-shot run cannot block waiting for a person.** Under `-e` or an
experimental generated command, elicitation and sampling both default to
`decline`, so a server that asks a question gets an immediate refusal instead
of hanging the caller. `--timeout` bounds everything else, and exit statuses
are typed, so a tool error (3) is
distinguishable from an empty result.

See [the scripting guide](https://github.com/joshrotenberg/mcp-repl/blob/main/docs/scripting.md)
for tasks, waiting on them, and
schema contracts.

## What it is good at

**Everything a tool declares, at the prompt.** Tab completes command names,
argument names, and enum values, reading them out of the tool's
`inputSchema` and following `$ref`s into `$defs` the way a generated schema is
shaped. `describe` shows the annotations, the schema, and a line you can copy
and run.

![describe echo, showing the tool's annotations, its input schema with per-field descriptions, and an example invocation](https://raw.githubusercontent.com/joshrotenberg/mcp-repl/main/docs/media/describe.png)

**The parts of MCP other clients skip.** Server-driven completion
(`completion/complete`), elicitation, and sampling all work here, and
SEP-2663 tasks run with a shell-style trailing `&`. When a server asks the
operator a question, the REPL says which server is asking and flags fields
whose names look like credentials before anything is typed. `--demo` has a
tool for each: `sign_in` asks you a question, `summarize` asks your client's
model for a completion.

![the demo server eliciting a username, an environment, and a boolean, each labelled with its declared type](https://raw.githubusercontent.com/joshrotenberg/mcp-repl/main/docs/media/elicitation.png)

**A shell, not just a viewer.** Capture a result into a variable, reference it
in a later command, filter it with a path, alias a command you type often, and
set parameter defaults shared by several calls. Time a tool with `bench`, trace
redacted JSON-RPC frames with `wire on`, and use `last` to reprint the previous
exchange whether or not tracing was on.

**Scriptable.** `-e/--exec` runs commands and exits; `--json` makes stdout
[NDJSON](https://github.com/ndjson/ndjson-spec), one value per command, with
typed exit statuses so a failure is distinguishable from an empty result.

![two JSON invocations piped through jq, followed by a schema violation exiting with status 3](https://raw.githubusercontent.com/joshrotenberg/mcp-repl/main/docs/media/scripting.png)

## The shell model

mcp-repl is a shell whose command set is the connected server. That is the
organizing idea, and most of the grammar arrived by following it rather than
by deciding it:

| mcp-repl | the shell equivalent |
| --- | --- |
| `name = command`, then `$name.path[0].field` | variables |
| `command \| path` | a pipe into a filter |
| `command &`, `jobs`, `wait`, `cancel` | job control |
| `alias`, `unalias` | aliases |
| `bind key=value`, `binds`, `unbind` | per-connection parameter defaults |
| `for $var in $list: command` | `for` |
| `tool <name>`, `builtin <name>` | `command` and `builtin`, for shadowed names |
| `history`, Ctrl-R | history |
| `-e` with typed exit statuses | a non-interactive shell |

Naming the model is useful because it settles scope questions in advance
rather than one at a time. Things that are shell constructs fit: `&&` and
`||`, redirecting output to a file, reading commands from a file, a richer
path selector. Things that are programming-language constructs do not:
functions, arithmetic, `if`, string manipulation, an embedded scripting
language.

That boundary is not a limitation to be worked around later. A shell's own
answer to a task that outgrows it is to hand off to a real language, and here
that language already exists: an MCP SDK in Python or TypeScript, called from
a real program. Growing a second one inside this prompt would cost a whole
ecosystem to arrive somewhere worse.

The known exception is small and worth stating. An external script cannot
hand a value back to a live prompt you are sitting at, so anything whose
entire point is interactive continuity belongs here rather than in a script.
Nothing else does.

`for` is the worked example. It ships with no conditional form on purpose:
iteration is a shell construct, but `if` is what would force expressions,
comparison, and truthiness into the language. When the need is "only the
high-priority ones," the answer is a more capable path selector, which is
bounded, rather than a test inside the loop, which is not.

## Documentation

| | |
| --- | --- |
| [Connecting](https://github.com/joshrotenberg/mcp-repl/blob/main/docs/connecting.md) | transports, bearer and OAuth auth, profiles, importing client configs, reconnecting, timeouts |
| [The command set](https://github.com/joshrotenberg/mcp-repl/blob/main/docs/commands.md) | every built-in, completion, aliases, capture and filtering, subscriptions, elicitation, output rendering |
| [Scripting](https://github.com/joshrotenberg/mcp-repl/blob/main/docs/scripting.md) | `--exec`, NDJSON, exit statuses, schema contracts |
| [Debugging a server](https://github.com/joshrotenberg/mcp-repl/blob/main/docs/debugging.md) | wire tracing, `last`, `bench` |
| [Release pipeline](https://github.com/joshrotenberg/mcp-repl/blob/main/docs/releases.md) | maintainer gates, native artifacts, failure and retry behavior |

## Other MCP clients

mcp-repl is an interactive shell for driving one server at a time, with
`connect` switching the live target without leaving the prompt. The neighbours
are shaped differently, and which one fits depends on what you are doing:

| | | |
| --- | --- | --- |
| [mcpc](https://github.com/apify/mcpc) | TypeScript | A shell-oriented client built around named sessions that persist across invocations, with broad protocol and OAuth support for automation and agents. |
| [MCP Inspector](https://github.com/modelcontextprotocol/inspector) | TypeScript | The official developer tool, with web, scriptable CLI, and interactive TUI surfaces backed by one client core. |
| [mcp-probe](https://github.com/conikeec/mcp-probe) | Rust | A ratatui debugging dashboard for interactive execution, protocol analysis, validation, and timing. |
| [mcptools](https://github.com/f/mcptools) | Go | A broader toolkit with one-shot commands, an interactive shell and web UI, plus mock, proxy, and guard modes. |

mcp-repl's particular shape is the line editor: the connected server's live
tool surface becomes top-level commands with schema-driven completion and
coercion. The same prompt handles prompts, resources, capture and filtering,
aliases, elicitation, sampling, server-driven completion, and SEP-2663 tasks;
the clients above overlap with different parts of that protocol surface but
organize the workflow differently.

## Protocol versions

The binary compiles both the stable and the final lifecycle, and the choice is
explicit. `--protocol stable` (the default) uses
`initialize`/`notifications/initialized`; `--protocol 2026-07-28` (alias
`final`) uses `server/discover` and sends the selected protocol metadata on
every request. Keeping stable as the default means upgrading mcp-repl cannot
silently change how it talks to a server you already use.

## Contributing

Bug reports and pull requests are welcome. `cargo test` runs the unit tests
plus a black-box suite that launches the built binary against a fixture server
over stdio and localhost HTTP, on both lifecycles. See
[the contributing guide](https://github.com/joshrotenberg/mcp-repl/blob/main/CONTRIBUTING.md).

The recordings above are generated with
[vhs](https://github.com/charmbracelet/vhs) from the tapes in
[the recording tapes](https://github.com/joshrotenberg/mcp-repl/tree/main/docs/tapes):

Build the release binary with `cargo build --release`, then run the desired
tape with `vhs docs/tapes/hero.tape` (or another tape from that directory).

## License

MIT or Apache-2.0, at your option.
