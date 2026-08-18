# Handoff

Session of 2026-08-15 on `joshrotenberg/mcp-repl`. Untracked on purpose; delete or commit as you like.

## The one idea worth carrying forward

**mcp-repl is a shell for MCP.** Not a REPL that keeps growing features, and not a scripting host. It already had the grammar before anyone said so:

| mcp-repl | shell |
| --- | --- |
| `name = command`, `$name` | variables |
| `command \| path` | pipe |
| `command &`, `jobs`, `wait`, `cancel` | job control |
| `alias w=tool wait` | aliases |
| `for $x in $list: cmd` | `for` |

This is the thing that answers scope questions without arguing them case by case. It says what to add (`&&`/`||`, redirection, a command file, globs over the surface) and what to refuse (functions, arithmetic, `if`, string manipulation). "Shell out to a real language" is the shell's own answer to complexity, and it is the right one here.

**It is not written down anywhere but this file.** Putting it in CLAUDE.md or the README is the highest-value small task on this list.

## Decisions made, with the reasoning

**The embedded scripting language is parked, deliberately.** #145 originally proposed a Lisp, was rewritten for rhai after a spike, then parked entirely. The reasoning is in that issue's appendix and it is worth reading before anyone reopens it.

Short version: the pitch was "MCP interfaces as a programmable API," the cost is a second ecosystem, and that cost does not shrink with the size of the language. Of six claimed benefits, one and a half survived contact with "or you could just write Python against an MCP SDK." The survivor is handing values back to the live prompt, which no external script can do. That is real and it is not worth a language.

The spike findings are preserved in the appendix so nothing needs rediscovering: the sync-to-async bridge is 50 lines, `Engine::new_raw()` makes feature subtraction genuinely enforceable, `${...}` interpolation is core syntax, and namespacing works both as modules and as custom types.

**#145 was rescoped to what actually had value:** extracting a reusable core from the binary. That stands on its own merits, because per-connection policy is currently process-global and write-once:

```
src/elicit.rs:69     static MODE: OnceLock<ElicitationMode>
src/sampling.rs:70   static MODE: OnceLock<SamplingMode>
src/lib.rs           static REQUEST_TIMEOUT_SECS: AtomicU64
```

Two servers cannot hold different elicitation or sampling policies even sequentially. That constrains `connect` today, with no scripting involved.

The extraction is smaller than it looks. `session`, `tool_args`, `property`, `schema_contract`, and `subscribe` already have zero intra-crate dependencies. After #156 moved `sanitize` out of `style`, the only presentation coupling left is `wire`, `sampling`, `jobs`, and `elicit` reaching for `paint`/`tag`, plus `AsyncOutput` in the last two.

The deliverable ends with a test asserting no core module references presentation. That test is most of the value: a second consumer proves an API is *sufficient*, a test proves it is *clean*, and cleanliness is what was actually missing.

**`for` shipped with no conditional form, on purpose.** Iteration is a shell construct; `if` is what forces expressions, comparison, and truthiness. When someone wants "only the high-priority ones," the answer is a richer path expression (`items[?priority=='high']`, which is JMESPath and bounded) rather than a test inside the loop. That reasoning is recorded on #155.

## Shipped this session

```
#151  -h is now a beginner's view (88 lines -> 29), detail stays in --help
#156  sanitize moved out of style into untrusted
#157  mcp-repl help answers with help; unrunnable programs are named
#158  for, one command per element of a captured list
      five dependabot PRs handled (four merged, #133 closed)
```

## Open

```
PR #162  elicitation enum validation      CI was running at session end
#160 p3  complete elicitation answers from the field schema
#163 p3  name tool/built-in collisions at connect
#145 p3  extract a reusable core (four rendering modules left)
#130 p3  container image
#131 p3  Homebrew formula
PR #137  chore: release v0.3.1, now many merges stale, likely wants regenerating
```

#160 and PR #162 share plumbing; #162 added `accepted_values`, which is most of what #160 needs.

Two housekeeping items nobody has done: there is no dependabot `ignore` rule for `dtolnay/rust-toolchain`, so #133 will come back (that pin is the MSRV check, never merge those), and PR #137 is stale.

## Gotchas, mostly earned the hard way

**Run `cargo fmt --check` after the last edit, not before it.** Cost a red CI on #151. The pre-push hook will not save you: `core.hooksPath` is globally set to `~/.git-hooks`, whose `pre-push` is a lefthook stub pointing at a deleted scratchpad path. It falls through to `echo`, which exits 0, so it always passes. This repo tracks no lefthook config anyway. CI is the only real gate.

**Do not infer module coupling from an import list.** I claimed twice that a module's only dependency on `style` was `sanitize`, both times from reading `use` lines rather than measuring. Both times wrong: they also used `paint` and `tag`. Grep for the actual symbols.

**Do not add validation without checking what coercion already accepts.** I added boolean validation against `true`/`false`; `coerce_field` deliberately accepts `y`, `yes`, and `on`, with an existing test pinning it. The elicitation e2e case feeds `y` and failed immediately.

**`--demo` cannot reproduce HTTP-fixture e2e flakes.** Zero failures in 22 runs while the real fixture failed 1 in 10, because demo startup is too fast to open the window. Reproduce against the real fixture or you will conclude a race does not exist.

**The e2e suite is one test function with many cases.** `cargo test --test e2e` runs `published_cli_covers_transports_and_protocol_lifecycles`, and a failure reports as `tests/e2e.rs:<line>`, which is the case. Run the built binary directly for repeated runs: `target/debug/deps/e2e-* --exact published_cli_covers_transports_and_protocol_lifecycles`.

**A tool result reaching `vars` is already unwrapped.** `r = echo message=hello` captures `"hello"`, not `{content: [{type: "text", text: "hello"}]}`. The content-block shape is the raw protocol, not what the prompt hands you.

## Conventions

Feature branch, draft PR first, conventional-commit prefixes, no em dashes anywhere, no AI attribution or commit trailers, author is joshrotenberg. Gate before every push: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
