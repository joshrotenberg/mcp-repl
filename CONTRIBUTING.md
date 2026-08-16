# Contributing to mcp-repl

## Scope

Before building a feature, read [The shell model](README.md#the-shell-model).
mcp-repl is a shell whose command set is the connected server, and that model
decides most scope questions on its own.

In short: shell constructs fit, programming-language constructs do not. `&&`
and `||`, output redirection, a file of commands, and a more capable path
selector are all in character. Functions, arithmetic, `if`, string
manipulation, and an embedded scripting language are not, and an embedded
language has already been proposed, spiked, and declined on those grounds in
[#145](https://github.com/joshrotenberg/mcp-repl/issues/145), which is worth
reading before reopening it.

A proposal that falls outside the model is not automatically refused, but it
does have to argue with the model rather than around it. Saying which shell
construct a feature corresponds to, or why the correspondence should not
apply here, is the useful form for that discussion.

## Getting Started

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
RUSTDOCFLAGS="-Dwarnings" cargo doc --no-deps --all-features
```

All of these must pass before submitting a PR. The black-box tests build and
spawn the fixture server in `examples/mcp_repl_fixture.rs`, so the first run
compiles tower-mcp's server features as dev-dependencies.

The command, routing/path, imported-config, wire-redaction, and alias
boundaries also have a dependency-free deterministic property corpus. Run it
on its own with:

```bash
cargo test --lib property_
```

The generated case count and seed live in `src/property.rs`. A fixed seed
keeps CI time and failures reproducible; add the smallest reproducer to the
matching regression corpus whenever one of these boundaries is fixed.

## Coverage

Install `cargo-llvm-cov`, then generate the navigable report with:

```bash
cargo install cargo-llvm-cov
./scripts/coverage.sh
```

The report starts at `target/llvm-cov/html/index.html`. For a compact baseline
without opening or generating a browser report, run
`./scripts/coverage.sh --summary-only`. Coverage is a map to untested branches,
not a merge threshold: prioritize security boundaries, state restoration, and
editor behavior over a repository-wide vanity percentage.

Adding or updating a dependency also needs:

```bash
cargo deny check
```

which checks the graph against RUSTSEC advisories, the license allowlist in
`deny.toml`, and the ban on wildcard versions and non-crates.io sources. CI
runs it on every push and daily, since an advisory can be published against a
dependency that has not changed. Install it with `cargo install cargo-deny`.

## Commit Messages

Use conventional-commit prefixes (`feat:`, `fix:`, `docs:`, `test:`,
`chore:`). release-plz generates the changelog and picks the next version from
them.

Maintainers should follow the gated, all-target release process in
[docs/releases.md](docs/releases.md). A release PR is not safe to merge until
its `Release gate` check succeeds.

## Relationship to tower-mcp

mcp-repl is built on [tower-mcp](https://github.com/joshrotenberg/tower-mcp)
and was developed in that workspace until version 0.2.0. CI tests against the
released framework by default; a scheduled job tests against tower-mcp git
main. Protocol-level bugs usually belong in the tower-mcp issue tracker,
REPL-level behavior belongs here.
