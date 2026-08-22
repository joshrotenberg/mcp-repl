#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 5 ]]; then
  echo "usage: $0 <target> <archive-version> <archive-extension> <binary-name> <binary-version>" >&2
  exit 2
fi

target=$1
version=$2
extension=$3
binary_name=$4
binary_version=$5

if [[ ! "$target" =~ ^[A-Za-z0-9_][A-Za-z0-9_.-]*$ ||
      ! "$version" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ||
      ! "$binary_version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "unsafe release package identity" >&2
  exit 2
fi
case "$extension:$binary_name" in
  tar.gz:mcp-repl | zip:mcp-repl.exe) ;;
  *)
    echo "unsupported release package shape: $extension/$binary_name" >&2
    exit 2
    ;;
esac

# The caller receives these fields from the validated release-target manifest.
# Keeping the packager data-driven prevents its own target table from drifting,
# including on Windows where jq is not a release prerequisite.
binary="target/${target}/release/${binary_name}"
if [[ ! -f "$binary" || -L "$binary" || ! -x "$binary" ]]; then
  echo "release binary is missing, linked, or not executable: $binary" >&2
  exit 1
fi
version_output=$(mktemp)
expected_version_output=$(mktemp)
cleanup_version_output() {
  rm -f "$version_output" "$expected_version_output"
}
trap cleanup_version_output EXIT
if ! "$binary" --version > "$version_output"; then
  echo "release binary could not report its version: $binary" >&2
  exit 1
fi
printf 'mcp-repl %s\n' "$binary_version" > "$expected_version_output"
if ! cmp -s "$version_output" "$expected_version_output"; then
  echo "release binary does not report mcp-repl $binary_version: $binary" >&2
  exit 1
fi
cleanup_version_output
trap - EXIT

stage="mcp-repl-${version}-${target}"
archive="${stage}.${extension}"
for output in "$stage" "$archive" "$archive.sha256"; do
  if [[ -e "$output" || -L "$output" ]]; then
    echo "release package output already exists: $output" >&2
    exit 1
  fi
done

mkdir -p "$stage/completions"
cp "$binary" "$stage/$binary_name"
cp README.md LICENSE-APACHE LICENSE-MIT "$stage/"

# Generate these from the binary being shipped so the documentation cannot
# drift from its actual flags. Every configured runner is target-native.
for shell in bash zsh fish; do
  "$binary" --completions "$shell" > "$stage/completions/mcp-repl.$shell"
done
"$binary" --man > "$stage/mcp-repl.1"

if [[ "$extension" == zip ]]; then
  7z a "$archive" "$stage" > /dev/null
else
  tar czf "$archive" "$stage"
fi

if command -v sha256sum > /dev/null; then
  digest=$(sha256sum "$archive" | awk '{print $1}')
else
  digest=$(shasum -a 256 "$archive" | awk '{print $1}')
fi
if [[ ! "$digest" =~ ^[0-9a-f]{64}$ ]]; then
  echo "could not compute canonical checksum for $archive" >&2
  exit 1
fi
printf '%s  %s\n' "$digest" "$archive" > "$archive.sha256"

printf '%s\n' "$archive"
