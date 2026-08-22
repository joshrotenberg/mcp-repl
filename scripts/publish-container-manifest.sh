#!/usr/bin/env bash
# Publish a content-addressed version manifest, refusing any existing version
# that differs, then reconcile `latest` to GitHub's current immutable release.
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "usage: $0 <vX.Y.Z> [digest-directory]" >&2
  exit 2
fi

tag=$1
digest_dir=${2:-/tmp/digests}
repository=${GH_REPO:-}
if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ||
      ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ||
      ! -d "$digest_dir" ]]; then
  echo "Invalid container publication arguments" >&2
  exit 2
fi

image_repository=$(printf '%s' "$repository" | tr '[:upper:]' '[:lower:]')
image="ghcr.io/$image_repository"
version=${tag#v}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

die() {
  echo "$*" >&2
  exit 1
}

trusted_release_tag() {
  local endpoint=$1
  local label=$2
  local release release_tag

  if ! release=$(gh api "$endpoint"); then
    die "Could not read $label"
  fi
  if ! release_tag=$(jq -er '
    select(type == "object") |
    select((.id | type) == "number" and .id > 0 and (.id | floor) == .id) |
    select((.tag_name | type) == "string") |
    select(.tag_name | test("^v[0-9]+\\.[0-9]+\\.[0-9]+$")) |
    select(.name == .tag_name) |
    select(.draft == false and .prerelease == false and .immutable == true) |
    select((.author.login // "") == "github-actions[bot]") |
    select((.author.type // "") == "Bot") |
    .tag_name
  ' <<<"$release"); then
    die "$label is not an immutable bot-owned canonical release"
  fi
  printf '%s\n' "$release_tag"
}

manifest_digest_set() {
  local raw=$1
  local label=$2

  jq -er --arg label "$label" '
    if
      type == "object" and
      (.manifests | type) == "array" and
      (.manifests | length) == 2 and
      ([.manifests[].digest] | unique | length) == 2 and
      ([.manifests[].digest] |
        all(type == "string" and test("^sha256:[0-9a-f]{64}$"))) and
      ([.manifests[].platform | (.os + "/" + .architecture)] | sort) ==
        ["linux/amd64", "linux/arm64"]
    then
      [.manifests[].digest] | sort | .[]
    else
      error($label + " is not the exact two-platform manifest")
    end
  ' <<<"$raw" 2> "$work/jq-error" || {
    sed -n '1,5p' "$work/jq-error" >&2
    die "$label is not the exact two-platform manifest"
  }
}

inspect_manifest() {
  local reference=$1
  local label=$2
  local raw

  if ! raw=$(docker buildx imagetools inspect --raw "$reference"); then
    die "Could not inspect $label"
  fi
  printf '%s\n' "$raw"
}

entries=()
while IFS= read -r -d '' entry; do
  entries+=("$entry")
done < <(find "$digest_dir" -mindepth 1 -maxdepth 1 -print0)
if [[ ${#entries[@]} -ne 2 ]]; then
  die "Expected exactly two architecture digest artifacts, found ${#entries[@]}"
fi

refs=()
expected_digests=()
for entry in "${entries[@]}"; do
  digest=${entry##*/}
  if [[ ! -f "$entry" || -L "$entry" || ! "$digest" =~ ^[0-9a-f]{64}$ ]]; then
    die "Invalid architecture digest artifact: $digest"
  fi
  refs+=("$image@sha256:$digest")
  expected_digests+=("sha256:$digest")
done
expected_set=$(printf '%s\n' "${expected_digests[@]}" | LC_ALL=C sort)

current_release_tag=$(trusted_release_tag \
  "repos/$repository/releases/tags/$tag" "GitHub release $tag")
if [[ "$current_release_tag" != "$tag" ]]; then
  die "GitHub release lookup returned $current_release_tag instead of $tag"
fi

version_ref="$image:$version"
if version_raw=$(docker buildx imagetools inspect --raw "$version_ref" \
    2> "$work/version-inspect-error"); then
  actual_set=$(manifest_digest_set "$version_raw" "Existing image $version_ref")
  if [[ "$actual_set" != "$expected_set" ]]; then
    die "Existing image $version_ref does not match this run's architecture digests"
  fi
  echo "Verified existing version manifest $version_ref"
else
  # GHCR/buildx currently reports an absent public tag in exactly this form.
  # Treat every other registry, proxy, or authentication failure as unknown
  # rather than risking a write after a false absence result.
  if ! grep -Fxq "ERROR: $version_ref: not found" \
      "$work/version-inspect-error"; then
    sed -n '1,10p' "$work/version-inspect-error" >&2
    die "Could not determine whether version manifest $version_ref exists"
  fi
  docker buildx imagetools create -t "$version_ref" "${refs[@]}"
  version_raw=$(inspect_manifest "$version_ref" "new image $version_ref")
  actual_set=$(manifest_digest_set "$version_raw" "New image $version_ref")
  if [[ "$actual_set" != "$expected_set" ]]; then
    die "New image $version_ref does not match this run's architecture digests"
  fi
  echo "Published version manifest $version_ref"
fi

# Distinct version jobs must not share a GitHub concurrency group: Actions
# coalesces pending jobs rather than maintaining a durable queue. Repeated
# reads around the write let overlapping jobs and manual publication converge
# on GitHub's current immutable release instead.
for attempt in 1 2 3 4 5; do
  latest_before=$(trusted_release_tag \
    "repos/$repository/releases/latest" "GitHub's latest release")
  latest_version_ref="$image:${latest_before#v}"
  latest_version_raw=$(inspect_manifest "$latest_version_ref" \
    "latest version image $latest_version_ref")
  latest_version_set=$(manifest_digest_set "$latest_version_raw" \
    "Latest version image $latest_version_ref")

  latest_confirmed=$(trusted_release_tag \
    "repos/$repository/releases/latest" "GitHub's latest release")
  if [[ "$latest_confirmed" != "$latest_before" ]]; then
    echo "Latest release changed from $latest_before to $latest_confirmed on attempt $attempt; retrying" >&2
    continue
  fi

  docker buildx imagetools create -t "$image:latest" "$latest_version_ref"

  latest_after=$(trusted_release_tag \
    "repos/$repository/releases/latest" "GitHub's latest release")
  if [[ "$latest_after" != "$latest_before" ]]; then
    echo "Latest release changed from $latest_before to $latest_after on attempt $attempt; retrying" >&2
    continue
  fi

  latest_raw=$(inspect_manifest "$image:latest" "image $image:latest")
  latest_set=$(manifest_digest_set "$latest_raw" "Image $image:latest")
  if [[ "$latest_set" != "$latest_version_set" ]]; then
    die "Image $image:latest does not match $latest_version_ref"
  fi
  echo "Reconciled $image:latest to immutable release $latest_before"
  exit 0
done

die "GitHub's latest release did not stabilize after five reconciliation attempts"
