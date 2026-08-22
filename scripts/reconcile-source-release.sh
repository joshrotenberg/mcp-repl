#!/usr/bin/env bash
# Finish or verify the registry -> tag -> draft sequence after cargo publish.
#
# Registry upload is intentionally isolated from GitHub write credentials. If
# an external service fails after accepting the crate, this separately
# credentialed step makes the partial state recoverable without ever
# republishing or moving a tag.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <source-sha> <release-action-outcome>" >&2
  exit 2
fi

source_sha=$1
release_outcome=$2
repository=${GITHUB_REPOSITORY:-}
github_token=${GH_TOKEN:-}
unset GH_TOKEN

if [[ ! "$source_sha" =~ ^[0-9a-fA-F]{40}$ ||
      ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ||
      -z "$github_token" ]]; then
  echo "Invalid source-release reconciliation environment" >&2
  exit 2
fi
case "$release_outcome" in
  success | failure) ;;
  *)
    echo "Invalid release action outcome: $release_outcome" >&2
    exit 2
    ;;
esac

max_attempts=${SOURCE_RELEASE_MAX_ATTEMPTS:-12}
retry_delay=${SOURCE_RELEASE_RETRY_DELAY_SECONDS:-5}
if [[ ! "$max_attempts" =~ ^[1-9][0-9]*$ ||
      ! "$retry_delay" =~ ^[0-9]+$ ||
      "$max_attempts" -gt 60 || "$retry_delay" -gt 60 ]]; then
  echo "Invalid source-release retry policy" >&2
  exit 2
fi

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

checked_out_sha=$(git rev-parse HEAD)
if [[ "$checked_out_sha" != "$source_sha" ]]; then
  echo "Checkout $checked_out_sha does not match release source $source_sha" >&2
  exit 1
fi

metadata=$(cargo metadata --locked --no-deps --format-version 1)
version=$(jq -er '.packages[] | select(.name == "mcp-repl") | .version' <<<"$metadata")
target_dir=$(jq -er '.target_directory' <<<"$metadata")
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ || -z "$target_dir" ]]; then
  echo "Cargo returned invalid release metadata" >&2
  exit 1
fi
tag="v$version"
tag_message="chore: Release package mcp-repl version $version"
tagger_name="github-actions[bot]"
tagger_email="41898282+github-actions[bot]@users.noreply.github.com"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
notes_file="$work/release-notes.md"
registry_response="$work/registry.json"

if ! "$root/scripts/extract-release-notes.sh" "$version" > "$notes_file"; then
  echo "Could not derive canonical release notes for $version" >&2
  exit 1
fi

# Build the exact upload artifact without executing package code on the runner
# that later receives a GitHub write token. Compilation already passed on a
# separate credential-free runner; this archive is the resumed-upload identity.
cargo package --locked --no-verify
crate_file="$target_dir/package/mcp-repl-$version.crate"
if [[ ! -f "$crate_file" ]]; then
  echo "cargo package did not produce $crate_file" >&2
  exit 1
fi
vcs_info="$work/.cargo_vcs_info.json"
if ! tar -xOf "$crate_file" \
  "mcp-repl-$version/.cargo_vcs_info.json" > "$vcs_info"; then
  echo "source package does not contain Cargo VCS metadata" >&2
  exit 1
fi
if ! jq -e \
  --arg source_sha "$source_sha" '
    .git.sha1 == $source_sha and .git.dirty == false
  ' "$vcs_info" > /dev/null; then
  echo "source package VCS metadata does not identify clean commit $source_sha" >&2
  exit 1
fi
if command -v sha256sum > /dev/null 2>&1; then
  local_checksum=$(sha256sum "$crate_file" | awk '{print $1}')
else
  local_checksum=$(shasum -a 256 "$crate_file" | awk '{print $1}')
fi
if [[ ! "$local_checksum" =~ ^[0-9a-f]{64}$ ]]; then
  echo "Could not compute the source package checksum" >&2
  exit 1
fi

registry_state=missing
for ((attempt = 1; attempt <= max_attempts; attempt++)); do
  curl_status=0
  http_status=$(curl \
    --silent \
    --show-error \
    --location \
    --connect-timeout 10 \
    --max-time 30 \
    --user-agent "mcp-repl-release/$version" \
    --output "$registry_response" \
    --write-out '%{http_code}' \
    "https://crates.io/api/v1/crates/mcp-repl/$version") || curl_status=$?

  if [[ "$curl_status" -eq 0 && "$http_status" == 200 ]]; then
    registry_state=present
    break
  fi
  if [[ "$curl_status" -ne 0 || "$http_status" != 404 ]]; then
    if (( attempt == max_attempts )); then
      echo "Could not verify mcp-repl $version on crates.io (curl $curl_status, HTTP $http_status)" >&2
      exit 1
    fi
  elif (( attempt == max_attempts )); then
    break
  fi
  sleep "$retry_delay"
done

if [[ "$registry_state" != present ]]; then
  echo "cargo publish finished with $release_outcome but mcp-repl $version is not on crates.io" >&2
  exit 1
fi
if ! registry_checksum=$(jq -er \
  --arg version "$version" '
    select(.version.crate == "mcp-repl") |
    select(.version.num == $version) |
    select(.version.yanked == false) |
    .version.checksum |
    select(type == "string" and test("^[0-9a-f]{64}$"))
  ' "$registry_response"); then
  echo "crates.io returned invalid or yanked metadata for mcp-repl $version" >&2
  exit 1
fi
if [[ "$registry_checksum" != "$local_checksum" ]]; then
  echo "crates.io checksum for mcp-repl $version does not match this release commit" >&2
  exit 1
fi

if ! tag_refs=$(GH_TOKEN="$github_token" gh api \
  "repos/${repository}/git/matching-refs/tags/${tag}"); then
  echo "Could not list GitHub refs matching $tag" >&2
  exit 1
fi
if ! exact_tag_refs=$(jq -ce \
  --arg ref "refs/tags/$tag" '
    if type != "array" then error("tag refs are not an array")
    else [.[] | select(.ref == $ref)]
    end
  ' <<<"$tag_refs"); then
  echo "GitHub returned malformed tag-ref data" >&2
  exit 1
fi
tag_count=$(jq -r 'length' <<<"$exact_tag_refs")
case "$tag_count" in
  0)
    if ! tag_object=$(GH_TOKEN="$github_token" gh api \
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
        select(.tag == $tag) |
        select(.message == $message) |
        select(.object.type == "commit") |
        select(.object.sha == $source_sha) |
        select(.tagger.name == $tagger_name) |
        select(.tagger.email == $tagger_email) |
        .sha |
        select(type == "string" and test("^[0-9a-fA-F]{40}$"))
      ' \
      <<<"$tag_object"); then
      echo "GitHub returned a noncanonical annotated tag object for $tag" >&2
      exit 1
    fi
    if ! GH_TOKEN="$github_token" gh api \
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

GH_TOKEN="$github_token" GH_REPO="$repository" \
  "$root/scripts/verify-release-tag.sh" "$tag" "$source_sha"

read_releases() {
  if ! release_pages=$(GH_TOKEN="$github_token" gh api --paginate --slurp \
    "repos/${repository}/releases?per_page=100"); then
    echo "Could not list GitHub releases" >&2
    return 1
  fi
  if ! matching_releases=$(jq -ce \
    --arg tag "$tag" '
      if type != "array" or any(.[]; type != "array")
      then error("release pages are malformed")
      else [.[][] | select(.tag_name == $tag)]
      end
    ' <<<"$release_pages"); then
    echo "GitHub returned malformed release data" >&2
    return 1
  fi
}

read_releases
release_count=$(jq -r 'length' <<<"$matching_releases")
case "$release_count" in
  0)
    if ! GH_TOKEN="$github_token" gh release create "$tag" \
      --repo "$repository" \
      --title "$tag" \
      --notes-file "$notes_file" \
      --draft \
      --verify-tag; then
      echo "Could not create draft GitHub release $tag" >&2
      exit 1
    fi
    echo "Created draft GitHub release $tag"
    read_releases
    release_count=$(jq -r 'length' <<<"$matching_releases")
    ;;
  1) ;;
  *)
    echo "GitHub returned multiple releases for $tag" >&2
    exit 1
    ;;
esac

if [[ "$release_count" != 1 ]]; then
  echo "GitHub did not retain exactly one release for $tag" >&2
  exit 1
fi

if ! release_is_draft=$(jq -er '
  .[0] |
  if type != "object" then error("release is not an object")
  elif (.draft | type) != "boolean" then error("draft state is not boolean")
  else (.draft | tostring)
  end
' <<<"$matching_releases"); then
  echo "GitHub release $tag returned an invalid draft state" >&2
  exit 1
fi

case "$release_is_draft" in
  true)
    GH_TOKEN="$github_token" GH_REPO="$repository" \
      "$root/scripts/verify-release.sh" "$tag" draft > /dev/null
    echo "Draft GitHub release $tag is ready for native assets"
    ;;
  false)
    # A retry can arrive after native publication completed. Accept that state
    # only if the immutable release and its complete downloadable asset set
    # pass the same content-bound recovery verification used by the binary
    # publisher. This lets downstream jobs proceed without trusting a merely
    # public, incomplete, or manually replaced release.
    GH_TOKEN="$github_token" GH_REPO="$repository" \
      "$root/scripts/publish-release.sh" "$tag" > /dev/null
    echo "Published immutable GitHub release $tag is complete and ready for downstream recovery"
    ;;
  *)
    echo "GitHub release $tag returned an invalid draft state: $release_is_draft" >&2
    exit 1
    ;;
esac

if [[ "$release_outcome" == failure ]]; then
  echo "Recovered the safe external state after cargo publish reported failure"
fi
