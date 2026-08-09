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

![mcp-repl starting disconnected, connecting to the demo server, completing schema-driven arguments, reporting progress, and running a tool as a background task](https://raw.githubusercontent.com/joshrotenberg/mcp-repl/main/docs/media/hero.gif)

## Install

On macOS or Linux, install the prebuilt x86_64 or arm64 binary with:

```bash
curl -fsSL https://raw.githubusercontent.com/joshrotenberg/mcp-repl/main/install.sh | sh
```

It verifies the release's published checksum before unpacking, and installs
to `~/.local/bin` unless `MCP_REPL_INSTALL_DIR` says otherwise. Reading a
script before piping it to a shell is the better habit, and this one is
short. On Windows, download the `x86_64-pc-windows-msvc.zip` from the
[latest release](https://github.com/joshrotenberg/mcp-repl/releases/latest),
extract `mcp-repl.exe`, and place it on `PATH`.

Or install from source on any supported platform:

```bash
cargo install mcp-repl
```

Completions and a man page come from the binary itself, and ship in the
release archives. The man page includes both startup options and the same
REPL built-in reference shown by `help <command>`:

```bash
mcp-repl --completions zsh > ~/.zfunc/_mcp-repl
mcp-repl --man > /usr/local/share/man/man1/mcp-repl.1
```

## Start here

Open the REPL first and choose a server from inside it:

```bash
mcp-repl
```

```text
mcp-repl> connect demo
```

`connect` also accepts an HTTP URL, saved profile, imported
`path.json:entry`, or stdio command. Run it again to switch servers without
losing command history or global aliases. Captured variables, background task
ids, resource subscriptions, and profile-scoped aliases are cleared because
they belong to the server being left. The original direct forms remain useful
for scripts and one-server sessions:

```bash
mcp-repl --demo
```

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

Inside, `help` lists the built-ins, `help <command>` gives usage, detail, and
runnable examples, and `find <word>` searches the server's surface *and* the
REPL's own commands.
If a tool and built-in share a name, the bare spelling is rejected as
ambiguous; `tool <name>` selects the server tool and `builtin <name>` selects
the REPL command.

See [docs/connecting.md](docs/connecting.md) for auth, OAuth, profiles, and
config imports.

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
time a tool with `bench`. `wire on` traces redacted JSON-RPC frames to stderr,
and `last` reprints the previous exchange whether or not tracing was on.

**Scriptable.** `-e/--exec` runs commands and exits; `--json` makes stdout
[NDJSON](https://github.com/ndjson/ndjson-spec), one value per command, with
typed exit statuses so a failure is distinguishable from an empty result.

![two JSON invocations piped through jq, followed by a schema violation exiting with status 3](https://raw.githubusercontent.com/joshrotenberg/mcp-repl/main/docs/media/scripting.png)

## Documentation

| | |
| --- | --- |
| [Connecting](docs/connecting.md) | transports, bearer and OAuth auth, profiles, importing client configs, reconnecting, timeouts |
| [The command set](docs/commands.md) | every built-in, completion, aliases, capture and filtering, subscriptions, elicitation, output rendering |
| [Scripting](docs/scripting.md) | `--exec`, NDJSON, exit statuses, schema contracts |
| [Debugging a server](docs/debugging.md) | wire tracing, `last`, `bench` |
| [Release pipeline](docs/releases.md) | maintainer gates, native artifacts, failure and retry behavior |

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
[CONTRIBUTING.md](CONTRIBUTING.md).

The recordings above are generated with
[vhs](https://github.com/charmbracelet/vhs) from the tapes in
[docs/tapes](docs/tapes):

```bash
./scripts/recordings.sh
```

## License

MIT or Apache-2.0, at your option.
