#!/usr/bin/env bash
# Exercise recovery across every registry/tag/draft publication boundary.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
reconcile="$root/scripts/reconcile-source-release.sh"
sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
metadata=$(cargo metadata --locked --no-deps --format-version 1)
version=$(jq -er '.packages[] | select(.name == "mcp-repl") | .version' <<<"$metadata")
tag="v$version"
if [[ ! "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Could not determine the current mcp-repl version" >&2
  exit 1
fi
"$root/scripts/release-targets.sh" validate
target_rows=()
while IFS= read -r row; do
  target_rows+=("$row")
done < <("$root/scripts/release-targets.sh" rows)

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
mkdir -p "$work/bin" "$work/target/package" "$work/state"
log="$work/operations.log"
expected_notes="$work/expected-notes.md"
if ! awk -v prefix="## [$version] - " '
  found && index($0, "## [") == 1 { exit }
  found { lines[++count] = $0 }
  !found && index($0, prefix) == 1 { found = 1 }
  END {
    first = 1
    while (first <= count && lines[first] ~ /^[[:space:]]*$/) first++
    last = count
    while (last >= first && lines[last] ~ /^[[:space:]]*$/) last--
    if (!found || first > last) exit 1
    for (line = first; line <= last; line++) print lines[line]
  }
' "$root/CHANGELOG.md" > "$expected_notes"; then
  echo "Could not build expected release notes for $version" >&2
  exit 1
fi

# The published-retry path must prove that the already-public release is the
# complete binary release that downstream recovery expects, not just a valid
# release object. Seed every manifest-derived archive and self-bound checksum
# produced by the native publication workflow.
public_assets="$work/published-assets"
mkdir -p "$public_assets"
for row in "${target_rows[@]}"; do
  IFS=$'\t' read -r target extension _binary <<<"$row"
  archive_name="mcp-repl-${tag}-${target}.${extension}"
  printf 'archive for %s\n' "$target" > "$public_assets/$archive_name"
  if command -v sha256sum > /dev/null 2>&1; then
    checksum=$(sha256sum "$public_assets/$archive_name" | awk '{print $1}')
  else
    checksum=$(shasum -a 256 "$public_assets/$archive_name" | awk '{print $1}')
  fi
  printf '%s  %s\n' "$checksum" "$archive_name" \
    > "$public_assets/$archive_name.sha256"
done

cat > "$work/bin/git" <<'STUB'
#!/bin/sh
set -eu
if [ "${1:-} ${2:-}" = "rev-parse HEAD" ]; then
  printf '%s\n' "${TEST_SOURCE_SHA:?}"
  exit 0
fi
echo "unexpected git call: $*" >&2
exit 1
STUB

cat > "$work/bin/cargo" <<'STUB'
#!/bin/sh
set -eu
case "${1:-}" in
  metadata)
    printf '{"packages":[{"name":"mcp-repl","version":"%s"}],"target_directory":"%s"}\n' \
      "${TEST_VERSION:?}" "${TEST_TARGET_DIR:?}"
    ;;
  package)
    package_root="${TEST_TARGET_DIR:?}/package"
    staging="$package_root/.stub-package"
    package_dir="$staging/mcp-repl-${TEST_VERSION:?}"
    mkdir -p "$package_dir"
    vcs_sha=${TEST_SOURCE_SHA:?}
    dirty=false
    [ "${GH_MODE:-valid}" = wrong_vcs_sha ] && vcs_sha=dddddddddddddddddddddddddddddddddddddddd
    [ "${GH_MODE:-valid}" = dirty_vcs ] && dirty=true
    printf '{"git":{"sha1":"%s","dirty":%s}}\n' "$vcs_sha" "$dirty" \
      > "$package_dir/.cargo_vcs_info.json"
    tar -czf "$package_root/mcp-repl-${TEST_VERSION}.crate" \
      -C "$staging" "mcp-repl-${TEST_VERSION}"
    ;;
  *)
    echo "unexpected cargo call: $*" >&2
    exit 1
    ;;
esac
STUB

cat > "$work/bin/curl" <<'STUB'
#!/bin/sh
set -eu
output=
connect_timeout=
max_time=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --output) output=$2; shift 2 ;;
    --connect-timeout) connect_timeout=$2; shift 2 ;;
    --max-time) max_time=$2; shift 2 ;;
    --write-out | --user-agent) shift 2 ;;
    *) shift ;;
  esac
done
[ -n "$output" ] || { echo "curl stub received no output path" >&2; exit 1; }
[ "$connect_timeout" = 10 ] || { echo "curl stub received no connect timeout" >&2; exit 1; }
[ "$max_time" = 30 ] || { echo "curl stub received no total timeout" >&2; exit 1; }

case "${GH_MODE:-valid}" in
  registry_network_error) exit 7 ;;
  registry_api_error)
    printf '{"errors":[{"detail":"unavailable"}]}\n' > "$output"
    printf '503'
    exit 0
    ;;
  registry_missing)
    printf '{"errors":[{"detail":"not found"}]}\n' > "$output"
    printf '404'
    exit 0
    ;;
esac

if command -v sha256sum > /dev/null 2>&1; then
  checksum=$(sha256sum "${TEST_CRATE_FILE:?}" | awk '{print $1}')
else
  checksum=$(shasum -a 256 "${TEST_CRATE_FILE:?}" | awk '{print $1}')
fi
case "${GH_MODE:-valid}" in
  registry_bad_checksum) checksum=ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff ;;
  registry_bad_metadata)
    printf '{}\n' > "$output"
    printf '200'
    exit 0
    ;;
esac
yanked=false
[ "${GH_MODE:-valid}" = registry_yanked ] && yanked=true
printf '{"version":{"crate":"mcp-repl","num":"%s","checksum":"%s","yanked":%s}}\n' \
  "${TEST_VERSION:?}" "$checksum" "$yanked" > "$output"
printf '200'
STUB

cat > "$work/bin/gh" <<'STUB'
#!/bin/sh
set -eu
mode=${GH_MODE:-valid}
state=${TEST_STATE_DIR:?}
log=${TEST_LOG:?}
sha=${TEST_SOURCE_SHA:?}
version=${TEST_VERSION:?}
tag=${TEST_TAG:?}

release_object() {
  draft=$1
  name=$2
  body=$3
  author=${4:-github-actions[bot]}
  author_type=${5:-Bot}
  immutable=${6:-false}
  printf '{"id":4242,"tag_name":"%s","name":"%s","body":%s,"draft":%s,"prerelease":false,"immutable":%s,"author":{"login":"%s","type":"%s"}}' \
    "$tag" "$name" "$body" "$draft" "$immutable" "$author" "$author_type"
}

release_json() {
  printf '[['
  release_object "$@"
  printf ']]\n'
}

published_release() {
  body=$(jq -Rs . < "${TEST_EXPECTED_NOTES:?}")
  name=$tag
  author='github-actions[bot]'
  author_type=Bot
  immutable=true
  case "$mode" in
    mutable_public) immutable=false ;;
    wrong_public) name="wrong-$tag" ;;
    foreign_public)
      author=octocat
      author_type=User
      ;;
  esac
  release_object false "$name" "$body" "$author" "$author_type" "$immutable"
}

if [ "${1:-}" = api ]; then
  call=$*
  case "$call" in
    "api repos/test/project/git/matching-refs/tags/$tag")
      if [ "$mode" = tag_list_failure ]; then
        echo "tag list unavailable" >&2
        exit 1
      fi
      case "$mode" in
        existing_draft | existing_public | mutable_public | wrong_public | foreign_public | incomplete_public | bad_draft | foreign_draft | lightweight_tag | moved_tag | multiple_releases | malformed_releases | wrong_tag_message) tag_exists=true ;;
        malformed_tags)
          printf '{}\n'
          exit 0
          ;;
        multiple_tags)
          printf '[{"ref":"refs/tags/%s"},{"ref":"refs/tags/%s"}]\n' "$tag" "$tag"
          exit 0
          ;;
        *) [ -f "$state/tag-created" ] && tag_exists=true || tag_exists=false ;;
      esac
      if [ "$tag_exists" = true ]; then
        printf '[{"ref":"refs/tags/%s","object":{"sha":"%s","type":"tag"}}]\n' "$tag" "$sha"
      else
        printf '[]\n'
      fi
      ;;
    api\ --method\ POST\ repos/test/project/git/tags*)
      printf 'tag-object-create %s\n' "$call" >> "$log"
      case "$call" in
        *"tagger[name]=github-actions[bot]"*"tagger[email]=41898282+github-actions[bot]@users.noreply.github.com"*) ;;
        *) echo "tag object creation omitted the canonical tagger" >&2; exit 1 ;;
      esac
      if [ "$mode" = tag_object_create_failure ]; then
        echo "tag object create unavailable" >&2
        exit 1
      fi
      if [ "$mode" = malformed_tag_object ]; then
        printf '{"sha":"short"}\n'
      else
        object_sha=$sha
        [ "$mode" = wrong_created_tag_object ] && object_sha=dddddddddddddddddddddddddddddddddddddddd
        printf '{"sha":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee","tag":"%s","message":"chore: Release package mcp-repl version %s","object":{"type":"commit","sha":"%s"},"tagger":{"name":"github-actions[bot]","email":"41898282+github-actions[bot]@users.noreply.github.com"}}\n' \
          "$tag" "$version" "$object_sha"
      fi
      ;;
    api\ --method\ POST\ repos/test/project/git/refs*)
      printf 'tag-ref-create %s\n' "$call" >> "$log"
      if [ "$mode" = tag_create_failure ]; then
        echo "tag create unavailable" >&2
        exit 1
      fi
      touch "$state/tag-created"
      printf '{}\n'
      ;;
    "api repos/test/project/git/ref/tags/$tag")
      if [ "$mode" = tag_verify_api_failure ]; then
        echo "tag lookup unavailable" >&2
        exit 1
      fi
      if [ "$mode" = lightweight_tag ]; then
        printf '{"ref":"refs/tags/%s","object":{"type":"commit","sha":"%s"}}\n' "$tag" "$sha"
      else
        printf '{"ref":"refs/tags/%s","object":{"type":"tag","sha":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"}}\n' "$tag"
      fi
      ;;
    "api repos/test/project/git/tags/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")
      message="chore: Release package mcp-repl version $version"
      [ "$mode" = wrong_tag_message ] && message="noncanonical release tag"
      printf '{"tag":"%s","message":"%s","object":{"type":"commit","sha":"%s"},"tagger":{"name":"github-actions[bot]","email":"41898282+github-actions[bot]@users.noreply.github.com"}}\n' \
        "$tag" "$message" "$sha"
      ;;
    "api repos/test/project/commits/$tag --jq .sha")
      if [ "$mode" = moved_tag ]; then
        printf 'dddddddddddddddddddddddddddddddddddddddd\n'
      else
        printf '%s\n' "$sha"
      fi
      ;;
    "api --paginate --slurp repos/test/project/releases?per_page=100")
      if [ "$mode" = release_list_failure ]; then
        echo "release list unavailable" >&2
        exit 1
      fi
      case "$mode" in
        existing_draft)
          body=$(jq -Rs . < "${TEST_EXPECTED_NOTES:?}")
          release_json true "$tag" "$body"
          ;;
        existing_public | mutable_public | wrong_public | foreign_public | incomplete_public)
          printf '[['
          published_release
          printf ']]\n'
          ;;
        bad_draft)
          release_json true "$tag" '"wrong notes"'
          ;;
        foreign_draft)
          release_json true "$tag" '"notes"' octocat User
          ;;
        multiple_releases)
          printf '[[{"tag_name":"%s"},{"tag_name":"%s"}]]\n' "$tag" "$tag"
          ;;
        malformed_releases) printf '{}\n' ;;
        *)
          if [ -f "$state/release-created" ]; then
            body=$(jq -Rs . < "${TEST_RELEASE_NOTES:?}")
            release_json true "$tag" "$body"
          else
            printf '[[]]\n'
          fi
          ;;
      esac
      ;;
    "api repos/test/project/releases/tags/$tag")
      case "$mode" in
        existing_draft)
          body=$(jq -Rs . < "${TEST_EXPECTED_NOTES:?}")
          release_object true "$tag" "$body"
          ;;
        existing_public | mutable_public | wrong_public | foreign_public | incomplete_public)
          published_release
          ;;
        bad_draft)
          release_object true "$tag" '"wrong notes"'
          ;;
        foreign_draft)
          release_object true "$tag" '"notes"' octocat User
          ;;
        *)
          if [ ! -f "$state/release-created" ]; then
            echo "release-by-tag queried before creation" >&2
            exit 1
          fi
          body=$(jq -Rs . < "${TEST_RELEASE_NOTES:?}")
          release_object true "$tag" "$body"
          ;;
      esac
      printf '\n'
      ;;
    "api --paginate --slurp repos/test/project/releases/4242/assets?per_page=100")
      printf '[['
      first=true
      for asset in "${TEST_PUBLIC_ASSETS:?}"/*; do
        name=${asset##*/}
        if [ "$mode" = incomplete_public ] &&
           [ "$name" = "mcp-repl-${tag}-x86_64-unknown-linux-gnu.tar.gz" ]; then
          continue
        fi
        if command -v sha256sum > /dev/null 2>&1; then
          digest=$(sha256sum "$asset" | awk '{print $1}')
        else
          digest=$(shasum -a 256 "$asset" | awk '{print $1}')
        fi
        size=$(wc -c < "$asset" | tr -d '[:space:]')
        [ "$first" = true ] || printf ','
        first=false
        printf '{"name":"%s","digest":"sha256:%s","size":%s,"state":"uploaded"}' \
          "$name" "$digest" "$size"
      done
      printf ']]\n'
      ;;
    *)
      echo "unexpected gh api call: $call" >&2
      exit 1
      ;;
  esac
  exit 0
fi

if [ "${1:-} ${2:-}" = "release download" ]; then
  shift 2
  [ "${1:-}" = "$tag" ] || { echo "unexpected download tag" >&2; exit 1; }
  shift
  destination=
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --repo)
        [ "${2:-}" = test/project ] || { echo "unexpected download repository" >&2; exit 1; }
        shift 2
        ;;
      --dir)
        destination=${2:-}
        shift 2
        ;;
      *)
        echo "unexpected release-download argument: $1" >&2
        exit 1
        ;;
    esac
  done
  [ -n "$destination" ] || { echo "release download has no destination" >&2; exit 1; }
  mkdir -p "$destination"
  cp "${TEST_PUBLIC_ASSETS:?}"/* "$destination/"
  exit 0
fi

if [ "${1:-} ${2:-}" = "release create" ]; then
  printf 'release-create %s\n' "$*" >> "$log"
  if [ "$mode" = release_create_failure ]; then
    echo "release create unavailable" >&2
    exit 1
  fi
  notes=
  while [ "$#" -gt 0 ]; do
    if [ "$1" = --notes-file ]; then
      notes=$2
      break
    fi
    shift
  done
  [ -n "$notes" ] || { echo "release create omitted notes" >&2; exit 1; }
  cp "$notes" "${TEST_RELEASE_NOTES:?}"
  touch "$state/release-created"
  printf 'https://github.com/test/project/releases/tag/%s\n' "$tag"
  exit 0
fi

echo "unexpected gh call: $*" >&2
exit 1
STUB

chmod +x "$work/bin/git" "$work/bin/cargo" "$work/bin/curl" "$work/bin/gh"

failures=0
assert_result() {
  local name=$1 status=$2 want_status=$3 output=$4 want_text=$5
  if [[ "$status" != "$want_status" ]]; then
    printf 'FAIL %s: exit %s, wanted %s\n%s\n' "$name" "$status" "$want_status" "$output" >&2
    failures=$((failures + 1))
    return 1
  fi
  if [[ "$output" != *"$want_text"* ]]; then
    printf 'FAIL %s: output did not mention %q\n%s\n' "$name" "$want_text" "$output" >&2
    failures=$((failures + 1))
    return 1
  fi
  printf 'ok   %s\n' "$name"
}

check_reconcile() {
  local name=$1 mode=$2 outcome=$3 want_status=$4 want_text=$5 want_tag_writes=$6 want_release=$7
  local output status
  : > "$log"
  rm -f "$work/state/tag-created" "$work/state/release-created" "$work/release-notes.md"
  set +e
  output=$(PATH="$work/bin:$PATH" \
    GH_MODE="$mode" \
    GH_TOKEN=token \
    GITHUB_REPOSITORY=test/project \
    SOURCE_RELEASE_MAX_ATTEMPTS=1 \
    SOURCE_RELEASE_RETRY_DELAY_SECONDS=0 \
    TEST_SOURCE_SHA="$sha" \
    TEST_VERSION="$version" \
    TEST_TAG="$tag" \
    TEST_TARGET_DIR="$work/target" \
    TEST_CRATE_FILE="$work/target/package/mcp-repl-$version.crate" \
    TEST_STATE_DIR="$work/state" \
    TEST_RELEASE_NOTES="$work/release-notes.md" \
    TEST_EXPECTED_NOTES="$expected_notes" \
    TEST_PUBLIC_ASSETS="$public_assets" \
    TEST_LOG="$log" \
    "$reconcile" "$sha" "$outcome" 2>&1)
  status=$?
  set -e
  if ! assert_result "$name" "$status" "$want_status" "$output" "$want_text"; then
    return 0
  fi

  case "$want_tag_writes" in
    none)
      if grep -Eq 'tag-(object|ref)-create' "$log"; then
        echo "FAIL $name: unexpected tag mutation attempt" >&2
        failures=$((failures + 1))
      fi
      ;;
    object)
      if ! grep -Fq 'tag-object-create' "$log" || grep -Fq 'tag-ref-create' "$log"; then
        echo "FAIL $name: expected only an annotated-tag object attempt" >&2
        failures=$((failures + 1))
      fi
      ;;
    both)
      if ! grep -Fq 'tag-object-create' "$log" || ! grep -Fq 'tag-ref-create' "$log"; then
        echo "FAIL $name: expected annotated-tag object and ref attempts" >&2
        failures=$((failures + 1))
      fi
      ;;
    *)
      echo "FAIL $name: invalid expected tag-write state $want_tag_writes" >&2
      failures=$((failures + 1))
      ;;
  esac

  if [[ "$want_release" == true ]] && ! grep -Fq 'release-create' "$log"; then
    echo "FAIL $name: expected release creation" >&2
    failures=$((failures + 1))
  elif [[ "$want_release" == false ]] && grep -Fq 'release-create' "$log"; then
    echo "FAIL $name: unexpected release creation" >&2
    failures=$((failures + 1))
  fi
}

check_reconcile "crate publication stages a tag and draft" valid success 0 "ready for native assets" both true
check_reconcile "an exact existing tag and draft are verified without mutation" existing_draft success 0 "ready for native assets" none false
check_reconcile "a crate-only action failure is recovered" valid failure 0 "Recovered the safe external state" both true
check_reconcile "an absent registry version is refused" registry_missing failure 1 "is not on crates.io" none false
check_reconcile "a registry outage is preserved" registry_api_error failure 1 "Could not verify" none false
check_reconcile "a registry network failure is preserved" registry_network_error failure 1 "Could not verify" none false
check_reconcile "a package from the wrong commit is refused" wrong_vcs_sha success 1 "VCS metadata" none false
check_reconcile "a dirty source package is refused" dirty_vcs success 1 "VCS metadata" none false
check_reconcile "malformed registry metadata is refused" registry_bad_metadata success 1 "invalid or yanked" none false
check_reconcile "a yanked registry version is refused" registry_yanked success 1 "invalid or yanked" none false
check_reconcile "a mismatched registry package is refused" registry_bad_checksum success 1 "does not match" none false
check_reconcile "a tag-list failure is preserved" tag_list_failure success 1 "Could not list" none false
check_reconcile "malformed tag data is refused" malformed_tags success 1 "malformed tag-ref" none false
check_reconcile "multiple exact tags are refused" multiple_tags success 1 "multiple exact refs" none false
check_reconcile "a tag-object failure is preserved" tag_object_create_failure success 1 "Could not create annotated" object false
check_reconcile "a malformed tag object is refused" malformed_tag_object success 1 "noncanonical annotated tag" object false
check_reconcile "a valid-looking tag object for the wrong source is refused before the ref" wrong_created_tag_object success 1 "noncanonical annotated tag" object false
check_reconcile "a tag-create failure is preserved" tag_create_failure success 1 "Could not create release tag" both false
check_reconcile "a tag verification API failure is preserved" tag_verify_api_failure success 1 "Could not read release tag" both false
check_reconcile "a lightweight existing tag is refused" lightweight_tag success 1 "is not one annotated tag object" none false
check_reconcile "a noncanonical annotated tag is refused" wrong_tag_message success 1 "does not match the trusted source and message" none false
check_reconcile "a moved existing tag is refused" moved_tag success 1 "Release tag $tag moved" none false
check_reconcile "a release-list failure is preserved" release_list_failure success 1 "Could not list GitHub releases" both false
check_reconcile "a release-create failure is preserved" release_create_failure success 1 "Could not create draft" both true
check_reconcile "multiple matching releases are refused" multiple_releases success 1 "multiple releases" none false
check_reconcile "malformed release data is refused" malformed_releases success 1 "malformed release data" none false
check_reconcile "a conflicting draft is refused" bad_draft success 1 "unexpected title or notes" none false
check_reconcile "a foreign draft is refused" foreign_draft success 1 "trusted draft boundary" none false
check_reconcile "an exact immutable public release recovers a repeated publish" existing_public failure 0 "ready for downstream recovery" none false
check_reconcile "a mutable public release is refused" mutable_public success 1 "trusted published boundary" none false
check_reconcile "a public release with the wrong title is refused" wrong_public success 1 "unexpected title or notes" none false
check_reconcile "a foreign public release is refused" foreign_public success 1 "trusted published boundary" none false
check_reconcile "an incomplete public release is refused" incomplete_public success 1 "not the exact expected" none false

if [[ "$failures" -ne 0 ]]; then
  echo "$failures source release check(s) failed" >&2
  exit 1
fi
echo "all source release reconciliation checks passed"
