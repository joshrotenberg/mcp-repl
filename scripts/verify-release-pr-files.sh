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

if ! file_pages=$(gh api --paginate --slurp \
  "repos/${repository}/pulls/${pr_number}/files?per_page=100"); then
  echo "Could not list files for release PR #$pr_number" >&2
  exit 1
fi
if ! changed_files=$(jq -ce '
  if type != "array" or any(.[]; type != "array")
  then error("file pages are malformed")
  else [.[][]]
  end
' <<<"$file_pages"); then
  echo "GitHub returned malformed release-PR file data" >&2
  exit 1
fi
if ! jq -e '
  length == 3 and
  all(.[];
    type == "object" and
    (.filename | type == "string") and
    .status == "modified" and
    (has("previous_filename") | not)) and
  ([.[].filename] | sort) == ["CHANGELOG.md", "Cargo.lock", "Cargo.toml"]
' <<<"$changed_files" > /dev/null; then
  echo "Release PR #$pr_number changes an unexpected file set:" >&2
  jq -c '[.[] | {filename, status, previous_filename}]' \
    <<<"$changed_files" >&2 || true
  exit 1
fi

echo "Release PR #$pr_number changes exactly the generated release files"
