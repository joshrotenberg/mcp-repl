#!/usr/bin/env bash
# Require the exact bot-authored release generated from this checkout's CHANGELOG.
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <tag> <draft|published> <expected-release-id>" >&2
  exit 2
fi

tag=$1
release_state=$2
expected_id=$3
repository=${GH_REPO:-}
case "$release_state" in
  draft)
    expected_draft=true
    state_label=Draft
    ;;
  published)
    expected_draft=false
    state_label=Published
    ;;
  *)
    echo "Invalid release state: $release_state" >&2
    exit 2
    ;;
esac
if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ||
      ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ||
      ! "$expected_id" =~ ^[1-9][0-9]*$ ]]; then
  echo "Invalid release verification arguments" >&2
  exit 2
fi

root=$(cd "$(dirname "$0")/.." && pwd)
version=${tag#v}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
expected_notes_file="$work/expected-notes.md"
release_body_file="$work/release-body.md"
if ! "$root/scripts/extract-release-notes.sh" "$version" > "$expected_notes_file"; then
  echo "Could not derive canonical release notes for $tag" >&2
  exit 1
fi

# GitHub's REST release-by-tag endpoint returns 404 for private drafts and is
# ambiguous when multiple drafts share a tag. Read the one discovered numeric
# identity directly for every state.
release_endpoint="repos/${repository}/releases/${expected_id}"
if ! release=$(gh api "$release_endpoint"); then
  echo "Could not read GitHub release $tag" >&2
  exit 1
fi
if ! release_id=$(jq -er '
  .id |
  select(type == "number" and . > 0 and floor == .)
' <<<"$release"); then
  echo "GitHub release $tag returned an invalid identity" >&2
  exit 1
fi
if [[ "$release_id" != "$expected_id" ]]; then
  echo "GitHub release $tag was replaced: expected $expected_id, found $release_id" >&2
  exit 1
fi
if ! jq -e \
  --arg tag "$tag" \
  --argjson expected_draft "$expected_draft" '
    type == "object" and
    .tag_name == $tag and
    .name == $tag and
    .draft == $expected_draft and
    .prerelease == false and
    ($expected_draft or .immutable == true) and
    (.author.login // "") == "github-actions[bot]" and
    (.author.type // "") == "Bot"
  ' <<<"$release" > /dev/null; then
  echo "GitHub release $tag does not match the trusted $release_state boundary" >&2
  exit 1
fi

if ! jq -jer '(.body // "") | select(type == "string")' \
  <<<"$release" > "$release_body_file" ||
   ! cmp -s "$release_body_file" "$expected_notes_file"; then
  echo "$state_label GitHub release $tag has unexpected title or notes" >&2
  exit 1
fi

printf '%s\n' "$release_id"
