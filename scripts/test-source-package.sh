#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

if ! command -v jq > /dev/null 2>&1; then
  echo "source-package verification requires jq" >&2
  exit 2
fi

metadata=$(cargo metadata --locked --no-deps --format-version 1)
version=$(jq -er '.packages[] | select(.name == "mcp-repl") | .version' <<<"$metadata")
release_recovery_epoch=$(jq -er '
  .packages[] | select(.name == "mcp-repl") |
  .metadata["mcp-repl"]["release-recovery-epoch"] |
  select(type == "number" and . >= 1 and floor == .)
' <<<"$metadata")
target_dir=$(jq -er '.target_directory' <<<"$metadata")
package_dir="$target_dir/package/mcp-repl-$version"
install_root=$(mktemp -d)
trap 'rm -rf "$install_root"' EXIT INT TERM

# This is a contributor-facing pre-commit check as well as a CI gate, so test
# the current candidate rather than requiring an already-clean worktree.
cargo package --locked --allow-dirty

# The explicit controller-only recovery trigger must survive Cargo's normalized
# manifest. Otherwise advancing it would not make the package differ from the
# accepted crate and release-plz would silently generate no fresh patch.
packaged_metadata=$(cargo metadata \
  --manifest-path "$package_dir/Cargo.toml" \
  --locked --no-deps --format-version 1)
packaged_release_recovery_epoch=$(jq -er '
  .packages[] | select(.name == "mcp-repl") |
  .metadata["mcp-repl"]["release-recovery-epoch"] |
  select(type == "number" and . >= 1 and floor == .)
' <<<"$packaged_metadata")
if [[ "$packaged_release_recovery_epoch" != "$release_recovery_epoch" ]]; then
  echo "published package changed release-recovery-epoch" >&2
  exit 1
fi

required=(
  Cargo.toml
  Cargo.lock
  README.md
  CHANGELOG.md
  LICENSE-APACHE
  LICENSE-MIT
  config.example.toml
  src
)
for path in "${required[@]}"; do
  if [[ ! -e "$package_dir/$path" ]]; then
    echo "published package is missing $path" >&2
    exit 1
  fi
done

repository_only=(
  .dockerignore
  .github
  Dockerfile
  SECURITY.md
  cliff.toml
  docs
  examples
  release-plz.toml
  scripts
  tests
)
for path in "${repository_only[@]}"; do
  if [[ -e "$package_dir/$path" ]]; then
    echo "published package unexpectedly contains $path" >&2
    exit 1
  fi
done

# Cargo's package verification compiles the extracted source, but it does not
# run its test targets. Exercise what a registry consumer actually receives so
# an excluded fixture can never leave a retained test target broken again.
cargo test \
  --manifest-path "$package_dir/Cargo.toml" \
  --all-targets \
  --all-features \
  --locked

cargo install \
  --path "$package_dir" \
  --root "$install_root" \
  --locked

reported_version=$("$install_root/bin/mcp-repl" --version)
if [[ "$reported_version" != "mcp-repl $version" ]]; then
  echo "installed package reported unexpected version: $reported_version" >&2
  exit 1
fi
