# Contributing to mcp-repl

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
