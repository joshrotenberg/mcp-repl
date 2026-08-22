#!/usr/bin/env bash
# Require the exact bot-authored release generated from this checkout's CHANGELOG.
set -euo pipefail

if [[ $# -lt 2 || $# -gt 3 ]]; then
  echo "usage: $0 <tag> <draft|published> [expected-release-id]" >&2
  exit 2
fi

tag=$1
release_state=$2
expected_id=${3:-}
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
      ( -n "$expected_id" && ! "$expected_id" =~ ^[1-9][0-9]*$ ) ]]; then
  echo "Invalid release verification arguments" >&2
  exit 2
fi

root=$(cd "$(dirname "$0")/.." && pwd)
version=${tag#v}
if ! expected_notes=$("$root/scripts/extract-release-notes.sh" "$version"); then
  echo "Could not derive canonical release notes for $tag" >&2
  exit 1
fi

if ! release=$(gh api "repos/${repository}/releases/tags/${tag}"); then
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
if [[ -n "$expected_id" && "$release_id" != "$expected_id" ]]; then
  echo "GitHub release $tag was replaced: expected $expected_id, found $release_id" >&2
  exit 1
fi
if ! jq -e \
  --arg tag "$tag" \
  --argjson expected_draft "$expected_draft" '
    type == "object" and
    .tag_name == $tag and
    .draft == $expected_draft and
    .prerelease == false and
    ($expected_draft or .immutable == true) and
    (.author.login // "") == "github-actions[bot]" and
    (.author.type // "") == "Bot"
  ' <<<"$release" > /dev/null; then
  echo "GitHub release $tag does not match the trusted $release_state boundary" >&2
  exit 1
fi

release_name=$(jq -r '.name // ""' <<<"$release")
release_body=$(jq -r '.body // ""' <<<"$release")
if [[ "$release_name" != "$tag" || "$release_body" != "$expected_notes" ]]; then
  echo "$state_label GitHub release $tag has unexpected title or notes" >&2
  exit 1
fi

printf '%s\n' "$release_id"
