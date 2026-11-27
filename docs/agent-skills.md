# Agent skills

[mcp-repl](../README.md) · [Connecting](connecting.md) · [Commands](commands.md) · [Scripting](scripting.md) · [Debugging](debugging.md) · [Agent skills](agent-skills.md)

mcp-repl embeds a skills-only plugin for Claude Code and Codex. The skill
teaches an agent to discover tools, inspect their schemas, and call them
through the `mcp-repl` on its `PATH`, using the MCP connection you authorize.
The bundle ships in the binary; no source checkout is needed.

```sh
mcp-repl plugin --help
mcp-repl plugin status --json
mcp-repl plugin install --provider both --dry-run
mcp-repl plugin install --provider claude
mcp-repl plugin update --provider codex
mcp-repl plugin uninstall --provider both
```

## Commands and output

- `install`, `update` and `uninstall` require `--provider claude|codex|both`.
  `status` defaults to both.
- `status` and `--dry-run` read mcp-repl's local ownership records and bundle
  files. They launch no provider and connect to no MCP server, so they report
  local ownership only. Provider settings, enabled state and registration are
  checked during a real operation.
- `--json` emits one report with an outcome per selected provider.
- Usage and argument errors are clap text diagnostics on stderr with exit 2,
  including under `--json`.
- With `both`, each provider is attempted. Any failure makes the process exit
  nonzero, even if the other provider succeeded.

## Providers

- A real change needs the `claude` or `codex` executable on `PATH`.
- Designed against Claude Code 2.1.331 and Codex CLI 0.171.0. Real provider
  lifecycle testing has been on macOS.
- On Windows the provider must be a native `.exe` on `PATH`; npm `.cmd` shims
  are not launched.
- The manager registers a local source named `mcp-repl-bundled` with each
  provider's CLI and operates on user installations.
- `CLAUDE_CONFIG_DIR` and `CODEX_HOME` are honored.
- `update` runs Claude Code's update command, or re-adds the Codex plugin.
  Restart a running provider session to load the refreshed skill.
- `install` on an already managed plugin keeps its version until `update`.

## Profiles

The mcp-repl state directory tracks one profile per provider. `install`
creates a missing profile directory, and each source stays at its registered
path. To manage Claude profiles that retain caches independently, set a
separate `XDG_STATE_HOME` (Unix) or `LOCALAPPDATA` (Windows) per profile.

## Safety checks

- Before changing anything, the manager checks for edited files, symlinks and
  conflicting ownership.
- It refuses a changed-bundle update of a disabled plugin, because a refresh
  can change the enabled state.
- Claude uninstall asks for plugin data to be kept.
- Providers own their caches. mcp-repl keeps ownership records for verified
  cache copies a provider leaves behind, and the current cache layout is part
  of the compatibility checks.
- The manager lock coordinates mcp-repl processes only. Do not run provider
  plugin commands or edit managed files concurrently.

## Pending operations

If a provider command fails after a change may have started, `status` and
`--dry-run` report `pending` and exit nonzero. Inspect the named receipt and
run the suggested read-only provider listing commands with the same profile.

A pending install may be cleared by removing only the named receipt and
rerunning `install`, and only when all three hold:

- The receipt belongs to the exact selected profile.
- Read-only provider listings show that neither the managed marketplace nor
  its plugin exists.
- The managed source and cache are both absent.

Every other pending state needs inspection and reconciliation before another
change.

- A pending install keeps the expected source files in its receipt, including
  when marketplace registration fails after the source was published. Keep
  both the source and the receipt for inspection.
- Keep ownership records for retained caches. Removing their receipt makes
  those caches foreign to the manager and blocks automatic reinstall. It does
  not authorize deleting them.
- Once a provider has removed all orphaned caches, run `uninstall` with the
  original profile to discard its inactive receipt before selecting another
  profile.

## Name collisions

- With an explicit server connection, `plugin` can still be a server tool name.
- The root command reserves the bare word `plugin`. Select a saved profile with
  that name as `mcp-repl --server plugin`.
- If your stdio server executable is named `plugin`, run `mcp-repl -- plugin …`.
