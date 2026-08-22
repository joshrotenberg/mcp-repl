#!/usr/bin/env bash
# Stage, finalize, or verify one exact GitHub binary release.
#
#   scripts/publish-release.sh stage <tag> [release-directory]
#   scripts/publish-release.sh finalize <tag> [release-directory]
#   scripts/publish-release.sh verify <tag> [release-directory]
#
# A release directory is the durable, consumer-facing output of the release
# pipeline: five files for every native target, three container evidence files,
# and the canonical release record. Draft retries never replace assets. They
# are accepted only after every remote asset has been downloaded and compared
# byte for byte with this exact local set.
set -euo pipefail

usage() {
  cat >&2 <<EOF
usage: $0 <stage|finalize|verify> <tag> [release-directory]
EOF
  exit 2
}

[[ $# -ge 2 && $# -le 3 ]] || usage

mode=$1
tag=$2
dist=${3:-dist}
repository=${GH_REPO:-}
root=$(cd "$(dirname "$0")/.." && pwd)
release_targets="$root/scripts/release-targets.sh"
verify_release="$root/scripts/verify-release.sh"
verify_release_tag="$root/scripts/verify-release-tag.sh"
package=mcp-repl
semver_component='(0|[1-9][0-9]*)'

case "$mode" in
  stage | finalize | verify) ;;
  *) usage ;;
esac
if [[ ! "$repository" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*$ ||
      ${#repository} -gt 201 ||
      ! "$tag" =~ ^v${semver_component}\.${semver_component}\.${semver_component}$ ||
      ${#tag} -gt 64 ]]; then
  echo "Invalid release publication arguments" >&2
  exit 2
fi
if [[ ! -d "$dist" || -L "$dist" ]]; then
  echo "Release directory must be a non-symlink directory: $dist" >&2
  exit 2
fi
dist=$(cd "$dist" && pwd -P)

fail() {
  echo "release-publisher: $*" >&2
  exit 1
}

for required_command in gh jq find sort cmp wc awk grep git; do
  command -v "$required_command" > /dev/null 2>&1 ||
    fail "$required_command is required"
done
if command -v sha256sum > /dev/null 2>&1; then
  sha256_command=sha256sum
elif command -v shasum > /dev/null 2>&1; then
  sha256_command=shasum
else
  fail "sha256sum or shasum is required"
fi

"$release_targets" validate > /dev/null ||
  fail "release target manifest validation failed"

work=$(mktemp -d)
cleanup() {
  local status=$?
  rm -rf "$work"
  return "$status"
}
trap cleanup EXIT INT TERM

expected_names_file="$work/expected-names"
identity_names_file="$work/identity-names"
: > "$expected_names_file"
: > "$identity_names_file"
expected_asset_names=()
identity_asset_names=()
target_rows=()
container_platforms_file="$work/container-platforms"
: > "$container_platforms_file"

# `expected-release-assets` is the single authoritative publication inventory.
# The row query below is retained only for checksum and release-record mapping.
while IFS= read -r name; do
  [[ -n "$name" ]] || fail "release target manifest emitted an empty asset name"
  expected_asset_names+=("$name")
  printf '%s\n' "$name" >> "$expected_names_file"
done < <("$release_targets" expected-release-assets "$tag")

while IFS= read -r row; do
  [[ -n "$row" ]] || fail "release target manifest emitted an empty native row"
  target_rows+=("$row")
  IFS=$'\t' read -r target extension binary <<<"$row"
  if [[ -z "$target" || -z "$extension" || -z "$binary" ]]; then
    fail "release target manifest emitted an incomplete native row"
  fi
done < <("$release_targets" rows)

[[ ${#target_rows[@]} -gt 0 ]] || fail "release target manifest has no native rows"
while IFS= read -r platform; do
  [[ "$platform" =~ ^[a-z0-9][a-z0-9._-]*/[a-z0-9][a-z0-9._-]*$ ]] ||
    fail "release target manifest emitted an unsafe container platform"
  printf '%s\n' "$platform" >> "$container_platforms_file"
done < <("$release_targets" container-platforms)
LC_ALL=C sort "$container_platforms_file" -o "$container_platforms_file"
[[ -s "$container_platforms_file" ]] ||
  fail "release target manifest has no container platforms"
record_name="$package-$tag-release.json"
for name in "${expected_asset_names[@]}"; do
  if [[ "$name" != "$record_name" ]]; then
    identity_asset_names+=("$name")
    printf '%s\n' "$name" >> "$identity_names_file"
  fi
done

expected_count=${#expected_asset_names[@]}
identity_count=${#identity_asset_names[@]}
if [[ "$expected_count" -ne $((${#target_rows[@]} * 5 + 4)) ||
      "$identity_count" -ne $((expected_count - 1)) ]]; then
  fail "release target manifest produced an inconsistent durable asset set"
fi
LC_ALL=C sort "$expected_names_file" -o "$expected_names_file"
LC_ALL=C sort "$identity_names_file" -o "$identity_names_file"
if [[ $(uniq "$expected_names_file" | wc -l | tr -d '[:space:]') -ne "$expected_count" ]]; then
  fail "release target manifest produced duplicate durable asset names"
fi

sha256_file() {
  local path=$1 digest
  if [[ "$sha256_command" == sha256sum ]]; then
    digest=$(sha256sum "$path" | awk '{print $1}')
  else
    digest=$(shasum -a 256 "$path" | awk '{print $1}')
  fi
  [[ "$digest" =~ ^[0-9a-f]{64}$ ]] ||
    fail "could not compute canonical SHA-256 for ${path##*/}"
  printf '%s\n' "$digest"
}

file_size() {
  local path=$1 size
  size=$(wc -c < "$path" | tr -d '[:space:]')
  [[ "$size" =~ ^[1-9][0-9]*$ ]] ||
    fail "could not compute a nonzero size for ${path##*/}"
  printf '%s\n' "$size"
}

verify_identity() {
  local identity=$1 path=$2 expected_name=$3
  local actual_digest actual_size
  actual_digest=$(sha256_file "$path")
  actual_size=$(file_size "$path")
  if ! jq -e \
    --arg name "$expected_name" \
    --arg sha256 "$actual_digest" \
    --argjson size "$actual_size" '
      type == "object" and
      ((keys | sort) == (["name", "sha256", "size"] | sort)) and
      .name == $name and .sha256 == $sha256 and .size == $size
    ' <<<"$identity" > /dev/null; then
    fail "release record identity does not match $expected_name"
  fi
}

verify_directory_inventory() {
  local directory=$1 label=$2
  local actual_names="$work/$label-names" entry name
  : > "$actual_names"
  while IFS= read -r -d '' entry; do
    name=${entry##*/}
    if [[ ! "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ||
          ! -f "$entry" || -L "$entry" || ! -s "$entry" ]]; then
      fail "$label contains an unsafe, linked, empty, or non-file entry: $name"
    fi
    printf '%s\n' "$name" >> "$actual_names"
  done < <(find "$directory" -mindepth 1 -maxdepth 1 -print0)
  LC_ALL=C sort "$actual_names" -o "$actual_names"
  if ! cmp -s "$actual_names" "$expected_names_file"; then
    echo "release-publisher: $label does not contain the exact expected $expected_count-file set" >&2
    echo "expected:" >&2
    sed 's/^/  /' "$expected_names_file" >&2
    echo "actual:" >&2
    sed 's/^/  /' "$actual_names" >&2
    exit 1
  fi
}

verify_checksum_files() {
  local directory=$1 row target extension binary archive archive_path
  local checksum_path checksum_line digest
  for row in "${target_rows[@]}"; do
    IFS=$'\t' read -r target extension binary <<<"$row"
    archive="$package-$tag-$target.$extension"
    archive_path="$directory/$archive"
    checksum_path="$archive_path.sha256"
    digest=$(sha256_file "$archive_path")
    checksum_line="$digest  $archive"
    if [[ "$(<"$checksum_path")" != "$checksum_line" ||
          $(file_size "$checksum_path") -ne $((${#checksum_line} + 1)) ]]; then
      fail "$archive.sha256 does not canonically self-check $archive"
    fi
  done
}

verify_json_assets() {
  local directory=$1 name
  for name in "${expected_asset_names[@]}"; do
    case "$name" in
      *.json)
        jq empty "$directory/$name" > /dev/null 2>&1 ||
          fail "$name is not valid JSON"
        ;;
    esac
    case "$name" in
      *.spdx.json)
        jq -e '
          type == "object" and
          .spdxVersion == "SPDX-2.3" and
          .SPDXID == "SPDXRef-DOCUMENT" and
          .dataLicense == "CC0-1.0" and
          (.name | type) == "string" and (.name | length) > 0 and
          (.documentNamespace | type) == "string" and
          (.documentNamespace | length) > 0 and
          (.creationInfo | type) == "object"
        ' "$directory/$name" > /dev/null 2>&1 ||
          fail "$name is not a canonical SPDX 2.3 document"
        ;;
      *.sigstore.json)
        jq -e '
          type == "object" and
          (.mediaType | type) == "string" and
          (.mediaType |
            test("^application/vnd\\.dev\\.sigstore\\.bundle\\.v[0-9]+\\.[0-9]+\\+json$")) and
          (.verificationMaterial | type) == "object" and
          (((.dsseEnvelope | type) == "object") or
           ((.messageSignature | type) == "object"))
        ' "$directory/$name" > /dev/null 2>&1 ||
          fail "$name is not a Sigstore bundle"
        ;;
    esac
  done
}

verified_source_sha=
verify_release_record() {
  local directory=$1
  local record_path="$directory/$record_name"
  local canonical="$work/record-canonical" version=${tag#v}
  local current_sha current_epoch cargo_version_line cargo_version_count cargo_version
  local identities="$work/record-identities.json" record_names="$work/record-identity-names"
  local record_platforms="$work/record-platforms" repository_lower expected_image
  local identity name path row target extension binary archive target_record

  jq -S -c . "$record_path" > "$canonical" ||
    fail "$record_name could not be canonicalized"
  if ! cmp -s "$record_path" "$canonical"; then
    fail "$record_name is not canonical sorted compact JSON"
  fi

  current_sha=$(git -C "$root" rev-parse --verify HEAD 2> /dev/null) ||
    fail "could not resolve the checked-out release source"
  current_epoch=$(git -C "$root" show -s --format=%ct HEAD 2> /dev/null) ||
    fail "could not resolve the checked-out release epoch"
  cargo_version_line=$(awk '
    /^\[package\][[:space:]]*$/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^[[:space:]]*version[[:space:]]*=/ { print }
  ' "$root/Cargo.toml")
  cargo_version_count=$(printf '%s\n' "$cargo_version_line" | sed '/^$/d' |
    wc -l | tr -d '[:space:]')
  cargo_version=$(printf '%s\n' "$cargo_version_line" | sed -n \
    's/^[[:space:]]*version[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p')
  if [[ "$cargo_version_count" != 1 || "$cargo_version" != "$version" ]]; then
    fail "Cargo package version does not match release tag $tag"
  fi
  repository_lower=$(printf '%s' "$repository" | tr '[:upper:]' '[:lower:]')
  expected_image="ghcr.io/$repository_lower"

  if ! jq -e \
    --arg package "$package" \
    --arg tag "$tag" \
    --arg version "$version" \
    --arg source_sha "$current_sha" \
    --argjson source_epoch "$current_epoch" \
    --arg expected_image "$expected_image" '
      type == "object" and
      ((keys | sort) == ([
        "schema_version", "package", "tag", "version", "source_sha",
        "source_epoch", "release_targets", "native", "container"
      ] | sort)) and
      .schema_version == 1 and .package == $package and
      .tag == $tag and .version == $version and
      .source_sha == $source_sha and .source_epoch == $source_epoch and
      (.source_sha | test("^[0-9a-f]{40}$")) and
      (.source_epoch | type) == "number" and
      .source_epoch >= 315532800 and .source_epoch <= 2147483647 and
      (.release_targets | type) == "object" and
      ((.release_targets | keys | sort) == (["name", "sha256", "size"] | sort)) and
      (.native | type) == "array" and
      all(.native[];
        type == "object" and
        ((keys | sort) == ([
          "target", "binary", "archive", "checksum", "sbom", "attestations"
        ] | sort)) and
        (.target | type) == "string" and (.binary | type) == "string" and
        (.attestations | type) == "object" and
        ((.attestations | keys | sort) == (["provenance", "sbom"] | sort))) and
      (.container | type) == "object" and
      ((.container | keys | sort) == ([
        "image", "manifest_digest", "platforms", "sbom", "attestations"
      ] | sort)) and
      (.container.image | type) == "string" and
      .container.image == $expected_image and
      (.container.manifest_digest | type) == "string" and
      (.container.manifest_digest | test("^sha256:[0-9a-f]{64}$")) and
      (.container.platforms | type) == "array" and
      all(.container.platforms[];
        type == "object" and
        ((keys | sort) == (["platform", "runnable_digest"] | sort)) and
        (.platform | type) == "string" and
        (.runnable_digest | type) == "string" and
        (.runnable_digest | test("^sha256:[0-9a-f]{64}$"))) and
      (.container.attestations | type) == "object" and
      ((.container.attestations | keys | sort) == (["provenance", "sbom"] | sort))
    ' "$record_path" > /dev/null; then
    fail "$record_name does not match the canonical release/source schema"
  fi
  verified_source_sha=$current_sha

  verify_identity "$(jq -c '.release_targets' "$record_path")" \
    "$root/release-targets.json" release-targets.json

  for row in "${target_rows[@]}"; do
    IFS=$'\t' read -r target extension binary <<<"$row"
    archive="$package-$tag-$target.$extension"
    if ! target_record=$(jq -ce \
      --arg target "$target" \
      --arg binary "$binary" \
      --arg archive "$archive" '
        [.native[] | select(.target == $target)] |
        select(length == 1) | .[0] |
        select(.binary == $binary) |
        select(.archive.name == $archive) |
        select(.checksum.name == ($archive + ".sha256")) |
        select(.sbom.name == ($archive + ".spdx.json")) |
        select(.attestations.provenance.name ==
          ($archive + ".provenance.sigstore.json")) |
        select(.attestations.sbom.name == ($archive + ".sbom.sigstore.json"))
      ' "$record_path"); then
      fail "$record_name does not bind the exact $target native artifacts"
    fi
    : "$target_record"
  done
  if [[ $(jq '.native | length' "$record_path") -ne ${#target_rows[@]} ]]; then
    fail "$record_name has an unexpected native target count"
  fi
  jq -r '.container.platforms[].platform | select(type == "string")' \
    "$record_path" | LC_ALL=C sort > "$record_platforms"
  if ! cmp -s "$record_platforms" "$container_platforms_file"; then
    fail "$record_name does not bind the exact container platform set"
  fi

  jq -c '[
    .native[] |
      .archive, .checksum, .sbom,
      .attestations.provenance, .attestations.sbom
  ] + [
    .container.sbom,
    .container.attestations.provenance,
    .container.attestations.sbom
  ]' "$record_path" > "$identities"
  if [[ $(jq 'length' "$identities") -ne "$identity_count" ]]; then
    fail "$record_name does not bind all $identity_count durable artifact identities"
  fi
  jq -r '.[].name | select(type == "string")' "$identities" |
    LC_ALL=C sort > "$record_names"
  if ! cmp -s "$record_names" "$identity_names_file"; then
    fail "$record_name does not bind the exact durable artifact name set"
  fi
  while IFS= read -r identity; do
    name=$(jq -er '.name | select(type == "string")' <<<"$identity") ||
      fail "$record_name contains an invalid artifact identity name"
    path="$directory/$name"
    verify_identity "$identity" "$path" "$name"
  done < <(jq -c '.[]' "$identities")
}

verify_complete_directory() {
  local directory=$1 label=$2
  verify_directory_inventory "$directory" "$label"
  verify_checksum_files "$directory"
  verify_json_assets "$directory"
  verify_release_record "$directory"
}

# No GitHub read or mutation is allowed before the complete local release has
# passed its exact inventory, checksum, record, tag, and source checks.
verify_complete_directory "$dist" local-release
if [[ ! "$verified_source_sha" =~ ^[0-9a-f]{40}$ ]]; then
  fail "local release validation did not bind one source SHA"
fi
verify_live_release_tag() {
  "$verify_release_tag" "$tag" "$verified_source_sha" > /dev/null ||
    fail "live annotated release tag $tag does not match the local release source"
}
verify_live_release_tag

remote_assets=
remote_count=0
remote_names_file="$work/remote-names"
load_remote_assets() {
  local release_id=$1 pages
  if ! pages=$(gh api --paginate --slurp \
    "repos/${repository}/releases/${release_id}/assets?per_page=100"); then
    fail "could not list assets on exact GitHub release $tag (id $release_id)"
  fi
  if ! remote_assets=$(jq -ce '
    if type != "array" or any(.[]; type != "array") or
       any(.[][]; type != "object")
    then error("malformed asset pages")
    else [.[][]]
    end
  ' <<<"$pages"); then
    fail "GitHub returned malformed asset data for release $tag"
  fi
  remote_count=$(jq -r 'length' <<<"$remote_assets")
}

require_remote_subset_inventory() {
  local unique_count name
  if ! jq -e '
    all(.[];
      (.name | type) == "string" and
      .state == "uploaded" and
      (.size | type) == "number" and .size > 0 and
      (.size == (.size | floor)))
  ' <<<"$remote_assets" > /dev/null; then
    fail "remote release contains malformed or incomplete asset metadata"
  fi
  jq -r '.[].name' <<<"$remote_assets" |
    LC_ALL=C sort > "$remote_names_file"
  unique_count=$(uniq "$remote_names_file" | wc -l | tr -d '[:space:]')
  if [[ "$unique_count" -ne "$remote_count" ]]; then
    fail "remote release contains duplicate asset names"
  fi
  while IFS= read -r name; do
    if ! grep -Fqx -- "$name" "$expected_names_file"; then
      fail "remote release contains unexpected asset name: $name"
    fi
  done < "$remote_names_file"
}

require_exact_remote_inventory() {
  require_remote_subset_inventory
  if [[ "$remote_count" -ne "$expected_count" ]] ||
     ! cmp -s "$remote_names_file" "$expected_names_file"; then
    fail "remote release does not contain the exact expected $expected_count-file set"
  fi
}

download_and_compare_remote() {
  local label=$1 name
  local remote_dir="$work/download-$label"
  local downloaded_names="$work/download-$label-names" entry
  mkdir "$remote_dir"
  if ! gh release download "$tag" \
    --repo "$repository" \
    --dir "$remote_dir"; then
    fail "could not download the exact assets from GitHub release $tag"
  fi
  : > "$downloaded_names"
  while IFS= read -r -d '' entry; do
    name=${entry##*/}
    if [[ ! "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ||
          ! -f "$entry" || -L "$entry" || ! -s "$entry" ]]; then
      fail "downloaded release contains an unsafe, linked, empty, or non-file entry: $name"
    fi
    printf '%s\n' "$name" >> "$downloaded_names"
  done < <(find "$remote_dir" -mindepth 1 -maxdepth 1 -print0)
  LC_ALL=C sort "$downloaded_names" -o "$downloaded_names"
  if ! cmp -s "$downloaded_names" "$remote_names_file"; then
    fail "downloaded release does not match the listed remote asset set"
  fi
  while IFS= read -r name; do
    if ! cmp -s "$dist/$name" "$remote_dir/$name"; then
      fail "downloaded remote asset differs byte-for-byte from local $name"
    fi
  done < "$remote_names_file"
}

verify_remote_release() {
  local release_id=$1 release_state=$2 label=$3
  load_remote_assets "$release_id"
  require_exact_remote_inventory
  download_and_compare_remote "$label"
  "$verify_release" "$tag" "$release_state" "$release_id" > /dev/null ||
    fail "GitHub release $tag changed while its assets were being verified"
  # Release identity and asset verification are separate GitHub reads. Close
  # the interval by authenticating the live annotated tag again after the
  # complete remote snapshot has been accepted.
  verify_live_release_tag
}

lookup_release_presence() {
  local owner=${repository%%/*} name=${repository#*/} response
  # These dollar-prefixed names belong to GraphQL, not the shell.
  # shellcheck disable=SC2016
  local query='query($owner:String!,$name:String!,$tag:String!){repository(owner:$owner,name:$name){release(tagName:$tag){id}}}'
  if ! response=$(gh api graphql \
    -f "query=$query" \
    -F "owner=$owner" \
    -F "name=$name" \
    -F "tag=$tag"); then
    fail "could not determine whether GitHub release $tag exists"
  fi
  if ! jq -e '.data.repository | type == "object"' <<<"$response" > /dev/null; then
    fail "GitHub returned malformed release lookup data for $tag"
  fi
  if jq -e '.data.repository.release == null' <<<"$response" > /dev/null; then
    return 1
  fi
  jq -e '.data.repository.release.id | type == "string" and length > 0' \
    <<<"$response" > /dev/null ||
    fail "GitHub returned malformed release identity data for $tag"
  return 0
}

live_release_id=
live_release_draft=
read_live_release_state() {
  local snapshot
  if ! snapshot=$(gh api "repos/${repository}/releases/tags/${tag}"); then
    fail "could not read live GitHub release state for $tag"
  fi
  if ! live_release_id=$(jq -er '
    .id | select(type == "number" and . > 0 and floor == .)
  ' <<<"$snapshot"); then
    fail "GitHub release $tag returned an invalid live identity"
  fi
  if ! live_release_draft=$(jq -er '
    if type != "object" or (.draft | type) != "boolean"
    then error("invalid draft state")
    else (.draft | tostring)
    end
  ' <<<"$snapshot"); then
    fail "GitHub release $tag returned an invalid live draft state"
  fi
}

case "$mode" in
  stage)
    if lookup_release_presence; then
      release_id=$("$verify_release" "$tag" draft) ||
        fail "existing GitHub release $tag is not the trusted draft"
    else
      notes_file="$work/release-notes.md"
      "$root/scripts/extract-release-notes.sh" "${tag#v}" > "$notes_file" ||
        fail "could not derive canonical release notes for $tag"
      if ! gh release create "$tag" \
        --repo "$repository" \
        --draft \
        --verify-tag \
        --title "$tag" \
        --notes-file "$notes_file" > /dev/null; then
        fail "could not create the trusted draft GitHub release $tag"
      fi
      verify_live_release_tag
      release_id=$("$verify_release" "$tag" draft) ||
        fail "created GitHub release $tag is not the trusted draft"
    fi

    load_remote_assets "$release_id"
    require_remote_subset_inventory
    if [[ "$remote_count" -gt 0 ]]; then
      download_and_compare_remote stage-existing
      "$verify_release" "$tag" draft "$release_id" > /dev/null ||
        fail "GitHub release $tag changed while existing assets were verified"
    fi
    missing_assets=()
    for name in "${expected_asset_names[@]}"; do
      if ! grep -Fqx -- "$name" "$remote_names_file"; then
        missing_assets+=("$dist/$name")
      fi
    done
    if [[ ${#missing_assets[@]} -gt 0 ]]; then
      if ! gh release upload "$tag" \
        --repo "$repository" \
        "${missing_assets[@]}"; then
        fail "could not upload the missing asset set to draft release $tag"
      fi
      verify_live_release_tag
      "$verify_release" "$tag" draft "$release_id" > /dev/null ||
        fail "GitHub release $tag changed while its assets were uploaded"
    fi
    verify_remote_release "$release_id" draft stage
    echo "Staged exact draft GitHub release $tag (id $release_id)"
    ;;

  finalize)
    read_live_release_state
    if [[ "$live_release_draft" == false ]]; then
      release_id=$("$verify_release" "$tag" published "$live_release_id") ||
        fail "already-published GitHub release $tag is not the trusted immutable release"
      verify_remote_release "$release_id" published finalize-already-published
      echo "Accepted already-finalized exact immutable GitHub release $tag (id $release_id)"
    else
      release_id=$("$verify_release" "$tag" draft "$live_release_id") ||
        fail "GitHub release $tag is not the trusted draft"
      verify_remote_release "$release_id" draft finalize-before
      patch_response=received
      if ! gh api --method PATCH \
        "repos/${repository}/releases/${release_id}" \
        -F draft=false \
        -f make_latest=legacy > /dev/null; then
        patch_response=lost
      fi
      verify_live_release_tag
      if ! "$verify_release" "$tag" published "$release_id" > /dev/null; then
        if [[ "$patch_response" == lost ]]; then
          fail "finalization response was lost and GitHub release $tag is not the trusted immutable release"
        fi
        fail "GitHub release $tag did not become the trusted immutable release"
      fi
      verify_remote_release "$release_id" published finalize-after
      if [[ "$patch_response" == lost ]]; then
        echo "Recovered committed finalization after the GitHub response was lost"
      fi
      echo "Finalized exact immutable GitHub release $tag (id $release_id)"
    fi
    ;;

  verify)
    release_id=$("$verify_release" "$tag" published) ||
      fail "GitHub release $tag is not the trusted immutable release"
    verify_remote_release "$release_id" published verify
    echo "Verified exact immutable GitHub release $tag (id $release_id)"
    ;;
esac
