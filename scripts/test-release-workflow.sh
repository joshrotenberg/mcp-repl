#!/usr/bin/env bash
# Exercise the trusted release-PR discovery, status, and dispatch boundaries.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
discover="$root/scripts/discover-release-pr.sh"
discover_merge="$root/scripts/discover-release-merge.sh"
report="$root/scripts/report-release-status.sh"
dispatch="$root/scripts/dispatch-release-binaries.sh"
verify_tag="$root/scripts/verify-release-tag.sh"
sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
metadata=$(cargo metadata --locked --no-deps --format-version 1)
version=$(jq -er '.packages[] | select(.name == "mcp-repl") | .version' <<<"$metadata")
tag="v$version"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT INT TERM
mkdir -p "$work/bin"
log="$work/gh.log"
expected_notes="$work/expected-notes.md"
"$root/scripts/extract-release-notes.sh" "$version" > "$expected_notes"
export TEST_TAG="$tag" TEST_VERSION="$version" TEST_EXPECTED_NOTES="$expected_notes"

sbom_predicate='--predicate-type https://spdx.dev/Document/v2.3'
[[ $(grep -Fc -- "$sbom_predicate" \
      "$root/.github/workflows/release-binaries.yml") -eq 3 ]] || {
  echo "release workflow must verify all three SBOM paths with the exact SPDX 2.3 predicate" >&2
  exit 1
}
[[ $(grep -Fc -- "$sbom_predicate" "$root/docs/releases.md") -eq 2 ]] || {
  echo "release documentation must use the exact SPDX 2.3 predicate" >&2
  exit 1
}

release_build="$root/.github/workflows/release-build.yml"
ci_workflow="$root/.github/workflows/ci.yml"
if grep -Fq 'github.event.pull_request.head.sha' "$ci_workflow"; then
  echo "CI must use one exact merge-or-release source for every release rehearsal" >&2
  exit 1
fi
# The release-target job validates and exports the source once; both reusable
# builders and the aggregate gate must consume that exact output.
# These single-quoted patterns intentionally match literal shell syntax.
# shellcheck disable=SC2016
[[ $(grep -Fc 'needs.release-targets.outputs.source_sha' "$ci_workflow") -eq 3 &&
    $(grep -Fc 'source_sha=$(git rev-parse HEAD)' "$ci_workflow") -eq 1 &&
    $(grep -Fc 'source_date_epoch=$(git show -s --format=%ct HEAD)' "$ci_workflow") -eq 1 ]] || {
  echo "CI release source and epoch must come from one validated checkout" >&2
  exit 1
}

touch_line=$(grep -Fn 'touch src/main.rs src/lib.rs' "$release_build" | cut -d: -f1)
# These single-quoted patterns intentionally match literal workflow variables.
# shellcheck disable=SC2016
auditable_build_line=$(grep -Fn \
  'cargo auditable build --release --locked --target "$RELEASE_TARGET"' \
  "$release_build" | cut -d: -f1)
# shellcheck disable=SC2016
auditable_verify_line=$(grep -Fn \
  '"$RELEASE_PYTHON" scripts/verify-auditable-binary.py' \
  "$release_build" | cut -d: -f1)
package_line=$(grep -Fn -- '- name: Rehearse release package' "$release_build" | cut -d: -f1)
if [[ ! "$touch_line" =~ ^[0-9]+$ ||
      ! "$auditable_build_line" =~ ^[0-9]+$ ||
      ! "$auditable_verify_line" =~ ^[0-9]+$ ||
      ! "$package_line" =~ ^[0-9]+$ ||
      "$touch_line" -ge "$auditable_build_line" ||
      "$auditable_build_line" -ge "$auditable_verify_line" ||
      "$auditable_verify_line" -ge "$package_line" ]]; then
  echo "native release must force an auditable relink and verify it before packaging" >&2
  exit 1
fi
# shellcheck disable=SC2016
[[ $(grep -Fc '"target/$RELEASE_TARGET/release/$RELEASE_BINARY"' \
      "$release_build") -eq 2 &&
    $(grep -Fc '"$PACKAGE_VERSION"' "$release_build") -eq 1 ]] || {
  echo "native audit verification must bind the matrix binary to the package version" >&2
  exit 1
}

for candidate in python3 python; do
  if command -v "$candidate" > /dev/null 2>&1 &&
    "$candidate" -c 'import sys; raise SystemExit(sys.version_info < (3, 8))' \
      > /dev/null 2>&1; then
    test_python=$candidate
    break
  fi
done
[[ -n "${test_python:-}" ]] || {
  echo "Python 3.8 or newer is required for native audit verification tests" >&2
  exit 1
}

audit_fixtures="$work/auditable"
mkdir -p "$audit_fixtures"
"$test_python" - "$audit_fixtures" "$version" <<'PY'
import json
import struct
import sys
import zlib
from pathlib import Path

destination = Path(sys.argv[1])
version = sys.argv[2]


def payload(root_version, include_unreachable=False):
    packages = [
        {
            "name": "mcp-repl",
            "version": root_version,
            "source": "local",
            "kind": "runtime",
            "dependencies": [1],
            "root": True,
        },
        {
            "name": "dependency",
            "version": "1.0.0",
            "source": "crates.io",
        },
    ]
    if include_unreachable:
        packages.append(
            {
                "name": "unreachable",
                "version": "1.0.0",
                "source": "crates.io",
            }
        )
    inventory = {
        "format": 1,
        "packages": packages,
    }
    return zlib.compress(json.dumps(inventory, separators=(",", ":")).encode())


def elf(audit_data):
    names = b"\0.dep-v0\0.shstrtab\0"
    audit_offset = 64
    names_offset = audit_offset + len(audit_data)
    section_offset = (names_offset + len(names) + 7) & ~7
    header = struct.pack(
        "<16sHHIQQQIHHHHHH",
        b"\x7fELF\x02\x01\x01" + bytes(9),
        2,
        62,
        1,
        0,
        0,
        section_offset,
        0,
        64,
        0,
        0,
        64,
        3,
        2,
    )
    section = struct.Struct("<IIQQQQIIQQ")
    sections = (
        bytes(section.size)
        + section.pack(1, 1, 0, 0, audit_offset, len(audit_data), 0, 0, 1, 0)
        + section.pack(9, 3, 0, 0, names_offset, len(names), 0, 0, 1, 0)
    )
    return header + audit_data + names + bytes(section_offset - names_offset - len(names)) + sections


def macho(audit_data):
    command_size = 72 + 80
    audit_offset = 32 + command_size
    header = struct.pack("<IiiIIIII", 0xFEEDFACF, 0x01000007, 3, 2, 1, command_size, 0, 0)
    segment = struct.pack(
        "<II16sQQQQiiII",
        0x19,
        command_size,
        b"__DATA",
        0,
        len(audit_data),
        audit_offset,
        len(audit_data),
        3,
        3,
        1,
        0,
    )
    section = struct.pack(
        "<16s16sQQIIIIIIII",
        b".dep-v0",
        b"__DATA",
        0,
        len(audit_data),
        audit_offset,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
    )
    return header + segment + section + audit_data


def pe(audit_data):
    pe_offset = 64
    optional_size = 240
    section_table = pe_offset + 24 + optional_size
    audit_offset = 512
    raw_size = (len(audit_data) + 511) & ~511
    dos = bytearray(pe_offset)
    dos[:2] = b"MZ"
    struct.pack_into("<I", dos, 60, pe_offset)
    coff = struct.pack("<HHIIIHH", 0x8664, 1, 0, 0, 0, optional_size, 0x22)
    optional = bytearray(optional_size)
    struct.pack_into("<H", optional, 0, 0x20B)
    section = struct.pack(
        "<8sIIIIIIHHI",
        b".dep-v0",
        len(audit_data),
        0,
        raw_size,
        audit_offset,
        0,
        0,
        0,
        0,
        0x40000040,
    )
    prefix = bytes(dos) + b"PE\0\0" + coff + bytes(optional) + section
    return prefix + bytes(audit_offset - len(prefix)) + audit_data + bytes(raw_size - len(audit_data))


valid = payload(version)
(destination / "valid.elf").write_bytes(elf(valid))
(destination / "valid.macho").write_bytes(macho(valid))
(destination / "valid.exe").write_bytes(pe(valid))
(destination / "wrong-root.elf").write_bytes(elf(payload("9.9.9")))
(destination / "unreachable.elf").write_bytes(
    elf(payload(version, include_unreachable=True))
)
(destination / "missing.elf").write_bytes(elf(valid).replace(b".dep-v0", b".no-v00", 1))
PY

auditable_verifier="$root/scripts/verify-auditable-binary.py"
for fixture in valid.elf valid.macho valid.exe; do
  "$test_python" "$auditable_verifier" \
    "$audit_fixtures/$fixture" mcp-repl "$version" > /dev/null
done
for fixture in wrong-root.elf unreachable.elf missing.elf; do
  if "$test_python" "$auditable_verifier" \
    "$audit_fixtures/$fixture" mcp-repl "$version" > /dev/null 2>&1; then
    echo "native audit verifier accepted invalid fixture: $fixture" >&2
    exit 1
  fi
done

cat > "$work/bin/gh" <<'STUB'
#!/bin/sh
set -eu

sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
release_head=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
release_base=cccccccccccccccccccccccccccccccccccccccc
mode=${GH_MODE:-valid}
tag=${TEST_TAG:?}
version=${TEST_VERSION:?}

pull_json() {
  state=open
  repository=test/project
  author='github-actions[bot]'
  author_type=Bot
  head_sha=$sha
  case "$mode" in
    closed) state=closed ;;
    cross_repo) repository=other/project ;;
    wrong_author) author=octocat; author_type=User ;;
    moved_head) head_sha=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb ;;
    bad_sha) head_sha=short ;;
  esac
  printf '{"number":42,"state":"%s","base":{"ref":"main"},"head":{"ref":"release-plz-v9","sha":"%s","repo":{"full_name":"%s"}},"user":{"login":"%s","type":"%s"}}' \
    "$state" "$head_sha" "$repository" "$author" "$author_type"
}

merge_json() {
  repository=test/project
  author='github-actions[bot]'
  author_type=Bot
  case "$mode" in
    merge_cross_repo) repository=other/project ;;
    merge_wrong_author) author=octocat; author_type=User ;;
  esac
  printf '{"number":42,"state":"closed","merged_at":"2026-08-21T00:00:00Z","merge_commit_sha":"%s","base":{"ref":"main","sha":"%s"},"head":{"ref":"release-plz-v9","sha":"%s","repo":{"full_name":"%s"}},"user":{"login":"%s","type":"%s"}}' \
    "$sha" "$release_base" "$release_head" "$repository" "$author" "$author_type"
}

if [ "${1:-}" = api ]; then
  shift
  case "${1:-}" in
    --paginate)
      case "$*" in
        "--paginate --slurp repos/test/project/pulls?state=open&base=main&per_page=100")
          case "$mode" in
            none) printf '[[]]\n' ;;
            malformed_list) printf '{}\n' ;;
            malformed_page) printf '[{}]\n' ;;
            multiple)
              one=$(pull_json)
              two=$(printf '%s' "$one" | sed 's/"number":42/"number":43/')
              printf '[[%s,%s]]\n' "$one" "$two"
              ;;
            paged_multiple)
              one=$(pull_json)
              two=$(printf '%s' "$one" | sed 's/"number":42/"number":43/')
              printf '[[%s],[%s]]\n' "$one" "$two"
              ;;
            trusted_and_cross_repo)
              one=$(pull_json)
              two=$(printf '%s' "$one" |
                sed 's/"number":42/"number":43/; s#"full_name":"test/project"#"full_name":"other/project"#')
              printf '[[%s,%s]]\n' "$one" "$two"
              ;;
            trusted_and_wrong_author)
              one=$(pull_json)
              two=$(printf '%s' "$one" |
                sed 's/"number":42/"number":43/; s/"login":"github-actions\[bot\]","type":"Bot"/"login":"octocat","type":"User"/')
              printf '[[%s,%s]]\n' "$one" "$two"
              ;;
            list_api_failure) echo "pull list unavailable" >&2; exit 1 ;;
            *) printf '[[%s]]\n' "$(pull_json)" ;;
          esac
          ;;
        "--paginate --slurp repos/test/project/commits/$sha/pulls?per_page=100")
          case "$mode" in
            merge_none) printf '[[]]\n' ;;
            merge_malformed_pulls) printf '{}\n' ;;
            merge_malformed_page) printf '[{}]\n' ;;
            merge_multiple)
              one=$(merge_json)
              two=$(printf '%s' "$one" | sed 's/"number":42/"number":43/')
              printf '[[%s,%s]]\n' "$one" "$two"
              ;;
            merge_paged_multiple)
              one=$(merge_json)
              two=$(printf '%s' "$one" | sed 's/"number":42/"number":43/')
              printf '[[%s],[%s]]\n' "$one" "$two"
              ;;
            merge_api_failure) echo "associated pulls unavailable" >&2; exit 1 ;;
            *) printf '[[%s]]\n' "$(merge_json)" ;;
          esac
          ;;
        --paginate\ repos/test/project/pulls/42/files?*)
          if [ "$mode" = files_api_failure ] || [ "$mode" = merge_files_api_failure ]; then
            echo "files API unavailable" >&2
            exit 1
          fi
          if [ "$mode" = bad_files ] || [ "$mode" = merge_bad_files ]; then
            printf '%s\n' CHANGELOG.md Cargo.toml README.md
          else
            printf '%s\n' Cargo.toml CHANGELOG.md Cargo.lock
          fi
          ;;
        *) echo "unexpected paginated API call: $*" >&2; exit 1 ;;
      esac
      ;;
    --method)
      if [ "$mode" = post_failure ]; then
        echo "status API unavailable" >&2
        exit 1
      fi
      printf '%s\n' "$*" >> "${GH_LOG:?}"
      ;;
    repos/test/project/pulls/42)
      if [ "$mode" = api_failure ]; then
        echo "pull API unavailable" >&2
        exit 1
      fi
      pull_json
      ;;
    repos/test/project/commits/*/statuses?*)
      if [ "$mode" = status_query_failure ]; then
        echo "status query unavailable" >&2
        exit 1
      fi
      if [ "${GH_GATE:-}" = merge ]; then
        case "$mode" in
          merge_malformed_status) printf '{}\n' ;;
          merge_no_status) printf '[]\n' ;;
          merge_failed_status)
            printf '[{"context":"Release gate","state":"failure","description":"Release validation failed","target_url":"https://github.com/test/project/actions/runs/99","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
          merge_foreign_status)
            printf '[{"context":"Release gate","state":"success","description":"Release validation passed","target_url":"https://github.com/test/project/actions/runs/99","creator":{"login":"octocat","type":"User"}}]\n'
            ;;
          merge_foreign_url)
            printf '[{"context":"Release gate","state":"success","description":"Release validation passed","target_url":"https://example.com/actions/runs/99","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
          merge_missing_url)
            printf '[{"context":"Release gate","state":"success","description":"Release validation passed","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
          *)
            printf '[{"context":"Release gate","state":"success","description":"Release validation passed","target_url":"https://github.com/test/project/actions/runs/99","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
        esac
      else
        case "${GH_CLAIM:-none}" in
          none) printf '[]\n' ;;
          malformed) printf '{}\n' ;;
          current)
            printf '[{"context":"Release gate","state":"pending","target_url":"https://github.com/test/project/actions/runs/99"}]\n'
            ;;
          owned_failure)
            printf '[{"context":"Release gate","state":"failure","target_url":"https://github.com/test/project/actions/runs/99"}]\n'
            ;;
          newer)
            printf '[{"context":"Release gate","state":"pending","target_url":"https://github.com/test/project/actions/runs/100"}]\n'
            ;;
          *) echo "unknown claim mode" >&2; exit 1 ;;
        esac
      fi
      ;;
    "repos/test/project/git/ref/tags/$tag")
      if [ "$mode" = tag_missing ]; then
        echo "tag not found" >&2
        exit 1
      fi
      if [ "$mode" = lightweight_tag ]; then
        printf '{"ref":"refs/tags/%s","object":{"type":"commit","sha":"%s"}}\n' "$tag" "$sha"
      else
        printf '{"ref":"refs/tags/%s","object":{"type":"tag","sha":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"}}\n' "$tag"
      fi
      ;;
    repos/test/project/git/tags/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee)
      tag_message="chore: Release package mcp-repl version $version"
      tag_source=$sha
      tagger=github-actions\[bot\]
      tagger_email='41898282+github-actions[bot]@users.noreply.github.com'
      case "$mode" in
        wrong_tag_message) tag_message='wrong message' ;;
        wrong_tag_object) tag_source=dddddddddddddddddddddddddddddddddddddddd ;;
        foreign_tagger) tagger=octocat ;;
        foreign_tagger_email) tagger_email=octocat@users.noreply.github.com ;;
        tag_object_failure) echo "tag object unavailable" >&2; exit 1 ;;
      esac
      printf '{"tag":"%s","message":"%s","object":{"type":"commit","sha":"%s"},"tagger":{"name":"%s","email":"%s"}}\n' \
        "$tag" "$tag_message" "$tag_source" "$tagger" "$tagger_email"
      ;;
    "repos/test/project/commits/$tag")
      if [ "$mode" = tag_resolve_failure ]; then
        echo "commit lookup unavailable" >&2
        exit 1
      fi
      case "$mode" in
        moved_tag) printf 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n' ;;
        invalid_tag_sha) printf 'short\n' ;;
        *) printf '%s\n' "$sha" ;;
      esac
      ;;
    "repos/test/project/releases/tags/$tag")
      body=$(jq -Rs . < "${TEST_EXPECTED_NOTES:?}")
      author='github-actions[bot]'
      author_type=Bot
      name=$tag
      draft=true
      immutable=false
      [ "$mode" = bad_release_draft ] && body='"wrong notes"'
      if [ "$mode" = published ] || [ "$mode" = bad_public_release ]; then
        draft=false
        immutable=true
      fi
      [ "$mode" = bad_public_release ] && body='"wrong public notes"'
      if [ "$mode" = foreign_release_draft ]; then
        author=octocat
        author_type=User
      fi
      printf '{"id":4242,"tag_name":"%s","name":"%s","body":%s,"draft":%s,"prerelease":false,"immutable":%s,"author":{"login":"%s","type":"%s"}}\n' \
        "$tag" "$name" "$body" "$draft" "$immutable" "$author" "$author_type"
      ;;
    repos/test/project/commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/pulls)
      case "$mode" in
        merge_none) printf '[]\n' ;;
        merge_malformed_pulls) printf '{}\n' ;;
        merge_multiple)
          one=$(merge_json)
          two=$(printf '%s' "$one" | sed 's/"number":42/"number":43/')
          printf '[%s,%s]\n' "$one" "$two"
          ;;
        merge_api_failure) echo "associated pulls unavailable" >&2; exit 1 ;;
        *) printf '[%s]\n' "$(merge_json)" ;;
      esac
      ;;
    repos/test/project/git/commits/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa)
      case "$mode" in
        merge_commit_api_failure) echo "commit unavailable" >&2; exit 1 ;;
        merge_commit_malformed) printf '{}\n' ;;
        merge_commit_two_parents)
          printf '{"sha":"%s","parents":[{"sha":"%s"},{"sha":"2"}],"tree":{"sha":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"}}\n' "$sha" "$release_base"
          ;;
        merge_commit_wrong_parent)
          printf '{"sha":"%s","parents":[{"sha":"dddddddddddddddddddddddddddddddddddddddd"}],"tree":{"sha":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"}}\n' "$sha"
          ;;
        *) printf '{"sha":"%s","parents":[{"sha":"%s"}],"tree":{"sha":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"}}\n' "$sha" "$release_base" ;;
      esac
      ;;
    repos/test/project/git/commits/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb)
      case "$mode" in
        merge_head_commit_api_failure) echo "head commit unavailable" >&2; exit 1 ;;
        merge_head_commit_malformed) printf '{}\n' ;;
        merge_wrong_tree)
          printf '{"sha":"%s","tree":{"sha":"dddddddddddddddddddddddddddddddddddddddd"}}\n' "$release_head"
          ;;
        *) printf '{"sha":"%s","tree":{"sha":"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"}}\n' "$release_head" ;;
      esac
      ;;
    repos/test/project/actions/runs/99)
      if [ "$mode" = merge_run_api_failure ]; then
        echo "workflow run unavailable" >&2
        exit 1
      fi
      if [ "$mode" = merge_malformed_run ]; then
        printf '{}\n'
        exit 0
      fi
      run_path=.github/workflows/release-plz.yml
      run_head=$release_base
      run_status=completed
      run_conclusion='"success"'
      case "$mode" in
        merge_wrong_run) run_path=.github/workflows/ci.yml ;;
        merge_wrong_base) run_head=dddddddddddddddddddddddddddddddddddddddd ;;
        merge_failed_run) run_conclusion='"failure"' ;;
        merge_pending_run) run_status=in_progress; run_conclusion=null ;;
        merge_retrying_run)
          if [ ! -f "${GH_RUN_STATE:?}" ]; then
            touch "$GH_RUN_STATE"
            run_status=in_progress
            run_conclusion=null
          fi
          ;;
      esac
      printf '{"id":99,"event":"push","status":"%s","conclusion":%s,"head_branch":"main","head_sha":"%s","path":"%s","html_url":"https://github.com/test/project/actions/runs/99","repository":{"full_name":"test/project"},"head_repository":{"full_name":"test/project"}}\n' \
        "$run_status" "$run_conclusion" "$run_head" "$run_path"
      ;;
    repos/test/project/actions/workflows/release-binaries.yml/runs?*)
      expected_runs_query="repos/test/project/actions/workflows/release-binaries.yml/runs?event=workflow_dispatch&head_sha=${sha}&per_page=100"
      if [ "$1" != "$expected_runs_query" ]; then
        echo "unexpected workflow-run filter: $1" >&2
        exit 1
      fi
      case "$mode" in
        run_query_failure) echo "workflow runs unavailable" >&2; exit 1 ;;
        active_dispatch) printf '1\n' ;;
        invalid_run_count) printf 'many\n' ;;
        *) printf '0\n' ;;
      esac
      ;;
    repos/test/project/pulls?*)
      case "$mode" in
        none) printf '[]\n' ;;
        malformed_list) printf '{}\n' ;;
        multiple)
          one=$(pull_json)
          two=$(printf '%s' "$one" | sed 's/"number":42/"number":43/')
          printf '[%s,%s]\n' "$one" "$two"
          ;;
        list_api_failure)
          echo "pull list unavailable" >&2
          exit 1
          ;;
        *) printf '[%s]\n' "$(pull_json)" ;;
      esac
      ;;
    *)
      echo "unexpected gh api call: $*" >&2
      exit 1
      ;;
  esac
  exit 0
fi

if [ "${1:-} ${2:-}" = "release view" ]; then
  case "$mode" in
    unreachable) echo "release API unavailable" >&2; exit 1 ;;
    published | bad_public_release) echo false ;;
    nonsense) echo banana ;;
    *) echo true ;;
  esac
  exit 0
fi

if [ "${1:-} ${2:-}" = "workflow run" ]; then
  if [ "$mode" = dispatch_failure ]; then
    echo "workflow API unavailable" >&2
    exit 1
  fi
  printf '%s\n' "$*" >> "${GH_LOG:?}"
  exit 0
fi

echo "unexpected gh call: $*" >&2
exit 1
STUB
chmod +x "$work/bin/gh"

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

check_discovery() {
  local name=$1 mode=$2 want_status=$3 want_text=$4 want_file_text=${5:-}
  local output status output_file="$work/github-output"
  : > "$output_file"
  set +e
  output=$(PATH="$work/bin:$PATH" GH_MODE="$mode" GH_LOG="$log" \
    GITHUB_REPOSITORY=test/project "$discover" "$output_file" 2>&1)
  status=$?
  set -e
  if ! assert_result "$name" "$status" "$want_status" "$output" "$want_text"; then
    return 0
  fi
  if [[ -n "$want_file_text" ]] && ! grep -Fq "$want_file_text" "$output_file"; then
    printf 'FAIL %s: output file did not contain %q\n' "$name" "$want_file_text" >&2
    failures=$((failures + 1))
  fi
}

check_merge() {
  local name=$1 mode=$2 want_status=$3 want_text=$4 want_file_text=${5:-}
  local attempts=${6:-1}
  local output status output_file="$work/merge-output"
  : > "$output_file"
  rm -f "$work/run-state"
  set +e
  output=$(PATH="$work/bin:$PATH" GH_MODE="$mode" GH_LOG="$log" \
    GH_GATE=merge GH_RUN_STATE="$work/run-state" GITHUB_SERVER_URL=https://github.com \
    RELEASE_GATE_MAX_ATTEMPTS="$attempts" RELEASE_GATE_RETRY_DELAY_SECONDS=0 \
    GITHUB_REPOSITORY=test/project GITHUB_SHA="$sha" \
    "$discover_merge" "$output_file" 2>&1)
  status=$?
  set -e
  if ! assert_result "$name" "$status" "$want_status" "$output" "$want_text"; then
    return 0
  fi
  if [[ -n "$want_file_text" ]] && ! grep -Fq "$want_file_text" "$output_file"; then
    printf 'FAIL %s: output file did not contain %q\n' "$name" "$want_file_text" >&2
    failures=$((failures + 1))
  fi
}

check_report() {
  local name=$1 mode=$2 want_status=$3 want_text=$4 should_post=$5 state=${6:-pending} claim=${7:-none}
  local output status
  : > "$log"
  set +e
  output=$(PATH="$work/bin:$PATH" GH_MODE="$mode" GH_CLAIM="$claim" GH_LOG="$log" \
    GITHUB_REPOSITORY=test/project "$report" 42 "$sha" "$state" \
    https://github.com/test/project/actions/runs/99 "Release CI $state" 2>&1)
  status=$?
  set -e
  if ! assert_result "$name" "$status" "$want_status" "$output" "$want_text"; then
    return 0
  fi
  if [[ "$should_post" == true ]]; then
    if ! grep -Fq "statuses/$sha" "$log" || ! grep -Fq 'context=Release gate' "$log"; then
      printf 'FAIL %s: expected status post was not recorded\n' "$name" >&2
      failures=$((failures + 1))
    fi
  elif [[ -s "$log" ]]; then
    printf 'FAIL %s: an untrusted status post was recorded\n' "$name" >&2
    failures=$((failures + 1))
  fi
}

check_dispatch() {
  local name=$1 mode=$2 want_status=$3 want_text=$4 should_dispatch=$5
  local output status
  : > "$log"
  set +e
  output=$(PATH="$work/bin:$PATH" GH_MODE="$mode" GH_LOG="$log" \
    TEST_TAG="$tag" TEST_VERSION="$version" TEST_EXPECTED_NOTES="$expected_notes" \
    GH_REPO=test/project "$dispatch" "$tag" "$sha" 2>&1)
  status=$?
  set -e
  if ! assert_result "$name" "$status" "$want_status" "$output" "$want_text"; then
    return 0
  fi
  if [[ "$should_dispatch" == true ]]; then
    if ! grep -Fq 'workflow run release-binaries.yml' "$log" ||
       ! grep -Fq -- "--ref $tag" "$log" ||
       ! grep -Fq -- "source_sha=$sha" "$log"; then
      printf 'FAIL %s: expected immutable-tag dispatch was not recorded\n' "$name" >&2
      failures=$((failures + 1))
    fi
  elif [[ -s "$log" ]]; then
    printf 'FAIL %s: an unexpected dispatch was recorded\n' "$name" >&2
    failures=$((failures + 1))
  fi
}

check_tag() {
  local name=$1 mode=$2 want_status=$3 want_text=$4
  local output status
  set +e
  output=$(PATH="$work/bin:$PATH" GH_MODE="$mode" GH_LOG="$log" \
    TEST_TAG="$tag" TEST_VERSION="$version" TEST_EXPECTED_NOTES="$expected_notes" \
    GH_REPO=test/project "$verify_tag" "$tag" "$sha" 2>&1)
  status=$?
  set -e
  if ! assert_result "$name" "$status" "$want_status" "$output" "$want_text"; then
    return 0
  fi
}

check_discovery "trusted release PR is discovered" valid 0 "Discovered trusted" "head_sha=$sha"
check_discovery "no release PR is a clean no-op" none 0 "No open" "pr_number="
check_discovery "multiple release PRs are refused" multiple 1 "found 2"
check_discovery "release PRs on multiple pages are all counted" paged_multiple 1 "found 2"
check_discovery "malformed release PR data is refused" malformed_list 1 "malformed pull-request data"
check_discovery "a malformed release PR page is refused" malformed_page 1 "malformed pull-request data"
check_discovery "cross-repository release lookalike is ignored" cross_repo 0 "No open" "pr_number="
check_discovery "wrong-author release lookalike is ignored" wrong_author 0 "No open" "pr_number="
check_discovery "a trusted PR wins over a fork lookalike" trusted_and_cross_repo 0 "Discovered trusted" "head_sha=$sha"
check_discovery "a trusted PR wins over a wrong-author lookalike" trusted_and_wrong_author 0 "Discovered trusted" "head_sha=$sha"
check_discovery "unexpected release files are refused" bad_files 1 "unexpected file set"

check_merge "a trusted release merge is discovered" valid 0 "exactly one trusted" "is_release_merge=true"
check_merge "an ordinary merge is a clean no-op" merge_none 0 "did not merge" "is_release_merge=false"
check_merge "multiple release merges are refused" merge_multiple 1 "found 2"
check_merge "release merges on multiple pages are all counted" merge_paged_multiple 1 "found 2"
check_merge "a cross-repository release merge is refused" merge_cross_repo 1 "trusted bot/repository"
check_merge "a wrong-author release merge is refused" merge_wrong_author 1 "trusted bot/repository"
check_merge "an associated-pull API failure is preserved" merge_api_failure 1 "Could not read"
check_merge "malformed associated-pull data is refused" merge_malformed_pulls 1 "malformed associated"
check_merge "a malformed associated-pull page is refused" merge_malformed_page 1 "malformed associated"
check_merge "a release commit API failure is preserved" merge_commit_api_failure 1 "Could not read release merge"
check_merge "malformed release commit data is refused" merge_commit_malformed 1 "one-parent"
check_merge "a two-parent release merge is refused" merge_commit_two_parents 1 "one-parent"
check_merge "a release commit on the wrong main base is refused" merge_commit_wrong_parent 1 "validated main base"
check_merge "a validated-head API failure is preserved" merge_head_commit_api_failure 1 "Could not read validated release head"
check_merge "malformed validated-head data is refused" merge_head_commit_malformed 1 "invalid commit data"
check_merge "a release commit with a different tree is refused" merge_wrong_tree 1 "does not match the validated release head"
check_merge "unexpected merged release files are refused" merge_bad_files 1 "unexpected file set"
check_merge "a merged release file API failure is preserved" merge_files_api_failure 1 "Could not list files"
check_merge "a missing classic release status is refused" merge_no_status 1 "not a trusted success"
check_merge "malformed classic status data is refused" merge_malformed_status 1 "malformed commit-status"
check_merge "a failed classic release status is refused" merge_failed_status 1 "not a trusted success"
check_merge "a foreign classic release status is refused" merge_foreign_status 1 "not a trusted success"
check_merge "a foreign release status URL is refused" merge_foreign_url 1 "untrusted target URL"
check_merge "a missing release status URL is refused" merge_missing_url 1 "malformed target URL"
check_merge "a status from the wrong workflow is refused" merge_wrong_run 1 "does not belong"
check_merge "a status from the wrong main base is refused" merge_wrong_base 1 "does not belong"
check_merge "a failed Release-plz run is refused" merge_failed_run 1 "does not belong"
check_merge "an unfinished Release-plz run is not trusted yet" merge_pending_run 1 "did not finish"
check_merge "the status-to-completed race is retried" merge_retrying_run 0 "exactly one trusted" "is_release_merge=true" 2
check_merge "malformed Release-plz run data is refused" merge_malformed_run 1 "malformed state"
check_merge "a Release-plz run API failure is preserved" merge_run_api_failure 1 "Could not read Release-plz"

check_report "pending status is posted to the exact head" valid 0 "Release gate: pending" true
check_report "success status is posted to the exact head" valid 0 "Release gate: success" true success current
check_report "a failed run can finalize successfully on retry" valid 0 "Release gate: success" true success owned_failure
check_report "a moved PR head is refused" moved_head 1 "no longer matches" false
check_report "a closed release PR is refused" closed 1 "no longer matches" false
check_report "a pull API failure is preserved" api_failure 1 "Could not read" false
check_report "a status API failure is preserved" post_failure 1 "Could not post" false
check_report "a status query failure is preserved" status_query_failure 1 "Could not read existing" false
check_report "malformed status data is refused" valid 1 "malformed commit-status" false pending malformed
check_report "an older pending run cannot steal the claim" valid 1 "newer release validation" false pending newer
check_report "an older final run cannot overwrite the claim" valid 1 "no longer owns" false success newer

check_dispatch "an unreachable release API is preserved" unreachable 1 "Could not read release" false
check_dispatch "a published release is an idempotent no-op" published 0 "already published" false
check_dispatch "bad published release metadata is refused" bad_public_release 1 "unexpected title or notes" false
check_dispatch "an invalid draft state is refused" nonsense 1 "unexpected draft state" false
check_dispatch "a run query failure is preserved" run_query_failure 1 "Could not check existing" false
check_dispatch "an invalid active-run count is refused" invalid_run_count 1 "invalid native publication" false
check_dispatch "an active publication is not duplicated" active_dispatch 0 "already active" false
check_dispatch "a draft with wrong notes is refused" bad_release_draft 1 "unexpected title or notes" false
check_dispatch "a foreign draft is refused" foreign_release_draft 1 "trusted draft boundary" false
check_dispatch "a dispatch API failure is preserved" dispatch_failure 1 "Could not dispatch" false
check_dispatch "a draft dispatches at its immutable tag" valid 0 "Dispatched native" true

check_tag "an unchanged release tag is accepted" valid 0 "still resolves"
check_tag "a missing release tag is refused" tag_missing 1 "Could not read release tag"
check_tag "a lightweight release tag is refused" lightweight_tag 1 "not one annotated"
check_tag "an unreadable annotated tag is refused" tag_object_failure 1 "Could not read annotated"
check_tag "a wrong annotated tag message is refused" wrong_tag_message 1 "does not match"
check_tag "an annotated tag for another commit is refused" wrong_tag_object 1 "does not match"
check_tag "a foreign annotated tagger is refused" foreign_tagger 1 "does not match"
check_tag "a foreign annotated tagger email is refused" foreign_tagger_email 1 "does not match"
check_tag "a tag lookup failure is preserved" tag_resolve_failure 1 "Could not resolve"
check_tag "a moved release tag is refused" moved_tag 1 "Release tag $tag moved"
check_tag "an invalid release commit is refused" invalid_tag_sha 1 "invalid commit"

if [[ "$failures" -ne 0 ]]; then
  echo "$failures release workflow guard check(s) failed" >&2
  exit 1
fi
echo "all release workflow guard checks passed"
