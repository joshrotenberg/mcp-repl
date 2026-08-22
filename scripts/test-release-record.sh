#!/usr/bin/env bash
# Exercise canonical release-record inventory, identity, and schema boundaries.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
builder="$root/scripts/build-release-record.sh"
container_sbom_builder="$root/scripts/build-container-sbom.sh"
release_targets="$root/scripts/release-targets.sh"
tag=v1.2.3
version=1.2.3
source_sha=0123456789abcdef0123456789abcdef01234567
source_epoch=1700000001
image=ghcr.io/test/project
staging_ref="$image:sha-$source_sha"
manifest_digest=sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
work=$(mktemp -d)
if [[ ${KEEP_TEST_WORK:-0} == 1 ]]; then
  trap 'printf "preserved release-record fixtures: %s\n" "$work" >&2' EXIT
else
  trap 'rm -rf "$work"' EXIT
fi

fail() {
  echo "release record test failed: $*" >&2
  exit 1
}

sha256_file() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

write_spdx() {
  local path=$1
  local name=$2
  local digest=$3
  jq -nS \
    --arg name "$name" \
    --arg digest "$digest" \
    --arg evident_comment "evident-by: indicates the package's existence is evident by the given file" \
    --arg namespace "https://github.com/test/project/releases/download/$tag/$name.spdx.json#$source_sha-$digest" '{
    spdxVersion: "SPDX-2.3",
    SPDXID: "SPDXRef-DOCUMENT",
    dataLicense: "CC0-1.0",
    name: $name,
    documentNamespace: $namespace,
    creationInfo: {
      created: "2023-11-14T22:13:21Z",
      creators: ["Tool: release-record-test"]
    },
    packages: [
      {
        SPDXID: "SPDXRef-DocumentRoot",
        name: $name,
        versionInfo: "1.2.3",
        supplier: "NOASSERTION",
        downloadLocation: "NOASSERTION",
        filesAnalyzed: false,
        checksums: [{algorithm: "SHA256", checksumValue: $digest}],
        licenseConcluded: "NOASSERTION",
        licenseDeclared: "NOASSERTION",
        copyrightText: "NOASSERTION",
        primaryPackagePurpose: "FILE"
      },
      {
        SPDXID: "SPDXRef-mcp-repl",
        name: "mcp-repl",
        versionInfo: "1.2.3",
        downloadLocation: "NOASSERTION",
        externalRefs: [{
          referenceCategory: "PACKAGE-MANAGER",
          referenceType: "purl",
          referenceLocator: "pkg:cargo/mcp-repl@1.2.3"
        }]
      },
      {
        SPDXID: "SPDXRef-dependency",
        name: "dependency",
        versionInfo: "9.8.7",
        externalRefs: [{
          referenceCategory: "PACKAGE-MANAGER",
          referenceType: "purl",
          referenceLocator: "pkg:cargo/dependency@9.8.7"
        }]
      }
    ],
    files: [{SPDXID: "SPDXRef-File-mcp-repl", fileName: "mcp-repl"}],
    relationships: [
      {
        spdxElementId: "SPDXRef-DOCUMENT",
        relationshipType: "DESCRIBES",
        relatedSpdxElement: "SPDXRef-DocumentRoot"
      },
      {
        spdxElementId: "SPDXRef-DocumentRoot",
        relationshipType: "CONTAINS",
        relatedSpdxElement: "SPDXRef-mcp-repl"
      },
      {
        spdxElementId: "SPDXRef-dependency",
        relationshipType: "DEPENDENCY_OF",
        relatedSpdxElement: "SPDXRef-mcp-repl"
      },
      {
        spdxElementId: "SPDXRef-mcp-repl",
        relationshipType: "OTHER",
        comment: $evident_comment,
        relatedSpdxElement: "SPDXRef-File-mcp-repl"
      }
    ]
  }' > "$path"
}

write_bundle() {
  local path=$1
  local subject_name=$2
  local subject_sha256=$3
  local predicate_type=$4
  local predicate='{"fixture":true}'
  if [[ $# -eq 5 ]]; then
    predicate=$(jq -c . "$5")
  fi
  jq -nS \
    --arg subject_name "$subject_name" \
    --arg subject_sha256 "$subject_sha256" \
    --arg predicate_type "$predicate_type" \
    --argjson predicate "$predicate" '
    {
      _type: "https://in-toto.io/Statement/v1",
      subject: [{
        name: $subject_name,
        digest: {sha256: $subject_sha256}
      }],
      predicateType: $predicate_type,
      predicate: $predicate
    } as $statement |
    {
    mediaType: "application/vnd.dev.sigstore.bundle.v0.3+json",
    verificationMaterial: {fixture: true},
    dsseEnvelope: {
      payloadType: "application/vnd.in-toto+json",
      payload: ($statement | tojson | @base64),
      signatures: [{sig: "fixture-signature"}]
    }
  }' > "$path"
}

mutate_bundle_statement() {
  local path=$1
  local filter=$2
  jq ".dsseEnvelope.payload |= (@base64d | fromjson | $filter | tojson | @base64)" \
    "$path" > "$case_dir/replacement-bundle.json"
  mv "$case_dir/replacement-bundle.json" "$path"
}

setup_case() {
  local name=$1
  case_dir="$work/$name"
  native_dir="$case_dir/native"
  container_dir="$case_dir/container"
  output_dir="$case_dir/output"
  output="$output_dir/release-record.json"
  mkdir -p "$native_dir" "$container_dir" "$output_dir"

  first_archive=
  while IFS=$'\t' read -r target archive_extension binary_name; do
    archive="$native_dir/mcp-repl-$tag-$target.$archive_extension"
    [[ -n "$first_archive" ]] || first_archive=$archive
    printf 'native archive for %s (%s)\n' "$target" "$binary_name" > "$archive"
    digest=$(sha256_file "$archive")
    printf '%s  %s\n' "$digest" "${archive##*/}" > "$archive.sha256"
    write_spdx "$archive.spdx.json" "${archive##*/}" "$digest"
    write_bundle \
      "$archive.provenance.sigstore.json" \
      "${archive##*/}" \
      "$digest" \
      "https://slsa.dev/provenance/v1"
    write_bundle \
      "$archive.sbom.sigstore.json" \
      "${archive##*/}" \
      "$digest" \
      "https://spdx.dev/Document/v2.3" \
      "$archive.spdx.json"
  done < <("$release_targets" rows)

  platform_map="$case_dir/platform-map.jsonl"
  : > "$platform_map"
  index=0
  while IFS= read -r platform; do
    index=$((index + 1))
    if [[ $index -eq 1 ]]; then
      runnable=sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
      build_output=sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc
    else
      runnable=sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
      build_output=sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd
    fi
    platform_file=${platform//\//-}.json
    jq -n \
      --arg package mcp-repl \
      --arg tag "$tag" \
      --arg source_sha "$source_sha" \
      --argjson source_epoch "$source_epoch" \
      --arg image "$image" \
      --arg platform "$platform" \
      --arg runnable_digest "$runnable" \
      --arg build_digest "$build_output" '{
        schema_version: 1,
        package: $package,
        tag: $tag,
        source_sha: $source_sha,
        source_epoch: $source_epoch,
        image: $image,
        platform: $platform,
        runnable_digest: $runnable_digest,
        build_digest: $build_digest,
        buildkit: {provenance: true, sbom: true}
      }' > "$container_dir/$platform_file"
    jq -cn \
      --arg platform "$platform" \
      --arg runnable_digest "$runnable" \
      --arg build_digest "$build_output" \
      '{
        platform: $platform,
        runnable_digest: $runnable_digest,
        build_digest: $build_digest
      }' >> "$platform_map"
  done < <("$release_targets" container-platforms)

  platforms=$(jq -cs 'sort_by(.platform)' "$platform_map")
  jq -n \
    --arg package mcp-repl \
    --arg tag "$tag" \
    --arg source_sha "$source_sha" \
    --argjson source_epoch "$source_epoch" \
    --arg image "$image" \
    --arg staging_ref "$staging_ref" \
    --arg manifest_digest "$manifest_digest" \
    --argjson platforms "$platforms" '{
      schema_version: 1,
      package: $package,
      tag: $tag,
      source_sha: $source_sha,
      source_epoch: $source_epoch,
      image: $image,
      staging_ref: $staging_ref,
      manifest_digest: $manifest_digest,
      platforms: $platforms
    }' > "$container_dir/image-manifest.json"

  container_base="$container_dir/mcp-repl-$tag-container"
  "$container_sbom_builder" \
    "$container_dir/image-manifest.json" \
    "$container_base.spdx.json" > /dev/null
  write_bundle \
    "$container_base.provenance.sigstore.json" \
    "$image" \
    "${manifest_digest#sha256:}" \
    "https://slsa.dev/provenance/v1"
  write_bundle \
    "$container_base.sbom.sigstore.json" \
    "$image" \
    "${manifest_digest#sha256:}" \
    "https://spdx.dev/Document/v2.3" \
    "$container_base.spdx.json"
}

run_builder() {
  "$builder" \
    "$tag" \
    "$source_sha" \
    "$source_epoch" \
    "$native_dir" \
    "$container_dir" \
    "$output" \
    > "$case_dir/stdout" 2> "$case_dir/stderr"
}

expect_failure() {
  local label=$1
  local diagnostic=$2
  if run_builder; then
    fail "$label unexpectedly built a release record"
  fi
  grep -Fq "$diagnostic" "$case_dir/stderr" ||
    fail "$label did not report '$diagnostic'"
  [[ ! -e "$output" && ! -L "$output" ]] ||
    fail "$label left a release record after failure"
}

replace_json() {
  local path=$1
  local filter=$2
  jq "$filter" "$path" > "$case_dir/replacement.json"
  mv "$case_dir/replacement.json" "$path"
}

setup_case valid
if ! run_builder; then
  cat "$case_dir/stderr" >&2
  fail "valid release inputs did not build a release record"
fi
canonical_output="$(cd "$output_dir" && pwd -P)/release-record.json"
[[ "$(<"$case_dir/stdout")" == "$canonical_output" ]] ||
  fail "valid build did not print the canonical output path"
[[ -f "$output" && ! -L "$output" ]] || fail "valid build omitted its output"
jq -e \
  --arg tag "$tag" \
  --arg version "$version" \
  --arg source_sha "$source_sha" \
  --argjson source_epoch "$source_epoch" \
  --arg manifest_digest "$manifest_digest" '
    type == "object" and
    ((keys | sort) == ([
      "schema_version", "package", "tag", "version", "source_sha",
      "source_epoch", "release_targets", "native", "container"
    ] | sort)) and
    .schema_version == 1 and .package == "mcp-repl" and
    .tag == $tag and .version == $version and
    .source_sha == $source_sha and .source_epoch == $source_epoch and
    (.native | length) == 7 and
    ([.native[].target] == ([.native[].target] | sort)) and
    (.container.image == "ghcr.io/test/project") and
    .container.manifest_digest == $manifest_digest and
    ((.container | keys | sort) == ([
      "image", "manifest_digest", "platforms", "sbom", "attestations"
    ] | sort)) and
    (.container.platforms | length) == 2 and
    all(.container.platforms[];
      (keys | sort) == (["platform", "runnable_digest"] | sort)) and
    ([.container.platforms[].platform] ==
      ([.container.platforms[].platform] | sort))
  ' "$output" > /dev/null || fail "valid record has the wrong schema or ordering"

canonical="$case_dir/canonical.json"
jq -S -c . "$output" > "$canonical"
cmp -s "$output" "$canonical" || fail "record is not canonical sorted compact JSON"

identities="$case_dir/identities.json"
jq '[
  .. | objects |
  select((keys | sort) == (["name", "sha256", "size"] | sort))
]' "$output" > "$identities"
[[ "$(jq length "$identities")" -eq 39 ]] ||
  fail "record did not bind all 38 durable artifacts plus the release target manifest"
for directory in "$native_dir" "$container_dir"; do
  while IFS= read -r -d '' artifact; do
    artifact_name=${artifact##*/}
    case "$artifact_name" in
      image-manifest.json | linux-*.json) continue ;;
    esac
    actual_digest=$(sha256_file "$artifact")
    actual_size=$(wc -c < "$artifact" | tr -d '[:space:]')
    jq -e \
      --arg name "$artifact_name" \
      --arg digest "$actual_digest" \
      --argjson size "$actual_size" '
        [.[] | select(
          .name == $name and .sha256 == $digest and .size == $size
        )] | length == 1
      ' "$identities" > /dev/null ||
      fail "record omitted or misidentified $artifact_name"
  done < <(find "$directory" -mindepth 1 -maxdepth 1 -type f -print0)
done

setup_case deterministic
run_builder
cmp -s "$work/valid/output/release-record.json" "$output" ||
  fail "identical release inputs did not produce byte-identical records"

setup_case shuffled_manifest
replace_json "$container_dir/image-manifest.json" '.platforms |= reverse'
run_builder
cmp -s "$work/valid/output/release-record.json" "$output" ||
  fail "manifest input ordering changed the canonical record"

setup_case deterministic_build_drift
replace_json "$container_dir/linux-amd64.json" \
  '.build_digest = ("sha256:" + ("1" * 64))'
replace_json "$container_dir/linux-arm64.json" \
  '.build_digest = ("sha256:" + ("2" * 64))'
replace_json "$container_dir/image-manifest.json" \
  '(.platforms[] | select(.platform == "linux/amd64") | .build_digest) =
    ("sha256:" + ("1" * 64)) |
   (.platforms[] | select(.platform == "linux/arm64") | .build_digest) =
    ("sha256:" + ("2" * 64))'
run_builder
cmp -s "$work/valid/output/release-record.json" "$output" ||
  fail "equivalent runnable images changed the record through BuildKit digest drift"

setup_case missing_native
rm "$first_archive"
expect_failure missing_native 'native does not contain the exact expected file set'

setup_case extra_native
printf '%s\n' extra > "$native_dir/unexpected.txt"
expect_failure extra_native 'native does not contain the exact expected file set'

setup_case native_subdirectory
mkdir "$native_dir/unexpected"
expect_failure native_subdirectory 'native contains an unsafe, linked, empty, or non-file entry'

setup_case linked_native
real_archive="$case_dir/real-archive"
mv "$first_archive" "$real_archive"
ln -s "$real_archive" "$first_archive"
expect_failure linked_native 'native contains an unsafe, linked, empty, or non-file entry'

setup_case empty_native
: > "$first_archive"
expect_failure empty_native 'native contains an unsafe, linked, empty, or non-file entry'

setup_case bad_checksum
printf '%064d  %s\n' 0 "${first_archive##*/}" > "$first_archive.sha256"
expect_failure bad_checksum 'does not canonically self-check'

setup_case checksum_extra_line
printf '%s\n' extra >> "$first_archive.sha256"
expect_failure checksum_extra_line 'does not canonically self-check'

setup_case invalid_native_sbom
printf '{}\n' > "$first_archive.spdx.json"
expect_failure invalid_native_sbom 'does not bind the exact native archive identity'

setup_case native_sbom_wrong_name
replace_json "$first_archive.spdx.json" '.name = "other.tar.gz"'
expect_failure native_sbom_wrong_name 'does not bind the exact native archive identity'

setup_case native_sbom_wrong_namespace
replace_json "$first_archive.spdx.json" '.documentNamespace = "https://example.invalid/unrelated"'
expect_failure native_sbom_wrong_namespace 'does not bind the exact native archive identity'

setup_case native_sbom_wrong_epoch
replace_json "$first_archive.spdx.json" '.creationInfo.created = "2023-11-14T22:13:22Z"'
expect_failure native_sbom_wrong_epoch 'does not bind the exact native archive identity'

setup_case native_sbom_document_describes_extension
replace_json "$first_archive.spdx.json" \
  '.documentDescribes = ["SPDXRef-DocumentRoot"]'
expect_failure native_sbom_document_describes_extension \
  'does not bind the exact native archive identity'

setup_case native_sbom_empty_packages
replace_json "$first_archive.spdx.json" '.packages = []'
expect_failure native_sbom_empty_packages 'does not bind the exact native archive identity'

setup_case native_sbom_wrong_root_checksum
replace_json "$first_archive.spdx.json" \
  '(.packages[] | select(.SPDXID == "SPDXRef-DocumentRoot") | .checksums[0].checksumValue) = ("9" * 64)'
expect_failure native_sbom_wrong_root_checksum 'does not bind the exact native archive identity'

setup_case native_sbom_root_analyzed
replace_json "$first_archive.spdx.json" \
  '(.packages[] | select(.SPDXID == "SPDXRef-DocumentRoot") | .filesAnalyzed) = true'
expect_failure native_sbom_root_analyzed \
  'does not bind the exact native archive identity'

setup_case native_sbom_wrong_mcp_purl
replace_json "$first_archive.spdx.json" \
  '(.packages[] | select(.SPDXID == "SPDXRef-mcp-repl") | .externalRefs[0].referenceLocator) = "pkg:cargo/other@1.2.3"'
expect_failure native_sbom_wrong_mcp_purl \
  'does not bind the exact native archive identity'

setup_case native_sbom_duplicate_mcp
replace_json "$first_archive.spdx.json" \
  '.packages += [(.packages[] | select(.SPDXID == "SPDXRef-mcp-repl") | .SPDXID = "SPDXRef-mcp-repl-copy")]'
expect_failure native_sbom_duplicate_mcp \
  'does not bind the exact native archive identity'

setup_case native_sbom_missing_describes
replace_json "$first_archive.spdx.json" \
  '.relationships |= map(select(.relationshipType != "DESCRIBES"))'
expect_failure native_sbom_missing_describes \
  'does not bind the exact native archive identity'

setup_case native_sbom_missing_contains
replace_json "$first_archive.spdx.json" \
  '.relationships |= map(select(.relationshipType != "CONTAINS"))'
expect_failure native_sbom_missing_contains 'does not bind the exact native archive identity'

setup_case native_sbom_missing_dependency_purl
replace_json "$first_archive.spdx.json" \
  '(.packages[] | select(.SPDXID == "SPDXRef-dependency")) |= del(.externalRefs)'
expect_failure native_sbom_missing_dependency_purl \
  'does not bind the exact native archive identity'

setup_case native_sbom_missing_dependency_edge
replace_json "$first_archive.spdx.json" \
  '.relationships |= map(select(.relationshipType != "DEPENDENCY_OF"))'
expect_failure native_sbom_missing_dependency_edge \
  'does not bind the exact native archive identity'

setup_case native_sbom_missing_binary_evidence
replace_json "$first_archive.spdx.json" \
  '.relationships |= map(select(.relationshipType != "OTHER"))'
expect_failure native_sbom_missing_binary_evidence \
  'does not bind the exact native archive identity'

setup_case native_sbom_wrong_binary_evidence_comment
replace_json "$first_archive.spdx.json" \
  '(.relationships[] | select(.relationshipType == "OTHER") | .comment) = "unrelated"'
expect_failure native_sbom_wrong_binary_evidence_comment \
  'does not bind the exact native archive identity'

setup_case native_sbom_wrong_binary_filename
replace_json "$first_archive.spdx.json" '.files[0].fileName = "unrelated"'
expect_failure native_sbom_wrong_binary_filename \
  'does not bind the exact native archive identity'

setup_case native_sbom_dangling_relationship
replace_json "$first_archive.spdx.json" \
  '.relationships[2].relatedSpdxElement = "SPDXRef-unlisted-file"'
expect_failure native_sbom_dangling_relationship \
  'does not bind the exact native archive identity'

setup_case invalid_native_bundle
printf '{}\n' > "$first_archive.provenance.sigstore.json"
expect_failure invalid_native_bundle 'does not bind the exact https://slsa.dev/provenance/v1 subject'

setup_case native_bundle_wrong_subject
mutate_bundle_statement "$first_archive.provenance.sigstore.json" \
  '.subject[0].name = "unrelated.tar.gz"'
expect_failure native_bundle_wrong_subject \
  'does not bind the exact https://slsa.dev/provenance/v1 subject'

setup_case native_bundle_wrong_digest
mutate_bundle_statement "$first_archive.sbom.sigstore.json" \
  '.subject[0].digest.sha256 = ("9" * 64)'
expect_failure native_bundle_wrong_digest \
  'does not bind the exact https://spdx.dev/Document/v2.3 subject'

setup_case native_bundle_swapped_predicate
mutate_bundle_statement "$first_archive.provenance.sigstore.json" \
  '.predicateType = "https://spdx.dev/Document/v2.3"'
expect_failure native_bundle_swapped_predicate \
  'does not bind the exact https://slsa.dev/provenance/v1 subject'

setup_case native_bundle_wrong_sbom
mutate_bundle_statement "$first_archive.sbom.sigstore.json" \
  '.predicate.name = "signed-but-unrelated.spdx.json"'
expect_failure native_bundle_wrong_sbom \
  'does not sign the exact adjacent SPDX document'

setup_case missing_container
rm "$container_dir/linux-amd64.json"
expect_failure missing_container 'container does not contain the exact expected file set'

setup_case extra_container
printf '%s\n' extra > "$container_dir/unexpected.txt"
expect_failure extra_container 'container does not contain the exact expected file set'

setup_case linked_container
mv "$container_dir/linux-amd64.json" "$case_dir/platform.json"
ln -s "$case_dir/platform.json" "$container_dir/linux-amd64.json"
expect_failure linked_container 'container contains an unsafe, linked, empty, or non-file entry'

setup_case platform_wrong_tag
replace_json "$container_dir/linux-amd64.json" '.tag = "v9.9.9"'
expect_failure platform_wrong_tag 'does not match its release/platform identity'

setup_case platform_wrong_source
replace_json "$container_dir/linux-amd64.json" '.source_sha = ("f" * 40)'
expect_failure platform_wrong_source 'does not match its release/platform identity'

setup_case platform_wrong_epoch
replace_json "$container_dir/linux-amd64.json" '.source_epoch += 1'
expect_failure platform_wrong_epoch 'does not match its release/platform identity'

setup_case platform_wrong_name
replace_json "$container_dir/linux-amd64.json" '.platform = "linux/arm64"'
expect_failure platform_wrong_name 'does not match its release/platform identity'

setup_case platform_bad_digest
replace_json "$container_dir/linux-amd64.json" '.runnable_digest = "sha256:short"'
expect_failure platform_bad_digest 'does not match its release/platform identity'

setup_case platform_bad_build_digest
replace_json "$container_dir/linux-amd64.json" '.build_digest = "short"'
expect_failure platform_bad_build_digest 'does not match its release/platform identity'

setup_case platform_extra_key
replace_json "$container_dir/linux-amd64.json" '.unexpected = true'
expect_failure platform_extra_key 'does not match its release/platform identity'

setup_case platform_bad_buildkit
replace_json "$container_dir/linux-amd64.json" '.buildkit.sbom = false'
expect_failure platform_bad_buildkit 'does not match its release/platform identity'

setup_case platform_malformed
printf '{\n' > "$container_dir/linux-amd64.json"
expect_failure platform_malformed 'does not match its release/platform identity'

setup_case platform_image_mismatch
replace_json "$container_dir/linux-amd64.json" '.image = "ghcr.io/other/project"'
expect_failure platform_image_mismatch 'does not bind one common image'

setup_case platform_image_unsafe
replace_json "$container_dir/linux-amd64.json" '.image = "docker.io/test/project"'
replace_json "$container_dir/linux-arm64.json" '.image = "docker.io/test/project"'
expect_failure platform_image_unsafe 'unsafe GHCR image repository'

setup_case manifest_mapping_mismatch
replace_json "$container_dir/image-manifest.json" \
  '.platforms[0].runnable_digest = ("sha256:" + ("f" * 64))'
expect_failure manifest_mapping_mismatch 'does not match the exact release/platform identity'

setup_case manifest_image_mismatch
replace_json "$container_dir/image-manifest.json" '.image = "ghcr.io/other/project"'
expect_failure manifest_image_mismatch 'does not match the exact release/platform identity'

setup_case manifest_staging_mismatch
replace_json "$container_dir/image-manifest.json" '.staging_ref = "ghcr.io/test/project:sha-deadbeef"'
expect_failure manifest_staging_mismatch 'does not match the exact release/platform identity'

setup_case manifest_bad_digest
replace_json "$container_dir/image-manifest.json" '.manifest_digest = "sha256:short"'
expect_failure manifest_bad_digest 'does not match the exact release/platform identity'

setup_case manifest_build_mismatch
replace_json "$container_dir/image-manifest.json" \
  '.platforms[0].build_digest = ("sha256:" + ("f" * 64))'
expect_failure manifest_build_mismatch 'does not match the exact release/platform identity'

setup_case manifest_extra_key
replace_json "$container_dir/image-manifest.json" '.unexpected = true'
expect_failure manifest_extra_key 'does not match the exact release/platform identity'

setup_case manifest_extra_platform
replace_json "$container_dir/image-manifest.json" \
  '.platforms += [{unexpected: true}]'
expect_failure manifest_extra_platform 'does not match the exact release/platform identity'

setup_case invalid_container_sbom
printf '{}\n' > "$container_dir/mcp-repl-$tag-container.spdx.json"
expect_failure invalid_container_sbom 'does not bind the exact container release identity'

setup_case unrelated_valid_container_sbom
unrelated_manifest="$case_dir/unrelated-image-manifest.json"
unrelated_sbom="$case_dir/unrelated.spdx.json"
jq '.manifest_digest = ("sha256:" + ("9" * 64))' \
  "$container_dir/image-manifest.json" > "$unrelated_manifest"
"$container_sbom_builder" "$unrelated_manifest" "$unrelated_sbom" > /dev/null
mv "$unrelated_sbom" "$container_dir/mcp-repl-$tag-container.spdx.json"
expect_failure unrelated_valid_container_sbom \
  'does not bind the exact container release identity'

setup_case invalid_container_bundle
printf '{}\n' > "$container_dir/mcp-repl-$tag-container.sbom.sigstore.json"
expect_failure invalid_container_bundle \
  'does not bind the exact https://spdx.dev/Document/v2.3 subject'

setup_case container_bundle_wrong_subject
mutate_bundle_statement \
  "$container_dir/mcp-repl-$tag-container.provenance.sigstore.json" \
  '.subject[0].name = "ghcr.io/other/project"'
expect_failure container_bundle_wrong_subject \
  'does not bind the exact https://slsa.dev/provenance/v1 subject'

setup_case container_bundle_swapped_predicate
mutate_bundle_statement \
  "$container_dir/mcp-repl-$tag-container.sbom.sigstore.json" \
  '.predicateType = "https://slsa.dev/provenance/v1"'
expect_failure container_bundle_swapped_predicate \
  'does not bind the exact https://spdx.dev/Document/v2.3 subject'

setup_case container_bundle_wrong_sbom
mutate_bundle_statement \
  "$container_dir/mcp-repl-$tag-container.sbom.sigstore.json" \
  '.predicate.name = "signed-but-unrelated.spdx.json"'
expect_failure container_bundle_wrong_sbom \
  'does not sign the exact adjacent SPDX document'

setup_case unsafe_output_name
output="$output_dir/../bad/name"
expect_failure unsafe_output_name 'output name or parent directory is unsafe'

setup_case output_in_native
output="$native_dir/release-record.json"
expect_failure output_in_native 'input directories and output parent must be distinct'

setup_case linked_native_directory
linked_native="$case_dir/native-link"
ln -s "$native_dir" "$linked_native"
native_dir=$linked_native
expect_failure linked_native_directory 'must be non-symlink directories'

setup_case linked_container_directory
linked_container_dir="$case_dir/container-link"
ln -s "$container_dir" "$linked_container_dir"
container_dir=$linked_container_dir
expect_failure linked_container_directory 'must be non-symlink directories'

expect_argument_failure() {
  local label=$1
  local diagnostic=$2
  shift 2
  setup_case "argument_$label"
  if "$builder" "$@" "$native_dir" "$container_dir" "$output" \
      > "$case_dir/stdout" 2> "$case_dir/stderr"; then
    fail "$label unexpectedly accepted unsafe release identity arguments"
  fi
  grep -Fq "$diagnostic" "$case_dir/stderr" ||
    fail "$label did not report '$diagnostic'"
  [[ ! -e "$output" && ! -L "$output" ]] || fail "$label created an output"
}

expect_argument_failure bad_tag 'tag must be a canonical' \
  v01.2.3 "$source_sha" "$source_epoch"
expect_argument_failure short_sha 'source SHA must be 40 lowercase' \
  "$tag" short "$source_epoch"
expect_argument_failure uppercase_sha 'source SHA must be 40 lowercase' \
  "$tag" 0123456789ABCDEF0123456789ABCDEF01234567 "$source_epoch"
expect_argument_failure leading_epoch 'source epoch must be canonical' \
  "$tag" "$source_sha" 01700000001
expect_argument_failure old_epoch 'source epoch must be canonical' \
  "$tag" "$source_sha" 315532799
expect_argument_failure future_epoch 'source epoch must be canonical' \
  "$tag" "$source_sha" 2147483648

setup_case existing_output
printf '%s\n' sentinel > "$output"
if run_builder; then
  fail "existing output unexpectedly succeeded"
fi
grep -Fq 'output must be a new path' "$case_dir/stderr" ||
  fail "existing output did not report its rejection"
[[ "$(<"$output")" == sentinel ]] || fail "existing output was overwritten"

setup_case dangling_output
outside="$case_dir/outside"
ln -s "$outside" "$output"
if run_builder; then
  fail "dangling output unexpectedly succeeded"
fi
grep -Fq 'output must be a new path' "$case_dir/stderr" ||
  fail "dangling output did not report its rejection"
[[ ! -e "$outside" ]] || fail "builder wrote through a dangling output"

echo "release record behavior tests passed"
