#!/usr/bin/env bash
# Exercise staged, versioned, and latest attested container publication.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
publish_script="$root/scripts/publish-container-manifest.sh"
publish=publish_with_fixture_cache
runnable_mapping="$root/scripts/container-runnable-mapping.jq"
python3 "$root/scripts/test-validate-oci-attestation.py"
[[ $(grep -Fc 'python3 ./scripts/validate-oci-attestation.py' \
      "$root/.github/workflows/container-build.yml") -eq 1 ]] || {
  echo "container build workflow must validate exact in-toto blob bytes" >&2
  exit 1
}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
mkdir -p "$work/bin" "$work/base"

export TEST_TAG=v0.3.0
export TEST_NEXT_TAG=v0.3.1
export TEST_SOURCE_SHA=1111111111111111111111111111111111111111
export TEST_OTHER_SOURCE_SHA=2222222222222222222222222222222222222222
export TEST_SOURCE_EPOCH=1700000001
export TEST_IMAGE=ghcr.io/test/project
export TEST_RUNNABLE_A=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
export TEST_RUNNABLE_B=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
export TEST_NEXT_RUNNABLE_A=7777777777777777777777777777777777777777777777777777777777777777
export TEST_NEXT_RUNNABLE_B=8888888888888888888888888888888888888888888888888888888888888888
export TEST_TAG_OBJECT_SHA=9999999999999999999999999999999999999999
export TEST_NEXT_TAG_OBJECT_SHA=9898989898989898989898989898989898989898
TEST_RELEASE_BODY=$("$root/scripts/extract-release-notes.sh" "${TEST_TAG#v}")
export TEST_RELEASE_BODY

publish_with_fixture_cache() {
  "$publish_script" --test-oci-blob-cache "$TEST_OCI_BLOB_CACHE" "$@"
}

sha256_path() {
  local path=$1
  local digest
  if command -v sha256sum > /dev/null 2>&1; then
    digest=$(sha256sum "$path" | awk '{print $1}')
  else
    digest=$(shasum -a 256 "$path" | awk '{print $1}')
  fi
  printf 'sha256:%s\n' "$digest"
}

write_raw() {
  local path=$1
  local filter=$2
  local value
  value=$(jq -cnS "$filter")
  printf '%s' "$value" > "$path"
}

rewrite_raw() {
  local path=$1
  local filter=$2
  local value
  value=$(jq -cS "$filter" "$path")
  printf '%s' "$value" > "$path"
}

rewrite_pretty() {
  local path=$1
  local filter=$2
  local temporary="$path.tmp"
  jq -S "$filter" "$path" > "$temporary"
  mv "$temporary" "$path"
}

rewrite_compact() {
  local path=$1
  local filter=$2
  local temporary="$path.tmp"
  jq -cS "$filter" "$path" > "$temporary"
  mv "$temporary" "$path"
}

write_statement() {
  local path=$1
  local runnable=$2
  local predicate=$3
  local version=$4
  local value digest
  value=$(jq -cnS \
    --arg runnable "$runnable" \
    --arg predicate "$predicate" \
    --arg version "$version" \
    --arg evidence_comment \
      "evident-by: indicates the package's existence is evident by the given file" '
      {
        _type: "https://in-toto.io/Statement/v1",
        subject: [{
          name: "pkg:docker/ghcr.io/test/project?platform=linux%2Ffixture",
          digest: {sha256: $runnable}
        }],
        predicateType: $predicate,
        predicate: (if $predicate == "https://slsa.dev/provenance/v1" then {
          buildDefinition: {
            buildType:
              "https://github.com/moby/buildkit/blob/master/docs/attestations/slsa-definitions.md",
            externalParameters: {},
            internalParameters: {},
            resolvedDependencies: []
          },
          runDetails: {
            builder: {
              id: "https://github.com/test/project/.github/workflows/container-build.yml"
            },
            metadata: {invocationId: "fixture"},
            byproducts: []
          }
        } else {
          SPDXID: "SPDXRef-DOCUMENT",
          spdxVersion: "SPDX-2.3",
          dataLicense: "CC0-1.0",
          name: "fixture",
          documentNamespace: ("https://example.test/sbom/" + $runnable),
          creationInfo: {created: "2023-11-14T22:13:21Z", creators: ["Tool: fixture"]},
          packages: [{
            SPDXID: "SPDXRef-DocumentRoot-Directory-fixture",
            name: "usr/local/bin",
            downloadLocation: "NOASSERTION"
          }, {
            SPDXID: "SPDXRef-Package-mcp-repl",
            name: "mcp-repl",
            versionInfo: $version,
            downloadLocation: "NOASSERTION",
            sourceInfo:
              "acquired package info from rust cargo manifest: /usr/local/bin/mcp-repl",
            externalRefs: [{
              referenceCategory: "PACKAGE-MANAGER",
              referenceType: "purl",
              referenceLocator: ("pkg:cargo/mcp-repl@" + $version)
            }]
          }, {
            SPDXID: "SPDXRef-Package-serde",
            name: "serde",
            versionInfo: "1.0.0",
            downloadLocation: "NOASSERTION",
            externalRefs: [{
              referenceCategory: "PACKAGE-MANAGER",
              referenceType: "purl",
              referenceLocator: "pkg:cargo/serde@1.0.0"
            }]
          }],
          files: [{
            SPDXID: "SPDXRef-File-mcp-repl",
            fileName: "usr/local/bin/mcp-repl",
            checksums: [{
              algorithm: "SHA256",
              checksumValue: ("a" * 64)
            }]
          }],
          relationships: [{
            spdxElementId: "SPDXRef-DOCUMENT",
            relationshipType: "DESCRIBES",
            relatedSpdxElement: "SPDXRef-DocumentRoot-Directory-fixture"
          }, {
            spdxElementId: "SPDXRef-DocumentRoot-Directory-fixture",
            relationshipType: "CONTAINS",
            relatedSpdxElement: "SPDXRef-Package-mcp-repl"
          }, {
            spdxElementId: "SPDXRef-Package-mcp-repl",
            relationshipType: "OTHER",
            relatedSpdxElement: "SPDXRef-File-mcp-repl",
            comment: $evidence_comment
          }, {
            spdxElementId: "SPDXRef-Package-serde",
            relationshipType: "DEPENDENCY_OF",
            relatedSpdxElement: "SPDXRef-Package-mcp-repl"
          }]
        } end)
      }
    ')
  printf '%s' "$value" > "$path"
  digest=$(sha256_path "$path")
  mv "$path" "$work/base/${digest#sha256:}.blob"
  printf '%s\n' "$work/base/${digest#sha256:}.blob"
}

write_attestation_manifest() {
  local path=$1
  local runnable=$2
  local provenance_path=$3
  local sbom_path=$4
  local provenance_digest provenance_size sbom_digest sbom_size value
  provenance_digest=$(sha256_path "$provenance_path")
  provenance_size=$(raw_size "$provenance_path")
  sbom_digest=$(sha256_path "$sbom_path")
  sbom_size=$(raw_size "$sbom_path")
  value=$(jq -cnS \
    --arg runnable "$runnable" \
    --arg provenance_digest "$provenance_digest" \
    --argjson provenance_size "$provenance_size" \
    --arg sbom_digest "$sbom_digest" \
    --argjson sbom_size "$sbom_size" '
      {
        schemaVersion: 2,
        mediaType: "application/vnd.oci.image.manifest.v1+json",
        artifactType: "application/vnd.docker.attestation.manifest.v1+json",
        config: {
          mediaType: "application/vnd.oci.empty.v1+json",
          digest: "sha256:44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a",
          size: 2,
          data: "e30="
        },
        layers: [
          {
            mediaType: "application/vnd.in-toto+json",
            digest: $provenance_digest,
            size: $provenance_size,
            annotations: {
              "in-toto.io/predicate-type": "https://slsa.dev/provenance/v1"
            }
          },
          {
            mediaType: "application/vnd.in-toto+json",
            digest: $sbom_digest,
            size: $sbom_size,
            annotations: {
              "in-toto.io/predicate-type": "https://spdx.dev/Document"
            }
          }
        ],
        subject: {
          mediaType: "application/vnd.oci.image.manifest.v1+json",
          digest: ("sha256:" + $runnable),
          size: 101
        }
      }
    ')
  printf '%s' "$value" > "$path"
}

raw_size() {
  wc -c < "$1" | tr -d '[:space:]'
}

rebind_attestation() {
  local index_path=$1
  local descriptor_index=$2
  local attestation_path=$3
  local digest size
  digest=$(sha256_path "$attestation_path")
  size=$(raw_size "$attestation_path")
  rewrite_raw "$index_path" "
    .manifests[$descriptor_index].digest = \"$digest\" |
    .manifests[$descriptor_index].size = $size
  "
}

rebind_layer_blob() {
  local attestation_path=$1
  local layer_index=$2
  local filter=$3
  local old_digest source temporary digest size
  old_digest=$(jq -er ".layers[$layer_index].digest" "$attestation_path")
  source="$TEST_OCI_BLOB_CACHE/${old_digest#sha256:}.blob"
  temporary="$TEST_OCI_BLOB_CACHE/.mutated-blob"
  jq -cS "$filter" "$source" > "$temporary"
  digest=$(sha256_path "$temporary")
  size=$(raw_size "$temporary")
  mv "$temporary" "$TEST_OCI_BLOB_CACHE/${digest#sha256:}.blob"
  rewrite_raw "$attestation_path" "
    .layers[$layer_index].digest = \"$digest\" |
    .layers[$layer_index].size = $size
  "
}

# shellcheck disable=SC2016
index_filter='
  {
    schemaVersion: 2,
    mediaType: "application/vnd.oci.image.index.v1+json",
    manifests: $manifests
  }
'

provenance_a=$(write_statement "$work/base/.provenance-a" \
  "$TEST_RUNNABLE_A" "https://slsa.dev/provenance/v1" "${TEST_TAG#v}")
sbom_a=$(write_statement "$work/base/.sbom-a" \
  "$TEST_RUNNABLE_A" "https://spdx.dev/Document" "${TEST_TAG#v}")
provenance_b=$(write_statement "$work/base/.provenance-b" \
  "$TEST_RUNNABLE_B" "https://slsa.dev/provenance/v1" "${TEST_TAG#v}")
sbom_b=$(write_statement "$work/base/.sbom-b" \
  "$TEST_RUNNABLE_B" "https://spdx.dev/Document" "${TEST_TAG#v}")
provenance_next_a=$(write_statement "$work/base/.provenance-next-a" \
  "$TEST_NEXT_RUNNABLE_A" "https://slsa.dev/provenance/v1" "${TEST_NEXT_TAG#v}")
sbom_next_a=$(write_statement "$work/base/.sbom-next-a" \
  "$TEST_NEXT_RUNNABLE_A" "https://spdx.dev/Document" "${TEST_NEXT_TAG#v}")
provenance_next_b=$(write_statement "$work/base/.provenance-next-b" \
  "$TEST_NEXT_RUNNABLE_B" "https://slsa.dev/provenance/v1" "${TEST_NEXT_TAG#v}")
sbom_next_b=$(write_statement "$work/base/.sbom-next-b" \
  "$TEST_NEXT_RUNNABLE_B" "https://spdx.dev/Document" "${TEST_NEXT_TAG#v}")

write_attestation_manifest "$work/base/attestation-amd64.raw" \
  "$TEST_RUNNABLE_A" "$provenance_a" "$sbom_a"
write_attestation_manifest "$work/base/attestation-arm64.raw" \
  "$TEST_RUNNABLE_B" "$provenance_b" "$sbom_b"
write_attestation_manifest "$work/base/attestation-amd64-alt.raw" \
  "$TEST_RUNNABLE_A" "$provenance_a" "$sbom_a"
write_attestation_manifest "$work/base/attestation-arm64-alt.raw" \
  "$TEST_RUNNABLE_B" "$provenance_b" "$sbom_b"
# Exercise BuildKit's legacy-compatible attestation storage as well as the
# current OCI-artifact form used by the primary fixtures.
for legacy_attestation in \
  "$work/base/attestation-amd64-alt.raw" \
  "$work/base/attestation-arm64-alt.raw"; do
  rewrite_raw "$legacy_attestation" '
    del(.artifactType, .subject) |
    .config = {
      mediaType: "application/vnd.oci.image.config.v1+json",
      digest: ("sha256:" + ("7" * 64)),
      size: 83
    }
  '
done
write_attestation_manifest "$work/base/attestation-amd64-next.raw" \
  "$TEST_NEXT_RUNNABLE_A" "$provenance_next_a" "$sbom_next_a"
write_attestation_manifest "$work/base/attestation-arm64-next.raw" \
  "$TEST_NEXT_RUNNABLE_B" "$provenance_next_b" "$sbom_next_b"

export TEST_ATTESTATION_A
export TEST_ATTESTATION_B
TEST_ATTESTATION_A=$(sha256_path "$work/base/attestation-amd64.raw")
TEST_ATTESTATION_B=$(sha256_path "$work/base/attestation-arm64.raw")
TEST_ATTESTATION_A_SIZE=$(raw_size "$work/base/attestation-amd64.raw")
TEST_ATTESTATION_B_SIZE=$(raw_size "$work/base/attestation-arm64.raw")
TEST_ATTESTATION_ALT_A=$(sha256_path "$work/base/attestation-amd64-alt.raw")
TEST_ATTESTATION_ALT_B=$(sha256_path "$work/base/attestation-arm64-alt.raw")
TEST_ATTESTATION_ALT_A_SIZE=$(raw_size "$work/base/attestation-amd64-alt.raw")
TEST_ATTESTATION_ALT_B_SIZE=$(raw_size "$work/base/attestation-arm64-alt.raw")
TEST_ATTESTATION_NEXT_A=$(sha256_path "$work/base/attestation-amd64-next.raw")
TEST_ATTESTATION_NEXT_B=$(sha256_path "$work/base/attestation-arm64-next.raw")
TEST_ATTESTATION_NEXT_A_SIZE=$(raw_size "$work/base/attestation-amd64-next.raw")
TEST_ATTESTATION_NEXT_B_SIZE=$(raw_size "$work/base/attestation-arm64-next.raw")

build_a_manifests=$(jq -cn \
  --arg runnable "$TEST_RUNNABLE_A" \
  --arg attestation "$TEST_ATTESTATION_A" \
  --argjson attestation_size "$TEST_ATTESTATION_A_SIZE" '
    [
      {
        mediaType: "application/vnd.oci.image.manifest.v1+json",
        digest: ("sha256:" + $runnable), size: 101,
        platform: {architecture: "amd64", os: "linux"}
      },
      {
        mediaType: "application/vnd.oci.image.manifest.v1+json",
        digest: $attestation, size: $attestation_size,
        annotations: {
          "vnd.docker.reference.digest": ("sha256:" + $runnable),
          "vnd.docker.reference.type": "attestation-manifest"
        },
        platform: {architecture: "unknown", os: "unknown"}
      }
    ]
  ')
build_b_manifests=$(jq -cn \
  --arg runnable "$TEST_RUNNABLE_B" \
  --arg attestation "$TEST_ATTESTATION_B" \
  --argjson attestation_size "$TEST_ATTESTATION_B_SIZE" '
    [
      {
        mediaType: "application/vnd.oci.image.manifest.v1+json",
        digest: ("sha256:" + $runnable), size: 101,
        platform: {architecture: "arm64", os: "linux"}
      },
      {
        mediaType: "application/vnd.oci.image.manifest.v1+json",
        digest: $attestation, size: $attestation_size,
        annotations: {
          "vnd.docker.reference.digest": ("sha256:" + $runnable),
          "vnd.docker.reference.type": "attestation-manifest"
        },
        platform: {architecture: "unknown", os: "unknown"}
      }
    ]
  ')
stage_manifests=$(jq -cn \
  --argjson a "$build_a_manifests" \
  --argjson b "$build_b_manifests" '$a + $b')

write_raw "$work/base/build-amd64.raw" \
  "($build_a_manifests) as \$manifests | $index_filter"
write_raw "$work/base/build-arm64.raw" \
  "($build_b_manifests) as \$manifests | $index_filter"
write_raw "$work/base/stage.raw" \
  "($stage_manifests) as \$manifests | $index_filter"

expected_public_mapping=$(jq -cnS \
  --arg a "$TEST_RUNNABLE_A" \
  --arg b "$TEST_RUNNABLE_B" '[
    {platform: "linux/amd64", runnable_digest: ("sha256:" + $a)},
    {platform: "linux/arm64", runnable_digest: ("sha256:" + $b)}
  ]')
public_mapping=$(jq -cer -f "$runnable_mapping" "$work/base/stage.raw")
[[ "$public_mapping" == "$expected_public_mapping" ]] || {
  echo "attested public index did not yield only runnable platform mappings" >&2
  exit 1
}
cp "$work/base/stage.raw" "$work/base/unmapped-attestation.raw"
rewrite_raw "$work/base/unmapped-attestation.raw" \
  '.manifests[1].annotations["vnd.docker.reference.digest"] = ("sha256:" + ("9" * 64))'
if jq -cer -f "$runnable_mapping" \
    "$work/base/unmapped-attestation.raw" > /dev/null 2>&1; then
  echo "public index mapping accepted evidence for an unknown runnable" >&2
  exit 1
fi

next_manifests=$(jq -cn \
  --arg a "$TEST_NEXT_RUNNABLE_A" \
  --arg b "$TEST_NEXT_RUNNABLE_B" \
  --arg attestation_a "$TEST_ATTESTATION_NEXT_A" \
  --arg attestation_b "$TEST_ATTESTATION_NEXT_B" \
  --argjson attestation_a_size "$TEST_ATTESTATION_NEXT_A_SIZE" \
  --argjson attestation_b_size "$TEST_ATTESTATION_NEXT_B_SIZE" '
    [
      {
        mediaType: "application/vnd.oci.image.manifest.v1+json",
        digest: ("sha256:" + $a), size: 101,
        platform: {architecture: "amd64", os: "linux"}
      },
      {
        mediaType: "application/vnd.oci.image.manifest.v1+json",
        digest: $attestation_a, size: $attestation_a_size,
        annotations: {
          "vnd.docker.reference.digest": ("sha256:" + $a),
          "vnd.docker.reference.type": "attestation-manifest"
        },
        platform: {architecture: "unknown", os: "unknown"}
      },
      {
        mediaType: "application/vnd.oci.image.manifest.v1+json",
        digest: ("sha256:" + $b), size: 101,
        platform: {architecture: "arm64", os: "linux"}
      },
      {
        mediaType: "application/vnd.oci.image.manifest.v1+json",
        digest: $attestation_b, size: $attestation_b_size,
        annotations: {
          "vnd.docker.reference.digest": ("sha256:" + $b),
          "vnd.docker.reference.type": "attestation-manifest"
        },
        platform: {architecture: "unknown", os: "unknown"}
      }
    ]
  ')
write_raw "$work/base/stage-next.raw" \
  "($next_manifests) as \$manifests | $index_filter"

cp "$work/base/stage.raw" "$work/base/stage-alt.raw"
rewrite_raw "$work/base/stage-alt.raw" "
  .manifests[1].digest = \"$TEST_ATTESTATION_ALT_A\" |
  .manifests[1].size = $TEST_ATTESTATION_ALT_A_SIZE |
  .manifests[3].digest = \"$TEST_ATTESTATION_ALT_B\" |
  .manifests[3].size = $TEST_ATTESTATION_ALT_B_SIZE
"
cp "$work/base/stage.raw" "$work/base/stage-swapped.raw"
rewrite_raw "$work/base/stage-swapped.raw" '
  .manifests[0].digest = ("sha256:" + ("b" * 64)) |
  .manifests[0].platform.architecture = "amd64" |
  .manifests[1].annotations["vnd.docker.reference.digest"] =
    ("sha256:" + ("b" * 64)) |
  .manifests[2].digest = ("sha256:" + ("a" * 64)) |
  .manifests[2].platform.architecture = "arm64" |
  .manifests[3].annotations["vnd.docker.reference.digest"] =
    ("sha256:" + ("a" * 64))
'
cp "$work/base/stage.raw" "$work/base/stage-no-arm-attestation.raw"
rewrite_raw "$work/base/stage-no-arm-attestation.raw" 'del(.manifests[3])'
cp "$work/base/stage.raw" "$work/base/stage-orphan-attestation.raw"
rewrite_raw "$work/base/stage-orphan-attestation.raw" '
  .manifests[3].annotations["vnd.docker.reference.digest"] =
    ("sha256:" + ("f" * 64))
'
cp "$work/base/stage.raw" "$work/base/stage-wrong-attestation-platform.raw"
rewrite_raw "$work/base/stage-wrong-attestation-platform.raw" '
  .manifests[3].platform = {architecture: "arm64", os: "linux"}
'

cat > "$work/bin/docker" <<'STUB'
#!/bin/sh
set -eu

state=${TEST_STATE:?}
image=${TEST_IMAGE:?}
mode=${DOCKER_MODE:-valid}
printf '%s\n' "$*" >> "$state/docker.log"

digest_file() {
  if command -v sha256sum > /dev/null 2>&1; then
    value=$(sha256sum "$1" | awk '{print $1}')
  else
    value=$(shasum -a 256 "$1" | awk '{print $1}')
  fi
  printf 'sha256:%s\n' "$value"
}

resolve_reference() {
  reference=$1
  resolved=
  case "$reference" in
    "$image:sha-$TEST_SOURCE_SHA")
      [ ! -f "$state/staging.raw" ] || resolved="$state/staging.raw"
      ;;
    "$image:${TEST_TAG#v}")
      [ ! -f "$state/version.raw" ] || resolved="$state/version.raw"
      ;;
    "$image:${TEST_NEXT_TAG#v}")
      [ ! -f "$state/version-next.raw" ] || resolved="$state/version-next.raw"
      ;;
    "$image:latest")
      [ ! -f "$state/latest.raw" ] || resolved="$state/latest.raw"
      ;;
    "$image"@sha256:*)
      wanted=${reference#*@}
      for candidate in "$state"/fixtures/*.raw "$state"/*.raw; do
        [ -f "$candidate" ] || continue
        if [ "$(digest_file "$candidate")" = "$wanted" ]; then
          resolved=$candidate
          break
        fi
      done
      ;;
  esac
}

missing() {
  reference=$1
  if { [ "$mode" = ambiguous_stage_absence ] &&
       [ "$reference" = "$image:sha-$TEST_SOURCE_SHA" ]; } ||
     { [ "$mode" = ambiguous_version_absence ] &&
       [ "$reference" = "$image:${TEST_TAG#v}" ]; }; then
    echo "proxy route not found" >&2
  elif [ "$mode" = noisy_stage_absence ] &&
       [ "$reference" = "$image:sha-$TEST_SOURCE_SHA" ]; then
    echo "warning: registry mirror fallback" >&2
    echo "ERROR: $reference: not found" >&2
  else
    echo "ERROR: $reference: not found" >&2
  fi
  exit 1
}

[ "${1:-}" = buildx ] && [ "${2:-}" = imagetools ] || {
  echo "unexpected docker command: $*" >&2
  exit 1
}
operation=${3:-}
shift 3
case "$operation" in
  inspect)
    kind=${1:-}
    case "$kind" in
      --raw)
        reference=${2:-}
        resolve_reference "$reference"
        [ -n "$resolved" ] || missing "$reference"
        cat "$resolved"
        ;;
      --format)
        [ "$#" -eq 3 ] || {
          echo "unexpected formatted inspect arguments" >&2
          exit 1
        }
        reference=$3
        resolve_reference "$reference"
        [ -n "$resolved" ] || missing "$reference"
        digest=$(digest_file "$resolved")
        if [ "$mode" = formatted_mismatch ] &&
           [ "$reference" = "${MISMATCH_REFERENCE:-}" ]; then
          digest="sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
        fi
        jq -cn --arg digest "$digest" '$digest'
        ;;
      *)
        echo "unexpected inspect arguments: $*" >&2
        exit 1
        ;;
    esac
    ;;
  create)
    metadata=
    target=
    source_1=
    source_2=
    source_count=0
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --metadata-file)
          metadata=${2:-}
          shift 2
          ;;
        --prefer-index=false)
          shift
          ;;
        -t)
          target=${2:-}
          shift 2
          ;;
        *)
          source_count=$((source_count + 1))
          if [ "$source_count" -eq 1 ]; then
            source_1=$1
          elif [ "$source_count" -eq 2 ]; then
            source_2=$1
          else
            echo "too many create sources" >&2
            exit 1
          fi
          shift
          ;;
      esac
    done
    [ -n "$metadata" ] && [ -n "$target" ] || {
      echo "create omitted metadata or target" >&2
      exit 1
    }
    case "$target" in
      "$image:sha-$TEST_SOURCE_SHA")
        [ "$mode" != create_stage_fail ] || {
          echo "stage create failed" >&2
          exit 1
        }
        [ "$source_count" -eq 2 ] || {
          echo "stage requires two build sources" >&2
          exit 1
        }
        if [ "$mode" = create_stage_bad ]; then
          cp "$state/fixtures/stage-swapped.raw" "$state/staging.raw"
        else
          cp "$state/fixtures/stage.raw" "$state/staging.raw"
        fi
        created="$state/staging.raw"
        ;;
      "$image:${TEST_TAG#v}")
        [ "$mode" != create_version_fail ] || {
          echo "version create failed" >&2
          exit 1
        }
        [ "$source_count" -eq 1 ] || {
          echo "version requires one source" >&2
          exit 1
        }
        if [ "$mode" = create_version_bad ]; then
          cp "$state/fixtures/stage-swapped.raw" "$state/version.raw"
        else
          resolve_reference "$source_1"
          [ -n "$resolved" ] || missing "$source_1"
          cp "$resolved" "$state/version.raw"
        fi
        created="$state/version.raw"
        ;;
      "$image:latest")
        [ "$mode" != create_latest_fail ] || {
          echo "latest create failed" >&2
          exit 1
        }
        [ "$source_count" -eq 1 ] || {
          echo "latest requires one source" >&2
          exit 1
        }
        if [ "$mode" = create_latest_bad ]; then
          cp "$state/fixtures/stage-swapped.raw" "$state/latest.raw"
        else
          resolve_reference "$source_1"
          [ -n "$resolved" ] || missing "$source_1"
          cp "$resolved" "$state/latest.raw"
        fi
        created="$state/latest.raw"
        ;;
      *)
        echo "unexpected create target: $target" >&2
        exit 1
        ;;
    esac
    digest=$(digest_file "$created")
    if [ "$mode" = create_metadata_bad ]; then
      digest="sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
    fi
    media_type=$(jq -r '.mediaType' "$created")
    size=$(wc -c < "$created" | tr -d '[:space:]')
    jq -n \
      --arg digest "$digest" \
      --arg media_type "$media_type" \
      --argjson size "$size" '
        {
          "containerimage.descriptor": {
            mediaType: $media_type,
            digest: $digest,
            size: $size
          },
          "image.name": "ghcr.io/test/project"
        }
      ' > "$metadata"
    ;;
  *)
    echo "unexpected imagetools operation: $operation" >&2
    exit 1
    ;;
esac
STUB

cat > "$work/bin/gh" <<'STUB'
#!/bin/sh
set -eu

state=${TEST_STATE:?}
mode=${GH_MODE:-valid}
printf '%s\n' "$*" >> "$state/gh.log"

digest_file() {
  if command -v sha256sum > /dev/null 2>&1; then
    value=$(sha256sum "$1" | awk '{print $1}')
  else
    value=$(shasum -a 256 "$1" | awk '{print $1}')
  fi
  printf 'sha256:%s\n' "$value"
}

release_asset() {
  asset_id=$1
  asset_path=$2
  asset_name=$(basename "$asset_path")
  size=$(wc -c < "$asset_path" | tr -d '[:space:]')
  digest=$(digest_file "$asset_path")
  if [ "$mode" = latest_bad_record_metadata ]; then
    case "$asset_name" in
      *-release.json)
        digest=sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
        ;;
    esac
  fi
  if [ "$mode" = latest_bad_bundle_metadata ]; then
    case "$asset_name" in
      *-container.provenance.sigstore.json)
        digest=sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
        ;;
    esac
  fi
  jq -cn \
    --argjson id "$asset_id" \
    --arg name "$asset_name" \
    --argjson size "$size" \
    --arg digest "$digest" \
    '{id: $id, name: $name, state: "uploaded", size: $size, digest: $digest}'
}

release_assets() {
  tag=$1
  id_base=$2
  container_base="mcp-repl-$tag-container"
  if [ "$mode" != latest_missing_record ]; then
    release_asset "$id_base" "$state/mcp-repl-$tag-release.json"
  fi
  release_asset "$((id_base + 1))" "$state/$container_base.spdx.json"
  release_asset "$((id_base + 2))" \
    "$state/$container_base.provenance.sigstore.json"
  release_asset "$((id_base + 3))" \
    "$state/$container_base.sbom.sigstore.json"
}

release_json() {
  tag=$1
  id=$2
  draft=$3
  immutable=$4
  author=$5
  name=$6
  body=$7
  jq -cn \
    --argjson id "$id" \
    --arg tag "$tag" \
    --arg name "$name" \
    --arg body "$body" \
    --argjson draft "$draft" \
    --argjson immutable "$immutable" \
    --arg author "$author" '
      {
        id: $id,
        tag_name: $tag,
        name: $name,
        body: $body,
        draft: $draft,
        prerelease: false,
        immutable: $immutable,
        author: {
          login: $author,
          type: (if $author == "github-actions[bot]" then "Bot" else "User" end)
        }
      }
    '
}

case "${1:-}" in
  attestation)
    case "${2:-}" in
      trusted-root)
        [ "$#" -eq 2 ] || {
          echo "unexpected trusted-root arguments: $*" >&2
          exit 1
        }
        printf '{"fixture":"trusted-root"}\n'
        exit 0
        ;;
      verify)
        [ "$#" -eq 18 ] || {
          echo "unexpected attestation verify arguments: $*" >&2
          exit 1
        }
        subject=$3
        [ "$4" = --bundle ] && bundle=$5
        [ "$6" = --predicate-type ] && predicate=$7
        [ "$8" = --repo ] && [ "$9" = test/project ]
        [ "${10}" = --custom-trusted-root ] && trusted_root=${11}
        [ "${12}" = --signer-workflow ] &&
          [ "${13}" = test/project/.github/workflows/release-binaries.yml ]
        [ "${14}" = --source-digest ] && source_sha=${15}
        [ "${16}" = --source-ref ] && source_ref=${17}
        [ "${18}" = --deny-self-hosted-runners ]
        [ -f "$trusted_root" ] && [ -s "$trusted_root" ]
        tag=${source_ref#refs/tags/}
        [ "$source_ref" = "refs/tags/$tag" ]
        record="$state/mcp-repl-$tag-release.json"
        [ -f "$record" ]
        manifest=$(jq -er '.container.manifest_digest' "$record")
        [ "$subject" = "oci://$TEST_IMAGE@$manifest" ]
        [ "$source_sha" = "$(jq -er '.source_sha' "$record")" ]
        case "$predicate" in
          https://slsa.dev/provenance/v1)
            expected_fixture=trusted-provenance
            ;;
          https://spdx.dev/Document/v2.3)
            expected_fixture=trusted-sbom
            ;;
          *)
            echo "unexpected predicate: $predicate" >&2
            exit 1
            ;;
        esac
        jq -e \
          --arg fixture "$expected_fixture" \
          --arg tag "$tag" \
          --arg source_sha "$source_sha" '
            . == {
              fixture: $fixture,
              source_sha: $source_sha,
              tag: $tag
            }
          ' "$bundle" > /dev/null
        [ "$mode" != latest_attestation_verify_fails ]
        printf '[]\n'
        exit 0
        ;;
      *)
        echo "unexpected gh attestation command: $*" >&2
        exit 1
        ;;
    esac
    ;;
  api)
    endpoint=${2:-}
    ;;
  *)
    echo "unexpected gh command: $*" >&2
    exit 1
    ;;
esac

case "$endpoint" in
  "repos/test/project/git/ref/tags/$TEST_TAG")
    jq -cn --arg ref "refs/tags/$TEST_TAG" --arg sha "$TEST_TAG_OBJECT_SHA" '
      {ref: $ref, object: {type: "tag", sha: $sha}}
    '
    ;;
  "repos/test/project/git/ref/tags/$TEST_NEXT_TAG")
    jq -cn \
      --arg ref "refs/tags/$TEST_NEXT_TAG" \
      --arg sha "$TEST_NEXT_TAG_OBJECT_SHA" '
        {ref: $ref, object: {type: "tag", sha: $sha}}
      '
    ;;
  "repos/test/project/git/tags/$TEST_TAG_OBJECT_SHA")
    source_sha=$TEST_SOURCE_SHA
    [ "$mode" != tag_moved ] || source_sha=$TEST_OTHER_SOURCE_SHA
    jq -cn \
      --arg tag "$TEST_TAG" \
      --arg source_sha "$source_sha" '
        {
          tag: $tag,
          message: ("chore: Release package mcp-repl version " + ($tag | ltrimstr("v"))),
          object: {type: "commit", sha: $source_sha},
          tagger: {
            name: "github-actions[bot]",
            email: "41898282+github-actions[bot]@users.noreply.github.com"
          }
        }
      '
    ;;
  "repos/test/project/git/tags/$TEST_NEXT_TAG_OBJECT_SHA")
    jq -cn \
      --arg tag "$TEST_NEXT_TAG" \
      --arg source_sha "$TEST_SOURCE_SHA" '
        {
          tag: $tag,
          message: ("chore: Release package mcp-repl version " + ($tag | ltrimstr("v"))),
          object: {type: "commit", sha: $source_sha},
          tagger: {
            name: "github-actions[bot]",
            email: "41898282+github-actions[bot]@users.noreply.github.com"
          }
        }
      '
    ;;
  "repos/test/project/commits/$TEST_TAG")
    if [ "$mode" = tag_moved ]; then
      printf '%s\n' "$TEST_OTHER_SOURCE_SHA"
    else
      printf '%s\n' "$TEST_SOURCE_SHA"
    fi
    ;;
  "repos/test/project/commits/$TEST_NEXT_TAG")
    printf '%s\n' "$TEST_SOURCE_SHA"
    ;;
  "repos/test/project/releases/tags/$TEST_TAG")
    count_file="$state/draft_release_calls"
    count=0
    [ ! -f "$count_file" ] || count=$(cat "$count_file")
    count=$((count + 1))
    printf '%s\n' "$count" > "$count_file"
    id=42
    draft=true
    immutable=false
    author='github-actions[bot]'
    name=$TEST_TAG
    body=$TEST_RELEASE_BODY
    case "$mode" in
      draft_public) draft=false; immutable=true ;;
      draft_foreign) author=octocat ;;
      draft_wrong_name) name=wrong ;;
      draft_wrong_body) body=wrong ;;
      draft_replaced) [ "$count" -eq 1 ] || id=43 ;;
      draft_malformed) printf '{}\n'; exit 0 ;;
    esac
    release_json "$TEST_TAG" "$id" "$draft" "$immutable" "$author" "$name" "$body"
    ;;
  repos/test/project/releases/latest)
    count_file="$state/latest_calls"
    count=0
    [ ! -f "$count_file" ] || count=$(cat "$count_file")
    count=$((count + 1))
    printf '%s\n' "$count" > "$count_file"
    tag=$TEST_TAG
    id=42
    author='github-actions[bot]'
    name=$tag
    draft=false
    immutable=true
    case "$mode" in
      latest_newer) tag=$TEST_NEXT_TAG; id=43; name=$tag ;;
      latest_race)
        if [ "$count" -gt 2 ]; then
          tag=$TEST_NEXT_TAG; id=43; name=$tag
        fi
        ;;
      latest_unstable)
        if [ $((count % 2)) -eq 0 ]; then
          tag=$TEST_NEXT_TAG; id=43; name=$tag
        fi
        ;;
      latest_foreign) author=octocat ;;
      latest_mutable) immutable=false ;;
      latest_draft) draft=true ;;
      latest_wrong_name) name=wrong ;;
      latest_malformed) printf '{}\n'; exit 0 ;;
    esac
    release_json "$tag" "$id" "$draft" "$immutable" "$author" "$name" ""
    ;;
  "repos/test/project/releases/42/assets?per_page=100")
    release_assets "$TEST_TAG" 420 | jq -cs '.'
    ;;
  "repos/test/project/releases/43/assets?per_page=100")
    release_assets "$TEST_NEXT_TAG" 430 | jq -cs '.'
    ;;
  repos/test/project/releases/assets/420)
    cat "$state/mcp-repl-$TEST_TAG-release.json"
    ;;
  repos/test/project/releases/assets/421)
    cat "$state/mcp-repl-$TEST_TAG-container.spdx.json"
    ;;
  repos/test/project/releases/assets/422)
    cat "$state/mcp-repl-$TEST_TAG-container.provenance.sigstore.json"
    ;;
  repos/test/project/releases/assets/423)
    cat "$state/mcp-repl-$TEST_TAG-container.sbom.sigstore.json"
    ;;
  repos/test/project/releases/assets/430)
    cat "$state/mcp-repl-$TEST_NEXT_TAG-release.json"
    ;;
  repos/test/project/releases/assets/431)
    cat "$state/mcp-repl-$TEST_NEXT_TAG-container.spdx.json"
    ;;
  repos/test/project/releases/assets/432)
    cat "$state/mcp-repl-$TEST_NEXT_TAG-container.provenance.sigstore.json"
    ;;
  repos/test/project/releases/assets/433)
    cat "$state/mcp-repl-$TEST_NEXT_TAG-container.sbom.sigstore.json"
    ;;
  *)
    echo "unexpected API endpoint: $endpoint" >&2
    exit 1
    ;;
esac
STUB

chmod +x "$work/bin/docker" "$work/bin/gh"
export PATH="$work/bin:$PATH"
export GH_REPO=test/project

write_metadata() {
  local platform build_file runnable output
  for platform in linux/amd64 linux/arm64; do
    case "$platform" in
      linux/amd64)
        build_file="$TEST_STATE/fixtures/build-amd64.raw"
        runnable="sha256:$TEST_RUNNABLE_A"
        ;;
      linux/arm64)
        build_file="$TEST_STATE/fixtures/build-arm64.raw"
        runnable="sha256:$TEST_RUNNABLE_B"
        ;;
    esac
    output="$TEST_METADATA/${platform//\//-}.json"
    jq -nS \
      --arg tag "$TEST_TAG" \
      --arg source_sha "$TEST_SOURCE_SHA" \
      --argjson source_epoch "$TEST_SOURCE_EPOCH" \
      --arg image "$TEST_IMAGE" \
      --arg platform "$platform" \
      --arg build_digest "$(sha256_path "$build_file")" \
      --arg runnable_digest "$runnable" '
        {
          schema_version: 1,
          package: "mcp-repl",
          tag: $tag,
          source_sha: $source_sha,
          source_epoch: $source_epoch,
          image: $image,
          platform: $platform,
          build_digest: $build_digest,
          runnable_digest: $runnable_digest,
          buildkit: {provenance: true, sbom: true}
        }
      ' > "$output"
  done
}

write_release_record_fixture() {
  local path=$1 tag=$2 manifest=$3 runnable_a=$4 runnable_b=$5
  local directory container_base sbom_identity provenance_identity
  local sbom_attestation_identity
  directory=$(dirname "$path")
  container_base="mcp-repl-$tag-container"
  sbom_identity=$(file_identity_fixture \
    "$directory/$container_base.spdx.json")
  provenance_identity=$(file_identity_fixture \
    "$directory/$container_base.provenance.sigstore.json")
  sbom_attestation_identity=$(file_identity_fixture \
    "$directory/$container_base.sbom.sigstore.json")
  jq -cS -n \
    --arg tag "$tag" \
    --arg source_sha "$TEST_SOURCE_SHA" \
    --argjson source_epoch "$TEST_SOURCE_EPOCH" \
    --arg image "$TEST_IMAGE" \
    --arg manifest "$manifest" \
    --arg runnable_a "$runnable_a" \
    --arg runnable_b "$runnable_b" \
    --argjson sbom "$sbom_identity" \
    --argjson provenance "$provenance_identity" \
    --argjson sbom_attestation "$sbom_attestation_identity" '
      {
        schema_version: 1,
        package: "mcp-repl",
        tag: $tag,
        version: ($tag | ltrimstr("v")),
        source_sha: $source_sha,
        source_epoch: $source_epoch,
        release_targets: {fixture: true},
        native: [],
        container: {
          image: $image,
          manifest_digest: $manifest,
          platforms: [
            {platform: "linux/amd64", runnable_digest: ("sha256:" + $runnable_a)},
            {platform: "linux/arm64", runnable_digest: ("sha256:" + $runnable_b)}
          ],
          sbom: $sbom,
          attestations: {
            provenance: $provenance,
            sbom: $sbom_attestation
          }
        }
      }
    ' > "$path"
}

file_identity_fixture() {
  local path=$1 digest size
  digest=$(sha256_path "$path")
  size=$(wc -c < "$path" | tr -d '[:space:]')
  jq -cn \
    --arg name "$(basename "$path")" \
    --arg sha256 "${digest#sha256:}" \
    --argjson size "$size" \
    '{name: $name, size: $size, sha256: $sha256}'
}

refresh_provenance_identity() {
  local tag=$1 record asset identity temporary
  record="$TEST_STATE/mcp-repl-$tag-release.json"
  asset="$TEST_STATE/mcp-repl-$tag-container.provenance.sigstore.json"
  identity=$(file_identity_fixture "$asset")
  temporary="$record.tmp"
  jq -cS --argjson identity "$identity" \
    '.container.attestations.provenance = $identity' \
    "$record" > "$temporary"
  mv "$temporary" "$record"
}

write_container_supply_fixture() {
  local tag=$1 container_base
  container_base="mcp-repl-$tag-container"
  jq -cS -n --arg tag "$tag" \
    '{fixture: "container-spdx", tag: $tag}' \
    > "$TEST_STATE/$container_base.spdx.json"
  jq -cS -n \
    --arg tag "$tag" \
    --arg source_sha "$TEST_SOURCE_SHA" '
      {fixture: "trusted-provenance", tag: $tag, source_sha: $source_sha}
    ' > "$TEST_STATE/$container_base.provenance.sigstore.json"
  jq -cS -n \
    --arg tag "$tag" \
    --arg source_sha "$TEST_SOURCE_SHA" '
      {fixture: "trusted-sbom", tag: $tag, source_sha: $source_sha}
    ' > "$TEST_STATE/$container_base.sbom.sigstore.json"
}

setup_case() {
  local name=$1
  TEST_STATE="$work/$name"
  TEST_METADATA="$TEST_STATE/metadata"
  TEST_OUTPUT="$TEST_STATE/output"
  TEST_OCI_BLOB_CACHE="$TEST_STATE/fixtures"
  export TEST_STATE TEST_METADATA TEST_OUTPUT TEST_OCI_BLOB_CACHE
  mkdir -p "$TEST_STATE/fixtures" "$TEST_METADATA" "$TEST_OUTPUT"
  cp "$work/base"/*.raw "$work/base"/*.blob "$TEST_STATE/fixtures/"
  write_container_supply_fixture "$TEST_TAG"
  write_container_supply_fixture "$TEST_NEXT_TAG"
  write_release_record_fixture \
    "$TEST_STATE/mcp-repl-$TEST_TAG-release.json" \
    "$TEST_TAG" \
    "$(sha256_path "$TEST_STATE/fixtures/stage.raw")" \
    "$TEST_RUNNABLE_A" \
    "$TEST_RUNNABLE_B"
  write_release_record_fixture \
    "$TEST_STATE/mcp-repl-$TEST_NEXT_TAG-release.json" \
    "$TEST_NEXT_TAG" \
    "$(sha256_path "$TEST_STATE/fixtures/stage-next.raw")" \
    "$TEST_NEXT_RUNNABLE_A" \
    "$TEST_NEXT_RUNNABLE_B"
  : > "$TEST_STATE/docker.log"
  : > "$TEST_STATE/gh.log"
  unset DOCKER_MODE GH_MODE MISMATCH_REFERENCE
  write_metadata
}

run_stage() {
  "$publish" stage "$TEST_TAG" "$TEST_SOURCE_SHA" "$TEST_SOURCE_EPOCH" \
    "$TEST_METADATA" "$TEST_OUTPUT"
}

prepare_version_case() {
  local name=$1
  setup_case "$name"
  run_stage > /dev/null
  : > "$TEST_STATE/docker.log"
  : > "$TEST_STATE/gh.log"
}

expect_failure() {
  local label=$1
  shift
  if "$@" > "$TEST_STATE/stdout" 2> "$TEST_STATE/stderr"; then
    echo "$label unexpectedly succeeded" >&2
    exit 1
  fi
}

assert_no_target() {
  local target=$1
  if grep -Fq "create " "$TEST_STATE/docker.log" &&
      grep -Fq -- "-t $target" "$TEST_STATE/docker.log"; then
    echo "Unexpected registry write to $target" >&2
    exit 1
  fi
}

# Dispatch is strict and produces no registry/API side effects.
setup_case bad_dispatch
expect_failure "missing stage argument" "$publish" stage "$TEST_TAG"
expect_failure "extra latest argument" "$publish" latest extra
if [[ -s "$TEST_STATE/docker.log" || -s "$TEST_STATE/gh.log" ]]; then
  echo "Invalid dispatch reached Docker or GitHub" >&2
  exit 1
fi

# Fresh stage: only the source-addressed alias is exposed and the record is
# exact, compact, recursively sorted JSON with sorted platform rows.
setup_case stage_fresh
run_stage
record="$TEST_OUTPUT/image-manifest.json"
test -f "$record" && test ! -L "$record"
jq -e \
  --arg tag "$TEST_TAG" \
  --arg source_sha "$TEST_SOURCE_SHA" \
  --argjson source_epoch "$TEST_SOURCE_EPOCH" \
  --arg image "$TEST_IMAGE" '
    (keys | sort) == ([
      "schema_version", "package", "tag", "source_sha", "source_epoch",
      "image", "staging_ref", "manifest_digest", "platforms"
    ] | sort) and
    .schema_version == 1 and .package == "mcp-repl" and
    .tag == $tag and .source_sha == $source_sha and
    .source_epoch == $source_epoch and .image == $image and
    .staging_ref == ($image + ":sha-" + $source_sha) and
    [.platforms[].platform] == ["linux/amd64", "linux/arm64"] and
    all(.platforms[];
      (keys | sort) == (["platform", "build_digest", "runnable_digest"] | sort))
  ' "$record" > /dev/null
jq -cS . "$record" > "$TEST_STATE/record.canonical"
cmp "$record" "$TEST_STATE/record.canonical"
test "$(jq -r '.manifest_digest' "$record")" = \
  "$(sha256_path "$TEST_STATE/staging.raw")"
grep -Fq -- "-t $TEST_IMAGE:sha-$TEST_SOURCE_SHA" "$TEST_STATE/docker.log"
assert_no_target "$TEST_IMAGE:${TEST_TAG#v}"
assert_no_target "$TEST_IMAGE:latest"
if [[ -s "$TEST_STATE/gh.log" ]]; then
  echo "Stage consulted GitHub release state" >&2
  exit 1
fi

# An exact staging rerun is read-only. A previous run's attestations may differ
# while the exact runnable mapping remains normative.
setup_case stage_existing
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/staging.raw"
run_stage > /dev/null
if grep -Fq "imagetools create" "$TEST_STATE/docker.log"; then
  echo "Exact existing staging image was replaced" >&2
  exit 1
fi

setup_case stage_existing_alternate_attestations
cp "$TEST_STATE/fixtures/stage-alt.raw" "$TEST_STATE/staging.raw"
run_stage > /dev/null
test "$(jq -r '.manifest_digest' "$TEST_OUTPUT/image-manifest.json")" = \
  "$(sha256_path "$TEST_STATE/staging.raw")"

# Exact platform->digest pairing and all attestation descriptor invariants are
# required for an existing alias.
for fixture in stage-swapped stage-no-arm-attestation \
  stage-orphan-attestation stage-wrong-attestation-platform; do
  setup_case "reject_$fixture"
  cp "$TEST_STATE/fixtures/$fixture.raw" "$TEST_STATE/staging.raw"
  expect_failure "$fixture staging manifest" run_stage
  if grep -Fq "imagetools create" "$TEST_STATE/docker.log"; then
    echo "$fixture staging mismatch triggered a registry write" >&2
    exit 1
  fi
done

# Each source build is independently bound to its raw digest, runnable mapping,
# and at least one mapped BuildKit attestation before staging.
setup_case build_missing_attestation
rewrite_raw "$TEST_STATE/fixtures/build-amd64.raw" 'del(.manifests[1])'
write_metadata
expect_failure "source build missing attestation" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_missing_provenance
rewrite_raw "$TEST_STATE/fixtures/attestation-amd64.raw" '
  .layers |= map(select(
    .annotations["in-toto.io/predicate-type"] !=
      "https://slsa.dev/provenance/v1"))
'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build missing provenance evidence" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_missing_sbom
rewrite_raw "$TEST_STATE/fixtures/attestation-amd64.raw" '
  .layers |= map(select(
    .annotations["in-toto.io/predicate-type"] !=
      "https://spdx.dev/Document"))
'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build missing SPDX SBOM evidence" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_malformed_attestation_manifest
rewrite_raw "$TEST_STATE/fixtures/attestation-amd64.raw" \
  '.config.size = "2"'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build malformed attestation manifest" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_non_intoto_attestation_layer
rewrite_raw "$TEST_STATE/fixtures/attestation-amd64.raw" '
  .layers += [{
    mediaType: "application/octet-stream",
    digest: ("sha256:" + ("f" * 64)),
    size: 1,
    annotations: {}
  }]
'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build non-in-toto attestation layer" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

# Layer annotations are only indexes. The exact referenced statement bytes,
# descriptor size, predicateType, predicate shape, and subject binding are all
# independently enforced before any staging alias can be written.
setup_case build_attestation_blob_digest_mismatch
provenance_digest=$(jq -er '.layers[0].digest' \
  "$TEST_STATE/fixtures/attestation-amd64.raw")
rewrite_raw \
  "$TEST_OCI_BLOB_CACHE/${provenance_digest#sha256:}.blob" \
  '.subject[0].digest.sha256 = ("c" * 64)'
expect_failure "source build attestation blob digest mismatch" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_attestation_blob_missing
provenance_digest=$(jq -er '.layers[0].digest' \
  "$TEST_STATE/fixtures/attestation-amd64.raw")
rm "$TEST_OCI_BLOB_CACHE/${provenance_digest#sha256:}.blob"
expect_failure "source build missing attestation blob" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_attestation_blob_size_mismatch
rewrite_raw "$TEST_STATE/fixtures/attestation-amd64.raw" \
  '.layers[0].size += 1'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build attestation blob size mismatch" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_malformed_attestation_statement
rebind_layer_blob "$TEST_STATE/fixtures/attestation-amd64.raw" 0 '{}'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build malformed in-toto statement" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_mismatched_statement_predicate
rebind_layer_blob "$TEST_STATE/fixtures/attestation-amd64.raw" 0 \
  '.predicateType = "https://spdx.dev/Document"'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build mismatched statement predicate" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_malformed_statement_predicate
rebind_layer_blob "$TEST_STATE/fixtures/attestation-amd64.raw" 0 \
  '.predicate = {}'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build malformed statement predicate" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_empty_sbom_inventory
rebind_layer_blob "$TEST_STATE/fixtures/attestation-amd64.raw" 1 \
  '.predicate.packages = []'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build empty SPDX inventory" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_wrong_sbom_package_version
rebind_layer_blob "$TEST_STATE/fixtures/attestation-amd64.raw" 1 \
  '.predicate.packages[1].versionInfo = "9.9.9"'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build wrong SPDX package version" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_wrong_sbom_package_purl
rebind_layer_blob "$TEST_STATE/fixtures/attestation-amd64.raw" 1 \
  '.predicate.packages[1].externalRefs[0].referenceLocator =
    "pkg:cargo/mcp-repl@9.9.9"'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build wrong SPDX package URL" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_wrong_spdx_version
rebind_layer_blob "$TEST_STATE/fixtures/attestation-amd64.raw" 1 \
  '.predicate.spdxVersion = "SPDX-2.2"'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build wrong SPDX version" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_unrooted_sbom_package
rebind_layer_blob "$TEST_STATE/fixtures/attestation-amd64.raw" 1 '
  .predicate.relationships |= map(select(
    .relationshipType != "CONTAINS"))
'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build unrooted SPDX package" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_unevidenced_sbom_package
rebind_layer_blob "$TEST_STATE/fixtures/attestation-amd64.raw" 1 '
  .predicate.relationships |= map(select(
    .relationshipType != "OTHER"))
'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build unevidenced SPDX package" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_sbom_without_dependency_edges
rebind_layer_blob "$TEST_STATE/fixtures/attestation-amd64.raw" 1 '
  .predicate.relationships |= map(select(
    .relationshipType != "DEPENDENCY_OF"))
'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build SPDX without dependency edges" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_mismapped_statement_subject
rebind_layer_blob "$TEST_STATE/fixtures/attestation-amd64.raw" 0 \
  '.subject[0].digest.sha256 = ("b" * 64)'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build mismapped statement subject" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case build_mismapped_attestation_subject
rewrite_raw "$TEST_STATE/fixtures/attestation-amd64.raw" \
  '.subject.digest = ("sha256:" + ("b" * 64))'
rebind_attestation "$TEST_STATE/fixtures/build-amd64.raw" 1 \
  "$TEST_STATE/fixtures/attestation-amd64.raw"
write_metadata
expect_failure "source build mismapped attestation subject" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case swapped_metadata_mapping
rewrite_pretty "$TEST_METADATA/linux-amd64.json" \
  '.runnable_digest = ("sha256:" + ("b" * 64))'
rewrite_pretty "$TEST_METADATA/linux-arm64.json" \
  '.runnable_digest = ("sha256:" + ("a" * 64))'
expect_failure "swapped metadata mappings" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

# Fail closed on metadata inventory, linkage, schema, and build claims.
setup_case metadata_extra
: > "$TEST_METADATA/.unexpected"
expect_failure "hidden metadata extra" run_stage

setup_case metadata_symlink
rm "$TEST_METADATA/linux-arm64.json"
ln -s linux-amd64.json "$TEST_METADATA/linux-arm64.json"
expect_failure "symlinked metadata" run_stage

for mutation in package image buildkit schema_key; do
  setup_case "metadata_$mutation"
  case "$mutation" in
    package)
      rewrite_pretty "$TEST_METADATA/linux-amd64.json" '.package = "other"'
      ;;
    image)
      rewrite_pretty "$TEST_METADATA/linux-amd64.json" '.image = "ghcr.io/other/project"'
      ;;
    buildkit)
      rewrite_pretty "$TEST_METADATA/linux-amd64.json" '.buildkit.sbom = false'
      ;;
    schema_key)
      rewrite_pretty "$TEST_METADATA/linux-amd64.json" '.unexpected = true'
      ;;
  esac
  expect_failure "$mutation metadata" run_stage
  assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"
done

setup_case metadata_noncanonical
jq -c . "$TEST_METADATA/linux-amd64.json" > "$TEST_METADATA/linux-amd64.tmp"
mv "$TEST_METADATA/linux-amd64.tmp" "$TEST_METADATA/linux-amd64.json"
expect_failure "noncanonical metadata" run_stage

setup_case output_not_empty
: > "$TEST_OUTPUT/stale"
expect_failure "nonempty output directory" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

# Only an exact Buildx absence permits a create, and all three digest views
# (raw, formatted, create metadata) must agree.
setup_case ambiguous_stage_absence
DOCKER_MODE=ambiguous_stage_absence
export DOCKER_MODE
expect_failure "ambiguous staging absence" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case noisy_stage_absence
DOCKER_MODE=noisy_stage_absence
export DOCKER_MODE
expect_failure "noisy staging absence" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case raw_formatted_mismatch
DOCKER_MODE=formatted_mismatch
MISMATCH_REFERENCE="$TEST_IMAGE@$(jq -r '.build_digest' \
  "$TEST_METADATA/linux-amd64.json")"
export DOCKER_MODE MISMATCH_REFERENCE
expect_failure "raw and formatted digest mismatch" run_stage
assert_no_target "$TEST_IMAGE:sha-$TEST_SOURCE_SHA"

setup_case bad_stage_create_result
DOCKER_MODE=create_stage_bad
export DOCKER_MODE
expect_failure "bad newly staged index" run_stage
test ! -e "$TEST_OUTPUT/image-manifest.json"
assert_no_target "$TEST_IMAGE:${TEST_TAG#v}"
assert_no_target "$TEST_IMAGE:latest"

setup_case bad_stage_create_metadata
DOCKER_MODE=create_metadata_bad
export DOCKER_MODE
expect_failure "bad stage create metadata" run_stage
test ! -e "$TEST_OUTPUT/image-manifest.json"

# Version publication verifies the exact staged record and trusted annotated
# tag/draft on both sides of the write, and never mutates latest.
prepare_version_case version_fresh
"$publish" version "$TEST_OUTPUT/image-manifest.json"
grep -Fq -- "-t $TEST_IMAGE:${TEST_TAG#v}" "$TEST_STATE/docker.log"
test "$(sha256_path "$TEST_STATE/version.raw")" = \
  "$(jq -r '.manifest_digest' "$TEST_OUTPUT/image-manifest.json")"
assert_no_target "$TEST_IMAGE:latest"
grep -Fq "git/ref/tags/$TEST_TAG" "$TEST_STATE/gh.log"
grep -Fq "releases/tags/$TEST_TAG" "$TEST_STATE/gh.log"

prepare_version_case version_existing
cp "$TEST_STATE/staging.raw" "$TEST_STATE/version.raw"
"$publish" version "$TEST_OUTPUT/image-manifest.json" > /dev/null
if grep -Fq -- "-t $TEST_IMAGE:${TEST_TAG#v}" "$TEST_STATE/docker.log"; then
  echo "Exact existing version image was replaced" >&2
  exit 1
fi

prepare_version_case version_existing_mismatch
cp "$TEST_STATE/fixtures/stage-swapped.raw" "$TEST_STATE/version.raw"
expect_failure "mismatched existing version" \
  "$publish" version "$TEST_OUTPUT/image-manifest.json"
if grep -Fq -- "-t $TEST_IMAGE:${TEST_TAG#v}" "$TEST_STATE/docker.log"; then
  echo "Mismatched existing version was overwritten" >&2
  exit 1
fi

prepare_version_case version_ambiguous_absence
DOCKER_MODE=ambiguous_version_absence
export DOCKER_MODE
expect_failure "ambiguous version absence" \
  "$publish" version "$TEST_OUTPUT/image-manifest.json"
assert_no_target "$TEST_IMAGE:${TEST_TAG#v}"

prepare_version_case record_mapping_mismatch
rewrite_compact "$TEST_OUTPUT/image-manifest.json" \
  '.platforms[0].runnable_digest = ("sha256:" + ("b" * 64))'
expect_failure "record mapping mismatch" \
  "$publish" version "$TEST_OUTPUT/image-manifest.json"
assert_no_target "$TEST_IMAGE:${TEST_TAG#v}"

prepare_version_case staging_digest_moved
cp "$TEST_STATE/fixtures/stage-alt.raw" "$TEST_STATE/staging.raw"
expect_failure "staging digest moved" \
  "$publish" version "$TEST_OUTPUT/image-manifest.json"
assert_no_target "$TEST_IMAGE:${TEST_TAG#v}"

prepare_version_case symlinked_record
ln -s "$TEST_OUTPUT/image-manifest.json" "$TEST_STATE/linked-record.json"
expect_failure "symlinked release record" \
  "$publish" version "$TEST_STATE/linked-record.json"
assert_no_target "$TEST_IMAGE:${TEST_TAG#v}"

for gh_mode in tag_moved draft_public draft_foreign draft_wrong_name \
  draft_wrong_body draft_malformed; do
  prepare_version_case "version_$gh_mode"
  GH_MODE=$gh_mode
  export GH_MODE
  expect_failure "$gh_mode trusted draft" \
    "$publish" version "$TEST_OUTPUT/image-manifest.json"
  assert_no_target "$TEST_IMAGE:${TEST_TAG#v}"
done

prepare_version_case version_replaced_draft
GH_MODE=draft_replaced
export GH_MODE
expect_failure "draft replaced around version write" \
  "$publish" version "$TEST_OUTPUT/image-manifest.json"
assert_no_target "$TEST_IMAGE:latest"

prepare_version_case bad_new_version
DOCKER_MODE=create_version_bad
export DOCKER_MODE
expect_failure "bad new version image" \
  "$publish" version "$TEST_OUTPUT/image-manifest.json"
assert_no_target "$TEST_IMAGE:latest"

prepare_version_case version_create_failure
DOCKER_MODE=create_version_fail
export DOCKER_MODE
expect_failure "version create failure" \
  "$publish" version "$TEST_OUTPUT/image-manifest.json"

prepare_version_case version_create_metadata_mismatch
DOCKER_MODE=create_metadata_bad
export DOCKER_MODE
expect_failure "version create metadata mismatch" \
  "$publish" version "$TEST_OUTPUT/image-manifest.json"
assert_no_target "$TEST_IMAGE:latest"

# Latest is a separate public-release convergence operation. It only copies an
# existing exact version index by immutable digest and preserves race retries.
setup_case latest_current
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
"$publish" latest
test "$(sha256_path "$TEST_STATE/latest.raw")" = \
  "$(sha256_path "$TEST_STATE/version.raw")"
assert_no_target "$TEST_IMAGE:${TEST_TAG#v}"
test "$(grep -Fc 'attestation verify oci://ghcr.io/test/project@' \
  "$TEST_STATE/gh.log")" -eq 2
grep -Fq -- \
  "--signer-workflow test/project/.github/workflows/release-binaries.yml --source-digest $TEST_SOURCE_SHA --source-ref refs/tags/$TEST_TAG --deny-self-hosted-runners" \
  "$TEST_STATE/gh.log"
if [[ $(grep -Fc "git/ref/tags/$TEST_TAG" "$TEST_STATE/gh.log") -lt 2 ]]; then
  echo "Latest publication did not re-authenticate its live annotated tag" >&2
  exit 1
fi

setup_case latest_newer
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
cp "$TEST_STATE/fixtures/stage-next.raw" "$TEST_STATE/version-next.raw"
GH_MODE=latest_newer
export GH_MODE
"$publish" latest > /dev/null
test "$(sha256_path "$TEST_STATE/latest.raw")" = \
  "$(sha256_path "$TEST_STATE/version-next.raw")"

setup_case latest_race
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
cp "$TEST_STATE/fixtures/stage-next.raw" "$TEST_STATE/version-next.raw"
GH_MODE=latest_race
export GH_MODE
"$publish" latest > /dev/null
test "$(sha256_path "$TEST_STATE/latest.raw")" = \
  "$(sha256_path "$TEST_STATE/version-next.raw")"
create_count=$(grep -Fc -- "-t $TEST_IMAGE:latest" "$TEST_STATE/docker.log")
if [[ "$create_count" -lt 2 ]]; then
  echo "Latest release race did not exercise convergence writes" >&2
  exit 1
fi

setup_case latest_unstable
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
cp "$TEST_STATE/fixtures/stage-next.raw" "$TEST_STATE/version-next.raw"
GH_MODE=latest_unstable
export GH_MODE
expect_failure "unstable latest release" "$publish" latest

setup_case latest_missing_version
expect_failure "missing latest version image" "$publish" latest
assert_no_target "$TEST_IMAGE:latest"

setup_case latest_moved_version
cp "$TEST_STATE/fixtures/stage-next.raw" "$TEST_STATE/version.raw"
expect_failure "moved latest version image" "$publish" latest
assert_no_target "$TEST_IMAGE:latest"

setup_case latest_record_digest_mismatch
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
rewrite_compact "$TEST_STATE/mcp-repl-$TEST_TAG-release.json" \
  '.container.manifest_digest = ("sha256:" + ("f" * 64))'
expect_failure "latest release record digest mismatch" "$publish" latest
assert_no_target "$TEST_IMAGE:latest"

setup_case latest_invalid_record
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
rewrite_compact "$TEST_STATE/mcp-repl-$TEST_TAG-release.json" 'del(.container.platforms)'
expect_failure "invalid latest release record" "$publish" latest
assert_no_target "$TEST_IMAGE:latest"

setup_case latest_missing_record
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
GH_MODE=latest_missing_record
export GH_MODE
expect_failure "missing latest release record" "$publish" latest
assert_no_target "$TEST_IMAGE:latest"

setup_case latest_bad_record_metadata
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
GH_MODE=latest_bad_record_metadata
export GH_MODE
expect_failure "bad latest release record metadata" "$publish" latest
assert_no_target "$TEST_IMAGE:latest"

setup_case latest_bad_bundle_metadata
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
GH_MODE=latest_bad_bundle_metadata
export GH_MODE
expect_failure "bad latest bundle metadata" "$publish" latest
assert_no_target "$TEST_IMAGE:latest"

# A bot-owned immutable release is still untrusted when its record or signed
# bundles were forged. Even internally consistent asset metadata cannot replace
# the workflow/source/ref policy enforced by Sigstore verification.
setup_case latest_forged_bot_record
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
rewrite_compact "$TEST_STATE/mcp-repl-$TEST_TAG-release.json" \
  ".source_sha = \"$TEST_OTHER_SOURCE_SHA\""
expect_failure "forged bot-owned release record" "$publish" latest
assert_no_target "$TEST_IMAGE:latest"

setup_case latest_forged_bot_bundle
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
rewrite_compact \
  "$TEST_STATE/mcp-repl-$TEST_TAG-container.provenance.sigstore.json" \
  '.fixture = "forged-provenance"'
refresh_provenance_identity "$TEST_TAG"
expect_failure "forged bot-owned release bundle" "$publish" latest
assert_no_target "$TEST_IMAGE:latest"

setup_case latest_tag_mismatch
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
GH_MODE=tag_moved
export GH_MODE
expect_failure "latest live annotated tag mismatch" "$publish" latest
assert_no_target "$TEST_IMAGE:latest"

for gh_mode in latest_foreign latest_mutable latest_draft \
  latest_wrong_name latest_malformed; do
  setup_case "$gh_mode"
  cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
  GH_MODE=$gh_mode
  export GH_MODE
  expect_failure "$gh_mode release" "$publish" latest
  assert_no_target "$TEST_IMAGE:latest"
done

setup_case latest_create_failure
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
DOCKER_MODE=create_latest_fail
export DOCKER_MODE
expect_failure "latest create failure" "$publish" latest

setup_case latest_bad_result
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
DOCKER_MODE=create_latest_bad
export DOCKER_MODE
expect_failure "bad latest result" "$publish" latest

setup_case latest_metadata_mismatch
cp "$TEST_STATE/fixtures/stage.raw" "$TEST_STATE/version.raw"
DOCKER_MODE=create_metadata_bad
export DOCKER_MODE
expect_failure "latest create metadata mismatch" "$publish" latest

echo "container manifest behavior tests passed"
