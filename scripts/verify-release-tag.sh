#!/usr/bin/env bash
# Require a live release tag to resolve to the dispatch-captured source SHA.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <tag> <expected-sha>" >&2
  exit 2
fi

tag=$1
expected_sha=$2
repository=${GH_REPO:-}
if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ||
      ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ||
      ! "$expected_sha" =~ ^[0-9a-fA-F]{40}$ ]]; then
  echo "Invalid release-tag verification arguments" >&2
  exit 2
fi

# Check the tag namespace and annotated object explicitly before resolving it
# as a commit. The commits endpoint also accepts branches and lightweight tags,
# which are too broad a trust boundary for publication.
if ! tag_ref=$(gh api "repos/${repository}/git/ref/tags/${tag}"); then
  echo "Could not read release tag $tag" >&2
  exit 1
fi
if ! tag_object_sha=$(jq -er \
  --arg ref "refs/tags/$tag" '
    select(.ref == $ref) |
    select(.object.type == "tag") |
    .object.sha |
    select(type == "string" and test("^[0-9a-fA-F]{40}$"))
  ' <<<"$tag_ref"); then
  echo "Release tag $tag is not one annotated tag object" >&2
  exit 1
fi
if ! tag_object=$(gh api "repos/${repository}/git/tags/${tag_object_sha}"); then
  echo "Could not read annotated release tag $tag" >&2
  exit 1
fi
expected_message="chore: Release package mcp-repl version ${tag#v}"
expected_tagger_name="github-actions[bot]"
expected_tagger_email="41898282+github-actions[bot]@users.noreply.github.com"
if ! jq -e \
  --arg tag "$tag" \
  --arg message "$expected_message" \
  --arg expected_sha "$expected_sha" \
  --arg tagger_name "$expected_tagger_name" \
  --arg tagger_email "$expected_tagger_email" '
    .tag == $tag and
    .message == $message and
    .object.type == "commit" and
    .object.sha == $expected_sha and
    (.tagger.name // "") == $tagger_name and
    (.tagger.email // "") == $tagger_email
  ' <<<"$tag_object" > /dev/null; then
  echo "Annotated release tag $tag does not match the trusted source and message" >&2
  exit 1
fi
if ! live_sha=$(gh api "repos/${repository}/commits/${tag}" --jq .sha); then
  echo "Could not resolve release tag $tag to a commit" >&2
  exit 1
fi
if [[ ! "$live_sha" =~ ^[0-9a-fA-F]{40}$ ]]; then
  echo "Release tag $tag resolved to an invalid commit: $live_sha" >&2
  exit 1
fi
if [[ "$live_sha" != "$expected_sha" ]]; then
  echo "Release tag $tag moved: expected $expected_sha, found $live_sha" >&2
  exit 1
fi

echo "Release tag $tag still resolves to $expected_sha"
