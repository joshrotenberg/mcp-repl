# Changelog

All notable changes to this project will be documented in this file.

## [0.3.1] - 2026-08-18

### Bug Fixes

- Arm the interrupt handler before a command starts ([#135](https://github.com/joshrotenberg/mcp-repl/pull/135))
- Accumulate multi-line piped commands ([#148](https://github.com/joshrotenberg/mcp-repl/pull/148))
- Answer `mcp-repl help` with help, and name what could not be spawned ([#157](https://github.com/joshrotenberg/mcp-repl/pull/157))
- Refuse an elicitation answer the field's enum rules out ([#162](https://github.com/joshrotenberg/mcp-repl/pull/162))
- Name tool/built-in collisions in the connect banner (closes #163) ([#165](https://github.com/joshrotenberg/mcp-repl/pull/165))

### Documentation

- Make -h a beginner's view and keep the detail in --help ([#151](https://github.com/joshrotenberg/mcp-repl/pull/151))
- Write down the shell model and what it decides ([#166](https://github.com/joshrotenberg/mcp-repl/pull/166))
- Lead with the one-shot client, not only the REPL ([#172](https://github.com/joshrotenberg/mcp-repl/pull/172))

### Features

- For, to run one command per element of a captured list ([#158](https://github.com/joshrotenberg/mcp-repl/pull/158))
- Bind, sticky parameter values across calls (closes #161) ([#168](https://github.com/joshrotenberg/mcp-repl/pull/168))
- Complete elicitation answers from the field schema (closes #160) ([#167](https://github.com/joshrotenberg/mcp-repl/pull/167))
- Publish a container image alongside the binaries ([#173](https://github.com/joshrotenberg/mcp-repl/pull/173))

### Miscellaneous Tasks

- Test the release guards instead of only linting them ([#129](https://github.com/joshrotenberg/mcp-repl/pull/129))

### Refactor

- Give a built-in one record instead of two parallel tables ([#134](https://github.com/joshrotenberg/mcp-repl/pull/134))
- Move sanitize out of style, into its own module ([#156](https://github.com/joshrotenberg/mcp-repl/pull/156))

### Testing

- Stabilize interactive final-task e2e ([#149](https://github.com/joshrotenberg/mcp-repl/pull/149))
- Pin that the core does not reach for presentation ([#174](https://github.com/joshrotenberg/mcp-repl/pull/174))



## [0.3.0] - 2026-08-09

### Bug Fixes

- Disambiguate tool and built-in name collisions ([#91](https://github.com/joshrotenberg/mcp-repl/pull/91))
- Reject malformed direct-tool arguments instead of dropping them ([#104](https://github.com/joshrotenberg/mcp-repl/pull/104))
- Reject malformed capture and filter paths ([#105](https://github.com/joshrotenberg/mcp-repl/pull/105))
- Restore subscriptions after reconnect ([#106](https://github.com/joshrotenberg/mcp-repl/pull/106))
- Use native Windows user directories ([#107](https://github.com/joshrotenberg/mcp-repl/pull/107))
- Recognize live HTTP status errors as reconnectable session loss ([#115](https://github.com/joshrotenberg/mcp-repl/pull/115))

### Documentation

- Grow in-app help so the markdown is a companion, not the manual ([#117](https://github.com/joshrotenberg/mcp-repl/pull/117))
- Refresh README security and client comparison claims ([#121](https://github.com/joshrotenberg/mcp-repl/pull/121))
- Make tape regeneration reliable and refresh the hero flow ([#122](https://github.com/joshrotenberg/mcp-repl/pull/122))

### Features

- Suggest a server when none was named ([#86](https://github.com/joshrotenberg/mcp-repl/pull/86))
- Read HTTP bearer from inherited file descriptor ([#90](https://github.com/joshrotenberg/mcp-repl/pull/90))
- Connect from inside the REPL instead of only from the command line ([#116](https://github.com/joshrotenberg/mcp-repl/pull/116))

### Miscellaneous Tasks

- Gate releases before publication ([#108](https://github.com/joshrotenberg/mcp-repl/pull/108))

### Refactor

- Split CLI orchestration and raise editor-path coverage ([#110](https://github.com/joshrotenberg/mcp-repl/pull/110))

### Testing

- Fuzz parser and redaction boundaries ([#111](https://github.com/joshrotenberg/mcp-repl/pull/111))
- Remove the bearer-fd descriptor reuse race ([#113](https://github.com/joshrotenberg/mcp-repl/pull/113))

### Security

- Require approval before imported HTTP configs forward credentials ([#103](https://github.com/joshrotenberg/mcp-repl/pull/103))
- Scrub malformed wire frames ([#109](https://github.com/joshrotenberg/mcp-repl/pull/109))



## [0.2.0] - 2026-08-06

### Bug Fixes

- Sanitize terminal output and restrict local file permissions (#34)
- Bound surface fetches and make in-flight commands interruptible (#36)
- Make listings, errors, and result routing consistent (#40)
- Widen wire redaction to the names secrets actually use (#53)
- Report a failed connection once, in the color that was asked for (#62)
- Ask only for the surface a server declares, and say when a listing fails (#66)
- Bound frame retention, suggestion cost, and the one-shot exit (#67)
- Render a server's error as a sentence, not a struct dump (#79)

### Documentation

- Rewrite the README around what the tool does, with recordings (#45)
- Regenerate the recordings and fix the tapes that silently did not (#80)

### Features

- Harden elicitation and confirm imported server commands (#39)
- Make the surface and the REPL's own commands discoverable (#41)
- Give the demo server typed schemas and a fuller surface (#44)
- Generate shell completions and a man page (#46)
- Adopt tower-mcp 0.19 and turn on what it unblocks (#47)
- Move history to the XDG state layout and trim long listings (#49)
- Give find the flags its exit statuses already implied (#50)
- Answer elicitation on the 2026-07-28 lifecycle too (#52)
- Save a resource to a file with read --out (#54)
- List other clients' configured servers with --scan (#55)
- Adopt tower-mcp 0.20 and tell the server about a cancelled call (#56)
- Answer a task parked on input_required with `task <id> respond` (#58)
- Let an --exec script wait for the tasks it started (#59)
- Log the decisions that never reach the wire, and pin the negotiated protocol version (#71)
- Search the surface with a regular expression (#72)
- Set the server's log verbosity with `loglevel` (#73)
- Make the request and completion timeouts configurable (#74)
- Report what --login and --logout did under --json (#75)
- Say how to background a call that was interrupted (#77)
- Demonstrate and document sampling (#78)

### Miscellaneous Tasks

- Move to the standalone [mcp-repl](https://github.com/joshrotenberg/mcp-repl) repository, extracted from [tower-mcp](https://github.com/joshrotenberg/tower-mcp) with full history
- Replace workspace-inherited manifest fields and dependencies with explicit standalone values
- Relocate the black-box test fixture into this repository's examples, excluded from the published package
- Own formatting, lint, test, package, and release workflows independently; add a scheduled compatibility lane against tower-mcp git main
- Check dependencies for advisories, licenses, and sources (#70)
- Attach prebuilt binaries and an install script to the release (#83)

### Testing

- Cover quoted key=value arguments end to end (#51)
- Make the unreadable-listing fixture independent of an upstream bug (#69)
- Pin that an absent pagination cursor is absent, not null (#76)

## [0.1.9] - 2026-08-04

### Bug Fixes

- **mcp-repl:** Subscribe to final surface changes ([#1159](https://github.com/joshrotenberg/tower-mcp/pull/1159))

### Features

- **mcp-repl:** Define strict scripting contracts ([#1162](https://github.com/joshrotenberg/tower-mcp/pull/1162))
- **mcp-repl:** Import standard MCP server configs ([#1164](https://github.com/joshrotenberg/tower-mcp/pull/1164))
- **mcp-repl:** Validate schema snapshots ([#1165](https://github.com/joshrotenberg/tower-mcp/pull/1165))
- **mcp-repl:** Add secure OAuth profiles ([#1166](https://github.com/joshrotenberg/tower-mcp/pull/1166))

### Miscellaneous Tasks

- **mcp-repl:** Prepare standalone lifecycle ([#1169](https://github.com/joshrotenberg/tower-mcp/pull/1169))

### Refactor

- **mcp-repl:** Split library core from binary ([#1167](https://github.com/joshrotenberg/tower-mcp/pull/1167))

### Testing

- **mcp-repl:** Add binary transport E2E coverage ([#1160](https://github.com/joshrotenberg/tower-mcp/pull/1160))



## [0.1.8] - 2026-08-03

### Miscellaneous Tasks

- Updated the following local packages: tower-mcp



## [0.1.7] - 2026-08-02

### Bug Fixes

- **mcp-repl:** Preserve quoted tool arguments ([#1138](https://github.com/joshrotenberg/tower-mcp/pull/1138))
- **mcp-repl:** Redraw around child stderr ([#1139](https://github.com/joshrotenberg/tower-mcp/pull/1139))

### Features

- **mcp-repl:** Surface task status transitions ([#1140](https://github.com/joshrotenberg/tower-mcp/pull/1140))



## [0.1.6] - 2026-08-01

### Miscellaneous Tasks

- Update Cargo.lock dependencies



## [0.1.5] - 2026-07-31

### Miscellaneous Tasks

- Update Cargo.lock dependencies



## [0.1.4] - 2026-07-30

### Features

- **mcp-repl:** Server profiles via config file ([#1017](https://github.com/joshrotenberg/tower-mcp/pull/1017))
- **mcp-repl:** Wire tracing and last-exchange inspection ([#1020](https://github.com/joshrotenberg/tower-mcp/pull/1020))
- **mcp-repl:** Respond to sampling/create requests ([#1023](https://github.com/joshrotenberg/tower-mcp/pull/1023))
- **mcp-repl:** Command aliases ([#1022](https://github.com/joshrotenberg/tower-mcp/pull/1022))
- **mcp-repl:** Auto-reconnect and session resurrection ([#1018](https://github.com/joshrotenberg/tower-mcp/pull/1018))
- **mcp-repl:** Find command and did-you-mean for unknown commands ([#1021](https://github.com/joshrotenberg/tower-mcp/pull/1021))
- **mcp-repl:** Subscribe to resource updates ([#1025](https://github.com/joshrotenberg/tower-mcp/pull/1025))
- **mcp-repl:** Bench command for tool latency sampling ([#1024](https://github.com/joshrotenberg/tower-mcp/pull/1024))
- **mcp-repl:** Output capture and pipe filtering ([#1033](https://github.com/joshrotenberg/tower-mcp/pull/1033))
- **repl:** Select stable or final protocol lifecycle ([#1055](https://github.com/joshrotenberg/tower-mcp/pull/1055))



## [0.1.3] - 2026-07-24

### Bug Fixes

- **mcp-repl:** Don't blame multi-instance for every not-initialized startup ([#1004](https://github.com/joshrotenberg/tower-mcp/pull/1004))

### Features

- **mcp-repl:** Fetch the startup surface concurrently ([#992](https://github.com/joshrotenberg/tower-mcp/pull/992))
- **mcp-repl:** Auth flags and per-call latency annotations ([#998](https://github.com/joshrotenberg/tower-mcp/pull/998))
- **mcp-repl:** List the tools at startup ([#1000](https://github.com/joshrotenberg/tower-mcp/pull/1000))
- **mcp-repl:** One-shot execution mode and persistent history ([#1003](https://github.com/joshrotenberg/tower-mcp/pull/1003))



## [0.1.2] - 2026-07-24

### Features

- **mcp-repl:** Info replays the full startup banner; resources/templates cross-hint ([#989](https://github.com/joshrotenberg/tower-mcp/pull/989))



## [0.1.1] - 2026-07-24

### Bug Fixes

- **mcp-repl:** Retry surface fetch when the server reports not-initialized ([#987](https://github.com/joshrotenberg/tower-mcp/pull/987))



## [0.1.0] - 2026-07-23

### Documentation

- **mcp-repl:** Live cratesio-mcp example and related tools ([#982](https://github.com/joshrotenberg/tower-mcp/pull/982))

### Features

- **examples:** Mcp-repl, an interactive MCP client REPL ([#966](https://github.com/joshrotenberg/tower-mcp/pull/966))
- **mcp-repl:** Reedline, colored output, describe, template completion ([#981](https://github.com/joshrotenberg/tower-mcp/pull/981))

### Miscellaneous Tasks

- **mcp-repl:** Prepare for crates.io publishing ([#980](https://github.com/joshrotenberg/tower-mcp/pull/980))


