#!/usr/bin/env bash
# Verify a complete set of release archives and publish the draft release.
#
#   scripts/publish-release.sh <tag> [dist-directory]
#
# Refuses unless every target produced an archive and a checksum, the
# directory holds exactly those files, and each checksum verifies. Only then
# are the assets uploaded and the draft made public, so a failed target
# cannot expose a partial binary release.
#
# Lives here rather than inline in the workflow so the failure paths can be
# exercised without cutting a release: see scripts/test-release-guards.sh.
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "usage: $0 <tag> [dist-directory]" >&2
  exit 2
fi

tag=$1
dist=${2:-dist}
repository=${GH_REPO:-}
root=$(cd "$(dirname "$0")/.." && pwd)
if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ||
      ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Invalid release publication environment" >&2
  exit 2
fi

"$root/scripts/release-targets.sh" validate
target_rows=()
while IFS= read -r row; do
  target_rows+=("$row")
done < <("$root/scripts/release-targets.sh" rows)
expected_asset_names=()
while IFS= read -r asset_name; do
  expected_asset_names+=("$asset_name")
done < <("$root/scripts/release-targets.sh" expected-assets "$tag")
expected_count=${#expected_asset_names[@]}
if [[ "${#target_rows[@]}" -eq 0 ||
      "$expected_count" -ne $((${#target_rows[@]} * 2)) ]]; then
  echo "Release target manifest produced an inconsistent asset set" >&2
  exit 1
fi

assets=()
verify_local_assets() {
  local directory=$1
  local target extension archive required archive_name checksum_line
  local checksum_lines recorded_checksum actual_checksum
  local entry_count actual_entries expected_entries

  assets=()
  local row _binary
  for row in "${target_rows[@]}"; do
    IFS=$'\t' read -r target extension _binary <<<"$row"
    archive="${directory}/mcp-repl-${tag}-${target}.${extension}"
    # A bare test would exit through `set -e` and say nothing at all, leaving a
    # log that ends in a bare failure with no cause named anywhere.
    for required in "$archive" "$archive.sha256"; do
      if [[ ! -f "$required" || -L "$required" ]]; then
        echo "Missing $required; the $target job produced no complete package" >&2
        return 1
      fi
    done

    archive_name=${archive##*/}
    checksum_line=$(<"$archive.sha256")
    checksum_lines=$(wc -l < "$archive.sha256" | tr -d '[:space:]')
    recorded_checksum=${checksum_line:0:64}
    if [[ "$checksum_lines" != 1 ||
          ! "$recorded_checksum" =~ ^[0-9a-f]{64}$ ||
          "${checksum_line:64:2}" != "  " ||
          "${checksum_line:66}" != "$archive_name" ]]; then
      echo "$archive.sha256 does not identify its own archive exactly" >&2
      return 1
    fi
    if command -v sha256sum > /dev/null 2>&1; then
      actual_checksum=$(sha256sum "$archive" | awk '{print $1}')
    else
      actual_checksum=$(shasum -a 256 "$archive" | awk '{print $1}')
    fi
    if [[ "$actual_checksum" != "$recorded_checksum" ]]; then
      echo "$archive_name: FAILED" >&2
      return 1
    fi
    assets+=("$archive" "$archive.sha256")
  done

  entry_count=$(find "$directory" -mindepth 1 -maxdepth 1 | wc -l | tr -d '[:space:]')
  actual_entries=$(find "$directory" -mindepth 1 -maxdepth 1 \
    -exec basename {} \; | LC_ALL=C sort)
  expected_entries=$(printf '%s\n' "${expected_asset_names[@]}" | LC_ALL=C sort)
  if [[ "$entry_count" != "$expected_count" ||
        "$actual_entries" != "$expected_entries" ]]; then
    echo "Expected exactly $expected_count release files, found $entry_count:" >&2
    find "$directory" -mindepth 1 -maxdepth 1 | LC_ALL=C sort >&2
    return 1
  fi
}

remote_assets=
load_remote_assets() {
  local expected_release_id=$1
  local asset_pages remote_count remote_names
  local expected_names

  if ! asset_pages=$(gh api --paginate --slurp \
    "repos/${repository}/releases/${expected_release_id}/assets?per_page=100"); then
    echo "Could not list assets on exact GitHub release $tag (id $expected_release_id)" >&2
    return 1
  fi
  if ! remote_assets=$(jq -ce '
    if type != "array" or
       any(.[]; type != "array") or
       any(.[][]; type != "object")
    then error("asset pages are malformed")
    else [.[][]]
    end
  ' <<<"$asset_pages"); then
    echo "GitHub returned malformed asset data for release $tag" >&2
    return 1
  fi

  remote_count=$(jq -r 'length' <<<"$remote_assets")
  expected_names=$(printf '%s\n' "${expected_asset_names[@]}" | LC_ALL=C sort)
  remote_names=$(jq -r '.[].name | select(type == "string")' \
    <<<"$remote_assets" | LC_ALL=C sort)
  if [[ "$remote_count" != "$expected_count" ||
        "$remote_names" != "$expected_names" ]]; then
    echo "Remote release assets are not the exact expected $expected_count-file set:" >&2
    jq -r '.[].name // "<missing name>"' <<<"$remote_assets" | LC_ALL=C sort >&2
    return 1
  fi
}

verify_remote_asset_content() {
  local asset asset_name asset_checksum asset_size

  for asset in "${assets[@]}"; do
    asset_name=${asset##*/}
    if command -v sha256sum > /dev/null 2>&1; then
      asset_checksum=$(sha256sum "$asset" | awk '{print $1}')
    else
      asset_checksum=$(shasum -a 256 "$asset" | awk '{print $1}')
    fi
    asset_size=$(wc -c < "$asset" | tr -d '[:space:]')
    if [[ ! "$asset_checksum" =~ ^[0-9a-f]{64}$ ||
          ! "$asset_size" =~ ^[0-9]+$ ]]; then
      echo "Could not derive local identity for $asset_name" >&2
      return 1
    fi
    if ! jq -e \
      --arg name "$asset_name" \
      --arg digest "sha256:$asset_checksum" \
      --argjson size "$asset_size" '
        [.[] | select(.name == $name)] as $matches |
        ($matches | length) == 1 and
        $matches[0].state == "uploaded" and
        $matches[0].digest == $digest and
        $matches[0].size == $size
      ' <<<"$remote_assets" > /dev/null; then
      echo "Remote asset identity does not match local $asset_name" >&2
      return 1
    fi
  done
}

# Resolve visibility before looking at the caller's files. A complete,
# already-public immutable release is an idempotent success: download and
# verify its own exact assets so a full recovery rerun can reach container jobs
# without assuming freshly rebuilt native archives are byte-reproducible.
if ! release_snapshot=$(gh api "repos/${repository}/releases/tags/${tag}"); then
  echo "Could not read GitHub release $tag" >&2
  exit 1
fi
if ! is_draft=$(jq -er '
  if (.draft | type) == "boolean" then (.draft | tostring)
  else error("draft state is not boolean")
  end
' \
  <<<"$release_snapshot"); then
  echo "GitHub release $tag returned an invalid draft state" >&2
  exit 1
fi
if [[ "$is_draft" == false ]]; then
  release_id=$("$root/scripts/verify-release.sh" "$tag" published)
  load_remote_assets "$release_id"
  public_assets=$(mktemp -d)
  trap 'rm -rf "$public_assets"' EXIT INT TERM
  if ! gh release download "$tag" \
    --repo "$repository" \
    --dir "$public_assets"; then
    echo "Could not download assets from published GitHub release $tag" >&2
    exit 1
  fi
  verify_local_assets "$public_assets"
  verify_remote_asset_content
  echo "Verified already-published exact GitHub release $tag (id $release_id)"
  exit 0
fi

# A draft must match the complete freshly built local set before any upload.
verify_local_assets "$dist"

# Bind both sides of the upload to the same exact bot-owned draft. If an
# upload fails, the release stays private and a rerun safely replaces any
# assets that reached it before the failure.
release_id=$("$root/scripts/verify-release.sh" "$tag" draft)
if ! gh release upload "$tag" "${assets[@]}" --clobber; then
  echo "Could not upload the complete asset set to release $tag" >&2
  exit 1
fi
"$root/scripts/verify-release.sh" "$tag" draft "$release_id" > /dev/null

# `gh release upload --clobber` replaces expected names but deliberately leaves
# unrelated assets alone. Bind the complete remote release—not merely the
# local directory—to the manifest-derived exact set before changing visibility.
load_remote_assets "$release_id"
verify_remote_asset_content

if ! gh api --method PATCH \
  "repos/${repository}/releases/${release_id}" \
  -F draft=false \
  -f make_latest=legacy \
  > /dev/null; then
  echo "Could not publish exact GitHub release $tag (id $release_id)" >&2
  exit 1
fi
"$root/scripts/verify-release.sh" "$tag" published "$release_id" > /dev/null
echo "Published exact GitHub release $tag (id $release_id)"
