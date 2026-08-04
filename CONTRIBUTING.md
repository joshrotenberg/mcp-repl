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

## Commit Messages

Use conventional-commit prefixes (`feat:`, `fix:`, `docs:`, `test:`,
`chore:`). release-plz generates the changelog and picks the next version from
them.

## Relationship to tower-mcp

mcp-repl is built on [tower-mcp](https://github.com/joshrotenberg/tower-mcp)
and was developed in that workspace until version 0.2.0. CI tests against the
released framework by default; a scheduled job tests against tower-mcp git
main. Protocol-level bugs usually belong in the tower-mcp issue tracker,
REPL-level behavior belongs here.
