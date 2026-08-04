//! Project lifecycle and release contract.
//!
//! `mcp-repl` began as an example in the
//! [tower-mcp](https://github.com/joshrotenberg/tower-mcp) workspace and moved
//! to this standalone repository at version 0.2.0 (2026-08-04). The move used
//! `git filter-repo`, so `git log --follow` traces every source file back to
//! its original tower-mcp commits. Versions before 0.2.0 were released from
//! the tower-mcp repository; their tags and release notes remain there.
//!
//! # Supported boundaries
//!
//! The application lives in the `mcp_repl` library and the binary is only a
//! thin call to [`crate::run_cli`]. The deliberately reusable seams are:
//!
//! - [`crate::config`] for native server and alias profiles;
//! - [`crate::import_config`] for explicit imports from standard MCP JSON
//!   configuration files; and
//! - [`crate::oauth_profile`] for non-secret OAuth profile metadata and secure
//!   credential-store access.
//!
//! Terminal editing, rendering, and command dispatch remain private. A related
//! tool such as `mcp2md` should not depend on all of `mcp-repl` merely to reuse
//! configuration: that would also couple it to the interactive terminal stack.
//! Keep such a tool independent unless real duplication justifies extracting a
//! narrow configuration or connection crate used by both projects.
//!
//! # Compatibility and release lanes
//!
//! The default CI lane builds against the released `tower-mcp` declared in the
//! manifest, which is what users install:
//!
//! ```text
//! cargo fmt --all -- --check
//! cargo clippy --all-targets --all-features -- -D warnings
//! cargo test --all-targets --all-features
//! RUSTDOCFLAGS=-Dwarnings cargo doc --no-deps --all-features
//! cargo package
//! ```
//!
//! A scheduled job additionally patches `tower-mcp` to git main and reruns the
//! test suite. That lane preserves the early-warning role this project played
//! inside the workspace: an upstream client-surface break shows up here within
//! a day instead of at the next framework release. A failure that reproduces
//! only in that job indicates tower-mcp main moved, not an mcp-repl
//! regression.
//!
//! This repository's release-plz workflow owns crates.io publication, tags,
//! GitHub releases, and changelog updates. Do not publish a version manually
//! in parallel with it.
//!
//! # Test fixture
//!
//! The black-box tests spawn `examples/mcp_repl_fixture.rs`, a deterministic
//! MCP server built from this repository's dev-dependencies. It is excluded
//! from the published package; the `examples/` directory here exists for the
//! test suite, not for documentation.
