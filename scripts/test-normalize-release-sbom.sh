#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
normalizer="$root/scripts/normalize-release-sbom.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM

tag=v1.2.3
archive=mcp-repl-v1.2.3-x86_64-unknown-linux-gnu.tar.gz
digest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
source_sha=0123456789abcdef0123456789abcdef01234567
epoch=1700000001

fail() {
  echo "release SBOM test failed: $*" >&2
  exit 1
}

seed() {
  local output=$1 created=$2 namespace=$3 reverse=$4
  jq -n \
    --arg created "$created" \
    --arg namespace "$namespace" \
    --argjson reverse "$reverse" '
      {
        spdxVersion: "SPDX-2.3",
        dataLicense: "CC0-1.0",
        SPDXID: "SPDXRef-DOCUMENT",
        name: "nondeterministic source",
        documentNamespace: $namespace,
        creationInfo: {
          created: $created,
          creators: ["Tool: syft-1.51.0", "Organization: Anchore, Inc"]
        },
        documentDescribes: ["SPDXRef-Package-z", "SPDXRef-Package-a"],
        packages: [
          {
            SPDXID: "SPDXRef-Package-z",
            name: "dependency",
            checksums: [
              {algorithm: "SHA256", checksumValue: ("b" * 64)},
              {algorithm: "SHA1", checksumValue: ("c" * 40)}
            ]
          },
          {SPDXID: "SPDXRef-Package-a", name: "mcp-repl"}
        ],
        relationships: [
          {
            spdxElementId: "SPDXRef-DOCUMENT",
            relationshipType: "DESCRIBES",
            relatedSpdxElement: "SPDXRef-Package-a"
          },
          {
            spdxElementId: "SPDXRef-Package-a",
            relationshipType: "DEPENDS_ON",
            relatedSpdxElement: "SPDXRef-Package-z"
          }
        ]
      } |
      if $reverse then
        .packages |= reverse |
        .relationships |= reverse |
        .documentDescribes |= reverse |
        .creationInfo.creators |= reverse |
        (.packages[] | select(has("checksums")) | .checksums) |= reverse
      else . end
    ' > "$output"
}

expect_failure() {
  local label=$1 diagnostic=$2
  shift 2
  if result=$("$normalizer" "$@" 2>&1); then
    fail "$label unexpectedly succeeded: $result"
  fi
  [[ "$result" == *"$diagnostic"* ]] ||
    fail "$label did not report '$diagnostic': $result"
}

first_raw="$work/first.raw.json"
second_raw="$work/second.raw.json"
first="$work/first.spdx.json"
second="$work/second.spdx.json"
seed "$first_raw" '2026-08-21T01:02:03Z' 'https://anchore.invalid/uuid-one' false
seed "$second_raw" '2026-08-22T04:05:06Z' 'https://anchore.invalid/uuid-two' true
"$normalizer" "$first_raw" "$first" "$archive" "$digest" "$source_sha" "$epoch" Test/Project "$tag" > /dev/null
"$normalizer" "$second_raw" "$second" "$archive" "$digest" "$source_sha" "$epoch" test/project "$tag" > /dev/null
cmp -s "$first" "$second" || fail "wall clock, UUID, or array order changed output bytes"
canonical="$work/canonical.json"
jq -S . "$first" > "$canonical"
cmp -s "$first" "$canonical" || fail "normalized SPDX is not recursively canonical JSON"
jq -e \
  --arg name "$archive" \
  --arg namespace "https://github.com/test/project/releases/download/$tag/$archive.spdx.json#$source_sha-$digest" '
    .name == $name and
    .documentNamespace == $namespace and
    .creationInfo.created == "2023-11-14T22:13:21Z" and
    any(.packages[]; .name == "mcp-repl")
  ' "$first" > /dev/null || fail "normalized SPDX identity is wrong"

stale="$work/stale.json"
printf 'sentinel\n' > "$stale"
expect_failure stale "output must be a new path" \
  "$first_raw" "$stale" "$archive" "$digest" "$source_sha" "$epoch" test/project "$tag"
[[ $(<"$stale") == sentinel ]] || fail "stale output was modified"

linked="$work/linked.json"
ln -s "$first_raw" "$linked"
expect_failure linked "input must be regular" \
  "$linked" "$work/linked-output" "$archive" "$digest" "$source_sha" "$epoch" test/project "$tag"

bad="$work/bad.json"
jq 'del(.packages)' "$first_raw" > "$bad"
expect_failure incomplete "complete Syft SPDX" \
  "$bad" "$work/bad-output" "$archive" "$digest" "$source_sha" "$epoch" test/project "$tag"
expect_failure bad_digest "identity arguments are invalid" \
  "$first_raw" "$work/bad-digest" "$archive" short "$source_sha" "$epoch" test/project "$tag"
expect_failure bad_source_sha "identity arguments are invalid" \
  "$first_raw" "$work/bad-source" "$archive" "$digest" short "$epoch" test/project "$tag"
expect_failure mismatched_tag "identity arguments are invalid" \
  "$first_raw" "$work/bad-tag" "$archive" "$digest" "$source_sha" "$epoch" test/project v9.9.9
expect_failure unsafe_repo "identity arguments are invalid" \
  "$first_raw" "$work/bad-repo" "$archive" "$digest" "$source_sha" "$epoch" '../project' "$tag"

mkdir "$work/failing-bin"
cat > "$work/failing-bin/cmp" <<'STUB'
#!/usr/bin/env bash
exit 1
STUB
chmod 755 "$work/failing-bin/cmp"
corrupt_output="$work/corrupt-output"
if result=$(PATH="$work/failing-bin:$PATH" "$normalizer" \
    "$first_raw" "$corrupt_output" "$archive" "$digest" "$source_sha" \
    "$epoch" test/project "$tag" 2>&1); then
  fail "failed installation comparison unexpectedly succeeded: $result"
fi
[[ "$result" == *"canonical SPDX installation failed"* ]] ||
  fail "failed installation comparison reported the wrong error: $result"
[[ ! -e "$corrupt_output" ]] ||
  fail "failed installation comparison left a partial output"

echo "release SBOM normalization behavior tests passed"
