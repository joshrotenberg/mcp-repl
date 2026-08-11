#!/usr/bin/env bash
# Exercise the failure paths of scripts/publish-release.sh.
#
#   scripts/test-release-guards.sh
#
# Those guards decide whether a release becomes public, and they only run
# during a release, so a mistake in them is discovered at the worst possible
# moment. This drives the real script against a stubbed `gh` and a fabricated
# set of archives, once per outcome.
#
# It asserts the message as well as the status: the bug that prompted this
# was a guard that failed correctly while saying something false about the
# release, which no exit code would have caught.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
publish="$root/scripts/publish-release.sh"
tag=v9.9.9

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

# A `gh` that reports what GH_MODE says and records nothing else, so the
# script can be run to completion without touching a real release.
mkdir -p "$work/bin"
cat > "$work/bin/gh" <<'STUB'
#!/bin/sh
case "$1 $2" in
  "release view")
    case "${GH_MODE:-draft}" in
      unreachable) echo "failed to run git: fatal: not a git repository" >&2; exit 1 ;;
      published)   echo false ;;
      *)           echo true ;;
    esac ;;
  *) echo "gh $*" ;;
esac
STUB
chmod +x "$work/bin/gh"

sha256() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$@"
  else
    shasum -a 256 "$@"
  fi
}

# A complete set, as five successful build jobs would leave behind.
seed_dist() {
  local dist=$1
  rm -rf "$dist"
  mkdir -p "$dist"
  local target extension
  for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
                x86_64-apple-darwin aarch64-apple-darwin x86_64-pc-windows-msvc; do
    extension=tar.gz
    [[ "$target" == x86_64-pc-windows-msvc ]] && extension=zip
    echo "archive for $target" > "$dist/mcp-repl-${tag}-${target}.${extension}"
    (
      cd "$dist"
      sha256 "mcp-repl-${tag}-${target}.${extension}" \
        > "mcp-repl-${tag}-${target}.${extension}.sha256"
    )
  done
}

failures=0
check() {
  local name=$1 mode=$2 want_status=$3 want_text=$4 dist=$5
  local output status
  set +e
  output=$(PATH="$work/bin:$PATH" GH_MODE="$mode" "$publish" "$tag" "$dist" 2>&1)
  status=$?
  set -e

  if [[ "$status" != "$want_status" ]]; then
    printf 'FAIL %s: exit %s, wanted %s\n%s\n' "$name" "$status" "$want_status" "$output" >&2
    failures=$((failures + 1))
    return
  fi
  if [[ "$output" != *"$want_text"* ]]; then
    printf 'FAIL %s: output did not mention %q\n%s\n' "$name" "$want_text" "$output" >&2
    failures=$((failures + 1))
    return
  fi
  printf 'ok   %s\n' "$name"
}

dist="$work/dist"

# The failure that shipped: `gh` itself broke, and the guard reported the
# release's state instead of saying the lookup failed.
seed_dist "$dist"
check "unreachable gh explains itself" unreachable 1 "Could not read release" "$dist"

seed_dist "$dist"
check "an already published release is refused" published 1 "already published" "$dist"

# A build job that produced nothing used to end the run silently.
seed_dist "$dist"
rm "$dist/mcp-repl-${tag}-aarch64-apple-darwin.tar.gz"
check "a missing archive names its target" draft 1 "aarch64-apple-darwin job produced no complete package" "$dist"

seed_dist "$dist"
rm "$dist/mcp-repl-${tag}-x86_64-pc-windows-msvc.zip.sha256"
check "a missing checksum names the file" draft 1 "Missing" "$dist"

seed_dist "$dist"
touch "$dist/unexpected.txt"
check "an unexpected file is listed" draft 1 "unexpected.txt" "$dist"

# Corruption has to be caught before anything is uploaded.
seed_dist "$dist"
echo "tampered" > "$dist/mcp-repl-${tag}-x86_64-unknown-linux-gnu.tar.gz"
check "a bad checksum stops the publish" draft 1 "FAILED" "$dist"

# And the path that matters most: a complete set publishes.
seed_dist "$dist"
check "a complete set is uploaded and published" draft 0 "release edit $tag --draft=false --latest" "$dist"

if [[ "$failures" -ne 0 ]]; then
  echo "$failures release guard check(s) failed" >&2
  exit 1
fi
echo "all release guard checks passed"
