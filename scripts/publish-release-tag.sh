#!/usr/bin/env bash
# Create or verify the canonical annotated release tag at the final boundary.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <tag> <source-sha>" >&2
  exit 2
fi

tag=$1
source_sha=$2
repository=${GH_REPO:-}
github_token=${GH_TOKEN:-}

if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ||
      ! "$tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ||
      ! "$source_sha" =~ ^[0-9a-f]{40}$ ||
      -z "$github_token" ]]; then
  echo "Invalid release-tag publication arguments" >&2
  exit 2
fi

for required_command in cargo gh git jq; do
  command -v "$required_command" > /dev/null 2>&1 || {
    echo "$required_command is required" >&2
    exit 1
  }
done

root=$(cd "$(dirname "$0")/.." && pwd)
checked_out_sha=$(git -C "$root" rev-parse --verify HEAD)
if [[ "$checked_out_sha" != "$source_sha" ]]; then
  echo "Checkout $checked_out_sha does not match release source $source_sha" >&2
  exit 1
fi

metadata=$(cargo metadata --locked --no-deps --format-version 1)
if ! version=$(jq -er '
  [.packages[] | select(.name == "mcp-repl") | .version] |
  if length == 1 then .[0] else error("mcp-repl package is not unique") end |
  select(type == "string")
' <<<"$metadata"); then
  echo "Cargo returned invalid release metadata" >&2
  exit 1
fi
if [[ "$tag" != "v$version" ]]; then
  echo "Release tag $tag does not match Cargo version $version" >&2
  exit 1
fi

tag_message="chore: Release package mcp-repl version $version"
tagger_name="github-actions[bot]"
tagger_email="41898282+github-actions[bot]@users.noreply.github.com"

if ! tag_refs=$(gh api "repos/${repository}/git/matching-refs/tags/${tag}"); then
  echo "Could not list GitHub refs matching $tag" >&2
  exit 1
fi
if ! exact_tag_refs=$(jq -ce \
  --arg ref "refs/tags/$tag" '
    if type != "array" or any(.[]; type != "object" or (.ref | type) != "string")
    then error("tag refs are malformed")
    else [.[] | select(.ref == $ref)]
    end
  ' <<<"$tag_refs"); then
  echo "GitHub returned malformed tag-ref data" >&2
  exit 1
fi

tag_count=$(jq -r 'length' <<<"$exact_tag_refs")
case "$tag_count" in
  0)
    if ! tag_object=$(gh api \
      --method POST \
      "repos/${repository}/git/tags" \
      -f tag="$tag" \
      -f message="$tag_message" \
      -f object="$source_sha" \
      -f type=commit \
      -f "tagger[name]=$tagger_name" \
      -f "tagger[email]=$tagger_email"); then
      echo "Could not create annotated tag object for $tag" >&2
      exit 1
    fi
    if ! tag_object_sha=$(jq -er \
      --arg tag "$tag" \
      --arg message "$tag_message" \
      --arg source_sha "$source_sha" \
      --arg tagger_name "$tagger_name" \
      --arg tagger_email "$tagger_email" '
        select(type == "object") |
        select(.tag == $tag and .message == $message) |
        select(.object.type == "commit" and .object.sha == $source_sha) |
        select(.tagger.name == $tagger_name and .tagger.email == $tagger_email) |
        .sha |
        select(type == "string" and test("^[0-9a-f]{40}$"))
      ' <<<"$tag_object"); then
      echo "GitHub returned a noncanonical annotated tag object for $tag" >&2
      exit 1
    fi
    if ! gh api \
      --method POST \
      "repos/${repository}/git/refs" \
      -f ref="refs/tags/$tag" \
      -f sha="$tag_object_sha" \
      > /dev/null; then
      echo "Could not create release tag $tag at $source_sha" >&2
      exit 1
    fi
    echo "Created release tag $tag at $source_sha"
    ;;
  1) ;;
  *)
    echo "GitHub returned multiple exact refs for $tag" >&2
    exit 1
    ;;
esac

GH_REPO="$repository" "$root/scripts/verify-release-tag.sh" "$tag" "$source_sha"
