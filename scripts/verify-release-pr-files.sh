#!/usr/bin/env bash
# Require release-plz to have changed only its three generated release files.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <owner/repository> <pr-number>" >&2
  exit 2
fi

repository=$1
pr_number=$2
if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ||
      ! "$pr_number" =~ ^[1-9][0-9]*$ ]]; then
  echo "Invalid release-PR file verification arguments" >&2
  exit 2
fi

if ! changed_files=$(gh api --paginate \
  "repos/${repository}/pulls/${pr_number}/files?per_page=100" \
  --jq '.[].filename'); then
  echo "Could not list files for release PR #$pr_number" >&2
  exit 1
fi

actual_files=$(printf '%s\n' "$changed_files" | sed '/^$/d' | LC_ALL=C sort -u)
expected_files=$'CHANGELOG.md\nCargo.lock\nCargo.toml'
if [[ "$actual_files" != "$expected_files" ]]; then
  echo "Release PR #$pr_number changes an unexpected file set:" >&2
  printf '%s\n' "$actual_files" >&2
  exit 1
fi

echo "Release PR #$pr_number changes exactly the generated release files"
