#!/usr/bin/env bash
# Stage, expose, and reconcile an attested multi-platform container release.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
release_targets="$root/scripts/release-targets.sh"
verify_release="$root/scripts/verify-release.sh"
verify_release_tag="$root/scripts/verify-release-tag.sh"
validate_oci_attestation="$root/scripts/validate-oci-attestation.py"
container_runnable_mapping="$root/scripts/container-runnable-mapping.jq"
package=mcp-repl
repository=${GH_REPO:-}
semver_component='(0|[1-9][0-9]*)'

die() {
  echo "container-manifest: $*" >&2
  exit 1
}

usage() {
  cat >&2 <<EOF
usage:
  $0 [--test-oci-blob-cache <directory>] stage <vX.Y.Z> <source-sha> <source-epoch> <metadata-dir> <output-dir>
  $0 version <image-manifest.json>
  $0 latest
EOF
  exit 2
}

if [[ $# -eq 0 ]]; then
  usage
fi
test_oci_blob_cache=
if [[ ${1:-} == --test-oci-blob-cache ]]; then
  if [[ $# -lt 3 ]]; then
    usage
  fi
  test_oci_blob_cache=$2
  shift 2
fi
mode=$1
case "$mode:$#" in
  stage:6 | version:2 | latest:1) ;;
  *) usage ;;
esac

if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "container-manifest: GH_REPO must be an owner/repository name" >&2
  exit 2
fi
image_repository=$(printf '%s' "$repository" | tr '[:upper:]' '[:lower:]')
image="ghcr.io/$image_repository"

for required_command in awk cmp docker find jq ln mktemp python3 rm sed sort tr wc; do
  command -v "$required_command" > /dev/null 2>&1 ||
    die "$required_command is required"
done
if [[ "$mode" != stage ]]; then
  command -v gh > /dev/null 2>&1 || die "gh is required"
fi
if command -v sha256sum > /dev/null 2>&1; then
  sha256_command=sha256sum
elif command -v shasum > /dev/null 2>&1; then
  sha256_command=shasum
else
  die "sha256sum or shasum is required"
fi

"$release_targets" validate > /dev/null ||
  die "release target manifest validation failed"

if ! platform_output=$("$release_targets" container-platforms); then
  die "could not load release container platforms"
fi
platforms=()
while IFS= read -r platform; do
  if [[ ! "$platform" =~ ^[a-z0-9][a-z0-9._-]*/[a-z0-9][a-z0-9._-]*$ ]]; then
    die "release target manifest emitted an unsafe container platform"
  fi
  platforms+=("$platform")
done <<<"$platform_output"
if [[ ${#platforms[@]} -eq 0 ]]; then
  die "release target manifest contains no container platforms"
fi
expected_platforms=$(printf '%s\n' "${platforms[@]}" |
  jq -Rsc '
    split("\n") | map(select(length > 0)) |
    if length > 0 and length == (unique | length)
    then sort
    else error("container platforms must be nonempty and unique")
    end
  ') || die "release target manifest emitted duplicate container platforms"

work=$(mktemp -d "${TMPDIR:-/tmp}/mcp-repl-container-manifest.XXXXXX")
output_temp=
cleanup() {
  rm -rf "$work"
  if [[ -n "$output_temp" ]]; then
    rm -f "$output_temp"
  fi
}
trap cleanup EXIT INT TERM
inspect_serial=0
create_serial=0

sha256_file() {
  local path=$1
  local digest
  if [[ "$sha256_command" == sha256sum ]]; then
    digest=$(sha256sum "$path" | awk '{print $1}')
  else
    digest=$(shasum -a 256 "$path" | awk '{print $1}')
  fi
  [[ "$digest" =~ ^[0-9a-f]{64}$ ]] ||
    die "could not compute SHA-256 for ${path##*/}"
  printf 'sha256:%s\n' "$digest"
}

validate_tag() {
  local tag=$1
  if [[ ! "$tag" =~ ^v${semver_component}\.${semver_component}\.${semver_component}$ ||
        ${#tag} -gt 64 ]]; then
    echo "container-manifest: tag must be a canonical vX.Y.Z version" >&2
    exit 2
  fi
}

validate_source_identity() {
  local source_sha=$1
  local source_epoch=$2
  if [[ ! "$source_sha" =~ ^[0-9a-f]{40}$ ]]; then
    echo "container-manifest: source SHA must be 40 lowercase hexadecimal characters" >&2
    exit 2
  fi
  if [[ ! "$source_epoch" =~ ^(0|[1-9][0-9]{0,9})$ ]] ||
      ((source_epoch < 315532800 || source_epoch > 2147483647)); then
    echo "container-manifest: source epoch must be canonical UTC seconds from 1980 through 2038" >&2
    exit 2
  fi
}

# Set INSPECT_RAW_FILE and INSPECT_DIGEST after independently checking the raw
# registry bytes against Buildx's resolver-reported descriptor digest. An exact
# Buildx not-found diagnostic is the only absence that callers may act on.
inspect_reference() {
  local reference=$1
  local label=$2
  local allow_absent=$3
  local raw_file error_file expected_error format_error formatted digest

  inspect_serial=$((inspect_serial + 1))
  raw_file="$work/inspect-$inspect_serial.raw"
  error_file="$work/inspect-$inspect_serial.stderr"
  format_error="$work/inspect-$inspect_serial-format.stderr"
  if ! docker buildx imagetools inspect --raw "$reference" \
      > "$raw_file" 2> "$error_file"; then
    if [[ "$allow_absent" == true ]]; then
      expected_error="$work/inspect-$inspect_serial-absent.stderr"
      printf 'ERROR: %s: not found\n' "$reference" > "$expected_error"
      if cmp -s "$error_file" "$expected_error"; then
        return 2
      fi
    fi
    sed -n '1,10p' "$error_file" >&2
    die "could not inspect $label"
  fi
  if [[ ! -s "$raw_file" || -L "$raw_file" ]]; then
    die "$label returned an empty or linked raw manifest"
  fi
  digest=$(sha256_file "$raw_file")
  if ! formatted=$(docker buildx imagetools inspect \
      --format '{{json .Manifest.Digest}}' "$reference" \
      2> "$format_error"); then
    sed -n '1,10p' "$format_error" >&2
    die "could not resolve the formatted digest for $label"
  fi
  if ! formatted=$(jq -er '
      select(type == "string") |
      select(test("^sha256:[0-9a-f]{64}$"))
    ' <<<"$formatted"); then
    die "$label returned an invalid formatted manifest digest"
  fi
  if [[ "$digest" != "$formatted" ]]; then
    die "$label raw manifest digest $digest differs from Buildx digest $formatted"
  fi
  INSPECT_RAW_FILE=$raw_file
  INSPECT_DIGEST=$digest
}

# Fetch, hash, and validate every in-toto blob in one mapped BuildKit
# attestation manifest. The validator accepts both OCI-artifact and legacy
# BuildKit storage, but always binds the statement subject to the runnable.
validate_attestation_manifest() {
  local raw_file=$1
  local runnable_digest=$2
  local runnable_size=$3
  local label=$4
  local predicates
  if [[ -n "$test_oci_blob_cache" ]]; then
    predicates=$(python3 "$validate_oci_attestation" \
      --test-blob-cache "$test_oci_blob_cache" \
      "$image" "$raw_file" "$runnable_digest" "$runnable_size" \
      "$package" "$EXPECTED_PACKAGE_VERSION") ||
      die "$label is not a valid BuildKit attestation manifest for $runnable_digest"
  elif ! predicates=$(python3 "$validate_oci_attestation" \
      "$image" "$raw_file" "$runnable_digest" "$runnable_size" \
      "$package" "$EXPECTED_PACKAGE_VERSION"); then
    die "$label is not a valid BuildKit attestation manifest for $runnable_digest"
  fi
  ATTESTATION_PREDICATES=$predicates
}

# Inspect every mapped attestation manifest by immutable digest. All of them
# must be structurally valid and consistently bound, while their in-toto layer
# annotations collectively provide SLSA provenance and an SPDX SBOM for each
# runnable platform.
validate_index_evidence() {
  local raw_file=$1
  local expected_rows=$2
  local label=$3
  local saved_raw_file=${INSPECT_RAW_FILE:-}
  local saved_digest=${INSPECT_DIGEST:-}
  local row_count row_index row platform runnable_digest runnable_size attestations
  local attestation_count attestation_index attestation digest descriptor_size
  local raw_size predicate has_provenance has_sbom

  row_count=$(jq -r 'length' <<<"$expected_rows")
  row_index=0
  while ((row_index < row_count)); do
    row=$(jq -c ".[$row_index]" <<<"$expected_rows")
    platform=$(jq -r '.platform' <<<"$row")
    runnable_digest=$(jq -r '.runnable_digest' <<<"$row")
    runnable_size=$(jq -er --arg runnable_digest "$runnable_digest" '
      def attestation:
        ((.annotations["vnd.docker.reference.type"] // "") ==
          "attestation-manifest");
      [
        .manifests[] |
        select(attestation | not) |
        select(.digest == $runnable_digest) |
        .size
      ] |
      if length == 1 then .[0]
      else error("runnable descriptor is not unique") end
    ' "$raw_file") ||
      die "$label has no unique runnable descriptor for $platform"
    attestations=$(jq -cer --arg runnable_digest "$runnable_digest" '
      def attestation:
        ((.annotations["vnd.docker.reference.type"] // "") ==
          "attestation-manifest");
      [
        .manifests[] |
        select(attestation) |
        select(.annotations["vnd.docker.reference.digest"] ==
          $runnable_digest) |
        {digest, size}
      ] | sort_by(.digest) |
      select(length > 0)
    ' "$raw_file") ||
      die "$label has no mapped attestation evidence for $platform"

    has_provenance=false
    has_sbom=false
    attestation_count=$(jq -r 'length' <<<"$attestations")
    attestation_index=0
    while ((attestation_index < attestation_count)); do
      attestation=$(jq -c ".[$attestation_index]" <<<"$attestations")
      digest=$(jq -r '.digest' <<<"$attestation")
      descriptor_size=$(jq -r '.size' <<<"$attestation")
      inspect_reference "$image@$digest" \
        "$label attestation evidence for $platform" false
      if [[ "$INSPECT_DIGEST" != "$digest" ]]; then
        die "$label attestation evidence for $platform resolved to the wrong digest"
      fi
      raw_size=$(wc -c < "$INSPECT_RAW_FILE" | tr -d '[:space:]')
      if [[ "$raw_size" != "$descriptor_size" ]]; then
        die "$label attestation evidence for $platform has a descriptor size mismatch"
      fi
      validate_attestation_manifest "$INSPECT_RAW_FILE" "$runnable_digest" \
        "$runnable_size" "$label attestation evidence for $platform"
      while IFS= read -r predicate; do
        case "$predicate" in
          https://slsa.dev/provenance/v1)
            has_provenance=true
            ;;
          https://spdx.dev/Document)
            has_sbom=true
            ;;
        esac
      done <<<"$ATTESTATION_PREDICATES"
      attestation_index=$((attestation_index + 1))
    done
    if [[ "$has_provenance" != true ]]; then
      die "$label has no BuildKit SLSA provenance evidence for $platform"
    fi
    if [[ "$has_sbom" != true ]]; then
      die "$label has no BuildKit SPDX SBOM evidence for $platform"
    fi
    row_index=$((row_index + 1))
  done

  INSPECT_RAW_FILE=$saved_raw_file
  INSPECT_DIGEST=$saved_digest
}

# Require one exact runnable descriptor per expected platform. Every remaining
# descriptor must be an unknown/unknown BuildKit attestation manifest mapped by
# annotation to one expected runnable, with at least one for every platform.
validate_index_mapping() {
  local raw_file=$1
  local expected_rows=$2
  local label=$3

  if ! jq -cer -f "$container_runnable_mapping" "$raw_file" > /dev/null; then
    die "$label is not a complete attested OCI image index"
  fi
  if ! jq -e --argjson expected "$expected_rows" '
    def sha256: type == "string" and test("^sha256:[0-9a-f]{64}$");
    def attestation:
      ((.annotations["vnd.docker.reference.type"] // "") ==
        "attestation-manifest");
    def platform_name: .platform.os + "/" + .platform.architecture;
    def string_annotations:
      ((.annotations? // {}) | type) == "object" and
      all((.annotations? // {})[]; type == "string");
    def descriptor:
      type == "object" and
      ((keys - [
        "mediaType", "digest", "size", "urls", "annotations", "data",
        "artifactType", "platform"
      ]) | length) == 0 and
      .mediaType == "application/vnd.oci.image.manifest.v1+json" and
      (.digest | sha256) and
      (.size | type) == "number" and .size > 0 and
      .size == (.size | floor) and
      (.platform | type) == "object" and
      ((.urls? // []) | type) == "array" and
      all((.urls? // [])[]; type == "string" and length > 0) and
      ((has("data") | not) or (.data | type) == "string") and
      ((has("artifactType") | not) or
        ((.artifactType | type) == "string" and .artifactType != "")) and
      string_annotations;

    ($expected | map({platform, runnable_digest}) | sort_by(.platform)) as $wanted |
    . as $index |
    select(($wanted | length) > 0) |
    select(($wanted | length) == ([$wanted[].platform] | unique | length)) |
    select(($wanted | length) == ([$wanted[].runnable_digest] | unique | length)) |
    select(all($wanted[];
      ((keys | sort) == (["platform", "runnable_digest"] | sort)) and
      (.platform | type) == "string" and
      (.runnable_digest | sha256))) |
    select(($index | type) == "object") |
    select(($index | keys) - [
      "schemaVersion", "mediaType", "manifests", "annotations"
    ] | length == 0) |
    select($index.schemaVersion == 2) |
    select($index.mediaType == "application/vnd.oci.image.index.v1+json") |
    select($index | string_annotations) |
    select(($index.manifests | type) == "array") |
    ($index.manifests | map(select(attestation | not))) as $runnables |
    ($index.manifests | map(select(attestation))) as $attestations |
    select(($runnables | length) == ($wanted | length)) |
    select(($attestations | length) >= ($wanted | length)) |
    select(([$index.manifests[].digest] | length) ==
      ([$index.manifests[].digest] | unique | length)) |
    select(all($index.manifests[]; descriptor)) |
    select(all($runnables[];
      . as $descriptor |
      ((.platform | keys | sort) == (["architecture", "os"] | sort)) and
      ((.annotations["vnd.docker.reference.type"]? // null) == null) and
      ((.annotations["vnd.docker.reference.digest"]? // null) == null) and
      any($wanted[];
        .platform == ($descriptor | platform_name) and
        .runnable_digest == $descriptor.digest))) |
    select(all($attestations[];
      . as $descriptor |
      ((.platform | keys | sort) == (["architecture", "os"] | sort)) and
      .platform.os == "unknown" and
      .platform.architecture == "unknown" and
      (.annotations["vnd.docker.reference.digest"] | sha256) and
      any($wanted[];
        .runnable_digest ==
          $descriptor.annotations["vnd.docker.reference.digest"]))) |
    select(($runnables | map({
      platform: platform_name,
      runnable_digest: .digest
    }) | sort_by(.platform)) == $wanted) |
    select([
      $wanted[].runnable_digest as $runnable_digest |
      any($attestations[];
        .annotations["vnd.docker.reference.digest"] == $runnable_digest)
    ] | all)
  ' "$raw_file" > /dev/null 2>&1; then
    die "$label is not the exact attested supported-platform manifest"
  fi
  validate_index_evidence "$raw_file" "$expected_rows" "$label"
}

validate_create_metadata() {
  local metadata_file=$1
  local expected_digest=$2
  local raw_file=$3
  local label=$4
  local raw_media raw_size

  if [[ ! -f "$metadata_file" || -L "$metadata_file" || ! -s "$metadata_file" ]]; then
    die "$label did not write regular nonempty create metadata"
  fi
  raw_media=$(jq -er '.mediaType |
    select(type == "string" and length > 0)' "$raw_file") ||
    die "$label raw manifest has no media type"
  raw_size=$(wc -c < "$raw_file" | tr -d '[:space:]')
  [[ "$raw_size" =~ ^[1-9][0-9]*$ ]] || die "$label raw manifest has invalid size"
  if ! jq -e \
      --arg digest "$expected_digest" \
      --arg media_type "$raw_media" \
      --argjson size "$raw_size" '
        .["containerimage.descriptor"] |
        type == "object" and
        .digest == $digest and
        .mediaType == $media_type and
        .size == $size
      ' "$metadata_file" > /dev/null 2>&1; then
    die "$label create metadata differs from its raw registry manifest"
  fi
}

create_index() {
  local target=$1
  local copy_one=$2
  shift 2
  local metadata_file
  local -a args

  create_serial=$((create_serial + 1))
  metadata_file="$work/create-$create_serial.json"
  args=(--metadata-file "$metadata_file")
  if [[ "$copy_one" == true ]]; then
    args+=(--prefer-index=false)
  fi
  args+=(-t "$target")
  if ! docker buildx imagetools create "${args[@]}" "$@"; then
    die "could not create $target"
  fi
  CREATE_METADATA_FILE=$metadata_file
}

validate_exact_directory() {
  local directory=$1
  local expected_file=$2
  local label=$3
  local actual="$work/$label.actual"
  local inventory="$work/$label.inventory"
  local entry name

  : > "$actual"
  if ! find "$directory" -mindepth 1 -maxdepth 1 -print0 > "$inventory"; then
    die "could not inventory $label directory"
  fi
  while IFS= read -r -d '' entry; do
    name=${entry##*/}
    if [[ ! "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ||
          ! -f "$entry" || -L "$entry" || ! -s "$entry" ]]; then
      die "$label contains an unsafe, linked, empty, or non-file entry: $name"
    fi
    printf '%s\n' "$name" >> "$actual"
  done < "$inventory"
  LC_ALL=C sort "$actual" -o "$actual"
  LC_ALL=C sort "$expected_file" -o "$expected_file"
  if ! cmp -s "$actual" "$expected_file"; then
    echo "container-manifest: $label does not contain the exact expected file set" >&2
    echo "expected:" >&2
    sed 's/^/  /' "$expected_file" >&2
    echo "actual:" >&2
    sed 's/^/  /' "$actual" >&2
    exit 1
  fi
}

require_empty_directory() {
  local directory=$1
  local inventory="$work/output.inventory"
  local entry
  if ! find "$directory" -mindepth 1 -maxdepth 1 -print0 > "$inventory"; then
    die "could not inventory output directory"
  fi
  while IFS= read -r -d '' entry; do
    die "output directory must be empty before staging: ${entry##*/}"
  done < "$inventory"
}

validate_platform_builds() {
  local rows=$1
  local count index row build_digest platform runnable_digest build_ref
  local expected

  count=$(jq -r 'length' <<<"$rows")
  index=0
  while ((index < count)); do
    row=$(jq -c ".[$index]" <<<"$rows")
    platform=$(jq -r '.platform' <<<"$row")
    build_digest=$(jq -r '.build_digest' <<<"$row")
    runnable_digest=$(jq -r '.runnable_digest' <<<"$row")
    build_ref="$image@$build_digest"
    inspect_reference "$build_ref" "platform build $platform" false
    if [[ "$INSPECT_DIGEST" != "$build_digest" ]]; then
      die "platform build $platform resolved to $INSPECT_DIGEST instead of $build_digest"
    fi
    expected=$(jq -cn \
      --arg platform "$platform" \
      --arg runnable_digest "$runnable_digest" \
      '[{platform: $platform, runnable_digest: $runnable_digest}]')
    validate_index_mapping "$INSPECT_RAW_FILE" "$expected" \
      "platform build $platform"
    index=$((index + 1))
  done
}

stage_manifest() {
  local tag=$1
  local source_sha=$2
  local source_epoch=$3
  local metadata_dir=$4
  local output_dir=$5
  local expected_files="$work/metadata.expected"
  local records="$work/platform-records.jsonl"
  local canonical="$work/canonical-platform.json"
  local platform platform_file platform_path normalized rows actual_platforms
  local staging_ref stage_status manifest_digest record_path build_digest
  local -a build_refs

  validate_tag "$tag"
  EXPECTED_PACKAGE_VERSION=${tag#v}
  validate_source_identity "$source_sha" "$source_epoch"
  if [[ ! -d "$metadata_dir" || -L "$metadata_dir" ||
        ! -d "$output_dir" || -L "$output_dir" ]]; then
    echo "container-manifest: metadata and output must be non-symlink directories" >&2
    exit 2
  fi
  metadata_dir=$(cd "$metadata_dir" && pwd -P)
  output_dir=$(cd "$output_dir" && pwd -P)
  if [[ "$metadata_dir" == "$output_dir" ]]; then
    echo "container-manifest: metadata and output directories must be distinct" >&2
    exit 2
  fi

  : > "$expected_files"
  : > "$records"
  for platform in "${platforms[@]}"; do
    platform_file=${platform//\//-}.json
    printf '%s\n' "$platform_file" >> "$expected_files"
  done
  validate_exact_directory "$metadata_dir" "$expected_files" metadata
  require_empty_directory "$output_dir"

  for platform in "${platforms[@]}"; do
    platform_file=${platform//\//-}.json
    platform_path="$metadata_dir/$platform_file"
    if ! jq -S . "$platform_path" > "$canonical" 2> /dev/null ||
        ! cmp -s "$platform_path" "$canonical"; then
      die "$platform_file must be canonical sorted JSON with one trailing newline"
    fi
    if ! normalized=$(jq -cer \
      --arg package "$package" \
      --arg tag "$tag" \
      --arg source_sha "$source_sha" \
      --argjson source_epoch "$source_epoch" \
      --arg image "$image" \
      --arg platform "$platform" '
        select(type == "object") |
        select((keys | sort) == ([
          "schema_version", "package", "tag", "source_sha", "source_epoch",
          "image", "platform", "build_digest", "runnable_digest", "buildkit"
        ] | sort)) |
        select(.schema_version == 1) |
        select(.package == $package and .tag == $tag) |
        select(.source_sha == $source_sha and .source_epoch == $source_epoch) |
        select(.image == $image and .platform == $platform) |
        select(.build_digest | type == "string" and
          test("^sha256:[0-9a-f]{64}$")) |
        select(.runnable_digest | type == "string" and
          test("^sha256:[0-9a-f]{64}$")) |
        select(.build_digest != .runnable_digest) |
        select(.buildkit == {provenance: true, sbom: true}) |
        {platform, build_digest, runnable_digest}
      ' "$platform_path"); then
      die "$platform_file does not match its exact build/source identity"
    fi
    printf '%s\n' "$normalized" >> "$records"
  done
  rows=$(jq -ces '
    sort_by(.platform) |
    select(length > 0) |
    select(length == ([.[].platform] | unique | length)) |
    select(length == ([.[].build_digest] | unique | length)) |
    select(length == ([.[].runnable_digest] | unique | length))
  ' "$records") || die "platform metadata contains duplicate identities"
  actual_platforms=$(jq -c '[.[].platform]' <<<"$rows")
  if [[ "$actual_platforms" != "$expected_platforms" ]]; then
    die "platform metadata does not cover the exact supported platforms"
  fi

  validate_platform_builds "$rows"
  build_refs=()
  for platform in "${platforms[@]}"; do
    build_digest=$(jq -er --arg platform "$platform" \
      '.[] | select(.platform == $platform) | .build_digest' <<<"$rows")
    build_refs+=("$image@$build_digest")
  done
  staging_ref="$image:sha-$source_sha"
  if inspect_reference "$staging_ref" "staging image $staging_ref" true; then
    # Runnable identity is normative across a recovery rerun. BuildKit evidence
    # may be run-specific, so current build_digest rows identify the submitted
    # inputs but do not claim that an older exact staging alias embeds them.
    validate_index_mapping "$INSPECT_RAW_FILE" "$rows" "staging image $staging_ref"
  else
    stage_status=$?
    if [[ $stage_status -ne 2 ]]; then
      die "could not determine whether staging image $staging_ref exists"
    fi
    create_index "$staging_ref" false "${build_refs[@]}"
    inspect_reference "$staging_ref" "new staging image $staging_ref" false
    validate_index_mapping "$INSPECT_RAW_FILE" "$rows" \
      "new staging image $staging_ref"
    validate_create_metadata "$CREATE_METADATA_FILE" "$INSPECT_DIGEST" \
      "$INSPECT_RAW_FILE" "staging image $staging_ref"
  fi
  manifest_digest=$INSPECT_DIGEST

  output_temp=$(mktemp "$output_dir/.image-manifest.XXXXXX")
  jq -cnS \
    --arg package "$package" \
    --arg tag "$tag" \
    --arg source_sha "$source_sha" \
    --argjson source_epoch "$source_epoch" \
    --arg image "$image" \
    --arg staging_ref "$staging_ref" \
    --arg manifest_digest "$manifest_digest" \
    --argjson platforms "$rows" '
      {
        schema_version: 1,
        package: $package,
        tag: $tag,
        source_sha: $source_sha,
        source_epoch: $source_epoch,
        image: $image,
        staging_ref: $staging_ref,
        manifest_digest: $manifest_digest,
        platforms: $platforms
      }
    ' > "$output_temp"
  record_path="$output_dir/image-manifest.json"
  if ! ln "$output_temp" "$record_path" 2> /dev/null; then
    die "refusing to replace existing image-manifest.json"
  fi
  rm -f "$output_temp"
  output_temp=
  echo "Staged exact attested image $staging_ref@$manifest_digest"
}

validate_release_record() {
  local record_path=$1
  local canonical="$work/canonical-image-manifest.json"
  local normalized

  if [[ ! -f "$record_path" || -L "$record_path" || ! -s "$record_path" ]]; then
    echo "container-manifest: image manifest must be a regular nonempty file" >&2
    exit 2
  fi
  if ! jq -cS . "$record_path" > "$canonical" 2> /dev/null ||
      ! cmp -s "$record_path" "$canonical"; then
    die "image-manifest.json must be canonical compact sorted JSON"
  fi
  if ! normalized=$(jq -cer \
    --arg package "$package" \
    --arg image "$image" \
    --argjson expected_platforms "$expected_platforms" '
      select(type == "object") |
      select((keys | sort) == ([
        "schema_version", "package", "tag", "source_sha", "source_epoch",
        "image", "staging_ref", "manifest_digest", "platforms"
      ] | sort)) |
      select(.schema_version == 1 and .package == $package) |
      select(.tag | type == "string") |
      select(.source_sha | type == "string" and test("^[0-9a-f]{40}$")) |
      select(.source_epoch | type == "number" and . == floor and
        . >= 315532800 and . <= 2147483647) |
      select(.image == $image) |
      select(.staging_ref == ($image + ":sha-" + .source_sha)) |
      select(.manifest_digest | type == "string" and
        test("^sha256:[0-9a-f]{64}$")) |
      select((.platforms | type) == "array") |
      select(.platforms == (.platforms | sort_by(.platform))) |
      select([.platforms[].platform] == $expected_platforms) |
      select(all(.platforms[];
        type == "object" and
        ((keys | sort) ==
          (["platform", "build_digest", "runnable_digest"] | sort)) and
        (.platform | type) == "string" and
        (.build_digest | type == "string" and
          test("^sha256:[0-9a-f]{64}$")) and
        (.runnable_digest | type == "string" and
          test("^sha256:[0-9a-f]{64}$")) and
        .build_digest != .runnable_digest)) |
      select((.platforms | length) ==
        ([.platforms[].build_digest] | unique | length)) |
      select((.platforms | length) ==
        ([.platforms[].runnable_digest] | unique | length))
    ' "$record_path"); then
    die "image-manifest.json does not match the exact release schema"
  fi
  RECORD_JSON=$normalized
}

trusted_draft_boundary() {
  local tag=$1
  local source_sha=$2
  local expected_release_id=${3:-}
  local release_id

  "$verify_release_tag" "$tag" "$source_sha" > /dev/null ||
    die "release tag $tag does not match source $source_sha"
  if [[ -n "$expected_release_id" ]]; then
    release_id=$("$verify_release" "$tag" draft "$expected_release_id") ||
      die "GitHub release $tag left its trusted draft boundary"
  else
    release_id=$("$verify_release" "$tag" draft) ||
      die "GitHub release $tag is not the trusted draft"
  fi
  printf '%s\n' "$release_id"
}

version_manifest() {
  local record_path=$1
  local tag source_sha source_epoch staging_ref manifest_digest rows version_ref
  local release_id version_status

  validate_release_record "$record_path"
  tag=$(jq -r '.tag' <<<"$RECORD_JSON")
  source_sha=$(jq -r '.source_sha' <<<"$RECORD_JSON")
  source_epoch=$(jq -r '.source_epoch' <<<"$RECORD_JSON")
  staging_ref=$(jq -r '.staging_ref' <<<"$RECORD_JSON")
  manifest_digest=$(jq -r '.manifest_digest' <<<"$RECORD_JSON")
  rows=$(jq -c '.platforms' <<<"$RECORD_JSON")
  validate_tag "$tag"
  EXPECTED_PACKAGE_VERSION=${tag#v}
  validate_source_identity "$source_sha" "$source_epoch"

  validate_platform_builds "$rows"
  inspect_reference "$staging_ref" "staging image $staging_ref" false
  validate_index_mapping "$INSPECT_RAW_FILE" "$rows" "staging image $staging_ref"
  if [[ "$INSPECT_DIGEST" != "$manifest_digest" ]]; then
    die "staging image $staging_ref moved from $manifest_digest to $INSPECT_DIGEST"
  fi

  release_id=$(trusted_draft_boundary "$tag" "$source_sha")
  version_ref="$image:${tag#v}"
  if inspect_reference "$version_ref" "version image $version_ref" true; then
    validate_index_mapping "$INSPECT_RAW_FILE" "$rows" "version image $version_ref"
    if [[ "$INSPECT_DIGEST" != "$manifest_digest" ]]; then
      die "existing version image $version_ref differs from $manifest_digest"
    fi
  else
    version_status=$?
    if [[ $version_status -ne 2 ]]; then
      die "could not determine whether version image $version_ref exists"
    fi
    create_index "$version_ref" true "$image@$manifest_digest"
    inspect_reference "$version_ref" "new version image $version_ref" false
    validate_index_mapping "$INSPECT_RAW_FILE" "$rows" "new version image $version_ref"
    if [[ "$INSPECT_DIGEST" != "$manifest_digest" ]]; then
      die "new version image $version_ref differs from $manifest_digest"
    fi
    validate_create_metadata "$CREATE_METADATA_FILE" "$manifest_digest" \
      "$INSPECT_RAW_FILE" "version image $version_ref"
  fi
  trusted_draft_boundary "$tag" "$source_sha" "$release_id" > /dev/null
  echo "Published exact version image $version_ref@$manifest_digest"
}

trusted_public_snapshot() {
  local endpoint=$1
  local label=$2
  local release snapshot

  if ! release=$(gh api "$endpoint"); then
    die "could not read $label"
  fi
  if ! snapshot=$(jq -cer '
      select(type == "object") |
      select(.id | type == "number" and . > 0 and . == floor) |
      select(.tag_name | type == "string" and
        test("^v(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)$")) |
      select(.name == .tag_name) |
      select(.draft == false and .prerelease == false and .immutable == true) |
      select((.author.login // "") == "github-actions[bot]") |
      select((.author.type // "") == "Bot") |
      {id, tag: .tag_name}
    ' <<<"$release"); then
    die "$label is not an immutable bot-owned canonical release"
  fi
  printf '%s\n' "$snapshot"
}

authenticate_public_release_tag() {
  local snapshot=$1
  local source_sha=$2
  local tag

  tag=$(jq -r '.tag' <<<"$snapshot")
  "$verify_release_tag" "$tag" "$source_sha" > /dev/null ||
    die "immutable release $tag does not match its live annotated tag"
}

download_record_bound_asset() {
  local assets=$1
  local identity=$2
  local label=$3
  local name expected_size expected_sha256 asset asset_id path
  local actual_size actual_digest

  name=$(jq -r '.name' <<<"$identity")
  expected_size=$(jq -r '.size' <<<"$identity")
  expected_sha256=$(jq -r '.sha256' <<<"$identity")
  if ! asset=$(jq -cer \
      --arg name "$name" \
      --argjson size "$expected_size" \
      --arg digest "sha256:$expected_sha256" '
        select(type == "array") |
        [.[] | select(.name == $name)] |
        select(length == 1) | .[0] |
        select(type == "object") |
        select(.id | type == "number" and . > 0 and . == floor) |
        select(.state == "uploaded") |
        select(.size == $size and .digest == $digest) |
        {id, name, size, digest}
      ' <<<"$assets"); then
    die "$label does not exactly match immutable release asset metadata"
  fi
  asset_id=$(jq -r '.id' <<<"$asset")
  path=$(mktemp "$work/release-asset.XXXXXX")
  if ! gh api "repos/$repository/releases/assets/$asset_id" \
      -H 'Accept: application/octet-stream' > "$path"; then
    die "could not download $label by immutable asset ID"
  fi
  if [[ ! -f "$path" || -L "$path" || ! -s "$path" ]]; then
    die "$label download is not one regular nonempty file"
  fi
  actual_size=$(wc -c < "$path" | tr -d '[:space:]')
  actual_digest=$(sha256_file "$path")
  if [[ "$actual_size" != "$expected_size" ||
        "$actual_digest" != "sha256:$expected_sha256" ]]; then
    die "$label differs from its release-record identity"
  fi
  DOWNLOADED_ASSET_PATH=$path
}

verify_release_record_attestations() {
  local snapshot=$1
  local tag source_sha manifest_digest provenance_identity sbom_identity
  local provenance_path sbom_path trusted_root subject
  local -a common

  tag=$(jq -r '.tag' <<<"$snapshot")
  source_sha=$RELEASE_RECORD_SOURCE_SHA
  manifest_digest=$RELEASE_RECORD_DIGEST
  provenance_identity=$RELEASE_RECORD_PROVENANCE
  sbom_identity=$RELEASE_RECORD_SBOM_ATTESTATION

  download_record_bound_asset "$RELEASE_ASSETS_JSON" \
    "$provenance_identity" "container provenance bundle for $tag"
  provenance_path=$DOWNLOADED_ASSET_PATH
  download_record_bound_asset "$RELEASE_ASSETS_JSON" \
    "$sbom_identity" "container SBOM bundle for $tag"
  sbom_path=$DOWNLOADED_ASSET_PATH

  trusted_root="$work/trusted-root.jsonl"
  if ! gh attestation trusted-root > "$trusted_root" ||
      [[ ! -f "$trusted_root" || -L "$trusted_root" || ! -s "$trusted_root" ]]; then
    die "could not load the Sigstore trusted root"
  fi
  subject="oci://$image@$manifest_digest"
  common=(
    --repo "$repository"
    --custom-trusted-root "$trusted_root"
    --signer-workflow "$repository/.github/workflows/release-binaries.yml"
    --source-digest "$source_sha"
    --source-ref "refs/tags/$tag"
    --deny-self-hosted-runners
  )
  if ! gh attestation verify "$subject" \
      --bundle "$provenance_path" \
      --predicate-type https://slsa.dev/provenance/v1 \
      "${common[@]}" > /dev/null; then
    die "container provenance bundle for $tag failed trusted verification"
  fi
  if ! gh attestation verify "$subject" \
      --bundle "$sbom_path" \
      --predicate-type https://spdx.dev/Document/v2.3 \
      "${common[@]}" > /dev/null; then
    die "container SBOM bundle for $tag failed trusted verification"
  fi
}

load_trusted_release_record() {
  local snapshot=$1 release_id tag record_name assets asset asset_id
  local expected_size expected_digest record_path actual_size actual_digest record

  release_id=$(jq -r '.id' <<<"$snapshot")
  tag=$(jq -r '.tag' <<<"$snapshot")
  record_name="$package-$tag-release.json"
  if ! assets=$(gh api \
      "repos/$repository/releases/$release_id/assets?per_page=100"); then
    die "could not list assets for immutable release $tag"
  fi
  if ! asset=$(jq -cer --arg name "$record_name" '
      select(type == "array") |
      [.[] | select(.name == $name)] |
      select(length == 1) | .[0] |
      select(type == "object") |
      select((keys | index("id")) != null) |
      select(.id | type == "number" and . > 0 and . == floor) |
      select(.state == "uploaded") |
      select(.size | type == "number" and . > 0 and . == floor) |
      select(.digest | type == "string" and
        test("^sha256:[0-9a-f]{64}$")) |
      {id, size, digest}
    ' <<<"$assets"); then
    die "immutable release $tag does not contain one canonical release record"
  fi
  asset_id=$(jq -r '.id' <<<"$asset")
  expected_size=$(jq -r '.size' <<<"$asset")
  expected_digest=$(jq -r '.digest' <<<"$asset")
  record_path=$(mktemp "$work/latest-release-record.XXXXXX")
  if ! gh api "repos/$repository/releases/assets/$asset_id" \
      -H 'Accept: application/octet-stream' > "$record_path"; then
    die "could not download the canonical release record for $tag"
  fi
  actual_size=$(wc -c < "$record_path" | tr -d '[:space:]')
  actual_digest=$(sha256_file "$record_path")
  if [[ "$actual_size" != "$expected_size" ||
        "$actual_digest" != "$expected_digest" ]]; then
    die "canonical release record for $tag differs from immutable asset metadata"
  fi
  if ! record=$(jq -cer \
      --arg package "$package" \
      --arg tag "$tag" \
      --arg image "$image" \
      --argjson expected_platforms "$expected_platforms" '
      select(type == "object") |
      select((keys | sort) == ([
        "schema_version", "package", "tag", "version", "source_sha",
        "source_epoch", "release_targets", "native", "container"
      ] | sort)) |
      select(.schema_version == 1 and .package == $package and .tag == $tag) |
      select(.version == ($tag | ltrimstr("v"))) |
      select(.source_sha | type == "string" and test("^[0-9a-f]{40}$")) |
      select(.source_epoch | type == "number" and . == floor and
        . >= 315532800 and . <= 2147483647) |
      select((.release_targets | type) == "object") |
      select((.native | type) == "array") |
      select((.container | type) == "object") |
      select((.container | keys | sort) == ([
        "image", "manifest_digest", "platforms", "sbom", "attestations"
      ] | sort)) |
      select(.container.image == $image) |
      select(.container.manifest_digest | type == "string" and
        test("^sha256:[0-9a-f]{64}$")) |
      select((.container.platforms | type) == "array") |
      select([.container.platforms[].platform] == $expected_platforms) |
      select(all(.container.platforms[];
        type == "object" and
        ((keys | sort) == (["platform", "runnable_digest"] | sort)) and
        (.runnable_digest | type == "string" and
          test("^sha256:[0-9a-f]{64}$")))) |
      select((.container.sbom | type) == "object") |
      def file_identity($name):
        type == "object" and
        ((keys | sort) == (["name", "size", "sha256"] | sort)) and
        .name == $name and
        (.size | type == "number" and . > 0 and . == floor) and
        (.sha256 | type) == "string" and
        (.sha256 | test("^[0-9a-f]{64}$"));
      select(.container.sbom |
        file_identity($package + "-" + $tag + "-container.spdx.json")) |
      select((.container.attestations | type) == "object") |
      select((.container.attestations | keys | sort) ==
        (["provenance", "sbom"] | sort)) |
      select(.container.attestations.provenance |
        file_identity($package + "-" + $tag +
          "-container.provenance.sigstore.json")) |
      select(.container.attestations.sbom |
        file_identity($package + "-" + $tag +
          "-container.sbom.sigstore.json")) |
      {
        source_sha: .source_sha,
        manifest_digest: .container.manifest_digest,
        platforms: .container.platforms,
        provenance: .container.attestations.provenance,
        sbom_attestation: .container.attestations.sbom
      }
    ' "$record_path"); then
    die "canonical release record for $tag has an invalid container identity"
  fi
  RELEASE_ASSETS_JSON=$assets
  RELEASE_RECORD_SOURCE_SHA=$(jq -r '.source_sha' <<<"$record")
  RELEASE_RECORD_DIGEST=$(jq -r '.manifest_digest' <<<"$record")
  RELEASE_RECORD_ROWS=$(jq -c '.platforms' <<<"$record")
  RELEASE_RECORD_PROVENANCE=$(jq -c '.provenance' <<<"$record")
  RELEASE_RECORD_SBOM_ATTESTATION=$(jq -c '.sbom_attestation' <<<"$record")
}

latest_manifest() {
  local attempt before confirmed after final tag version_ref latest_ref
  local version_digest latest_digest

  latest_ref="$image:latest"
  for attempt in 1 2 3 4 5; do
    before=$(trusted_public_snapshot \
      "repos/$repository/releases/latest" "GitHub's latest release")
    tag=$(jq -r '.tag' <<<"$before")
    validate_tag "$tag"
    EXPECTED_PACKAGE_VERSION=${tag#v}
    load_trusted_release_record "$before"
    authenticate_public_release_tag "$before" "$RELEASE_RECORD_SOURCE_SHA"
    verify_release_record_attestations "$before"
    version_ref="$image:${tag#v}"
    inspect_reference "$version_ref" "latest version image $version_ref" false
    version_digest=$INSPECT_DIGEST
    validate_index_mapping "$INSPECT_RAW_FILE" "$RELEASE_RECORD_ROWS" \
      "latest version image $version_ref"
    if [[ "$version_digest" != "$RELEASE_RECORD_DIGEST" ]]; then
      die "version image $version_ref differs from immutable release record $RELEASE_RECORD_DIGEST"
    fi

    confirmed=$(trusted_public_snapshot \
      "repos/$repository/releases/latest" "GitHub's latest release")
    if [[ "$confirmed" != "$before" ]]; then
      echo "Latest release changed before write on attempt $attempt; retrying" >&2
      continue
    fi
    # The immutable release, its canonical record, and both signed bundles have
    # now been authenticated. Re-resolve the live annotated tag immediately
    # before granting the mutable registry write.
    authenticate_public_release_tag "$confirmed" "$RELEASE_RECORD_SOURCE_SHA"

    create_index "$latest_ref" true "$image@$version_digest"
    after=$(trusted_public_snapshot \
      "repos/$repository/releases/latest" "GitHub's latest release")
    if [[ "$after" != "$before" ]]; then
      echo "Latest release changed after write on attempt $attempt; retrying" >&2
      continue
    fi
    authenticate_public_release_tag "$after" "$RELEASE_RECORD_SOURCE_SHA"

    inspect_reference "$latest_ref" "image $latest_ref" false
    latest_digest=$INSPECT_DIGEST
    validate_index_mapping "$INSPECT_RAW_FILE" "$RELEASE_RECORD_ROWS" "image $latest_ref"
    if [[ "$latest_digest" != "$RELEASE_RECORD_DIGEST" ]]; then
      die "image $latest_ref differs from immutable release record $RELEASE_RECORD_DIGEST"
    fi
    validate_create_metadata "$CREATE_METADATA_FILE" "$latest_digest" \
      "$INSPECT_RAW_FILE" "image $latest_ref"
    final=$(trusted_public_snapshot \
      "repos/$repository/releases/latest" "GitHub's latest release")
    if [[ "$final" != "$before" ]]; then
      echo "Latest release changed during verification on attempt $attempt; retrying" >&2
      continue
    fi
    authenticate_public_release_tag "$final" "$RELEASE_RECORD_SOURCE_SHA"
    echo "Reconciled $latest_ref to immutable release $tag@$version_digest"
    return 0
  done
  die "GitHub's latest release did not stabilize after five reconciliation attempts"
}

case "$mode" in
  stage)
    stage_manifest "$2" "$3" "$4" "$5" "$6"
    ;;
  version)
    version_manifest "$2"
    ;;
  latest)
    latest_manifest
    ;;
esac
