#!/usr/bin/env bash
# Stage, finalize, or verify one exact GitHub binary release.
#
#   scripts/publish-release.sh stage <tag> [release-directory]
#   scripts/publish-release.sh finalize <tag> [release-directory]
#   scripts/publish-release.sh verify <tag> [release-directory]
#   scripts/publish-release.sh resume <tag> <release-directory> <release-id>
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
       $0 resume <tag> <release-directory> <release-id>
EOF
  exit 2
}

[[ $# -ge 2 && $# -le 4 ]] || usage

mode=$1
tag=$2
dist=${3:-dist}
expected_release_id=${4:-}
repository=${GH_REPO:-}
root=$(cd "$(dirname "$0")/.." && pwd)
release_targets="$root/scripts/release-targets.sh"
verify_release="$root/scripts/verify-release.sh"
verify_release_tag="$root/scripts/verify-release-tag.sh"
package=mcp-repl
semver_component='(0|[1-9][0-9]*)'

case "$mode" in
  stage | finalize | verify)
    [[ $# -le 3 ]] || usage
    ;;
  resume)
    [[ $# -eq 4 && "$expected_release_id" =~ ^[1-9][0-9]*$ ]] || usage
    ;;
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

for required_command in gh jq find sort cmp wc awk grep git sleep; do
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
verified_remote_asset_snapshot=
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
  local unique_count unique_id_count name
  if ! jq -e '
    all(.[];
      (.id | type) == "number" and .id > 0 and
      (.id == (.id | floor)) and
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
  unique_id_count=$(jq '[.[].id] | unique | length' <<<"$remote_assets")
  if [[ "$unique_id_count" -ne "$remote_count" ]]; then
    fail "remote release contains duplicate asset identities"
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

remote_asset_snapshot() {
  jq -c 'sort_by(.name) | map({id, name, size, state})' <<<"$remote_assets"
}

download_remote_asset() {
  local asset_id=$1 name=$2 expected_size=$3 destination=$4 actual_size
  if ! gh api "repos/${repository}/releases/assets/${asset_id}" \
    -H 'Accept: application/octet-stream' > "$destination"; then
    fail "could not download exact GitHub release asset $name (id $asset_id)"
  fi
  if [[ ! -f "$destination" || -L "$destination" ]]; then
    fail "downloaded release asset $name is not a regular file"
  fi
  actual_size=$(file_size "$destination")
  if [[ "$actual_size" -ne "$expected_size" ]]; then
    fail "downloaded release asset $name does not match its listed size"
  fi
}

download_and_compare_remote() {
  local release_id=$1 label=$2 asset_id name expected_size destination
  local remote_dir="$work/download-$label"
  mkdir "$remote_dir"
  while IFS=$'\t' read -r asset_id name expected_size; do
    destination="$remote_dir/$name"
    download_remote_asset "$asset_id" "$name" "$expected_size" "$destination"
    if ! cmp -s "$dist/$name" "$destination"; then
      fail "downloaded remote asset differs byte-for-byte from local $name"
    fi
  done < <(jq -r 'sort_by(.name)[] | [.id, .name, .size] | @tsv' \
    <<<"$remote_assets")
}

verify_remote_release() {
  local release_id=$1 release_state=$2 label=$3 before_snapshot after_snapshot
  load_remote_assets "$release_id"
  require_exact_remote_inventory
  before_snapshot=$(remote_asset_snapshot)
  download_and_compare_remote "$release_id" "$label"
  "$verify_release" "$tag" "$release_state" "$release_id" > /dev/null ||
    fail "GitHub release $tag changed while its assets were being verified"
  require_unique_release_id "$release_id"
  load_remote_assets "$release_id"
  require_exact_remote_inventory
  after_snapshot=$(remote_asset_snapshot)
  if [[ "$after_snapshot" != "$before_snapshot" ]]; then
    fail "GitHub release $tag assets changed while they were being verified"
  fi
  verified_remote_asset_snapshot=$after_snapshot
  # Close the interval between release, asset, and tag identity reads before
  # accepting the complete remote snapshot.
  verify_live_release_tag
}

located_release_id=
rest_release_ids=
rest_release_count=0
rest_release_snapshot=
read_rest_release_ids() {
  local pages
  if ! pages=$(gh api --paginate --slurp \
    "repos/${repository}/releases?per_page=100"); then
    fail "could not list GitHub releases while locating $tag"
  fi
  if ! rest_release_snapshot=$(jq -ce '
    if type != "array" or any(.[]; type != "array") or
       any(.[][];
         type != "object" or
         (.id | type) != "number" or .id <= 0 or
         .id != (.id | floor) or
         (.tag_name | type) != "string")
    then error("malformed release pages")
    else
      [.[][] | {id, tag_name}] | sort_by(.id, .tag_name) |
      if ([.[].id] | unique | length) != length
      then error("duplicate paginated release identity")
      else .
      end
    end
  ' <<<"$pages"); then
    fail "GitHub returned malformed paginated release data for $tag"
  fi
  rest_release_ids=$(jq -c --arg tag "$tag" \
    '[.[] | select(.tag_name == $tag) | .id]' <<<"$rest_release_snapshot")
  rest_release_count=$(jq 'length' <<<"$rest_release_ids")
}

locate_release() {
  local attempt candidate_id='' candidate_snapshot='' candidate_state=''
  local observed_id='' observed_state='' seen_id=''
  located_release_id=
  for attempt in 1 2 3 4 5 6 7 8 9 10; do
    read_rest_release_ids
    if [[ "$rest_release_count" -gt 1 ]]; then
      fail "multiple GitHub releases use tag $tag"
    fi
    observed_id=
    if [[ "$rest_release_count" -eq 1 ]]; then
      observed_id=$(jq -r '.[0]' <<<"$rest_release_ids")
      if [[ -n "$seen_id" && "$seen_id" != "$observed_id" ]]; then
        fail "GitHub release identity for $tag changed during lookup"
      fi
      seen_id=$observed_id
      observed_state=found
    else
      observed_state=absent
    fi
    if [[ "$candidate_state" == "$observed_state" &&
          "$candidate_snapshot" == "$rest_release_snapshot" ]]; then
      if [[ "$observed_state" == found ]]; then
        if [[ "$candidate_id" != "$observed_id" ]]; then
          fail "GitHub release identity for $tag changed between stable observations"
        fi
        located_release_id=$observed_id
        return 0
      fi
      if [[ -z "$seen_id" ]]; then
        return 1
      fi
    fi
    candidate_state=$observed_state
    candidate_snapshot=$rest_release_snapshot
    candidate_id=$observed_id
    [[ "$attempt" -eq 10 ]] || sleep 1
  done
  fail "GitHub release identity for $tag did not converge"
}

wait_for_release_presence() {
  local attempt candidate_id='' candidate_snapshot='' observed_id='' seen_id=''
  located_release_id=
  for attempt in 1 2 3 4 5 6 7 8 9 10; do
    read_rest_release_ids
    if [[ "$rest_release_count" -gt 1 ]]; then
      fail "multiple GitHub releases use tag $tag"
    fi
    observed_id=
    if [[ "$rest_release_count" -eq 1 ]]; then
      observed_id=$(jq -r '.[0]' <<<"$rest_release_ids")
      if [[ -n "$seen_id" && "$seen_id" != "$observed_id" ]]; then
        fail "GitHub release identity for $tag changed while it became visible"
      fi
      seen_id=$observed_id
      if [[ -n "$candidate_id" &&
            "$candidate_snapshot" == "$rest_release_snapshot" ]]; then
        if [[ "$candidate_id" != "$observed_id" ]]; then
          fail "GitHub release identity for $tag changed between stable observations"
        fi
        located_release_id=$observed_id
        return 0
      fi
      candidate_id=$observed_id
      candidate_snapshot=$rest_release_snapshot
    else
      candidate_id=
      candidate_snapshot=
    fi
    [[ "$attempt" -eq 10 ]] || sleep 1
  done
  return 1
}

require_unique_release_id() {
  local expected_id=$1
  if ! locate_release; then
    fail "GitHub release $tag disappeared"
  fi
  if [[ "$located_release_id" != "$expected_id" ]]; then
    fail "GitHub release $tag was replaced: expected $expected_id, found $located_release_id"
  fi
}

live_release_id=
live_release_draft=
read_release_state_by_id() {
  local expected_id=$1 snapshot
  if ! snapshot=$(gh api "repos/${repository}/releases/${expected_id}"); then
    fail "could not read live GitHub release state for $tag"
  fi
  if ! live_release_id=$(jq -er --arg tag "$tag" --argjson id "$expected_id" '
    select(type == "object" and .tag_name == $tag) |
    .id | select(type == "number" and . == $id)
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

read_live_release_state() {
  if ! locate_release; then
    fail "could not read live GitHub release state for $tag"
  fi
  read_release_state_by_id "$located_release_id"
}

upload_missing_asset() {
  local release_id=$1 path=$2 name=${2##*/} expected_size response
  local upload_received=true asset asset_id remote_size recovery_dir
  expected_size=$(file_size "$path")
  if ! response=$(gh api --method POST \
    "https://uploads.github.com/repos/${repository}/releases/${release_id}/assets" \
    -H 'Content-Type: application/octet-stream' \
    -f "name=$name" \
    --input "$path"); then
    upload_received=false
  fi
  if [[ "$upload_received" == true ]] && jq -e \
    --arg name "$name" \
    --argjson size "$expected_size" '
      type == "object" and
      (.id | type) == "number" and .id > 0 and .id == (.id | floor) and
      .name == $name and .state == "uploaded" and .size == $size
    ' <<<"$response" > /dev/null 2>&1; then
    return 0
  fi

  # A network error or malformed response may follow a committed upload. Never
  # retry blindly: reconcile the selected release ID and accept only exact bytes.
  load_remote_assets "$release_id"
  require_remote_subset_inventory
  if ! asset=$(jq -ce --arg name "$name" '
    [.[] | select(.name == $name)] | select(length == 1) | .[0]
  ' <<<"$remote_assets"); then
    fail "could not upload exact asset $name to draft release $tag"
  fi
  asset_id=$(jq -r '.id' <<<"$asset")
  remote_size=$(jq -r '.size' <<<"$asset")
  recovery_dir="$work/upload-recovery-$name"
  mkdir "$recovery_dir"
  download_remote_asset "$asset_id" "$name" "$remote_size" \
    "$recovery_dir/$name"
  if ! cmp -s "$path" "$recovery_dir/$name"; then
    fail "ambiguous upload for $name committed different remote bytes"
  fi
  echo "Recovered committed asset upload for $name after its response was lost"
}

stage_exact_draft() {
  local release_id=$1 name missing_count=0
  local missing_assets=()
  release_id=$("$verify_release" "$tag" draft "$release_id") ||
    fail "GitHub release $tag is not the trusted draft"
  load_remote_assets "$release_id"
  require_remote_subset_inventory
  if [[ "$remote_count" -gt 0 ]]; then
    download_and_compare_remote "$release_id" stage-existing
    "$verify_release" "$tag" draft "$release_id" > /dev/null ||
      fail "GitHub release $tag changed while existing assets were verified"
  fi
  for name in "${expected_asset_names[@]}"; do
    if ! grep -Fqx -- "$name" "$remote_names_file"; then
      missing_assets[missing_count]="$dist/$name"
      missing_count=$((missing_count + 1))
    fi
  done
  if [[ "$missing_count" -gt 0 ]]; then
    # Rebind the exact tag identity across two full release-list snapshots at
    # the mutation boundary. A duplicate or replacement appearing after the
    # initial lookup must fail before this invocation uploads any bytes.
    require_unique_release_id "$release_id"
    "$verify_release" "$tag" draft "$release_id" > /dev/null ||
      fail "GitHub release $tag changed immediately before asset upload"
    verify_live_release_tag
    for name in "${missing_assets[@]}"; do
      upload_missing_asset "$release_id" "$name"
    done
    "$verify_release" "$tag" draft "$release_id" > /dev/null ||
      fail "GitHub release $tag changed while its assets were uploaded"
  fi
  verify_remote_release "$release_id" draft stage
}

finalize_exact_release() {
  local release_id=$1 release_draft=$2 patch_response current_snapshot
  if [[ "$release_draft" == false ]]; then
    release_id=$("$verify_release" "$tag" published "$release_id") ||
      fail "already-published GitHub release $tag is not the trusted immutable release"
    verify_remote_release "$release_id" published finalize-already-published
    echo "Accepted already-finalized exact immutable GitHub release $tag (id $release_id)"
    return 0
  fi

  release_id=$("$verify_release" "$tag" draft "$release_id") ||
    fail "GitHub release $tag is not the trusted draft"
  verify_remote_release "$release_id" draft finalize-before
  require_unique_release_id "$release_id"
  load_remote_assets "$release_id"
  require_exact_remote_inventory
  current_snapshot=$(remote_asset_snapshot)
  if [[ -z "$verified_remote_asset_snapshot" ||
        "$current_snapshot" != "$verified_remote_asset_snapshot" ]]; then
    fail "GitHub release $tag assets changed immediately before finalization"
  fi
  # The stable uniqueness check above deliberately spans two observations.
  # Close that interval over the release metadata and annotated tag once more
  # before making the visibility change.
  "$verify_release" "$tag" draft "$release_id" > /dev/null ||
    fail "GitHub release $tag changed immediately before finalization"
  verify_live_release_tag
  load_remote_assets "$release_id"
  require_exact_remote_inventory
  current_snapshot=$(remote_asset_snapshot)
  if [[ "$current_snapshot" != "$verified_remote_asset_snapshot" ]]; then
    fail "GitHub release $tag assets changed immediately before finalization"
  fi
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
}

case "$mode" in
  stage)
    if locate_release; then
      release_id=$located_release_id
    else
      notes_file="$work/release-notes.md"
      "$root/scripts/extract-release-notes.sh" "${tag#v}" > "$notes_file" ||
        fail "could not derive canonical release notes for $tag"
      create_response=received
      if ! gh release create "$tag" \
        --repo "$repository" \
        --draft \
        --verify-tag \
        --title "$tag" \
        --notes-file "$notes_file" > /dev/null; then
        create_response=lost
      fi
      verify_live_release_tag
      if ! wait_for_release_presence; then
        if [[ "$create_response" == lost ]]; then
          fail "draft creation response was lost and GitHub release $tag is absent"
        fi
        fail "created GitHub release $tag could not be bound to one unique numeric identity"
      fi
      release_id=$located_release_id
      release_id=$("$verify_release" "$tag" draft "$release_id") ||
        fail "created GitHub release $tag is not the trusted draft"
      if [[ "$create_response" == lost ]]; then
        echo "Recovered committed draft creation after the GitHub response was lost"
      fi
    fi
    stage_exact_draft "$release_id"
    echo "Staged exact draft GitHub release $tag (id $release_id)"
    ;;

  finalize)
    read_live_release_state
    finalize_exact_release "$live_release_id" "$live_release_draft"
    ;;

  verify)
    if ! locate_release; then
      fail "GitHub release $tag is absent"
    fi
    release_id=$("$verify_release" "$tag" published "$located_release_id") ||
      fail "GitHub release $tag is not the trusted immutable release"
    verify_remote_release "$release_id" published verify
    echo "Verified exact immutable GitHub release $tag (id $release_id)"
    ;;

  resume)
    if ! locate_release; then
      fail "recovery requires existing GitHub release $tag (id $expected_release_id)"
    fi
    if [[ "$located_release_id" != "$expected_release_id" ]]; then
      fail "GitHub release $tag was replaced: expected $expected_release_id, found $located_release_id"
    fi
    read_release_state_by_id "$expected_release_id"
    if [[ "$live_release_draft" == true ]]; then
      stage_exact_draft "$expected_release_id"
      echo "Staged exact draft GitHub release $tag (id $expected_release_id)"
    fi
    finalize_exact_release "$expected_release_id" "$live_release_draft"
    ;;
esac
