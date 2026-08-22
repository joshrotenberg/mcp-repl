#!/usr/bin/env bash
# Enforce the Linux ABI promised by the authoritative release target manifest.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <target> <binary-path>" >&2
  exit 2
fi

target=$1
binary=$2
root=${RELEASE_TARGETS_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}
manifest=${RELEASE_TARGETS_FILE:-"$root/release-targets.json"}
targets_helper="$root/scripts/release-targets.sh"

fail() {
  echo "release binary verification failed for $target: $*" >&2
  exit 1
}

if [[ ! -x "$targets_helper" ]]; then
  fail "$targets_helper is not executable"
fi
if ! "$targets_helper" validate; then
  fail "release target manifest validation failed"
fi
if [[ ! -f "$binary" || -L "$binary" || ! -x "$binary" ]]; then
  fail "$binary must be a regular, non-symlinked executable"
fi

if ! row=$(jq -cer --arg target "$target" '
  [.native[] | select(.target == $target)] |
  if length == 1 then .[0] else error("unknown release target") end
' "$manifest"); then
  fail "target is not a unique manifest entry"
fi
os=$(jq -r '.os' <<<"$row")
arch=$(jq -r '.arch' <<<"$row")
libc=$(jq -r '.libc // ""' <<<"$row")
if [[ "$os" != linux || ( "$libc" != gnu && "$libc" != musl ) ]]; then
  fail "ABI verification only applies to manifest Linux GNU and musl targets"
fi

if command -v readelf > /dev/null 2>&1; then
  readelf_command=readelf
elif command -v llvm-readelf > /dev/null 2>&1; then
  readelf_command=llvm-readelf
else
  fail "readelf or llvm-readelf is required"
fi

if ! header=$(LC_ALL=C "$readelf_command" -hW "$binary" 2>&1); then
  fail "readelf could not parse $binary as ELF: $header"
fi
case "$arch" in
  x86_64)
    if ! grep -Eq 'Machine:[[:space:]]+(Advanced Micro Devices X86-64|X86-64)' \
      <<<"$header"; then
      fail "ELF machine does not match manifest architecture x86_64"
    fi
    ;;
  aarch64)
    if ! grep -Eq 'Machine:[[:space:]]+AArch64' <<<"$header"; then
      fail "ELF machine does not match manifest architecture aarch64"
    fi
    ;;
  *)
    fail "manifest contains an unsupported Linux architecture: $arch"
    ;;
esac

if [[ "$libc" == musl ]]; then
  if ! program_headers=$(LC_ALL=C "$readelf_command" -lW "$binary" 2>&1); then
    fail "could not inspect ELF program headers: $program_headers"
  fi
  if awk '$1 == "INTERP" { found = 1 } END { exit !found }' \
    <<<"$program_headers"; then
    fail "musl artifact is dynamically launched: ELF INTERP is present"
  fi

  if ! dynamic_section=$(LC_ALL=C "$readelf_command" -dW "$binary" 2>&1); then
    fail "could not inspect ELF dynamic dependencies: $dynamic_section"
  fi
  if grep -Eq '\(NEEDED\)' <<<"$dynamic_section"; then
    fail "musl artifact is dynamically linked: ELF NEEDED is present"
  fi

  echo "Verified static musl ABI for $target"
  exit 0
fi

if ! version_info=$(LC_ALL=C "$readelf_command" --version-info --wide \
  "$binary" 2>&1); then
  fail "could not inspect ELF symbol versions: $version_info"
fi
glibc_requirements=$(grep -Eo 'GLIBC_[A-Za-z0-9_]+(\.[A-Za-z0-9_]+)*' \
  <<<"$version_info" | LC_ALL=C sort -u || true)
if [[ -z "$glibc_requirements" ]]; then
  fail "GNU artifact declares no numeric GLIBC symbol versions"
fi
unsupported_glibc=$(grep -Ev '^GLIBC_[0-9]+(\.[0-9]+)+$' \
  <<<"$glibc_requirements" || true)
if [[ -n "$unsupported_glibc" ]]; then
  unsupported_glibc_list=$(paste -sd, - <<<"$unsupported_glibc")
  fail "declares unsupported nonnumeric GLIBC requirements: $unsupported_glibc_list"
fi
glibc_versions=${glibc_requirements//GLIBC_/}
glibc_ceiling=$(jq -r '.linux.gnu_max_glibc' "$manifest")

version_greater_than() {
  awk -v left="$1" -v right="$2" '
    BEGIN {
      left_count = split(left, left_parts, ".")
      right_count = split(right, right_parts, ".")
      count = left_count > right_count ? left_count : right_count
      for (part_index = 1; part_index <= count; part_index++) {
        left_value = part_index <= left_count ? left_parts[part_index] + 0 : 0
        right_value = part_index <= right_count ? right_parts[part_index] + 0 : 0
        if (left_value > right_value) exit 0
        if (left_value < right_value) exit 1
      }
      exit 1
    }
  '
}

while IFS= read -r required_glibc; do
  if version_greater_than "$required_glibc" "$glibc_ceiling"; then
    fail "requires GLIBC_$required_glibc, newer than manifest ceiling GLIBC_$glibc_ceiling"
  fi
done <<<"$glibc_versions"

echo "Verified GNU ABI at or below GLIBC_$glibc_ceiling for $target"
