#!/usr/bin/env bash
# Print the one non-empty top CHANGELOG section for an exact release version.
set -euo pipefail

if [[ $# -ne 1 || ! "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "usage: $0 <version>" >&2
  exit 2
fi

version=$1
root=$(cd "$(dirname "$0")/.." && pwd)

if ! awk -v prefix="## [$version] - " '
  index($0, "## [") == 1 {
    headings++
    if (headings == 1) {
      if (index($0, prefix) != 1) invalid = 1
      else { found = 1; collecting = 1 }
      next
    }
    if (index($0, prefix) == 1) duplicate = 1
    collecting = 0
    next
  }
  collecting { lines[++count] = $0 }
  END {
    if (!found || invalid || duplicate) exit 1
    first = 1
    while (first <= count && lines[first] ~ /^[[:space:]]*$/) first++
    last = count
    while (last >= first && lines[last] ~ /^[[:space:]]*$/) last--
    if (first > last) exit 1
    for (line = first; line <= last; line++) print lines[line]
  }
' "$root/CHANGELOG.md"; then
  echo "CHANGELOG.md has no unique non-empty top section for $version" >&2
  exit 1
fi
