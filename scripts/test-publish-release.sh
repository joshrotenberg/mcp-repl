#!/usr/bin/env bash
# Exercise the GitHub release publisher's local, retry, and visibility guards.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
publisher="$root/scripts/publish-release.sh"
release_targets="$root/scripts/release-targets.sh"
version=$(awk '
  /^\[package\][[:space:]]*$/ { in_package = 1; next }
  /^\[/ { in_package = 0 }
  in_package && /^[[:space:]]*version[[:space:]]*=/ {
    if (match($0, /"[^"]+"/)) print substr($0, RSTART + 1, RLENGTH - 2)
  }
' "$root/Cargo.toml")
tag="v$version"
source_sha=$(git -C "$root" rev-parse --verify HEAD)
source_epoch=$(git -C "$root" show -s --format=%ct HEAD)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
dist="$work/dist"
remote="$work/remote"
state="$work/release-state"
log="$work/gh.log"
tag_moved="$work/tag-moved"
expected_notes="$work/expected-notes.md"
"$root/scripts/extract-release-notes.sh" "$version" > "$expected_notes"

fail() {
  echo "release publisher test failed: $*" >&2
  exit 1
}

sha256_file() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

identity() {
  local path=$1 digest size
  digest=$(sha256_file "$path")
  size=$(wc -c < "$path" | tr -d '[:space:]')
  jq -cn \
    --arg name "${path##*/}" \
    --arg sha256 "$digest" \
    --argjson size "$size" \
    '{name: $name, sha256: $sha256, size: $size}'
}

write_spdx() {
  local path=$1 label=$2
  jq -cn --arg label "$label" '{
    spdxVersion: "SPDX-2.3",
    SPDXID: "SPDXRef-DOCUMENT",
    dataLicense: "CC0-1.0",
    name: $label,
    documentNamespace: ("https://example.invalid/spdx/" + ($label | @uri)),
    creationInfo: {creators: ["Tool: test-publish-release"]},
    packages: []
  }' > "$path"
}

write_bundle() {
  local path=$1 label=$2
  jq -cn --arg label "$label" '{
    mediaType: "application/vnd.dev.sigstore.bundle.v0.3+json",
    verificationMaterial: {fixture: $label},
    dsseEnvelope: {payloadType: $label, payload: "e30=", signatures: []}
  }' > "$path"
}

seed_dist() {
  rm -rf "$dist"
  mkdir -p "$dist"
  local native_rows="$work/native-records.jsonl"
  local platforms_file="$work/platforms.jsonl"
  local row target extension binary archive digest
  local archive_id checksum_id sbom_id provenance_id sbom_bundle_id
  : > "$native_rows"
  : > "$platforms_file"

  while IFS= read -r row; do
    IFS=$'\t' read -r target extension binary <<<"$row"
    archive="$dist/mcp-repl-$tag-$target.$extension"
    printf 'release archive for %s\n' "$target" > "$archive"
    digest=$(sha256_file "$archive")
    printf '%s  %s\n' "$digest" "${archive##*/}" > "$archive.sha256"
    write_spdx "$archive.spdx.json" "$target SPDX"
    write_bundle "$archive.provenance.sigstore.json" "$target provenance"
    write_bundle "$archive.sbom.sigstore.json" "$target SBOM attestation"

    archive_id=$(identity "$archive")
    checksum_id=$(identity "$archive.sha256")
    sbom_id=$(identity "$archive.spdx.json")
    provenance_id=$(identity "$archive.provenance.sigstore.json")
    sbom_bundle_id=$(identity "$archive.sbom.sigstore.json")
    jq -cn \
      --arg target "$target" \
      --arg binary "$binary" \
      --argjson archive "$archive_id" \
      --argjson checksum "$checksum_id" \
      --argjson sbom "$sbom_id" \
      --argjson provenance "$provenance_id" \
      --argjson sbom_bundle "$sbom_bundle_id" '{
        target: $target,
        binary: $binary,
        archive: $archive,
        checksum: $checksum,
        sbom: $sbom,
        attestations: {provenance: $provenance, sbom: $sbom_bundle}
      }' >> "$native_rows"
  done < <("$release_targets" rows)

  container_base="$dist/mcp-repl-$tag-container"
  write_spdx "$container_base.spdx.json" "container SPDX"
  write_bundle "$container_base.provenance.sigstore.json" "container provenance"
  write_bundle "$container_base.sbom.sigstore.json" "container SBOM attestation"
  container_sbom=$(identity "$container_base.spdx.json")
  container_provenance=$(identity "$container_base.provenance.sigstore.json")
  container_sbom_bundle=$(identity "$container_base.sbom.sigstore.json")

  platform_index=0
  while IFS= read -r platform; do
    platform_index=$((platform_index + 1))
    if [[ "$platform_index" -eq 1 ]]; then
      runnable_digest="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    else
      runnable_digest="sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    fi
    jq -cn \
      --arg platform "$platform" \
      --arg runnable_digest "$runnable_digest" \
      '{platform: $platform, runnable_digest: $runnable_digest}' \
      >> "$platforms_file"
  done < <("$release_targets" container-platforms)

  native=$(jq -cs 'sort_by(.target)' "$native_rows")
  platforms=$(jq -cs 'sort_by(.platform)' "$platforms_file")
  release_targets_id=$(identity "$root/release-targets.json")
  jq -S -c -n \
    --arg package mcp-repl \
    --arg tag "$tag" \
    --arg version "$version" \
    --arg source_sha "$source_sha" \
    --argjson source_epoch "$source_epoch" \
    --argjson release_targets "$release_targets_id" \
    --argjson native "$native" \
    --arg image ghcr.io/test/project \
    --arg manifest_digest "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc" \
    --argjson platforms "$platforms" \
    --argjson container_sbom "$container_sbom" \
    --argjson container_provenance "$container_provenance" \
    --argjson container_sbom_bundle "$container_sbom_bundle" '{
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
    }' > "$dist/mcp-repl-$tag-release.json"

  expected=$("$release_targets" expected-release-assets "$tag" | LC_ALL=C sort)
  actual=$(find "$dist" -mindepth 1 -maxdepth 1 -exec basename {} \; | LC_ALL=C sort)
  [[ "$actual" == "$expected" ]] || fail "fixture did not create the authoritative asset set"
}

reset_remote() {
  local release_state=$1
  rm -rf "$remote"
  mkdir -p "$remote"
  printf '%s\n' "$release_state" > "$state"
  : > "$log"
  rm -f "$tag_moved"
}

copy_exact_remote() {
  cp "$dist"/* "$remote/"
}

mkdir -p "$work/bin"
# The publisher invokes the real verification helper, so this stub models the
# small GitHub surface used by both scripts and persists release visibility and
# uploaded bytes across each invocation.
cat > "$work/bin/gh" <<'STUB'
#!/bin/sh
set -eu
repository=test/project
tag=${TEST_TAG:?}
source_sha=${TEST_SOURCE_SHA:?}
release_id=4242
tag_object_sha=dddddddddddddddddddddddddddddddddddddddd
tag_source_sha=$source_sha
if [ -e "${TEST_TAG_MOVED:?}" ]; then
  tag_object_sha=eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee
  tag_source_sha=ffffffffffffffffffffffffffffffffffffffff
fi
release_state=$(cat "${TEST_STATE:?}")

case "${1:-} ${2:-}" in
  "api graphql")
    if [ "${GH_MODE:-normal}" = lookup_failure ]; then
      echo "lookup failed" >&2
      exit 1
    fi
    if [ "$release_state" = absent ]; then
      printf '{"data":{"repository":{"release":null}}}\n'
    else
      printf '{"data":{"repository":{"release":{"id":"R_test"}}}}\n'
    fi
    ;;

  "api repos/$repository/releases/tags/$tag")
    [ "$release_state" != absent ] || {
      echo "release not found" >&2
      exit 1
    }
    body=$(jq -Rs . < "${TEST_EXPECTED_NOTES:?}")
    if [ "$release_state" = draft ]; then
      draft=true
      immutable=false
    else
      draft=false
      immutable=true
      [ "${GH_MODE:-normal}" != published_mutable ] || immutable=false
    fi
    printf '{"id":%s,"tag_name":"%s","name":"%s","body":%s,"draft":%s,"prerelease":false,"immutable":%s,"author":{"login":"github-actions[bot]","type":"Bot"}}\n' \
      "$release_id" "$tag" "$tag" "$body" "$draft" "$immutable"
    ;;

  "api repos/$repository/git/ref/tags/$tag")
    printf '{"ref":"refs/tags/%s","object":{"type":"tag","sha":"%s"}}\n' \
      "$tag" "$tag_object_sha"
    ;;

  "api repos/$repository/git/tags/$tag_object_sha")
    printf '{"tag":"%s","message":"chore: Release package mcp-repl version %s","object":{"type":"commit","sha":"%s"},"tagger":{"name":"github-actions[bot]","email":"41898282+github-actions[bot]@users.noreply.github.com"}}\n' \
      "$tag" "${tag#v}" "$tag_source_sha"
    ;;

  "api repos/$repository/commits/$tag")
    [ "${3:-}" = --jq ] && [ "${4:-}" = .sha ] || {
      echo "unexpected commit resolution arguments" >&2
      exit 1
    }
    printf '%s\n' "$tag_source_sha"
    ;;

  "api --paginate")
    [ "${3:-}" = --slurp ] || {
      echo "missing --slurp" >&2
      exit 1
    }
    [ "${4:-}" = "repos/$repository/releases/$release_id/assets?per_page=100" ] || {
      echo "unexpected asset URL: ${4:-}" >&2
      exit 1
    }
    printf '[[ '
    first=true
    duplicate_done=false
    for asset in "${TEST_REMOTE:?}"/*; do
      [ -e "$asset" ] || continue
      name=${asset##*/}
      size=$(wc -c < "$asset" | tr -d '[:space:]')
      [ "$first" = true ] || printf ','
      first=false
      asset_json=$(jq -cn --arg name "$name" --argjson size "$size" \
        '{name: $name, size: $size, state: "uploaded"}')
      printf '%s' "$asset_json"
      if [ "${GH_MODE:-normal}" = remote_duplicate ] &&
         [ "$duplicate_done" = false ]; then
        printf ',%s' "$asset_json"
        duplicate_done=true
      fi
    done
    printf ']]\n'
    ;;

  "api --method")
    [ "${3:-}" = PATCH ] || {
      echo "unexpected mutation method" >&2
      exit 1
    }
    [ "${4:-}" = "repos/$repository/releases/$release_id" ] || {
      echo "unexpected mutation URL" >&2
      exit 1
    }
    printf 'patch %s\n' "$*" >> "${TEST_LOG:?}"
    if [ "${GH_MODE:-normal}" = patch_failure ]; then
      echo "patch failed before commit" >&2
      exit 1
    fi
    printf '%s\n' published > "${TEST_STATE:?}"
    if [ "${GH_MODE:-normal}" = tag_move_after_patch ]; then
      : > "${TEST_TAG_MOVED:?}"
    fi
    if [ "${GH_MODE:-normal}" = patch_response_loss ]; then
      echo "patch response lost" >&2
      exit 1
    fi
    printf '{}\n'
    ;;

  "release create")
    [ "$release_state" = absent ] || {
      echo "release already exists" >&2
      exit 1
    }
    printf 'create %s\n' "$*" >> "${TEST_LOG:?}"
    printf '%s\n' draft > "${TEST_STATE:?}"
    ;;

  "release upload")
    shift 2
    [ "${1:-}" = "$tag" ] || {
      echo "unexpected upload tag" >&2
      exit 1
    }
    shift
    [ "${1:-}" = --repo ] && [ "${2:-}" = "$repository" ] || {
      echo "unexpected upload repository" >&2
      exit 1
    }
    shift 2
    [ "$#" -gt 0 ] || {
      echo "upload has no assets" >&2
      exit 1
    }
    printf 'upload' >> "${TEST_LOG:?}"
    for asset in "$@"; do
      [ "$asset" != --clobber ] || {
        echo "publisher attempted --clobber" >&2
        exit 1
      }
      [ ! -e "${TEST_REMOTE:?}/${asset##*/}" ] || {
        echo "publisher attempted to replace an existing asset" >&2
        exit 1
      }
      printf ' %s' "${asset##*/}" >> "${TEST_LOG:?}"
      cp "$asset" "${TEST_REMOTE:?}/"
      if [ "${GH_MODE:-normal}" = upload_failure ]; then
        printf '\n' >> "${TEST_LOG:?}"
        echo "simulated partial upload" >&2
        exit 1
      fi
    done
    printf '\n' >> "${TEST_LOG:?}"
    if [ "${GH_MODE:-normal}" = tag_move_after_upload ]; then
      : > "${TEST_TAG_MOVED:?}"
    fi
    ;;

  "release download")
    shift 2
    [ "${1:-}" = "$tag" ] || {
      echo "unexpected download tag" >&2
      exit 1
    }
    shift
    destination=
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --repo)
          [ "${2:-}" = "$repository" ] || exit 1
          shift 2
          ;;
        --dir)
          destination=${2:-}
          shift 2
          ;;
        *)
          echo "unexpected download argument: $1" >&2
          exit 1
          ;;
      esac
    done
    [ -n "$destination" ] || exit 1
    if [ "${GH_MODE:-normal}" = download_failure ]; then
      echo "download failed" >&2
      exit 1
    fi
    cp "${TEST_REMOTE:?}"/* "$destination/"
    ;;

  *)
    echo "unexpected gh call: $*" >&2
    exit 1
    ;;
esac
STUB
chmod +x "$work/bin/gh"

failures=0
check() {
  local name=$1 operation=$2 want_status=$3 want_text=$4 mutation=$5
  local gh_mode=${6:-normal}
  local output status
  : > "$log"
  set +e
  output=$(PATH="$work/bin:$PATH" \
    TEST_TAG="$tag" \
    TEST_SOURCE_SHA="$source_sha" \
    TEST_STATE="$state" \
    TEST_TAG_MOVED="$tag_moved" \
    TEST_REMOTE="$remote" \
    TEST_LOG="$log" \
    TEST_EXPECTED_NOTES="$expected_notes" \
    GH_MODE="$gh_mode" \
    GH_REPO=test/project \
    "$publisher" "$operation" "$tag" "$dist" 2>&1)
  status=$?
  set -e
  if [[ "$status" != "$want_status" ]]; then
    printf 'FAIL %s: exit %s, wanted %s\n%s\n' \
      "$name" "$status" "$want_status" "$output" >&2
    failures=$((failures + 1))
    return
  fi
  if [[ "$output" != *"$want_text"* ]]; then
    printf 'FAIL %s: output did not mention %q\n%s\n' \
      "$name" "$want_text" "$output" >&2
    failures=$((failures + 1))
    return
  fi
  case "$mutation" in
    none)
      if [[ -s "$log" ]]; then
        printf 'FAIL %s: unexpected mutation: %s\n' "$name" "$(<"$log")" >&2
        failures=$((failures + 1))
      fi
      ;;
    upload)
      if ! grep -q '^upload ' "$log" || grep -q '^\(create\|patch\) ' "$log"; then
        printf 'FAIL %s: expected upload only: %s\n' "$name" "$(<"$log")" >&2
        failures=$((failures + 1))
      fi
      ;;
    create-upload)
      if ! grep -q '^create ' "$log" || ! grep -q '^upload ' "$log" ||
         grep -q '^patch ' "$log"; then
        printf 'FAIL %s: expected create and upload only: %s\n' "$name" "$(<"$log")" >&2
        failures=$((failures + 1))
      fi
      ;;
    patch)
      if ! grep -q '^patch ' "$log" || grep -q '^\(create\|upload\) ' "$log"; then
        printf 'FAIL %s: expected visibility patch only: %s\n' "$name" "$(<"$log")" >&2
        failures=$((failures + 1))
      elif ! grep -Fq 'draft=false' "$log" || ! grep -Fq 'make_latest=legacy' "$log"; then
        printf 'FAIL %s: patch did not preserve finalization policy\n' "$name" >&2
        failures=$((failures + 1))
      fi
      ;;
    *) fail "unknown mutation expectation: $mutation" ;;
  esac
  printf 'ok   %s\n' "$name"
}

seed_dist
reset_remote absent
check "stage creates a missing draft and uploads the complete set" stage 0 \
  "Staged exact draft" create-upload
[[ "$(<"$state")" == draft ]] || fail "stage made a new draft public"
[[ $(find "$remote" -mindepth 1 -maxdepth 1 | wc -l | tr -d '[:space:]') -eq 39 ]] ||
  fail "stage did not upload all 39 assets"
grep -q -- '--clobber' "$log" && fail "stage used --clobber"

seed_dist
reset_remote draft
check "stage uploads an empty existing draft without publishing" stage 0 \
  "Staged exact draft" upload

seed_dist
reset_remote draft
check "stage detects a tag moved while assets are uploaded" stage 1 \
  "live annotated release tag" upload tag_move_after_upload

seed_dist
reset_remote draft
check "a failed upload leaves a private partial draft" stage 1 \
  "could not upload the missing asset set" upload upload_failure
[[ "$(<"$state")" == draft ]] || fail "failed stage made the release public"
[[ $(find "$remote" -mindepth 1 -maxdepth 1 | wc -l | tr -d '[:space:]') -eq 1 ]] ||
  fail "upload-failure fixture did not leave one partial asset"
check "a retry safely completes the matching partial draft" stage 0 \
  "Staged exact draft" upload
[[ $(find "$remote" -mindepth 1 -maxdepth 1 | wc -l | tr -d '[:space:]') -eq 39 ]] ||
  fail "safe upload retry did not complete all 39 assets"

seed_dist
reset_remote draft
check "an ambiguous release lookup cannot create or upload" stage 1 \
  "could not determine whether GitHub release" none lookup_failure

seed_dist
reset_remote draft
copy_exact_remote
check "stage accepts an exact byte-identical retry" stage 0 \
  "Staged exact draft" none

seed_dist
reset_remote draft
copy_exact_remote
rm "$remote/mcp-repl-$tag-container.sbom.sigstore.json"
check "stage safely fills a matching partial existing upload" stage 0 \
  "Staged exact draft" upload

seed_dist
reset_remote draft
first_remote=$("$release_targets" expected-release-assets "$tag" | sed -n '1p')
cp "$dist/$first_remote" "$remote/"
printf '%s\n' tampered > "$remote/$first_remote"
check "stage refuses a byte-mismatched partial subset" stage 1 \
  "differs byte-for-byte" none

seed_dist
reset_remote draft
first_remote=$("$release_targets" expected-release-assets "$tag" | sed -n '1p')
cp "$dist/$first_remote" "$remote/"
check "stage refuses duplicate remote asset metadata" stage 1 \
  "duplicate asset names" none remote_duplicate

seed_dist
reset_remote draft
first_remote=$("$release_targets" expected-release-assets "$tag" | sed -n '1p')
cp "$dist/$first_remote" "$remote/"
check "stage refuses to upload when existing asset download fails" stage 1 \
  "could not download the exact assets" none download_failure

seed_dist
reset_remote draft
copy_exact_remote
record="$remote/mcp-repl-$tag-release.json"
jq -S -c '.container.manifest_digest = ("sha256:" + ("d" * 64))' \
  "$record" > "$work/changed-record"
mv "$work/changed-record" "$record"
check "stage refuses a complete but byte-different retry" stage 1 \
  "differs byte-for-byte" none

seed_dist
reset_remote draft
copy_exact_remote
printf '%s\n' unexpected > "$remote/unexpected.txt"
check "stage refuses an extra remote asset" stage 1 \
  "unexpected asset name" none

seed_dist
reset_remote published
copy_exact_remote
check "stage refuses an already-published release" stage 1 \
  "is not the trusted draft" none

seed_dist
reset_remote draft
copy_exact_remote
check "finalize publishes only after exact remote verification" finalize 0 \
  "Finalized exact immutable" patch
[[ "$(<"$state")" == published ]] || fail "finalize did not publish the release"

seed_dist
reset_remote draft
copy_exact_remote
check "finalize detects a tag moved during publication" finalize 1 \
  "live annotated release tag" patch tag_move_after_patch
[[ "$(<"$state")" == published ]] ||
  fail "tag-movement fixture did not commit publication"

seed_dist
reset_remote published
copy_exact_remote
check "finalize idempotently accepts an exact already-published release" finalize 0 \
  "Accepted already-finalized exact immutable" none

seed_dist
reset_remote published
copy_exact_remote
record="$remote/mcp-repl-$tag-release.json"
jq -S -c '.container.manifest_digest = ("sha256:" + ("d" * 64))' \
  "$record" > "$work/published-changed-record"
mv "$work/published-changed-record" "$record"
check "finalize refuses a byte-mismatched already-published release" finalize 1 \
  "differs byte-for-byte" none

seed_dist
reset_remote published
copy_exact_remote
check "finalize refuses an already-published mutable release" finalize 1 \
  "is not the trusted immutable release" none published_mutable

seed_dist
reset_remote draft
copy_exact_remote
check "finalize recovers when a committed PATCH response is lost" finalize 0 \
  "Recovered committed finalization" patch patch_response_loss
[[ "$(<"$state")" == published ]] || fail "response-loss recovery did not retain publication"

seed_dist
reset_remote draft
copy_exact_remote
check "finalize refuses a failed PATCH that did not commit" finalize 1 \
  "response was lost and GitHub release" patch patch_failure
[[ "$(<"$state")" == draft ]] || fail "uncommitted patch failure changed visibility"

seed_dist
reset_remote draft
copy_exact_remote
rm "$remote/mcp-repl-$tag-container.spdx.json"
check "finalize refuses partial remote assets without publishing" finalize 1 \
  "does not contain the exact expected" none
[[ "$(<"$state")" == draft ]] || fail "failed finalize changed release visibility"

seed_dist
reset_remote published
copy_exact_remote
check "verify accepts an exact immutable published release" verify 0 \
  "Verified exact immutable" none

seed_dist
reset_remote draft
copy_exact_remote
check "verify refuses a draft" verify 1 \
  "is not the trusted immutable release" none

seed_dist
reset_remote draft
first_archive=$("$release_targets" rows | sed -n '1s/^\([^[:space:]]*\)[[:space:]]\([^[:space:]]*\).*/mcp-repl-'"$tag"'-\1.\2/p')
outside="$work/outside-archive"
mv "$dist/$first_archive" "$outside"
ln -s "$outside" "$dist/$first_archive"
check "a local symlink is refused before any GitHub call" stage 1 \
  "linked, empty, or non-file" none

seed_dist
reset_remote draft
: > "$dist/mcp-repl-$tag-container.spdx.json"
check "an empty local asset is refused before any GitHub call" stage 1 \
  "linked, empty, or non-file" none

seed_dist
reset_remote draft
printf '%s\n' unexpected > "$dist/unexpected.txt"
check "an extra local asset is refused before any GitHub call" stage 1 \
  "does not contain the exact expected" none

seed_dist
reset_remote draft
local_record="$dist/mcp-repl-$tag-release.json"
jq -S -c '.source_sha = ("f" * 40)' "$local_record" > "$work/bad-record"
mv "$work/bad-record" "$local_record"
check "a record for different source is refused before any GitHub call" stage 1 \
  "canonical release/source schema" none

seed_dist
reset_remote draft
marker="$work/injection-ran"
set +e
output=$(PATH="$work/bin:$PATH" GH_REPO=test/project \
  "$publisher" stage "${tag};touch $marker" "$dist" 2>&1)
status=$?
set -e
[[ "$status" -eq 2 && "$output" == *"Invalid release publication arguments"* ]] ||
  fail "unsafe tag was not rejected as an argument error"
[[ ! -e "$marker" ]] || fail "unsafe tag executed shell content"

set +e
output=$(PATH="$work/bin:$PATH" GH_REPO="test/project;touch-$marker" \
  "$publisher" stage "$tag" "$dist" 2>&1)
status=$?
set -e
[[ "$status" -eq 2 && "$output" == *"Invalid release publication arguments"* ]] ||
  fail "unsafe repository was not rejected as an argument error"
[[ ! -e "$marker" ]] || fail "unsafe repository executed shell content"

set +e
output=$(PATH="$work/bin:$PATH" GH_REPO=test/project \
  "$publisher" "stage;touch $marker" "$tag" "$dist" 2>&1)
status=$?
set -e
[[ "$status" -eq 2 && "$output" == *"usage:"* ]] ||
  fail "unsafe operation was not rejected as a usage error"
[[ ! -e "$marker" ]] || fail "unsafe operation executed shell content"
printf 'ok   unsafe operation, tag, and repository arguments are inert\n'

if [[ "$failures" -ne 0 ]]; then
  echo "$failures release publisher check(s) failed" >&2
  exit 1
fi
echo "all release publisher checks passed"
