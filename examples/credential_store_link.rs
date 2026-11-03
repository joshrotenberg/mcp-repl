//! Compile-only regression fixture for native credential-store dependencies.

use mcp_repl::oauth_profile::CredentialStore;

fn main() {
    // Keep the public constructor reachable for the linker without calling it:
    // this fixture must never open, read, or modify a native credential store.
    std::hint::black_box(CredentialStore::keyring as fn(&str) -> Result<CredentialStore, String>);
}
