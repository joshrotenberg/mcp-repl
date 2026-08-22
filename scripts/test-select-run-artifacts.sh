#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
selector="$root/scripts/select-run-artifacts.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

fail() {
  echo "run artifact test failed: $*" >&2
  exit 1
}

setup_case() {
  case_dir="$work/$1"
  downloads="$case_dir/downloads"
  output="$case_dir/output"
  mkdir -p "$downloads" "$output"
}

artifact() {
  local name=$1 file=$2 contents=$3
  mkdir -p "$downloads/$name"
  printf '%s\n' "$contents" > "$downloads/$name/$file"
}

flat_file() {
  local file=$1 contents=$2
  printf '%s\n' "$contents" > "$downloads/$file"
}

expect_failure() {
  local label=$1 diagnostic=$2
  shift 2
  if result=$("$selector" "$@" 2>&1); then
    fail "$label unexpectedly succeeded: $result"
  fi
  [[ "$result" == *"$diagnostic"* ]] ||
    fail "$label did not report '$diagnostic': $result"
}

setup_case fallback
artifact release-linux-1 archive one
"$selector" 2 "$downloads" "$output" release-linux > /dev/null
[[ $(<"$output/archive") == one ]] || fail "earlier successful attempt was not selected"

setup_case flat_first
flat_file archive one
flat_file archive.sha256 checksum
result=$("$selector" 1 "$downloads" "$output" release-linux)
[[ $(<"$output/archive") == one &&
   $(<"$output/archive.sha256") == checksum &&
   "$result" == "Selected sole flattened artifact for release-linux" ]] ||
  fail "sole first-attempt artifact was not selected from the flattened layout"

setup_case flat_prior
flat_file archive prior
"$selector" 2 "$downloads" "$output" release-linux > /dev/null
[[ $(<"$output/archive") == prior ]] ||
  fail "sole prior-attempt artifact was not selected from the flattened layout"

setup_case current
artifact release-linux-1 archive one
artifact release-linux-2 archive two
"$selector" 2 "$downloads" "$output" release-linux > /dev/null
[[ $(<"$output/archive") == two ]] || fail "current attempt did not supersede earlier output"

setup_case incomplete_newer
artifact release-linux-1 archive one
mkdir -p "$downloads/release-linux-2"
expect_failure incomplete_newer "release-linux-2 is empty" \
  2 "$downloads" "$output" release-linux
[[ ! -e "$output/archive" ]] ||
  fail "an incomplete newer attempt fell back to older bytes"

setup_case mixed
artifact native-linux-1 linux old-linux
artifact native-linux-3 linux new-linux
artifact native-macos-2 macos macos
"$selector" 3 "$downloads" "$output" native-linux native-macos > /dev/null
[[ $(<"$output/linux") == new-linux && $(<"$output/macos") == macos ]] ||
  fail "mixed producer attempts were not resolved per logical artifact"

setup_case future
artifact release-linux-3 archive future
expect_failure future "future attempt" 2 "$downloads" "$output" release-linux

setup_case missing
artifact release-linux-1 archive one
expect_failure missing "no artifact is available for release-macos" \
  2 "$downloads" "$output" release-linux release-macos

setup_case flat_multiple_bases
flat_file archive one
expect_failure flat_multiple_bases "cannot satisfy multiple artifact bases" \
  2 "$downloads" "$output" release-linux release-macos

setup_case mixed_layout
flat_file direct one
artifact release-linux-1 archive one
expect_failure mixed_layout "mixes flattened files and artifact directories" \
  1 "$downloads" "$output" release-linux

setup_case unexpected
artifact release-linux-1 archive one
artifact unrelated-1 other other
expect_failure unexpected "unexpected artifact directory" \
  1 "$downloads" "$output" release-linux

setup_case collision
artifact native-linux-1 shared linux
artifact native-macos-1 shared macos
expect_failure collision "collide on output file" \
  1 "$downloads" "$output" native-linux native-macos
[[ $(<"$output/shared") == linux ]] ||
  fail "a colliding artifact overwrote the first selected file"

setup_case linked
mkdir -p "$downloads/release-linux-1"
printf 'real\n' > "$case_dir/real"
ln -s "$case_dir/real" "$downloads/release-linux-1/archive"
expect_failure linked "unsafe, linked, empty" \
  1 "$downloads" "$output" release-linux

setup_case flat_linked
printf 'real\n' > "$case_dir/real"
ln -s "$case_dir/real" "$downloads/archive"
expect_failure flat_linked "unsafe entry" \
  1 "$downloads" "$output" release-linux

setup_case flat_empty
: > "$downloads/archive"
expect_failure flat_empty "unsafe or empty file" \
  1 "$downloads" "$output" release-linux

setup_case flat_unsafe_name
flat_file 'unsafe name' one
expect_failure flat_unsafe_name "unsafe or empty file" \
  1 "$downloads" "$output" release-linux

setup_case flat_special
mkfifo "$downloads/archive"
expect_failure flat_special "unsafe entry" \
  1 "$downloads" "$output" release-linux

setup_case empty
mkdir -p "$downloads/release-linux-1"
expect_failure empty "is empty" 1 "$downloads" "$output" release-linux

setup_case dirty_output
artifact release-linux-1 archive one
printf 'sentinel\n' > "$output/existing"
expect_failure dirty_output "output directory must be empty" \
  1 "$downloads" "$output" release-linux
[[ $(<"$output/existing") == sentinel ]] || fail "dirty output was modified"

setup_case unsafe_base
expect_failure unsafe_base "unsafe artifact base" \
  1 "$downloads" "$output" '../release'

setup_case duplicate_base
expect_failure duplicate_base "must be unique" \
  1 "$downloads" "$output" release release

echo "run artifact selection behavior tests passed"
