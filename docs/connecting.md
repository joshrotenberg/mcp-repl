# Connecting

[mcp-repl](../README.md) · [Connecting](connecting.md) · [Commands](commands.md) · [Scripting](scripting.md) · [Debugging](debugging.md)


Every way to reach a server, and how credentials are handled.

## Shell completions and the man page

mcp-repl generates both from its own command definition, so they never drift
from the flags the binary actually accepts:

```bash
mcp-repl --completions zsh > ~/.zfunc/_mcp-repl
mcp-repl --completions bash > /etc/bash_completion.d/mcp-repl
mcp-repl --completions fish > ~/.config/fish/completions/mcp-repl.fish
mcp-repl --man > /usr/local/share/man/man1/mcp-repl.1
```

`bash`, `zsh`, `fish`, `powershell`, and `elvish` are supported. Completion
covers the flags and their accepted values, so `--protocol <Tab>` offers
`stable` and `2026-07-28`, and `--elicitation <Tab>` offers `prompt` and
`decline`.

Both generators run before anything connects: they need no config file, no
server, and no terminal, which is what lets a packaging script call the
binary it just built.

## Run

```bash
# Against the bundled in-process demo router (no external server):
mcp-repl --demo

# Spawn any stdio MCP server as a child process:
mcp-repl -- ./my-server --stdio

# Connect to a streamable HTTP server:
mcp-repl --http http://127.0.0.1:3001/mcp

# Import one named server from a repository or client JSON config:
mcp-repl path/to/.mcp.json:server-name

# Opt into the final, sessionless 2026-07-28 lifecycle:
mcp-repl --protocol 2026-07-28 --http http://127.0.0.1:3001/mcp
```

The binary compiles both stable and final protocol support. Runtime selection
is explicit: `--protocol stable` (the default) uses
`initialize`/`notifications/initialized`, while `--protocol 2026-07-28`
(`--protocol final` is an alias) uses `server/discover` and sends the selected
protocol metadata on every request. Keeping stable as the default means an
mcp-repl upgrade cannot silently change an existing server's lifecycle.

### Try it against a live server

[cratesio-mcp](https://github.com/joshrotenberg/cratesio-mcp) (an MCP server
for the crates.io registry, also built on tower-mcp) runs a public instance:

```bash
mcp-repl --http https://cratesio-mcp.fly.dev/
```

```text
cratesio-mcp> search_crates query=tower-mcp per_page=3
cratesio-mcp> get_crate_health name=serde
cratesio-mcp> read crates://tokio/info
cratesio-mcp> prompt analyze_crate crate_name=axum
```

### Authenticated servers

Attach credentials to an `--http` connection:

```bash
# Bearer token. Prefer MCP_BEARER: a --bearer on the command line is visible
# in `ps` and shell history.
MCP_BEARER="$TOKEN" mcp-repl --http https://internal.example/mcp
mcp-repl --http https://internal.example/mcp --bearer "$TOKEN"

# Unix: inherit an ephemeral pipe as fd 3, then restore stdin for the REPL.
token-helper | mcp-repl --bearer-fd 3 3<&0 </dev/tty \
  --http https://internal.example/mcp

# Arbitrary headers, repeatable (split on the first colon):
mcp-repl --http https://internal.example/mcp --header "X-Api-Key: abc"
```

`--bearer-fd` is the most constrained static-credential form. It is Unix-only,
owns and closes the inherited descriptor before the async runtime or network
connection starts, removes one trailing LF or CRLF, and limits input to 16384
bytes. Empty, non-UTF-8, non-ASCII, multiline, closed, and stdio-reserved
descriptors are usage errors. The token is never written to history, profiles,
debug output, or error messages.

It also fails closed when any other authorization source is present:
`--bearer`, `MCP_BEARER`, a profile `bearer`/`bearer_env`, an Authorization
header, or OAuth. Unset the competing source rather than relying on the normal
static-auth precedence rules. On non-Unix platforms, use `MCP_BEARER` or a
secure OAuth profile.

`--bearer` and `--header` apply only to HTTP connections; they are ignored
(with a warning) for the demo and stdio-child transports. `--bearer-fd` is
rejected instead of ignored, because silently discarding secret input is not
safe.

For an MCP server using OAuth authorization-code + PKCE, create a named login
without opening an MCP session:

```bash
# Discovers the protected resource and authorization server, opens the browser,
# receives the redirect on an ephemeral loopback port, and saves the credentials.
mcp-repl --login work --http https://mcp.example.com/mcp \
  --oauth-scope openid --oauth-scope offline_access

# Reuse its saved URL directly, retarget it with --http, or select it through
# a server profile.
mcp-repl --oauth work
mcp-repl --oauth work --http https://mcp-alt.example.com/mcp

# If automatic browser launch is unavailable, print the URL and wait for the
# loopback redirect (remote use requires forwarding that loopback callback).
mcp-repl --login work --http https://mcp.example.com/mcp --no-browser

# Remove both profile metadata and credentials.
mcp-repl --logout work

# Report what was created, for a script that provisions profiles.
mcp-repl --login work --http https://mcp.example.com/mcp --json
```

`--json` works on both, following the same NDJSON conventions `--exec` does:
one value on stdout, prompts and progress on stderr, and the standard error
envelope on failure.

```json
{"profile":"work","serverUrl":"https://mcp.example.com/mcp","scopes":["openid","offline_access"]}
{"profile":"work","removed":true}
```

The scopes reported are the ones actually recorded, not the ones requested,
which is what a provisioning script needs to know. No credential appears
there and none can: the tokens are in the operating-system credential store
and mcp-repl's config side never holds them. `--no-browser` benefits most,
since that flow is the one a remote or headless setup drives.

Login follows MCP protected-resource and authorization-server discovery,
requires PKCE S256, tries an optional Client ID Metadata Document before
Dynamic Client Registration, and requests refresh-token support when the
server advertises it. Use
`--oauth-client-id-metadata-document https://client.example/metadata.json` for
CIMD or `--oauth-authorization-server ISSUER` to select one exact issuer when
discovery advertises several.

Only non-secret routing metadata is written to `config.toml`. Access tokens,
refresh tokens, and dynamically registered client secrets are kept in macOS
Keychain, Windows Credential Manager, or the Linux Secret Service through the
platform credential store. If no secure store is available, mcp-repl fails
closed; it never writes a plaintext credential fallback. A saved expired token
is refreshed automatically. A failed refresh tells you to run `--login` again;
an explicit login discards the unusable token while retaining reusable DCR
registration.

`--exec`/`--json` never starts an interactive authorization or opens a browser.
It either restores/refreshes the saved credential or exits with an actionable
`--login` command. Runtime insufficient-scope challenges are retried at most
twice; interactive sessions can authorize the added scopes, while one-shot
commands fail immediately with the same login guidance.

## Profiles

A config file names servers so a connection is `mcp-repl <name>` instead of a
URL plus repeated auth flags, and tokens stay out of shell history. The file
lives at `$XDG_CONFIG_HOME/mcp-repl/config.toml`, falling back to
`~/.config/mcp-repl/config.toml`; `--config <path>` reads a different one.

```toml
[servers.cratesio]
transport = "http"                 # http | stdio
url = "https://cratesio-mcp.fly.dev/"
bearer_env = "CRATESIO_TOKEN"      # read the token from the environment
headers = { "X-Api-Key" = "abc" }

[repl]
history_capacity = 5000            # lines of command history to keep
request_timeout = 300              # default for --timeout, in seconds
completion_timeout_ms = 500        # how long Tab waits on the server

[oauth.work]
url = "https://mcp.example.com/mcp"
scopes = ["openid", "offline_access"]

[servers.work]
transport = "http"
oauth = "work"
headers = { "X-Tenant" = "acme" }

[servers.local]
transport = "stdio"
command = ["./my-server", "--stdio"]
```

The `[repl]` table holds settings that are not about any one connection:

| key | default | what it does |
| --- | --- | --- |
| `history_capacity` | 1000 | lines of command history kept on disk. `0` keeps none, like `--no-history` |
| `request_timeout` | 120 | seconds before a request is abandoned, when `--timeout` does not say. `0` waits indefinitely |
| `completion_timeout_ms` | 2000 | how long Tab waits for a server's `completion/complete`. This runs between keystrokes, so a slow server makes Tab feel broken rather than merely unhelpful; lower it before raising it |

A flag beats the config, and the config beats these defaults. `--timeout 0`
is a setting, not an absence: it asks to wait indefinitely even when the
config names a limit.

Unknown keys are refused rather than ignored, here and in every other table,
because a setting that appears to apply and does not is worse than one that
fails loudly:

```text
error: config.toml: TOML parse error at line 2, column 1
  |
2 | history_capacty = 50
  | ^^^^^^^^^^^^^^^
unknown field `history_capacty`, expected one of `history_capacity`,
`request_timeout`, `completion_timeout_ms`
```

```bash
mcp-repl --list-servers            # the configured profiles
mcp-repl --server cratesio         # connect by name
mcp-repl cratesio                  # a bare name works too
```

- `transport` is optional: a profile with a `url` is HTTP, one with a
  `command` is stdio, and one with `oauth` is HTTP and may reuse that OAuth
  profile's saved URL. A profile with both a URL/OAuth selection and a command
  must say which.
- Explicit flags override profile fields. `--http <url>` retargets the URL
  while keeping the profile's auth; `--bearer` replaces the profile's token;
  each `--header` overrides the profile header of the same name.
- OAuth precedence is explicit static authorization (`--bearer` or
  `--header Authorization`) first, then explicit `--oauth`, then a server
  profile's `oauth`, then native/imported static credentials, and finally
  `MCP_BEARER`. A server profile cannot combine `oauth` with `bearer`,
  `bearer_env`, or an `Authorization` header; non-auth headers remain valid.
- The bare-name form only resolves when the single positional matches a
  configured profile, so spawning a stdio server by bare name still works.
  Because everything after the first positional belongs to the spawned
  command, use `--server <name>` when other flags follow.
- Secrets: `bearer_env` names an environment variable holding the token. An
  unset variable is an error rather than a silent anonymous connection. An
  inline `bearer = "..."` works but warns, since it puts the token in the
  file.
- An unknown profile name errors with the list of known names, and a missing
  `--config` file is an error. A missing file at the default location is not:
  profiles are opt-in.

### Importing standard MCP configs

`--scan` prints what other MCP clients already have configured, so you do not
have to remember where a config lives:

```text
$ mcp-repl --scan
/work/api/.mcp.json
  local     stdio node server.js --stdio
  registry   http https://registry.example.com/mcp
/Users/you/.claude.json
  github    stdio npx -y @modelcontextprotocol/server-github
3 servers in 2 files. Connect with `mcp-repl <path>:<entry>`.
```

It reads `.mcp.json`, `.vscode/mcp.json`, and `.cursor/mcp.json` in the
current directory, plus `~/.claude.json` and the Claude Desktop config for
your platform. It connects to nothing and runs nothing: the selectors are
printed for you to pass back. Entries are described as written, so an entry
using `${env:TOKEN}` is listed even when the variable is not set. A file that
exists but cannot be parsed is reported rather than skipped, and the command
exits non-zero when nothing was found. `--json` gives one object per file,
each entry carrying a ready-made `selector`.

An explicit `PATH:ENTRY` selector imports a named server from the common JSON
format used by repository `.mcp.json` files, VS Code, Claude, Cursor, and other
MCP clients. Both `mcpServers` and `servers` roots are accepted; automatic
file discovery is deliberately deferred so the selected source is always
visible in the command:

```bash
mcp-repl .mcp.json:local
mcp-repl .vscode/mcp.json:remote
mcp-repl --server "$HOME/Library/Application Support/Claude/claude_desktop_config.json:github"
```

```json
{
  "mcpServers": {
    "local": {
      "command": "cargo",
      "args": ["run", "--manifest-path", "${workspaceFolder}/server/Cargo.toml"],
      "env": { "API_TOKEN": "${env:HOST_API_TOKEN}" },
      "cwd": "${workspaceFolder}"
    }
  },
  "servers": {
    "remote": {
      "type": "http",
      "url": "${env:MCP_URL}",
      "headers": { "Authorization": "Bearer ${env:MCP_TOKEN}" }
    }
  }
}
```

- `stdio` entries preserve `command`, ordered `args`, `env`, and `cwd`.
  Relative working directories resolve from the config's workspace directory
  (the parent of `.vscode` for `.vscode/mcp.json`, otherwise the file's
  directory). The child inherits the current environment, with imported `env`
  values overriding matching keys.
- `http` and `streamable-http` entries preserve `url` and `headers`. Legacy
  `sse` entries are rejected because mcp-repl connects with Streamable HTTP.
- `${env:NAME}` and `${NAME}` read the launching environment;
  `${workspaceFolder}`, `${workspaceFolderBasename}`, and `${userHome}` are
  also supported. A missing variable is an error. `${input:...}` is rejected
  with guidance because an imported interactive input has no portable value
  outside the client that defined it.
- Precedence is explicit flags first, then the imported entry, then native
  profiles when no import was selected. `--http` can retarget an imported HTTP
  entry while retaining its headers; `--bearer` and repeated `--header` values
  override imported authentication. `MCP_BEARER` remains the final bearer
  fallback when no selected configuration or flag supplies one.
- Unknown entries list available names in sorted order. Entries with both a
  command and URL, conflicting transport declarations, missing required
  fields, or transport-specific fields on the wrong transport are refused.
- Native global aliases remain available for imported connections. Imported
  files do not define mcp-repl aliases, so aliases created while using one are
  global.

#### Approving an imported command

Selecting an imported `stdio` entry runs its command, and the same file
chooses which of your environment variables that command receives. It is
executable code that arrived with a repository, so the first time an entry
is used the REPL shows what it resolved to and asks:

```text
[import] /work/api/.mcp.json:local wants to start a server process on this machine:
  command: node /work/api/mcp-server.js --stdio
  cwd:     /work/api
  env:     GITHUB_TOKEN, REGION
  warning: GITHUB_TOKEN looks like a credential, and its value goes to this program
The imported file chooses the program and which of your environment variables
it receives. Approve it only if you trust that file.
  start it? [y/N]>
```

Approving records the entry in `approved-imports.toml` beside the config
file, so the question is asked once. The record holds the command, working
directory, and variable *names* (never their values), and it is matched
exactly: if the entry later runs a different command, moves, or asks for
more variables, it does not match and you are asked again. Delete a block
to be asked about that entry again, or delete the file to forget every
approval.

A session with nobody to ask (`--exec`, `--json`, or a non-terminal stdin)
refuses rather than prompting, and exits 2 with the entry named. Pass
`--trust-import` to skip the check in automation, which approves without
recording anything.

Native profiles in your own config file are not gated: you wrote them.
mcp-repl never invokes a shell for an entry and never prints imported
environment or header values, but literal secrets in the source file are
still secrets at rest.

`MCP_BEARER` is removed from every spawned stdio child's environment. It
is an HTTP credential, and nothing reached over stdio has a use for it.

### Reconnecting

A remote server that restarts, OOMs, or sits behind an edge returning 502/503
can interrupt a connection. On an `--http` connection the REPL notices this,
creates a fresh transport, repeats the selected stable or final handshake,
re-fetches the surface, and retries the command once. For stable servers this
also replaces the lost session; final connections are sessionless.

```text
> search query=tower
[reconnected]
... results ...
```

The retry is bounded to a single attempt, so a server that is really down
fails fast with its original error rather than hanging the prompt. Task ids
do not survive a reconnect (they belong to the session that created them), so
`task`, `wait`, and `cancel` never trigger one.

Pass `--no-reconnect` to turn this off and see session-loss errors as they
arrive. stdio children and `--demo` are never reconnected: there, a lost
session means the server process itself is gone.

### Interrupting and timing out

Ctrl-C during a running command cancels that command and returns to the
prompt; the session, its history, and any background tasks survive. At the
prompt it keeps its usual meaning and clears the line. In `--exec` mode an
interrupt stops the sequence and exits 6.

Cancelling stops the REPL waiting; it does not tell the server to stop. A
tool with side effects may still be running on the other end.

A server that accepts a request and never answers is a different problem,
since a script has nobody to press Ctrl-C. `--timeout <seconds>` (default
120) gives up and reports a transport error:

```bash
# A tighter bound for a scripted health check:
mcp-repl --timeout 10 --http https://example/mcp -e "search query=x"
```

```bash
# Wait indefinitely, whatever the server does:
mcp-repl --timeout 0 -- ./my-server
```

The deadline covers tool calls, `read`, `prompt`, `bench` calls, and surface
fetches, on both transports, and it replaces the HTTP transport's own 30s
limit so raising it raises the real one. A tool that legitimately runs longer
than the deadline is better run task-augmented with a trailing `&`.
`wait <id>` is exempt, because outliving the call is what a task is for; give
it its own bound with `wait <id> --timeout 300`.

A surface list is bounded too: paging stops after 100 pages or 10000 entries,
and says so, rather than following an endless cursor. Repeated
`list_changed` notifications are coalesced, so a server that emits them in a
loop causes one refresh rather than one per notification.
