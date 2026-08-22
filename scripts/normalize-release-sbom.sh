#!/usr/bin/env bash
# Remove Syft's wall-clock and UUID entropy and emit canonical SPDX JSON for a
# native release archive. SPDX collection arrays are sets, so recursively
# sorting them preserves meaning while making equivalent inventories byte-stable.
set -euo pipefail

if [[ $# -ne 8 ]]; then
  echo "usage: $0 <input> <output> <archive-name> <archive-sha256> <source-sha> <source-epoch> <owner/repo> <vX.Y.Z>" >&2
  exit 2
fi

input=$1
output=$2
archive_name=$3
archive_sha256=$4
source_sha=$5
source_epoch=$6
repository=$7
tag=$8

fail() {
  echo "release-sbom: $*" >&2
  exit 1
}

if [[ ! -f "$input" || -L "$input" || ! -s "$input" ||
      -z "$output" || -e "$output" || -L "$output" ]]; then
  echo "release-sbom: input must be regular and output must be a new path" >&2
  exit 2
fi
if [[ ! "$archive_name" =~ ^mcp-repl-v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)-[A-Za-z0-9_.-]+\.(tar\.gz|zip)$ ||
      ! "$archive_sha256" =~ ^[0-9a-f]{64}$ ||
      ! "$source_sha" =~ ^[0-9a-f]{40}$ ||
      ! "$source_epoch" =~ ^(0|[1-9][0-9]{0,9})$ ||
      ! "$repository" =~ ^[A-Za-z0-9][A-Za-z0-9_.-]*/[A-Za-z0-9][A-Za-z0-9_.-]*$ ||
      ! "$tag" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ||
      "$archive_name" != mcp-repl-"$tag"-* ]] ||
    ((source_epoch < 315532800 || source_epoch > 2147483647)); then
  echo "release-sbom: release identity arguments are invalid" >&2
  exit 2
fi
repository=$(printf '%s' "$repository" | tr '[:upper:]' '[:lower:]')

output_parent=$(dirname "$output")
output_name=$(basename "$output")
if [[ ! -d "$output_parent" || -L "$output_parent" ||
      ! "$output_name" =~ ^[A-Za-z0-9][A-Za-z0-9._+-]*$ ]]; then
  echo "release-sbom: output parent or filename is unsafe" >&2
  exit 2
fi
output_parent=$(cd "$output_parent" && pwd -P)
output="$output_parent/$output_name"

created=$(jq -nr --argjson epoch "$source_epoch" '$epoch | todateiso8601') ||
  fail "could not convert source epoch"
namespace="https://github.com/$repository/releases/download/$tag/$archive_name.spdx.json#$source_sha-$archive_sha256"
temporary=$(mktemp "$output_parent/.release-sbom.XXXXXX")
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

if ! jq -eS \
    --arg name "$archive_name" \
    --arg created "$created" \
    --arg namespace "$namespace" '
      def canonical:
        if type == "object" then with_entries(.value |= canonical)
        elif type == "array" then map(canonical) | sort_by(tojson)
        else .
        end;
      select(type == "object") |
      select(.spdxVersion == "SPDX-2.3") |
      select(.dataLicense == "CC0-1.0") |
      select(.SPDXID == "SPDXRef-DOCUMENT") |
      select((.creationInfo | type) == "object") |
      select((.creationInfo.creators | type) == "array" and
        (.creationInfo.creators | length) > 0) |
      select((.packages | type) == "array" and (.packages | length) > 1) |
      select(all(.packages[];
        type == "object" and
        (.SPDXID | type) == "string" and
        (.name | type) == "string")) |
      select(any(.packages[]; .name == "mcp-repl")) |
      select((.relationships | type) == "array") |
      .name = $name |
      .documentNamespace = $namespace |
      .creationInfo.created = $created |
      canonical
    ' "$input" > "$temporary"; then
  fail "input is not a complete Syft SPDX 2.3 inventory"
fi
if [[ ! -s "$temporary" ]] || ! jq -e \
    --arg name "$archive_name" \
    --arg created "$created" \
    --arg namespace "$namespace" '
      .name == $name and
      .creationInfo.created == $created and
      .documentNamespace == $namespace and
      any(.packages[]; .name == "mcp-repl")
    ' "$temporary" > /dev/null; then
  fail "canonical SPDX generation failed"
fi

if ! (
  created_output=false
  # Invoked by the EXIT trap below.
  # shellcheck disable=SC2329
  cleanup_output_copy() {
    local status=$?
    if [[ $status -ne 0 && "$created_output" == true ]]; then
      rm -f "$output"
    fi
    exit "$status"
  }
  trap cleanup_output_copy EXIT
  set -o noclobber
  exec 9> "$output"
  created_output=true
  cat "$temporary" >&9
) 2> /dev/null; then
  fail "output appeared while normalizing the SPDX document"
fi
output_created=true
if [[ ! -f "$output" || -L "$output" ]] || ! cmp -s "$temporary" "$output"; then
  fail "canonical SPDX installation failed"
fi
printf '%s\n' "$output"
