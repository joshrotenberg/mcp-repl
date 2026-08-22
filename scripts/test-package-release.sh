#!/usr/bin/env bash
# Exercise package-release.sh's binary identity boundary before archiving.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
packager="$root/scripts/package-release.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

fail() {
  echo "release package test failed: $*" >&2
  exit 1
}

setup_case() {
  local name=$1
  case_dir="$work/$name"
  mkdir -p "$case_dir/target/test-target/release"
  cp "$root/README.md" "$root/LICENSE-APACHE" "$root/LICENSE-MIT" "$case_dir/"
  cat > "$case_dir/target/test-target/release/mcp-repl" <<'BINARY'
#!/usr/bin/env bash
set -euo pipefail

case "${1:-}" in
  --version)
    case "${PACKAGE_BINARY_MODE:-valid}" in
      valid) printf '%s\n' 'mcp-repl 1.2.3' ;;
      nonzero) printf '%s\n' 'mcp-repl 1.2.3'; exit 9 ;;
      extra_newline) printf 'mcp-repl 1.2.3\n\n' ;;
      trailing_bytes) printf 'mcp-repl 1.2.3\ntrailing' ;;
      wrong) printf '%s\n' 'mcp-repl 9.9.9' ;;
      *) exit 10 ;;
    esac
    ;;
  --completions)
    printf '# %s completion\n' "${2:-missing}"
    ;;
  --man)
    printf '%s\n' '.TH MCP-REPL 1'
    ;;
  *) exit 2 ;;
esac
BINARY
  chmod 755 "$case_dir/target/test-target/release/mcp-repl"
}

run_package() {
  local mode=$1
  (
    cd "$case_dir"
    PACKAGE_BINARY_MODE=$mode "$packager" \
      test-target v1.2.3 tar.gz mcp-repl 1.2.3
  ) > "$case_dir/stdout" 2> "$case_dir/stderr"
}

expect_failure() {
  local mode=$1
  local diagnostic=$2
  setup_case "$mode"
  if run_package "$mode"; then
    fail "$mode unexpectedly packaged a binary"
  fi
  grep -Fq "$diagnostic" "$case_dir/stderr" ||
    fail "$mode did not report '$diagnostic'"
  [[ ! -e "$case_dir/mcp-repl-v1.2.3-test-target" ]] ||
    fail "$mode created a release stage before rejecting the binary"
}

setup_case valid
run_package valid
archive=$(<"$case_dir/stdout")
[[ "$archive" == mcp-repl-v1.2.3-test-target.tar.gz ]] ||
  fail "valid package emitted an unexpected archive name: $archive"
[[ -f "$case_dir/$archive" && -f "$case_dir/$archive.sha256" ]] ||
  fail "valid package did not produce the archive and checksum"
tar tzf "$case_dir/$archive" |
  grep -Fxq 'mcp-repl-v1.2.3-test-target/mcp-repl' ||
  fail "valid package omitted the binary"

expect_failure nonzero 'could not report its version'
expect_failure extra_newline 'does not report mcp-repl 1.2.3'
expect_failure trailing_bytes 'does not report mcp-repl 1.2.3'
expect_failure wrong 'does not report mcp-repl 1.2.3'

expect_dangling_output() {
  local name=$1
  local output_name=$2
  setup_case "$name"
  outside="$case_dir/outside-output"
  ln -s "$outside" "$case_dir/$output_name"
  if run_package valid; then
    fail "$name followed a dangling output symlink"
  fi
  grep -Fq 'release package output already exists' "$case_dir/stderr" ||
    fail "$name did not reject the existing output path"
  [[ ! -e "$outside" ]] ||
    fail "$name wrote through a dangling output symlink"
}

expect_dangling_output dangling_stage mcp-repl-v1.2.3-test-target
expect_dangling_output dangling_archive mcp-repl-v1.2.3-test-target.tar.gz
expect_dangling_output dangling_checksum mcp-repl-v1.2.3-test-target.tar.gz.sha256

echo "release package behavior tests passed"
