#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
builder="$root/scripts/build-container-sbom.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

tag=v1.2.3
sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
epoch=1700000001
manifest_digest=sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb

fail() {
  echo "container SBOM test failed: $*" >&2
  exit 1
}

seed_manifest() {
  local output=$1
  jq -nS \
    --arg tag "$tag" \
    --arg sha "$sha" \
    --argjson epoch "$epoch" \
    --arg manifest_digest "$manifest_digest" '
    {
      schema_version: 1,
      package: "mcp-repl",
      tag: $tag,
      source_sha: $sha,
      source_epoch: $epoch,
      image: "ghcr.io/test/project",
      staging_ref: ("ghcr.io/test/project:sha-" + $sha),
      manifest_digest: $manifest_digest,
      platforms: [
        {
          platform: "linux/amd64",
          build_digest: "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
          runnable_digest: "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
        },
        {
          platform: "linux/arm64",
          build_digest: "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
          runnable_digest: "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
        }
      ]
    }
  ' > "$output"
}

expect_failure() {
  local label=$1 diagnostic=$2 input=$3 output=$4
  if result=$("$builder" "$input" "$output" 2>&1); then
    fail "$label unexpectedly succeeded: $result"
  fi
  [[ "$result" == *"$diagnostic"* ]] ||
    fail "$label did not report '$diagnostic': $result"
}

input="$work/image-manifest.json"
first="$work/first.spdx.json"
second="$work/second.spdx.json"
seed_manifest "$input"
[[ $("$builder" "$input" "$first") == "$first" ]] || fail "wrong output path"
"$builder" "$input" "$second" > /dev/null
cmp -s "$first" "$second" || fail "same image manifest produced different SBOM bytes"
jq -e \
  --arg namespace "https://github.com/test/project/releases/download/$tag/mcp-repl-$tag-container.spdx.json#$sha-${manifest_digest#sha256:}" \
  --arg image "ghcr.io/test/project" \
  --arg manifest_digest "$manifest_digest" '
    .spdxVersion == "SPDX-2.3" and
    .documentNamespace == $namespace and
    .creationInfo.created == "2023-11-14T22:13:21Z" and
    .documentDescribes == ["SPDXRef-ImageIndex"] and
    ([.packages[].SPDXID] | sort) ==
      (["SPDXRef-ImageIndex", "SPDXRef-Platform-linux-amd64",
        "SPDXRef-Platform-linux-arm64"] | sort) and
    all(.packages[]; .downloadLocation == "NOASSERTION") and
    ([.packages[] |
      select(.SPDXID == "SPDXRef-ImageIndex") |
      .comment] == [
        "Immutable OCI index " + $image + "@" + $manifest_digest +
        "; the complete per-platform component inventories and build provenance are embedded OCI BuildKit attestations committed by this index."
      ]) and
    all(.packages[] | select(.SPDXID | startswith("SPDXRef-Platform-"));
      (.comment | startswith("Immutable OCI platform manifest " + $image + "@sha256:"))) and
    ([.relationships[] | select(.relationshipType == "CONTAINS")] | length) == 2
  ' "$first" > /dev/null || fail "generated SPDX identity is incomplete"

stale="$work/stale.spdx.json"
printf 'sentinel\n' > "$stale"
expect_failure "stale output" "output already exists" "$input" "$stale"
[[ $(<"$stale") == sentinel ]] || fail "stale output was changed"

linked_input="$work/linked-input.json"
ln -s "$input" "$linked_input"
expect_failure "linked input" "nonempty regular" "$linked_input" "$work/linked.spdx.json"

linked_output="$work/linked-output.json"
ln -s "$work/absent" "$linked_output"
expect_failure "linked output" "output already exists" "$input" "$linked_output"
[[ ! -e "$work/absent" ]] || fail "builder wrote through output symlink"

directory_output="$work/directory-output.json"
mkdir "$directory_output"
expect_failure "directory output" "output already exists" "$input" "$directory_output"
[[ -d "$directory_output" && ! -L "$directory_output" ]] ||
  fail "directory output was changed"

mutate_and_fail() {
  local label=$1 filter=$2
  local bad="$work/$label.json"
  jq "$filter" "$input" > "$bad"
  expect_failure "$label" "strict release schema" "$bad" "$work/$label.spdx.json"
}

mutate_and_fail bad_tag '.tag = "../v1.2.3"'
mutate_and_fail bad_package '.package = "other"'
mutate_and_fail bad_sha '.source_sha = "short"'
mutate_and_fail bad_epoch '.source_epoch = 0'
mutate_and_fail bad_image '.image = "docker.io/test/project"'
mutate_and_fail bad_staging '.staging_ref = "ghcr.io/test/project:latest"'
mutate_and_fail bad_manifest_digest '.manifest_digest = "sha256:short"'
mutate_and_fail missing_platform '.platforms = .platforms[:-1]'
mutate_and_fail reversed_platforms '.platforms |= reverse'
mutate_and_fail duplicate_runnable '.platforms[1].runnable_digest = .platforms[0].runnable_digest'
mutate_and_fail extra_key '.platforms[0].unexpected = true'

echo "container SPDX behavior tests passed"
