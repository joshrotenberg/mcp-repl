#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <target> <version>" >&2
  exit 2
fi

target=$1
version=$2

case "$target" in
  x86_64-unknown-linux-gnu | aarch64-unknown-linux-gnu | x86_64-apple-darwin | aarch64-apple-darwin)
    binary="target/${target}/release/mcp-repl"
    extension=tar.gz
    ;;
  x86_64-pc-windows-msvc)
    binary="target/${target}/release/mcp-repl.exe"
    extension=zip
    ;;
  *)
    echo "unsupported release target: $target" >&2
    exit 2
    ;;
esac

if [[ ! "$version" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]]; then
  echo "unsafe release version: $version" >&2
  exit 2
fi

stage="mcp-repl-${version}-${target}"
archive="${stage}.${extension}"
if [[ -e "$stage" || -e "$archive" || -e "$archive.sha256" ]]; then
  echo "release package already exists: $stage" >&2
  exit 1
fi

mkdir -p "$stage/completions"
cp "$binary" "$stage/"
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
  sha256sum "$archive" > "$archive.sha256"
else
  shasum -a 256 "$archive" > "$archive.sha256"
fi

printf '%s\n' "$archive"
