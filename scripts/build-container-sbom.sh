#!/usr/bin/env bash
# Build a deterministic SPDX document for the published multi-platform index.
# Detailed component inventories remain embedded as BuildKit SBOM attestations
# on each runnable platform; this document binds those platforms to the final
# index that GitHub attests and the release record names.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <image-manifest.json> <output.spdx.json>" >&2
  exit 2
fi

input=$1
output=$2
root=$(cd "$(dirname "$0")/.." && pwd)
targets="$root/scripts/release-targets.sh"

fail() {
  echo "container SBOM: $*" >&2
  exit 1
}

if [[ ! -f "$input" || -L "$input" || ! -s "$input" ]]; then
  fail "input must be a nonempty regular image manifest"
fi
if [[ -e "$output" || -L "$output" ]]; then
  fail "output already exists: $output"
fi
case "$output" in
  */*) output_dir=${output%/*}; [[ -n "$output_dir" ]] || output_dir=/ ;;
  *) output_dir=. ;;
esac
if [[ ! -d "$output_dir" || -L "$output_dir" ]]; then
  fail "output directory must be a regular directory: $output_dir"
fi

"$targets" validate
expected_platforms=$("$targets" container-platforms | jq -Rsc '
  split("\n") | map(select(length > 0))
')

if ! identity=$(jq -cer --argjson expected "$expected_platforms" '
  select(type == "object") |
  select((keys | sort) ==
    (["schema_version", "package", "tag", "source_sha", "source_epoch", "image",
      "staging_ref", "manifest_digest", "platforms"] | sort)) |
  select(.schema_version == 1) |
  select(.package == "mcp-repl") |
  select(.tag | type == "string" and
    test("^v(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)\\.(0|[1-9][0-9]*)$")) |
  select(.source_sha | type == "string" and test("^[0-9a-f]{40}$")) |
  select(.source_epoch | type == "number" and floor == . and
    . >= 315532800 and . <= 2147483647) |
  select(.image | type == "string" and
    test("^ghcr\\.io/[a-z0-9_.-]+/[a-z0-9_.-]+$")) |
  select(.staging_ref == (.image + ":sha-" + .source_sha)) |
  select(.manifest_digest | type == "string" and
    test("^sha256:[0-9a-f]{64}$")) |
  select(.platforms | type == "array" and length == ($expected | length)) |
  select([.platforms[].platform] == $expected) |
  select([.platforms[].platform] | unique | length == ($expected | length)) |
  select(all(.platforms[];
    type == "object" and
    ((keys | sort) ==
      (["platform", "build_digest", "runnable_digest"] | sort)) and
    (.build_digest | type == "string" and test("^sha256:[0-9a-f]{64}$")) and
    (.runnable_digest | type == "string" and test("^sha256:[0-9a-f]{64}$")))) |
  select([.platforms[].runnable_digest] | unique | length == ($expected | length)) |
  {
    package,
    tag,
    source_sha,
    source_epoch,
    image,
    manifest_digest,
    platforms
  }
' "$input"); then
  fail "image manifest does not match the strict release schema"
fi

tag=$(jq -r '.tag' <<<"$identity")
source_sha=$(jq -r '.source_sha' <<<"$identity")
source_epoch=$(jq -r '.source_epoch' <<<"$identity")
image=$(jq -r '.image' <<<"$identity")
manifest_digest=$(jq -r '.manifest_digest' <<<"$identity")
created=$(jq -nr --argjson epoch "$source_epoch" '$epoch | todateiso8601')
repository=${image#ghcr.io/}
manifest_hex=${manifest_digest#sha256:}
namespace="https://github.com/$repository/releases/download/$tag/mcp-repl-$tag-container.spdx.json#$source_sha-$manifest_hex"

temporary=$(mktemp "$output_dir/.container-sbom.XXXXXX")
output_created=false
cleanup() {
  local status=$?
  rm -f "$temporary"
  if [[ $status -ne 0 && "$output_created" == true ]]; then
    rm -f "$output"
  fi
  return "$status"
}
trap cleanup EXIT INT TERM

jq -nS \
  --arg tag "$tag" \
  --arg version "${tag#v}" \
  --arg image "$image" \
  --arg manifest_digest "$manifest_digest" \
  --arg namespace "$namespace" \
  --arg created "$created" \
  --argjson platforms "$(jq -c '.platforms' <<<"$identity")" '
  def checksum($digest):
    [{algorithm: "SHA256", checksumValue: ($digest | sub("^sha256:"; ""))}];
  def platform_id($platform):
    "SPDXRef-Platform-" + ($platform | gsub("/"; "-"));
  {
    spdxVersion: "SPDX-2.3",
    dataLicense: "CC0-1.0",
    SPDXID: "SPDXRef-DOCUMENT",
    name: ($image + ":" + $version),
    documentNamespace: $namespace,
    creationInfo: {
      created: $created,
      creators: ["Tool: mcp-repl release pipeline"]
    },
    documentDescribes: ["SPDXRef-ImageIndex"],
    packages: ([{
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
    }]),
    relationships: ([{
      spdxElementId: "SPDXRef-DOCUMENT",
      relationshipType: "DESCRIBES",
      relatedSpdxElement: "SPDXRef-ImageIndex"
    }] + [$platforms[] | {
      spdxElementId: "SPDXRef-ImageIndex",
      relationshipType: "CONTAINS",
      relatedSpdxElement: platform_id(.platform)
    }]),
    annotations: [{
      annotationDate: $created,
      annotationType: "OTHER",
      annotator: "Tool: mcp-repl release pipeline",
      comment: "This deterministic index SBOM supplements the detailed BuildKit SPDX and SLSA attestations stored with the OCI image."
    }]
  }
' > "$temporary"

if [[ ! -s "$temporary" ]] || ! jq -e '
    .spdxVersion == "SPDX-2.3" and
    (.packages | length) == 3 and
    (.relationships | length) == 3
  ' "$temporary" > /dev/null; then
  fail "generated SPDX document failed its structural check"
fi
if ! (
  created=false
  # Invoked by the EXIT trap below.
  # shellcheck disable=SC2329
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
  cat "$temporary" >&9
) 2> /dev/null; then
  fail "output appeared while generating the container SBOM: $output"
fi
output_created=true
if [[ ! -f "$output" || -L "$output" ]] || ! cmp -s "$temporary" "$output"; then
  fail "generated container SBOM installation failed"
fi
printf '%s\n' "$output"
