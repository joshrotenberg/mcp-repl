#!/usr/bin/env bash
# Build the canonical identity record for one complete native/container release.
set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "usage: $0 <tag> <source-sha> <source-epoch> <native-dir> <container-dir> <output>" >&2
  exit 2
fi

tag=$1
source_sha=$2
source_epoch=$3
native_dir=$4
container_dir=$5
output=$6

root=$(cd "$(dirname "$0")/.." && pwd)
release_targets="$root/scripts/release-targets.sh"
package=mcp-repl
semver_component='(0|[1-9][0-9]*)'

die() {
  echo "release-record: $*" >&2
  exit 1
}

if [[ ! "$tag" =~ ^v${semver_component}\.${semver_component}\.${semver_component}$ ||
      ${#tag} -gt 64 ]]; then
  echo "release-record: tag must be a canonical vX.Y.Z version" >&2
  exit 2
fi
version=${tag#v}
if [[ ! "$source_sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo "release-record: source SHA must be 40 lowercase hexadecimal characters" >&2
  exit 2
fi
if [[ ! "$source_epoch" =~ ^(0|[1-9][0-9]{0,9})$ ]] ||
  ((source_epoch < 315532800 || source_epoch > 2147483647)); then
  echo "release-record: source epoch must be canonical UTC seconds from 1980 through 2038" >&2
  exit 2
fi
if [[ ! -d "$native_dir" || -L "$native_dir" ||
      ! -d "$container_dir" || -L "$container_dir" ]]; then
  echo "release-record: native and container inputs must be non-symlink directories" >&2
  exit 2
fi
if [[ -z "$output" || -e "$output" || -L "$output" ]]; then
  echo "release-record: output must be a new path" >&2
  exit 2
fi

output_parent=$(dirname "$output")
output_name=$(basename "$output")
if [[ ! "$output_name" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ||
      ! -d "$output_parent" || -L "$output_parent" ]]; then
  echo "release-record: output name or parent directory is unsafe" >&2
  exit 2
fi

native_dir=$(cd "$native_dir" && pwd -P)
container_dir=$(cd "$container_dir" && pwd -P)
output_parent=$(cd "$output_parent" && pwd -P)
output="$output_parent/$output_name"
if [[ "$native_dir" == "$container_dir" ||
      "$output_parent" == "$native_dir" ||
      "$output_parent" == "$container_dir" ]]; then
  echo "release-record: input directories and output parent must be distinct" >&2
  exit 2
fi

for required_command in jq find sort cmp wc awk cat; do
  command -v "$required_command" > /dev/null 2>&1 ||
    die "$required_command is required"
done
if command -v sha256sum > /dev/null 2>&1; then
  sha256_command=sha256sum
elif command -v shasum > /dev/null 2>&1; then
  sha256_command=shasum
else
  die "sha256sum or shasum is required"
fi

"$release_targets" validate > /dev/null ||
  die "release target manifest validation failed"

work=$(mktemp -d "$output_parent/.release-record.XXXXXX")
output_created=false
cleanup_record() {
  local status=$?
  if [[ $status -ne 0 && "$output_created" == true ]]; then
    rm -f "$output"
  fi
  rm -rf "$work"
  return "$status"
}
trap cleanup_record EXIT
native_expected="$work/native.expected"
container_expected="$work/container.expected"
native_records="$work/native-records.jsonl"
platform_records="$work/platform-records.jsonl"
: > "$native_expected"
: > "$container_expected"
: > "$native_records"
: > "$platform_records"

sha256_file() {
  local path=$1
  local digest
  if [[ "$sha256_command" == sha256sum ]]; then
    digest=$(sha256sum "$path" | awk '{print $1}')
  else
    digest=$(shasum -a 256 "$path" | awk '{print $1}')
  fi
  [[ "$digest" =~ ^[0-9a-f]{64}$ ]] ||
    die "could not compute canonical SHA-256 for ${path##*/}"
  printf '%s\n' "$digest"
}

file_size() {
  local path=$1
  local size
  size=$(wc -c < "$path" | tr -d '[:space:]')
  [[ "$size" =~ ^(0|[1-9][0-9]*)$ ]] ||
    die "could not compute canonical size for ${path##*/}"
  printf '%s\n' "$size"
}

file_identity() {
  local path=$1
  local name=${path##*/}
  local digest size
  digest=$(sha256_file "$path")
  size=$(file_size "$path")
  jq -cn \
    --arg name "$name" \
    --arg sha256 "$digest" \
    --argjson size "$size" \
    '{name: $name, size: $size, sha256: $sha256}'
}

require_native_spdx() {
  local path=$1
  local archive_name=$2
  local archive_digest=$3
  local repository=$4
  local namespace created

  namespace="https://github.com/$repository/releases/download/$tag/$archive_name.spdx.json#$source_sha-$archive_digest"
  created=$(jq -nr --argjson epoch "$source_epoch" '$epoch | todateiso8601') ||
    die "could not derive the native SBOM creation timestamp"
  # Syft can independently classify a PE binary with the same display name as
  # its Cargo package. Bind the release crate by its exact version and Cargo
  # purl; a name-only uniqueness check would reject that valid extra evidence.
  jq -e \
    --arg archive_name "$archive_name" \
    --arg archive_digest "$archive_digest" \
    --arg version "$version" \
    --arg namespace "$namespace" \
    --arg evident_comment "evident-by: indicates the package's existence is evident by the given file" \
    --arg created "$created" '
    type == "object" and
    .spdxVersion == "SPDX-2.3" and
    .SPDXID == "SPDXRef-DOCUMENT" and
    .dataLicense == "CC0-1.0" and
    .name == $archive_name and
    .documentNamespace == $namespace and
    (has("documentDescribes") | not) and
    (.creationInfo | type) == "object" and
    .creationInfo.created == $created and
    (.creationInfo.creators | type) == "array" and
    (.creationInfo.creators | length) > 0 and
    all(.creationInfo.creators[]; type == "string" and length > 0) and
    (.packages | type) == "array" and
    (.packages | length) > 1 and
    all(.packages[];
      type == "object" and
      (.SPDXID | type) == "string" and (.SPDXID | length) > 0 and
      (.name | type) == "string" and (.name | length) > 0) and
    ([.packages[].SPDXID] | length) == ([.packages[].SPDXID] | unique | length) and
    (.files | type) == "array" and
    (.files | length) > 0 and
    all(.files[];
      type == "object" and
      (.SPDXID | type) == "string" and (.SPDXID | length) > 0 and
      (.fileName | type) == "string" and (.fileName | length) > 0) and
    ([.files[].SPDXID] | length) ==
      ([.files[].SPDXID] | unique | length) and
    (.relationships | type) == "array" and
    (.relationships | length) > 0 and
    (((.packages | map(.SPDXID)) +
       ((.files? // []) | map(.SPDXID)) +
       ["SPDXRef-DOCUMENT"]) as $ids |
      all(.relationships[];
        . as $relationship |
        type == "object" and
        (.spdxElementId | type) == "string" and
        (.relatedSpdxElement | type) == "string" and
        (.relationshipType | type) == "string" and
        (.relationshipType | length) > 0 and
        ($ids | index($relationship.spdxElementId)) != null and
        ($ids | index($relationship.relatedSpdxElement)) != null) and
      ([.packages[] |
        select(.name == "mcp-repl") |
        select(.versionInfo == $version) |
        select(.downloadLocation == "NOASSERTION") |
        select(any(.externalRefs[]?;
          .referenceCategory == "PACKAGE-MANAGER" and
          .referenceType == "purl" and
          .referenceLocator == ("pkg:cargo/mcp-repl@" + $version))) |
        .SPDXID] | unique) as $mcp_ids |
      ([.packages[] |
        select(.name == $archive_name) |
        select(.versionInfo == $version) |
        select(.supplier == "NOASSERTION") |
        select(.downloadLocation == "NOASSERTION") |
        select(.filesAnalyzed == false) |
        select(.licenseConcluded == "NOASSERTION") |
        select(.licenseDeclared == "NOASSERTION") |
        select(.copyrightText == "NOASSERTION") |
        select(.primaryPackagePurpose == "FILE") |
        select((.checksums | type) == "array") |
        select(any(.checksums[];
          .algorithm == "SHA256" and .checksumValue == $archive_digest)) |
        .SPDXID] | unique) as $root_ids |
      ($mcp_ids | length) == 1 and
      ($root_ids | length) == 1 and
      ($mcp_ids[0]) as $mcp_id |
      ($root_ids[0]) as $root_id |
      ([.packages[] |
        select(.SPDXID != $mcp_id and .SPDXID != $root_id) |
        select((.versionInfo | type) == "string" and
          (.versionInfo | length) > 0) |
        select(any(.externalRefs[]?;
          .referenceCategory == "PACKAGE-MANAGER" and
          .referenceType == "purl" and
          (.referenceLocator | type) == "string" and
          (.referenceLocator | test("^pkg:cargo/[^@/?#]+@[^/?#]+")))) |
        .SPDXID] | unique) as $cargo_dependency_ids |
      ([.files[] |
        select(.fileName | test("(^|/)mcp-repl(\\.exe)?$")) |
        .SPDXID] | unique) as $binary_file_ids |
      ($cargo_dependency_ids | length) > 0 and
      ($binary_file_ids | length) > 0 and
      any(.relationships[];
        .spdxElementId == "SPDXRef-DOCUMENT" and
        .relationshipType == "DESCRIBES" and
        .relatedSpdxElement == $root_id) and
      any(.relationships[];
        .spdxElementId == $root_id and
        .relationshipType == "CONTAINS" and
        .relatedSpdxElement == $mcp_id) and
      any(.relationships[];
        . as $relationship |
        ($cargo_dependency_ids | index($relationship.spdxElementId)) != null and
        $relationship.relationshipType == "DEPENDENCY_OF" and
        $relationship.relatedSpdxElement == $mcp_id) and
      any(.relationships[];
        . as $relationship |
        $relationship.spdxElementId == $mcp_id and
        $relationship.relationshipType == "OTHER" and
        $relationship.comment == $evident_comment and
        ($binary_file_ids | index($relationship.relatedSpdxElement)) != null))
  ' "$path" > /dev/null ||
    die "${path##*/} does not bind the exact native archive identity"
}

require_container_spdx() {
  local path=$1
  local image=$2
  local version=$3
  local tag=$4
  local source_sha=$5
  local source_epoch=$6
  local manifest_digest=$7
  local platforms=$8
  local repository=${image#ghcr.io/}
  local manifest_hex=${manifest_digest#sha256:}
  local namespace created

  namespace="https://github.com/$repository/releases/download/$tag/$package-$tag-container.spdx.json#$source_sha-$manifest_hex"
  created=$(jq -nr --argjson epoch "$source_epoch" '$epoch | todateiso8601') ||
    die "could not derive the container SBOM creation timestamp"
  jq -e \
    --arg image "$image" \
    --arg version "$version" \
    --arg manifest_digest "$manifest_digest" \
    --arg namespace "$namespace" \
    --arg created "$created" \
    --argjson platforms "$platforms" '
      def checksum($digest):
        [{algorithm: "SHA256", checksumValue: ($digest | sub("^sha256:"; ""))}];
      def platform_id($platform):
        "SPDXRef-Platform-" + ($platform | gsub("/"; "-"));
      def package_identity:
        {
          SPDXID,
          name,
          versionInfo,
          downloadLocation,
          filesAnalyzed,
          checksums,
          licenseConcluded,
          licenseDeclared,
          copyrightText,
          comment
        };
      type == "object" and
      ((keys | sort) == ([
        "spdxVersion", "dataLicense", "SPDXID", "name",
        "documentNamespace", "creationInfo", "documentDescribes",
        "packages", "relationships", "annotations"
      ] | sort)) and
      .spdxVersion == "SPDX-2.3" and
      .dataLicense == "CC0-1.0" and
      .SPDXID == "SPDXRef-DOCUMENT" and
      .name == ($image + ":" + $version) and
      .documentNamespace == $namespace and
      .creationInfo == {
        created: $created,
        creators: ["Tool: mcp-repl release pipeline"]
      } and
      .documentDescribes == ["SPDXRef-ImageIndex"] and
      ([.packages[] | package_identity] | sort_by(.SPDXID)) ==
        ([{
          SPDXID: "SPDXRef-ImageIndex",
          name: $image,
          versionInfo: $version,
          downloadLocation: "NOASSERTION",
          filesAnalyzed: false,
          checksums: checksum($manifest_digest),
          licenseConcluded: "NOASSERTION",
          licenseDeclared: "NOASSERTION",
          copyrightText: "NOASSERTION",
          comment: ("Immutable OCI index " + $image + "@" + $manifest_digest +
            "; the complete per-platform component inventories and build provenance are embedded OCI BuildKit attestations committed by this index.")
        }] + [$platforms[] | {
          SPDXID: platform_id(.platform),
          name: ($image + " (" + .platform + ")"),
          versionInfo: $version,
          downloadLocation: "NOASSERTION",
          filesAnalyzed: false,
          checksums: checksum(.runnable_digest),
          licenseConcluded: "NOASSERTION",
          licenseDeclared: "NOASSERTION",
          copyrightText: "NOASSERTION",
          comment: ("Immutable OCI platform manifest " + $image + "@" +
            .runnable_digest + "; its component SBOM is an OCI attestation on the enclosing release index.")
        }] | sort_by(.SPDXID)) and
      ([.relationships[] | {
        spdxElementId,
        relationshipType,
        relatedSpdxElement
      }] | sort_by(.spdxElementId, .relatedSpdxElement)) ==
        ([{
          spdxElementId: "SPDXRef-DOCUMENT",
          relationshipType: "DESCRIBES",
          relatedSpdxElement: "SPDXRef-ImageIndex"
        }] + [$platforms[] | {
          spdxElementId: "SPDXRef-ImageIndex",
          relationshipType: "CONTAINS",
          relatedSpdxElement: platform_id(.platform)
        }] | sort_by(.spdxElementId, .relatedSpdxElement)) and
      (.annotations | type) == "array" and
      (.annotations | length) == 1 and
      .annotations[0].annotationDate == $created and
      .annotations[0].annotationType == "OTHER" and
      .annotations[0].annotator == "Tool: mcp-repl release pipeline"
    ' "$path" > /dev/null 2>&1 ||
    die "${path##*/} does not bind the exact container release identity"
}

require_sigstore_bundle() {
  local path=$1
  local subject_name=$2
  local subject_sha256=$3
  local predicate_type=$4
  jq -e \
    --arg subject_name "$subject_name" \
    --arg subject_sha256 "$subject_sha256" \
    --arg predicate_type "$predicate_type" '
    type == "object" and
    .mediaType == "application/vnd.dev.sigstore.bundle.v0.3+json" and
    (.verificationMaterial | type) == "object" and
    (has("messageSignature") | not) and
    (.dsseEnvelope | type) == "object" and
    ((.dsseEnvelope | keys | sort) ==
      (["payloadType", "payload", "signatures"] | sort)) and
    .dsseEnvelope.payloadType == "application/vnd.in-toto+json" and
    (.dsseEnvelope.payload | type) == "string" and
    (.dsseEnvelope.payload | length) > 0 and
    (.dsseEnvelope.signatures | type) == "array" and
    (.dsseEnvelope.signatures | length) > 0 and
    all(.dsseEnvelope.signatures[];
      type == "object" and
      (.sig | type) == "string" and (.sig | length) > 0) and
    (try (.dsseEnvelope.payload | @base64d | fromjson) catch null) as $statement |
    ($statement | type) == "object" and
    (($statement | keys | sort) ==
      (["_type", "subject", "predicateType", "predicate"] | sort)) and
    $statement._type == "https://in-toto.io/Statement/v1" and
    $statement.predicateType == $predicate_type and
    ($statement.predicate | type) == "object" and
    $statement.subject == [{
      name: $subject_name,
      digest: {sha256: $subject_sha256}
    }]
  ' "$path" > /dev/null 2>&1 ||
    die "${path##*/} does not bind the exact $predicate_type subject"
}

require_bundle_predicate() {
  local bundle_path=$1
  local predicate_path=$2
  jq -e --slurpfile expected "$predicate_path" '
    (try (.dsseEnvelope.payload | @base64d | fromjson) catch null) as $statement |
    ($expected | length) == 1 and
    $statement.predicate == $expected[0]
  ' "$bundle_path" > /dev/null 2>&1 ||
    die "${bundle_path##*/} does not sign the exact adjacent SPDX document"
}

validate_exact_directory() {
  local directory=$1
  local expected=$2
  local label=$3
  local actual="$work/$label.actual"
  local entry name
  : > "$actual"
  while IFS= read -r -d '' entry; do
    name=${entry##*/}
    if [[ ! "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ||
          ! -f "$entry" || -L "$entry" || ! -s "$entry" ]]; then
      die "$label contains an unsafe, linked, empty, or non-file entry: $name"
    fi
    printf '%s\n' "$name" >> "$actual"
  done < <(find "$directory" -mindepth 1 -maxdepth 1 -print0)
  LC_ALL=C sort -u "$actual" -o "$actual"
  LC_ALL=C sort -u "$expected" -o "$expected"
  if ! cmp -s "$actual" "$expected"; then
    echo "release-record: $label does not contain the exact expected file set" >&2
    echo "expected:" >&2
    sed 's/^/  /' "$expected" >&2
    echo "actual:" >&2
    sed 's/^/  /' "$actual" >&2
    exit 1
  fi
}

# First derive the only accepted native inventory from the validated manifest.
while IFS=$'\t' read -r target archive_extension binary_name; do
  [[ -n "$target" && -n "$archive_extension" && -n "$binary_name" ]] ||
    die "release target manifest emitted an incomplete native row"
  archive_name="$package-$tag-$target.$archive_extension"
  printf '%s\n' \
    "$archive_name" \
    "$archive_name.sha256" \
    "$archive_name.spdx.json" \
    "$archive_name.provenance.sigstore.json" \
    "$archive_name.sbom.sigstore.json" >> "$native_expected"
done < <("$release_targets" rows)

# Container metadata uses a reversible, safe filename for each platform.
while IFS= read -r platform; do
  [[ "$platform" =~ ^[a-z0-9][a-z0-9._-]*/[a-z0-9][a-z0-9._-]*$ ]] ||
    die "release target manifest emitted an unsafe container platform"
  platform_file=${platform//\//-}.json
  printf '%s\n' "$platform_file" >> "$container_expected"
done < <("$release_targets" container-platforms)
container_image_base="$package-$tag-container"
printf '%s\n' \
  image-manifest.json \
  "$container_image_base.spdx.json" \
  "$container_image_base.provenance.sigstore.json" \
  "$container_image_base.sbom.sigstore.json" >> "$container_expected"

validate_exact_directory "$native_dir" "$native_expected" native
validate_exact_directory "$container_dir" "$container_expected" container

# Verify every native checksum and bind every file identity into one target row.
while IFS=$'\t' read -r target archive_extension binary_name; do
  archive_name="$package-$tag-$target.$archive_extension"
  archive_path="$native_dir/$archive_name"
  checksum_path="$native_dir/$archive_name.sha256"
  sbom_path="$native_dir/$archive_name.spdx.json"
  provenance_path="$native_dir/$archive_name.provenance.sigstore.json"
  sbom_bundle_path="$native_dir/$archive_name.sbom.sigstore.json"

  archive_digest=$(sha256_file "$archive_path")
  checksum_line="$archive_digest  $archive_name"
  checksum_size=$(file_size "$checksum_path")
  if [[ "$checksum_size" -ne $((${#checksum_line} + 1)) ||
        "$(<"$checksum_path")" != "$checksum_line" ]]; then
    die "$archive_name.sha256 does not canonically self-check $archive_name"
  fi
  # Repository identity is validated later from all container platform rows.
  require_sigstore_bundle \
    "$provenance_path" \
    "$archive_name" \
    "$archive_digest" \
    "https://slsa.dev/provenance/v1"
  require_sigstore_bundle \
    "$sbom_bundle_path" \
    "$archive_name" \
    "$archive_digest" \
    "https://spdx.dev/Document/v2.3"

  archive_identity=$(file_identity "$archive_path")
  checksum_identity=$(file_identity "$checksum_path")
  sbom_identity=$(file_identity "$sbom_path")
  provenance_identity=$(file_identity "$provenance_path")
  sbom_bundle_identity=$(file_identity "$sbom_bundle_path")
  jq -cn \
    --arg target "$target" \
    --arg binary "$binary_name" \
    --argjson archive "$archive_identity" \
    --argjson checksum "$checksum_identity" \
    --argjson sbom "$sbom_identity" \
    --argjson provenance "$provenance_identity" \
    --argjson sbom_bundle "$sbom_bundle_identity" '
      {
        target: $target,
        binary: $binary,
        archive: $archive,
        checksum: $checksum,
        sbom: $sbom,
        attestations: {
          provenance: $provenance,
          sbom: $sbom_bundle
        }
      }
    ' >> "$native_records"
done < <("$release_targets" rows)

# Validate platform metadata against both the runnable registry identity and
# the credential-free build output from which it was imported.
while IFS= read -r platform; do
  platform_file=${platform//\//-}.json
  platform_path="$container_dir/$platform_file"
  if ! normalized_platform=$(jq -cer \
    --arg package "$package" \
    --arg tag "$tag" \
    --arg source_sha "$source_sha" \
    --argjson source_epoch "$source_epoch" \
    --arg platform "$platform" '
      select(type == "object") |
      select((keys | sort) == ([
        "schema_version", "package", "tag", "source_sha", "source_epoch",
        "image", "platform", "runnable_digest", "build_digest", "buildkit"
      ] | sort)) |
      select(.schema_version == 1) |
      select(.package == $package and .tag == $tag) |
      select(.source_sha == $source_sha and .source_epoch == $source_epoch) |
      select(.platform == $platform) |
      select((.image | type) == "string") |
      select(.buildkit == {provenance: true, sbom: true}) |
      select(.runnable_digest | type == "string" and test("^sha256:[0-9a-f]{64}$")) |
      select(.build_digest | type == "string" and test("^sha256:[0-9a-f]{64}$")) |
      {
        image,
        platform,
        runnable_digest,
        build_digest
      }
    ' "$platform_path"); then
    die "$platform_file does not match its release/platform identity"
  fi
  # The staging metadata identity is checked, but its build-output digest is
  # intentionally not durable release identity: BuildKit evidence can vary
  # across equivalent rebuilds while the runnable registry digest remains the
  # normative platform identity.
  file_identity "$platform_path" > /dev/null
  jq -cn \
    --argjson platform "$normalized_platform" \
    '$platform' >> "$platform_records"
done < <("$release_targets" container-platforms)

platform_rows=$(jq -cs 'sort_by(.platform)' "$platform_records")
if ! image=$(jq -er '
  ([.[].image] | unique) as $images |
  select(($images | length) == 1) |
  $images[0]
' <<<"$platform_rows"); then
  die "container platform metadata does not bind one common image"
fi
image_repository=$image
if [[ ! "$image_repository" =~ ^ghcr\.io/[a-z0-9][a-z0-9._-]*/[a-z0-9][a-z0-9._-]*$ ]]; then
  die "container platform metadata contains an unsafe GHCR image repository"
fi
repository=${image_repository#ghcr.io/}
while IFS=$'\t' read -r target archive_extension _binary_name; do
  archive_name="$package-$tag-$target.$archive_extension"
  archive_path="$native_dir/$archive_name"
  archive_digest=$(sha256_file "$archive_path")
  require_native_spdx \
    "$native_dir/$archive_name.spdx.json" \
    "$archive_name" \
    "$archive_digest" \
    "$repository"
  require_bundle_predicate \
    "$native_dir/$archive_name.sbom.sigstore.json" \
    "$native_dir/$archive_name.spdx.json"
done < <("$release_targets" rows)
platforms=$(jq -c \
  'map({platform, runnable_digest}) | sort_by(.platform)' <<<"$platform_rows")
manifest_platforms=$(jq -c \
  'map({platform, runnable_digest, build_digest}) | sort_by(.platform)' \
  <<<"$platform_rows")
manifest_path="$container_dir/image-manifest.json"
if ! manifest_fields=$(jq -cer \
  --arg package "$package" \
  --arg tag "$tag" \
  --arg source_sha "$source_sha" \
  --argjson source_epoch "$source_epoch" \
  --arg image "$image" \
  --arg staging_ref "$image:sha-$source_sha" \
  --argjson expected_platforms "$manifest_platforms" '
    select(type == "object") |
    select((keys | sort) == ([
      "schema_version", "package", "tag", "source_sha", "source_epoch",
      "image", "staging_ref", "manifest_digest", "platforms"
    ] | sort)) |
    select(.schema_version == 1) |
    select(.package == $package and .tag == $tag) |
    select(.source_sha == $source_sha and .source_epoch == $source_epoch) |
    select(.image == $image) |
    select(.staging_ref == $staging_ref) |
    select(.manifest_digest | type == "string" and test("^sha256:[0-9a-f]{64}$")) |
    select((.image | type) == "string") |
    select((.platforms | type) == "array") |
    select((.platforms | length) == ($expected_platforms | length)) |
    select(all(.platforms[];
      type == "object" and
      ((keys | sort) == ([
        "platform", "runnable_digest", "build_digest"
      ] | sort)) and
      ((.platform | type) == "string") and
      (.runnable_digest | type == "string" and test("^sha256:[0-9a-f]{64}$")) and
      (.build_digest | type == "string" and test("^sha256:[0-9a-f]{64}$")))) |
    select((.platforms | sort_by(.platform)) == $expected_platforms) |
    {image, manifest_digest}
  ' "$manifest_path"); then
  die "image-manifest.json does not match the exact release/platform identity"
fi
manifest_digest=$(jq -er '.manifest_digest' <<<"$manifest_fields")

container_sbom_path="$container_dir/$container_image_base.spdx.json"
container_provenance_path="$container_dir/$container_image_base.provenance.sigstore.json"
container_sbom_bundle_path="$container_dir/$container_image_base.sbom.sigstore.json"
require_container_spdx \
  "$container_sbom_path" \
  "$image_repository" \
  "$version" \
  "$tag" \
  "$source_sha" \
  "$source_epoch" \
  "$manifest_digest" \
  "$platforms"
manifest_hex=${manifest_digest#sha256:}
require_sigstore_bundle \
  "$container_provenance_path" \
  "$image_repository" \
  "$manifest_hex" \
  "https://slsa.dev/provenance/v1"
require_sigstore_bundle \
  "$container_sbom_bundle_path" \
  "$image_repository" \
  "$manifest_hex" \
  "https://spdx.dev/Document/v2.3"
require_bundle_predicate "$container_sbom_bundle_path" "$container_sbom_path"

# Validate the staging manifest file identity without making its nondeterministic
# BuildKit digest fields part of the durable release record.
file_identity "$manifest_path" > /dev/null
container_sbom_identity=$(file_identity "$container_sbom_path")
container_provenance_identity=$(file_identity "$container_provenance_path")
container_sbom_bundle_identity=$(file_identity "$container_sbom_bundle_path")
native=$(jq -cs 'sort_by(.target)' "$native_records")
release_targets_identity=$(file_identity "$root/release-targets.json")

record_path="$work/$output_name"
jq -S -c -n \
  --arg package "$package" \
  --arg tag "$tag" \
  --arg version "$version" \
  --arg source_sha "$source_sha" \
  --argjson source_epoch "$source_epoch" \
  --argjson release_targets "$release_targets_identity" \
  --argjson native "$native" \
  --arg image "$image_repository" \
  --arg manifest_digest "$manifest_digest" \
  --argjson platforms "$platforms" \
  --argjson container_sbom "$container_sbom_identity" \
  --argjson container_provenance "$container_provenance_identity" \
  --argjson container_sbom_bundle "$container_sbom_bundle_identity" '
    {
      schema_version: 1,
      package: $package,
      tag: $tag,
      version: $version,
      source_sha: $source_sha,
      source_epoch: $source_epoch,
      release_targets: $release_targets,
      native: $native,
      container: {
        image: $image,
        manifest_digest: $manifest_digest,
        platforms: $platforms,
        sbom: $container_sbom,
        attestations: {
          provenance: $container_provenance,
          sbom: $container_sbom_bundle
        }
      }
    }
  ' > "$record_path"

if [[ ! -s "$record_path" ]] || ! jq empty "$record_path" > /dev/null 2>&1; then
  die "canonical release record generation failed"
fi
chmod 644 "$record_path"
# Keep the exclusive descriptor open for the entire copy. Reopening the path
# after a successful noclobber create would introduce a symlink-swap window.
if ! (
  created=false
  # Invoked by the EXIT trap below.
  # shellcheck disable=SC2317,SC2329
  cleanup_output_copy() {
    local status=$?
    if [[ $status -ne 0 && "$created" == true ]]; then
      rm -f "$output"
    fi
    exit "$status"
  }
  trap cleanup_output_copy EXIT
  set -o noclobber
  exec 9> "$output"
  created=true
  cat "$record_path" >&9
) 2> /dev/null; then
  die "output appeared while building the release record"
fi
output_created=true
if [[ ! -f "$output" || -L "$output" ]] || ! cmp -s "$record_path" "$output"; then
  die "canonical release record installation failed"
fi
printf '%s\n' "$output"
