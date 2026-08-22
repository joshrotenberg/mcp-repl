#!/usr/bin/env bash
# Exercise the failure paths of scripts/publish-release.sh.
#
#   scripts/test-release-guards.sh
#
# Those guards decide whether a release becomes public, and they only run
# during a release, so a mistake in them is discovered at the worst possible
# moment. This drives the real script against a stubbed `gh` and a fabricated
# set of archives, once per outcome.
#
# It asserts the message as well as the status: the bug that prompted this
# was a guard that failed correctly while saying something false about the
# release, which no exit code would have caught.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
publish="$root/scripts/publish-release.sh"
metadata=$(cargo metadata --locked --no-deps --format-version 1)
version=$(jq -er '.packages[] | select(.name == "mcp-repl") | .version' <<<"$metadata")
tag="v$version"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
log="$work/gh.log"
state="$work/gh.state"
expected_notes="$work/expected-notes.md"
"$root/scripts/extract-release-notes.sh" "$version" > "$expected_notes"

# A `gh` that reports what GH_MODE says and records nothing else, so the
# script can be run to completion without touching a real release.
mkdir -p "$work/bin"
cat > "$work/bin/gh" <<'STUB'
#!/bin/sh
set -eu
mode=${GH_MODE:-draft}
tag=${TEST_TAG:?}
case "${1:-} ${2:-}" in
  "api repos/test/project/releases/tags/$tag")
    if [ "$mode" = unreachable ]; then
      echo "release API unavailable" >&2
      exit 1
    fi
    body=$(jq -Rs . < "${TEST_EXPECTED_NOTES:?}")
    draft=true
    author='github-actions[bot]'
    author_type=Bot
    release_id=4242
    immutable=false
    case "$mode" in
      published | published_bad_digest | published_download_failure)
        draft=false
        immutable=true
        ;;
      published_mutable) draft=false ;;
    esac
    if [ -f "${GH_STATE:?}.published" ]; then
      draft=false
      immutable=true
    fi
    [ "$mode" = bad_notes ] && body='"wrong notes"'
    if [ "$mode" = foreign ]; then
      author=octocat
      author_type=User
    fi
    if [ "$mode" = replaced ] && [ -f "${GH_STATE:?}" ]; then
      release_id=4343
    fi
    printf '{"id":%s,"tag_name":"%s","name":"%s","body":%s,"draft":%s,"prerelease":false,"immutable":%s,"author":{"login":"%s","type":"%s"}}\n' \
      "$release_id" "$tag" "$tag" "$body" "$draft" "$immutable" "$author" "$author_type"
    ;;
  "api --paginate")
    if [ "${3:-}" != --slurp ] ||
       [ "${4:-}" != "repos/test/project/releases/4242/assets?per_page=100" ]; then
      echo "unexpected asset-list call: $*" >&2
      exit 1
    fi
    if [ "$mode" = asset_list_failure ]; then
      echo "asset list unavailable" >&2
      exit 1
    fi
    if [ "$mode" = remote_malformed ]; then
      printf '{}\n'
      exit 0
    fi

    printf '[['
    first=true
    for asset in "${TEST_DIST:?}"/*; do
      name=${asset##*/}
      selected="mcp-repl-${tag}-x86_64-unknown-linux-gnu.tar.gz"
      if [ "$mode" = remote_missing ] && [ "$name" = "$selected" ]; then
        continue
      fi
      if command -v sha256sum > /dev/null 2>&1; then
        digest=$(sha256sum "$asset" | awk '{print $1}')
      else
        digest=$(shasum -a 256 "$asset" | awk '{print $1}')
      fi
      size=$(wc -c < "$asset" | tr -d '[:space:]')
      state=uploaded
      case "$mode" in
        remote_bad_digest | published_bad_digest)
          [ "$name" = "$selected" ] && \
            digest=ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff
          ;;
      esac
      [ "$mode" = remote_bad_size ] && [ "$name" = "$selected" ] && \
        size=$((size + 1))
      [ "$mode" = remote_bad_state ] && [ "$name" = "$selected" ] && \
        state=new

      [ "$first" = true ] || printf ','
      first=false
      printf '{"name":"%s","digest":"sha256:%s","size":%s,"state":"%s"}' \
        "$name" "$digest" "$size" "$state"
      if [ "$mode" = remote_duplicate ] && [ "$name" = "$selected" ]; then
        printf ',{"name":"%s","digest":"sha256:%s","size":%s,"state":"%s"}' \
          "$name" "$digest" "$size" "$state"
      fi
    done
    if [ "$mode" = remote_extra ]; then
      [ "$first" = true ] || printf ','
      printf '{"name":"unexpected.bin","digest":"sha256:%064d","size":0,"state":"uploaded"}' 0
    fi
    printf ']]\n'
    ;;
  "api --method")
    printf 'release-patch %s\n' "$*" >> "${GH_LOG:?}"
    if [ "$mode" = patch_failure ]; then
      echo "release patch unavailable" >&2
      exit 1
    fi
    touch "${GH_STATE:?}.published"
    printf '{}\n'
    ;;
  "release download")
    if [ "$mode" = published_download_failure ]; then
      echo "release download unavailable" >&2
      exit 1
    fi
    shift 2
    [ "${1:-}" = "$tag" ] || { echo "unexpected download tag" >&2; exit 1; }
    shift
    destination=
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --repo)
          [ "${2:-}" = test/project ] || { echo "unexpected download repo" >&2; exit 1; }
          shift 2
          ;;
        --dir) destination=${2:-}; shift 2 ;;
        *) echo "unexpected release-download argument: $1" >&2; exit 1 ;;
      esac
    done
    [ -n "$destination" ] || { echo "release download has no destination" >&2; exit 1; }
    mkdir -p "$destination"
    cp "${TEST_DIST:?}"/* "$destination/"
    ;;
  "release upload")
    printf 'release-upload %s\n' "$*" >> "${GH_LOG:?}"
    [ "$mode" = replaced ] && touch "${GH_STATE:?}"
    if [ "$mode" = upload_failure ]; then
      echo "release upload unavailable" >&2
      exit 1
    fi
    ;;
  *) echo "unexpected gh call: $*" >&2; exit 1 ;;
esac
STUB
chmod +x "$work/bin/gh"

sha256() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$@"
  else
    shasum -a 256 "$@"
  fi
}

# A complete set, as five successful build jobs would leave behind.
seed_dist() {
  local dist=$1
  rm -rf "$dist"
  mkdir -p "$dist"
  local target extension
  for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
                x86_64-apple-darwin aarch64-apple-darwin x86_64-pc-windows-msvc; do
    extension=tar.gz
    [[ "$target" == x86_64-pc-windows-msvc ]] && extension=zip
    echo "archive for $target" > "$dist/mcp-repl-${tag}-${target}.${extension}"
    (
      cd "$dist"
      sha256 "mcp-repl-${tag}-${target}.${extension}" \
        > "mcp-repl-${tag}-${target}.${extension}.sha256"
    )
  done
}

failures=0
check() {
  local name=$1 mode=$2 want_status=$3 want_text=$4 dist=$5 want_mutation=$6
  local output status
  : > "$log"
  rm -f "$state" "$state.published"
  set +e
  output=$(PATH="$work/bin:$PATH" GH_MODE="$mode" GH_LOG="$log" GH_STATE="$state" \
    TEST_TAG="$tag" TEST_EXPECTED_NOTES="$expected_notes" TEST_DIST="$dist" \
    GH_REPO=test/project \
    "$publish" "$tag" "$dist" 2>&1)
  status=$?
  set -e

  if [[ "$status" != "$want_status" ]]; then
    printf 'FAIL %s: exit %s, wanted %s\n%s\n' "$name" "$status" "$want_status" "$output" >&2
    failures=$((failures + 1))
    return
  fi
  if [[ "$output" != *"$want_text"* ]]; then
    printf 'FAIL %s: output did not mention %q\n%s\n' "$name" "$want_text" "$output" >&2
    failures=$((failures + 1))
    return
  fi
  case "$want_mutation" in
    none)
      if [[ -s "$log" ]]; then
        printf 'FAIL %s: unexpected GitHub mutation\n' "$name" >&2
        failures=$((failures + 1))
      fi
      ;;
    upload)
      if ! grep -Fq release-upload "$log" || grep -Fq release-patch "$log"; then
        printf 'FAIL %s: expected upload without publication\n' "$name" >&2
        failures=$((failures + 1))
      fi
      ;;
    publish)
      if ! grep -Fq release-upload "$log" || ! grep -Fq release-patch "$log"; then
        printf 'FAIL %s: expected upload and exact-id publication\n' "$name" >&2
        failures=$((failures + 1))
      elif ! grep -Fq 'repos/test/project/releases/4242' "$log" ||
           ! grep -Fq 'make_latest=legacy' "$log"; then
        printf 'FAIL %s: publication did not preserve exact-id/latest ordering\n' "$name" >&2
        failures=$((failures + 1))
      fi
      ;;
  esac
  printf 'ok   %s\n' "$name"
}

dist="$work/dist"

# The failure that shipped: `gh` itself broke, and the guard reported the
# release's state instead of saying the lookup failed.
seed_dist "$dist"
check "unreachable gh explains itself" unreachable 1 "Could not read GitHub release" "$dist" none

seed_dist "$dist"
check "an exact published release is an idempotent success" published 0 \
  "Verified already-published exact GitHub release" "$dist" none

seed_dist "$dist"
check "a mutable published release is refused" published_mutable 1 \
  "trusted published boundary" "$dist" none

seed_dist "$dist"
check "a mismatched published release is refused" published_bad_digest 1 \
  "identity does not match" "$dist" none

seed_dist "$dist"
check "a published-asset download failure is preserved" published_download_failure 1 \
  "Could not download assets" "$dist" none

seed_dist "$dist"
check "a draft with wrong notes is refused" bad_notes 1 "unexpected title or notes" "$dist" none

seed_dist "$dist"
check "a foreign draft is refused" foreign 1 "trusted draft boundary" "$dist" none

# A build job that produced nothing used to end the run silently.
seed_dist "$dist"
rm "$dist/mcp-repl-${tag}-aarch64-apple-darwin.tar.gz"
check "a missing archive names its target" draft 1 "aarch64-apple-darwin job produced no complete package" "$dist" none

seed_dist "$dist"
rm "$dist/mcp-repl-${tag}-x86_64-pc-windows-msvc.zip.sha256"
check "a missing checksum names the file" draft 1 "Missing" "$dist" none

seed_dist "$dist"
touch "$dist/unexpected.txt"
check "an unexpected file is listed" draft 1 "unexpected.txt" "$dist" none

# Corruption has to be caught before anything is uploaded.
seed_dist "$dist"
echo "tampered" > "$dist/mcp-repl-${tag}-x86_64-unknown-linux-gnu.tar.gz"
check "a bad checksum stops the publish" draft 1 "FAILED" "$dist" none

seed_dist "$dist"
echo "tampered but misdirected" > "$dist/mcp-repl-${tag}-x86_64-unknown-linux-gnu.tar.gz"
cp "$dist/mcp-repl-${tag}-aarch64-unknown-linux-gnu.tar.gz.sha256" \
  "$dist/mcp-repl-${tag}-x86_64-unknown-linux-gnu.tar.gz.sha256"
check "a checksum cannot name a different target" draft 1 \
  "does not identify its own archive exactly" "$dist" none

seed_dist "$dist"
windows_archive="mcp-repl-${tag}-x86_64-pc-windows-msvc.zip"
windows_digest=$(sha256 "$dist/$windows_archive" | awk '{print $1}')
printf '%s *%s\n' "$windows_digest" "$windows_archive" \
  > "$dist/$windows_archive.sha256"
check "a Windows binary-mode marker is not mistaken for canonical output" draft 1 \
  "does not identify its own archive exactly" "$dist" none

seed_dist "$dist"
check "an upload failure leaves the draft private" upload_failure 1 "Could not upload" "$dist" upload

seed_dist "$dist"
check "a replaced draft is refused after upload" replaced 1 "was replaced" "$dist" upload

seed_dist "$dist"
check "an asset-list failure leaves the draft private" asset_list_failure 1 "Could not list assets" "$dist" upload

seed_dist "$dist"
check "malformed remote assets leave the draft private" remote_malformed 1 "malformed asset data" "$dist" upload

seed_dist "$dist"
check "a stale remote asset leaves the draft private" remote_extra 1 "not the exact expected" "$dist" upload

seed_dist "$dist"
check "a missing remote asset leaves the draft private" remote_missing 1 "not the exact expected" "$dist" upload

seed_dist "$dist"
check "a duplicate remote asset leaves the draft private" remote_duplicate 1 "not the exact expected" "$dist" upload

seed_dist "$dist"
check "a wrong remote digest leaves the draft private" remote_bad_digest 1 "identity does not match" "$dist" upload

seed_dist "$dist"
check "a wrong remote size leaves the draft private" remote_bad_size 1 "identity does not match" "$dist" upload

seed_dist "$dist"
check "an incomplete remote upload leaves the draft private" remote_bad_state 1 "identity does not match" "$dist" upload

seed_dist "$dist"
check "a publication API failure is preserved" patch_failure 1 "Could not publish exact" "$dist" publish

# And the path that matters most: a complete set publishes.
seed_dist "$dist"
check "a complete set is uploaded and published" draft 0 "Published exact GitHub release" "$dist" publish

if [[ "$failures" -ne 0 ]]; then
  echo "$failures release guard check(s) failed" >&2
  exit 1
fi
echo "all release guard checks passed"
