#!/usr/bin/env bash
# Exercise the trusted release-PR discovery, status, and dispatch boundaries.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
discover="$root/scripts/discover-release-pr.sh"
discover_merge="$root/scripts/discover-release-merge.sh"
report="$root/scripts/report-release-status.sh"
dispatch_validation="$root/scripts/dispatch-release-validation.sh"
verify_tag="$root/scripts/verify-release-tag.sh"
attach_release_main="$root/scripts/attach-release-main.sh"
sha=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
release_base=cccccccccccccccccccccccccccccccccccccccc
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
container_build="$root/.github/workflows/container-build.yml"
ci_workflow="$root/.github/workflows/ci.yml"
release_workflow="$root/.github/workflows/release-binaries.yml"
release_plz="$root/.github/workflows/release-plz.yml"
release_publish="$root/.github/workflows/release-publish.yml"
if grep -Fq 'github.event.pull_request.head.sha' "$ci_workflow"; then
  echo "CI must use one exact merge-or-release source for every release rehearsal" >&2
  exit 1
fi
# No API caller may select the workflow code that validates a release. A
# repository dispatch always runs CI and its local reusable workflows from the
# default-branch commit; the candidate SHA crosses only as authenticated data
# and is fetched without flowing into actions/checkout's ref input.
if grep -Fq 'release_ref' "$ci_workflow" ||
  grep -Fq 'workflow_call:' "$ci_workflow" ||
  grep -Fq 'workflow_dispatch:' "$ci_workflow" ||
  grep -Fq 'workflow_dispatch:' "$release_plz" ||
  grep -Eq 'ref:.*(client_payload|inputs\.source_sha)' \
    "$ci_workflow" "$release_build" "$container_build"; then
  echo "release validation workflow code must remain default-branch controlled" >&2
  exit 1
fi
if grep -Fq 'workflow_dispatch:' "$release_workflow" ||
  grep -Fq 'workflow_dispatch:' "$release_publish" ||
  grep -Eq 'ref:.*(client_payload|recovery_release_sha|RECOVERY_RELEASE_SHA)' \
    "$release_publish" "$release_workflow" ||
  grep -Eq 'git .*fetch.*(client_payload|recovery_release_sha|RECOVERY_RELEASE_SHA)' \
    "$release_publish" "$release_workflow" ||
  grep -Eq '(SOURCE_SHA|source_sha|source-digest):.*(recovery_release_sha|RECOVERY_RELEASE_SHA)' \
    "$release_publish" "$release_workflow" ||
  [[ $(grep -Fc 'workflow_call:' "$release_workflow") -ne 1 ||
     $(grep -Fc 'uses: ./.github/workflows/release-binaries.yml' \
       "$release_publish") -ne 1 ]]; then
  echo "binary publication must be called at the frozen trusted main event" >&2
  exit 1
fi
# These single-quoted patterns intentionally match literal workflow syntax.
# shellcheck disable=SC2016
if [[ $(grep -Fc 'repository_dispatch:' "$release_publish") -ne 1 ||
      $(grep -Fc 'types: [release_publish_recovery]' "$release_publish") -ne 1 ||
      $(grep -Fc './scripts/verify-release-recovery.sh pre-publish "$GITHUB_OUTPUT"' \
        "$release_publish") -ne 1 ||
      $(grep -Fc 'recovery_release_sha: ${{ needs.preflight.outputs.recovery_release_sha }}' \
        "$release_publish") -ne 1 ||
      $(grep -Fc 'RECOVERY_RELEASE_SHA: ${{ inputs.recovery_release_sha }}' \
        "$release_workflow") -ne 1 ||
      $(grep -Fc 'SOURCE_SHA: ${{ github.sha }}' "$release_workflow") -ne 7 ||
      $(grep -Fc 'post-publish "$recovery_output" "$RECOVERY_RELEASE_SHA"' \
        "$release_workflow") -ne 1 ]]; then
  echo "release recovery must carry authorization evidence without selecting release source" >&2
  exit 1
fi
reconcile_job=$(sed -n '/^  reconcile:/,/^  binaries:/p' "$release_publish")
if grep -Fq '      contents: write' <<<"$reconcile_job"; then
  echo "registry reconciliation must not receive GitHub publication authority" >&2
  exit 1
fi
version_job_line=$(grep -n '^  version_image:' "$release_workflow" | cut -d: -f1)
publish_job_line=$(grep -n '^  publish_release:' "$release_workflow" | cut -d: -f1)
tag_publish_line=$(grep -Fn 'publish-release-tag.sh' "$release_workflow" | cut -d: -f1)
version_job=$(sed -n '/^  version_image:/,/^  publish_release:/p' "$release_workflow")
publish_job=$(sed -n '/^  publish_release:/,/^  latest_image:/p' "$release_workflow")
if [[ ! "$version_job_line" =~ ^[0-9]+$ ||
      ! "$publish_job_line" =~ ^[0-9]+$ ||
      ! "$tag_publish_line" =~ ^[0-9]+$ ||
      "$version_job_line" -ge "$publish_job_line" ||
      "$publish_job_line" -ge "$tag_publish_line" ||
      $(grep -Fc 'retention-days: 90' "$release_workflow") -ne 1 ||
      $(grep -Fc '      - staging_smoke' <<<"$version_job") -ne 1 ||
      $(grep -Fc '      - assemble' <<<"$version_job") -ne 1 ||
      $(grep -Fc '      packages: write' <<<"$version_job") -ne 1 ||
      $(grep -Fc '      - version_image' <<<"$publish_job") -ne 1 ||
      $(grep -Fc '      contents: write' <<<"$publish_job") -ne 1 ||
      $(grep -Fc 'publish-release.sh stage' <<<"$publish_job") -ne 1 ||
      $(grep -Fc 'publish-release.sh finalize' <<<"$publish_job") -ne 1 ]]; then
  echo "tag and release publication must stay behind the complete versioned release set" >&2
  exit 1
fi
# These patterns intentionally match literal workflow and shell syntax.
# shellcheck disable=SC2016
[[ $(grep -Fc "ref: \${{ github.sha }}" "$ci_workflow") -eq 8 &&
    $(grep -Fc "ref: \${{ github.sha }}" "$release_build") -eq 2 &&
    $(grep -Fc "ref: \${{ github.sha }}" "$container_build") -eq 2 &&
    $(grep -Fc "ref: \${{ github.sha }}" "$release_workflow") -eq 9 &&
    $(grep -Fc 'repository_dispatch:' "$ci_workflow") -eq 1 &&
    $(grep -Fc 'types: [release_validation]' "$ci_workflow") -eq 1 &&
    $(grep -Fc 'git -c protocol.version=2 fetch --no-tags --depth=1 origin "$SOURCE_SHA"' \
      "$ci_workflow") -eq 7 &&
    $(grep -Fc 'inputs.source_sha || github.sha' "$release_build") -eq 2 &&
    $(grep -Fc 'inputs.source_sha || github.sha' "$container_build") -eq 2 &&
    $(grep -Fc 'needs.release-targets.outputs.source_sha' "$ci_workflow") -eq 1 &&
    $(grep -Fc "source_sha=\$(git rev-parse HEAD)" "$ci_workflow") -eq 1 &&
    $(grep -Fc "source_date_epoch=\$(git show -s --format=%ct HEAD)" "$ci_workflow") -eq 1 ]] || {
  echo "release source and epoch must come from frozen event checkouts" >&2
  exit 1
}
# shellcheck disable=SC2016
attach_main_line=$(grep -Fn './scripts/attach-release-main.sh "$GITHUB_SHA"' "$release_plz" |
  cut -d: -f1)
# shellcheck disable=SC2016
release_plz_update_line=$(grep -Fn '"$RUNNER_TEMP/release-plz" update' "$release_plz" |
  cut -d: -f1)
# release-plz requires an attached branch and upstream to resolve repository
# metadata. The attachment must preserve the frozen event SHA and happen before
# candidate generation.
# shellcheck disable=SC2016
if [[ ! "$attach_main_line" =~ ^[0-9]+$ ||
      ! "$release_plz_update_line" =~ ^[0-9]+$ ||
      "$attach_main_line" -ge "$release_plz_update_line" ||
      $(grep -Fc './scripts/attach-release-main.sh "$GITHUB_SHA"' "$release_plz") -ne 1 ||
      ! -x "$attach_release_main" ||
      $(grep -Fc "ref: \${{ github.sha }}" "$release_plz") -ne 2 ]]; then
  echo "release-plz must attach the frozen event commit to origin/main" >&2
  exit 1
fi

attachment_repo="$work/attachment-repo"
git clone --quiet --no-local "$root" "$attachment_repo"
(
  cd "$attachment_repo"
  frozen=$(git rev-parse HEAD)
  # A nested clone from GitHub's detached PR checkout may not advertise a
  # local main branch. Production fetches origin/main explicitly via
  # checkout's fetch-depth: 0, so seed the same remote-tracking topology here.
  git update-ref refs/remotes/origin/main "$frozen"
  git checkout --quiet --detach "$frozen"
  "$attach_release_main" "$frozen" >/dev/null
  [[ $(git symbolic-ref --short HEAD) == main &&
     $(git rev-parse HEAD) == "$frozen" &&
     $(git rev-parse --abbrev-ref --symbolic-full-name '@{upstream}') == origin/main ]]
) || {
  echo "release-plz attachment must execute against a detached exact SHA" >&2
  exit 1
}
if grep -Fq 'Swatinem/rust-cache' "$release_build"; then
  echo "final native release builds must not consume shared Actions caches" >&2
  exit 1
fi
if [[ $(grep -Fc 'Swatinem/rust-cache' "$ci_workflow") -ne 5 ||
      $(grep -Fc "if: needs.source.outputs.release_validation != 'true'" \
        "$ci_workflow") -ne 4 ]]; then
  echo "release validation must skip every shared CI cache" >&2
  exit 1
fi
[[ $(grep -Fc 'RELEASE_VALIDATION_COMPLETION_ATTEMPTS:-720' \
      "$dispatch_validation") -eq 1 &&
    $(grep -Fc 'completion_attempts" -gt 720' \
      "$dispatch_validation") -eq 1 &&
    $(grep -Fc 'RELEASE_VALIDATION_RETRY_DELAY_SECONDS:-10' \
      "$dispatch_validation") -eq 1 ]] || {
  echo "cache-free release validation must allow the reviewed two-hour window" >&2
  exit 1
}
# shellcheck disable=SC2016
[[ $(grep -Fc 'client_payload[validation_claim]' "$dispatch_validation") -eq 1 &&
    $(grep -Fc 'event_type=release_validation' "$dispatch_validation") -eq 1 &&
    $(grep -Fc 'event=repository_dispatch&branch=main' \
      "$dispatch_validation") -eq 1 &&
    $(grep -Fc 'dispatch-release-validation.sh' "$release_plz") -eq 1 &&
    $(grep -Fc 'producer_attempt: ${{ steps.identity.outputs.producer_attempt }}' \
      "$release_plz") -eq 1 &&
    $(grep -Fc 'RELEASE_PRODUCER_ATTEMPT: ${{ needs.release-pr.outputs.producer_attempt }}' \
      "$release_plz") -eq 2 &&
    $(grep -Fc 'retention-days: 90' "$release_plz") -eq 1 &&
    $(grep -Fc '"${VALIDATION_CLAIM##*-}" != "$EXPECTED_SOURCE"' \
      "$ci_workflow") -eq 1 &&
    -x "$dispatch_validation" ]] || {
  echo "release-plz must dispatch and bind default-branch-controlled CI" >&2
  exit 1
}

# A reusable workflow cannot elevate the token granted by its caller. The
# container build therefore inherits the caller's exact maximum: read-only in
# CI, and contents-read/package-write only in trusted release publication. The
# manifest job still narrows itself to contents-read explicitly.
if grep -Eq '^permissions:' "$container_build"; then
  echo "container workflow must inherit the caller's maximum permissions" >&2
  exit 1
fi
container_build_job=$(sed -n '/^  build:/,$p' "$container_build")
if grep -Eq '^    permissions:' <<<"$container_build_job"; then
  echo "container build job must not unconditionally elevate caller permissions" >&2
  exit 1
fi
ci_container_job=$(sed -n '/^  release-container:/,/^  release-gate:/p' "$ci_workflow")
release_container_job=$(sed -n '/^  container:/,/^  container_manifest:/p' \
  "$release_workflow")
if [[ $(grep -Fc '      contents: read' <<<"$ci_container_job") -ne 1 ]] ||
  grep -Fq '      packages: write' <<<"$ci_container_job"; then
  echo "container rehearsal caller must remain contents-read only" >&2
  exit 1
fi
if [[ $(grep -Fc '      contents: read' <<<"$release_container_job") -ne 1 ||
      $(grep -Fc '      packages: write' <<<"$release_container_job") -ne 1 ]]; then
  echo "container publication caller must grant its exact registry write boundary" >&2
  exit 1
fi

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
if grep -Fq 'cargo auditable --version' "$release_build" "$root/Dockerfile"; then
  echo "cargo-auditable identity must not be confused with Cargo's global version flag" >&2
  exit 1
fi
auditable_install_line=$(grep -Fn \
  "cargo install --locked --version \"\$CARGO_AUDITABLE_VERSION\" cargo-auditable" \
  "$release_build" | cut -d: -f1)
auditable_metadata_line=$(grep -Fn \
  "grep -Fx \"cargo-auditable v\$CARGO_AUDITABLE_VERSION:\"" \
  "$release_build" | cut -d: -f1)
auditable_command_line=$(grep -Fn 'command -v cargo-auditable > /dev/null' \
  "$release_build" | cut -d: -f1)
docker_auditable_install_line=$(grep -Fn \
  "cargo install --locked --version \"\$CARGO_AUDITABLE_VERSION\" cargo-auditable" \
  "$root/Dockerfile" | cut -d: -f1)
docker_auditable_metadata_line=$(grep -Fn \
  "grep -Fx \"cargo-auditable v\$CARGO_AUDITABLE_VERSION:\"" \
  "$root/Dockerfile" | cut -d: -f1)
docker_auditable_command_line=$(grep -Fn 'command -v cargo-auditable > /dev/null' \
  "$root/Dockerfile" | cut -d: -f1)
docker_auditable_build_line=$(grep -Fn 'cargo auditable build --release --locked' \
  "$root/Dockerfile" | cut -d: -f1)
if [[ ! "$auditable_install_line" =~ ^[0-9]+$ ||
      ! "$auditable_metadata_line" =~ ^[0-9]+$ ||
      ! "$auditable_command_line" =~ ^[0-9]+$ ||
      ! "$docker_auditable_install_line" =~ ^[0-9]+$ ||
      ! "$docker_auditable_metadata_line" =~ ^[0-9]+$ ||
      ! "$docker_auditable_command_line" =~ ^[0-9]+$ ||
      ! "$docker_auditable_build_line" =~ ^[0-9]+$ ||
      "$auditable_install_line" -ge "$auditable_metadata_line" ||
      "$auditable_metadata_line" -ge "$auditable_command_line" ||
      "$auditable_command_line" -ge "$auditable_build_line" ||
      "$docker_auditable_install_line" -ge "$docker_auditable_metadata_line" ||
      "$docker_auditable_metadata_line" -ge "$docker_auditable_command_line" ||
      "$docker_auditable_command_line" -ge "$docker_auditable_build_line" ]]; then
  echo "native and container builds must verify cargo-auditable's installed package metadata" >&2
  exit 1
fi

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
  head_branch=release-plz-v9
  base_sha=$release_base
  case "$mode" in
    closed | validation_closed) state=closed ;;
    cross_repo | validation_cross_repo) repository=other/project ;;
    wrong_author | validation_wrong_author) author=octocat; author_type=User ;;
    moved_head | validation_moved_pull) head_sha=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb ;;
    propagating_head)
      if [ ! -f "${GH_DISCOVERY_STATE:?}" ]; then
        touch "$GH_DISCOVERY_STATE"
        head_sha=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
      fi
      ;;
    validation_wrong_branch) head_branch=release-plz-other ;;
    report_wrong_base) base_sha=dddddddddddddddddddddddddddddddddddddddd ;;
    bad_sha) head_sha=short ;;
  esac
  printf '{"number":42,"state":"%s","base":{"ref":"main","sha":"%s"},"head":{"ref":"release-plz-v9","sha":"%s","repo":{"full_name":"%s"}},"user":{"login":"%s","type":"%s"}}' \
    "$state" "$base_sha" "$head_sha" "$repository" "$author" "$author_type" |
    sed "s/\"ref\":\"release-plz-v9\"/\"ref\":\"$head_branch\"/"
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

validation_run_json() {
  run_id=199
  run_attempt=1
  title=${TEST_VALIDATION_CLAIM:?}
  event=repository_dispatch
  path=.github/workflows/ci.yml
  branch=main
  head_sha=$release_base
  repository=test/project
  head_repository=test/project
  actor='github-actions[bot]'
  actor_type=Bot
  status=completed
  conclusion=success
  case "$mode" in
    validation_wrong_title) title='release-validation-other' ;;
    validation_wrong_event) event=workflow_dispatch ;;
    validation_wrong_path) path=.github/workflows/release-plz.yml ;;
    validation_wrong_run_branch) branch=release-plz-v9 ;;
    validation_wrong_run_sha | validation_mutated_run) head_sha=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb ;;
    validation_wrong_repo) repository=other/project ;;
    validation_wrong_head_repo) head_repository=other/project ;;
    validation_wrong_run_actor) actor=octocat; actor_type=User ;;
    validation_wrong_attempt) run_attempt=2 ;;
    validation_pending) status=in_progress; conclusion='' ;;
    validation_failed) conclusion=failure ;;
  esac
  printf '{"id":%s,"run_attempt":%s,"display_title":"%s","event":"%s","path":"%s","head_branch":"%s","head_sha":"%s","html_url":"https://github.com/test/project/actions/runs/%s","repository":{"full_name":"%s"},"head_repository":{"full_name":"%s"},"actor":{"login":"%s","type":"%s"},"status":"%s","conclusion":%s}\n' \
    "$run_id" "$run_attempt" "$title" "$event" "$path" "$branch" "$head_sha" "$run_id" \
    "$repository" "$head_repository" "$actor" "$actor_type" "$status" \
    "$(if [ -n "$conclusion" ]; then printf '"%s"' "$conclusion"; else printf null; fi)"
}

parent_run_json() {
  parent_id=${1:-98}
  parent_attempt=${2:-1}
  parent_path=.github/workflows/release-plz.yml
  parent_head=$release_base
  parent_branch=main
  parent_status=completed
  parent_conclusion=success
  parent_event=push
  parent_repository=test/project
  parent_head_repository=test/project
  case "$mode" in
    merge_wrong_parent_path|report_wrong_parent_path) parent_path=.github/workflows/ci.yml ;;
    merge_wrong_parent_head|report_wrong_parent_head) parent_head=dddddddddddddddddddddddddddddddddddddddd ;;
    merge_wrong_parent_branch|report_wrong_parent_branch) parent_branch=release-plz-v9 ;;
    merge_wrong_parent_event|report_wrong_parent_event) parent_event=pull_request ;;
    merge_wrong_parent_attempt) parent_attempt=2 ;;
    merge_wrong_parent_repo|report_wrong_parent_repo) parent_repository=other/project ;;
    merge_wrong_parent_head_repo|report_wrong_parent_head_repo) parent_head_repository=other/project ;;
    merge_failed_parent_run) parent_conclusion=failure ;;
    merge_pending_parent_run)
      parent_status=in_progress
      parent_conclusion=''
      ;;
    merge_retrying_parent_run)
      if [ ! -f "${GH_RUN_STATE:?}" ]; then
        touch "$GH_RUN_STATE"
        parent_status=in_progress
        parent_conclusion=''
      fi
      ;;
  esac
  printf '{"id":%s,"run_attempt":%s,"event":"%s","status":"%s","conclusion":%s,"head_branch":"%s","head_sha":"%s","path":"%s","html_url":"https://github.com/test/project/actions/runs/%s","repository":{"full_name":"%s"},"head_repository":{"full_name":"%s"}}\n' \
    "$parent_id" "$parent_attempt" "$parent_event" "$parent_status" \
    "$(if [ -n "$parent_conclusion" ]; then printf '"%s"' "$parent_conclusion"; else printf null; fi)" \
    "$parent_branch" "$parent_head" "$parent_path" "$parent_id" \
    "$parent_repository" "$parent_head_repository"
}

if [ "${1:-}" = api ]; then
  shift
  case "${1:-}" in
    --paginate)
      case "$*" in
        "--paginate --slurp repos/test/project/pulls?state=open&base=main&per_page=100")
          if [ -n "${GH_DISCOVERY_LOG:-}" ]; then
            printf 'list\n' >> "$GH_DISCOVERY_LOG"
          fi
          case "$mode" in
            none) printf '[[]]\n' ;;
            propagating_missing)
              if [ ! -f "${GH_DISCOVERY_STATE:?}" ]; then
                touch "$GH_DISCOVERY_STATE"
                printf '[[]]\n'
              else
                printf '[[%s]]\n' "$(pull_json)"
              fi
              ;;
            propagating_multiple)
              one=$(pull_json)
              if [ ! -f "${GH_DISCOVERY_STATE:?}" ]; then
                touch "$GH_DISCOVERY_STATE"
                one=$(printf '%s' "$one" | sed "s/$sha/$release_head/")
                printf '[[%s]]\n' "$one"
              else
                two=$(printf '%s' "$one" | sed 's/"number":42/"number":43/')
                printf '[[%s,%s]]\n' "$one" "$two"
              fi
              ;;
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
        "--paginate --slurp repos/test/project/pulls/42/files?per_page=100")
          if [ -n "${GH_DISCOVERY_LOG:-}" ]; then
            printf 'files\n' >> "$GH_DISCOVERY_LOG"
          fi
          if [ "$mode" = files_api_failure ] || [ "$mode" = merge_files_api_failure ]; then
            echo "files API unavailable" >&2
            exit 1
          fi
          case "$mode" in
            malformed_files) printf '{}\n' ;;
            malformed_file_page) printf '[{}]\n' ;;
            renamed_files)
              printf '%s\n' '[[{"filename":"CHANGELOG.md","status":"modified"},{"filename":"Cargo.lock","status":"modified"},{"filename":"Cargo.toml","status":"renamed","previous_filename":".github/workflows/ci.yml"}]]'
              ;;
            copied_files)
              printf '%s\n' '[[{"filename":"CHANGELOG.md","status":"modified"},{"filename":"Cargo.lock","status":"modified"},{"filename":"Cargo.toml","status":"copied","previous_filename":".github/workflows/ci.yml"}]]'
              ;;
            bad_files|merge_bad_files|validation_bad_files)
              printf '%s\n' '[[{"filename":"CHANGELOG.md","status":"modified"},{"filename":"Cargo.toml","status":"modified"},{"filename":"README.md","status":"modified"}]]'
              ;;
            *)
              printf '%s\n' '[[{"filename":"Cargo.toml","status":"modified"},{"filename":"CHANGELOG.md","status":"modified"},{"filename":"Cargo.lock","status":"modified"}]]'
              ;;
          esac
          ;;
        *) echo "unexpected paginated API call: $*" >&2; exit 1 ;;
      esac
      ;;
    --method)
      if [ "$mode" = post_failure ] ||
        [ "$mode" = validation_dispatch_failure ]; then
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
    repos/test/project/git/ref/heads/release-plz-v9)
      if [ "$mode" = validation_branch_api_failure ]; then
        echo "branch lookup unavailable" >&2
        exit 1
      fi
      branch_sha=$sha
      [ "$mode" = validation_moved_branch ] && \
        branch_sha=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
      printf '{"ref":"refs/heads/release-plz-v9","object":{"type":"commit","sha":"%s"}}\n' \
        "$branch_sha"
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
            printf '[{"context":"Release gate","state":"failure","description":"Release validation failed","target_url":"https://github.com/test/project/actions/runs/99/attempts/1","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
          merge_foreign_status)
            printf '[{"context":"Release gate","state":"success","description":"Release validation passed","target_url":"https://github.com/test/project/actions/runs/99/attempts/1","creator":{"login":"octocat","type":"User"}}]\n'
            ;;
          merge_foreign_url)
            printf '[{"context":"Release gate","state":"success","description":"Release validation passed","target_url":"https://example.com/actions/runs/99/attempts/1","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
          merge_missing_url)
            printf '[{"context":"Release gate","state":"success","description":"Release validation passed","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
          merge_bare_run_url)
            printf '[{"context":"Release gate","state":"success","description":"Release validation passed","target_url":"https://github.com/test/project/actions/runs/99","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
          merge_child_attempt_two)
            printf '[{"context":"Release gate","state":"success","description":"Release validation passed","target_url":"https://github.com/test/project/actions/runs/99/attempts/2","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
          *)
            printf '[{"context":"Release gate","state":"success","description":"Release validation passed","target_url":"https://github.com/test/project/actions/runs/99/attempts/1","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
        esac
      else
        case "${GH_CLAIM:-none}" in
          none) printf '[]\n' ;;
          malformed) printf '{}\n' ;;
          current)
            printf '[{"context":"Release gate","state":"pending","description":"Release CI pending","target_url":"https://github.com/test/project/actions/runs/98/attempts/1","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
          current_attempt_two)
            printf '[{"context":"Release gate","state":"pending","description":"Release CI pending","target_url":"https://github.com/test/project/actions/runs/98/attempts/2","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
          owned_failure)
            printf '[{"context":"Release gate","state":"failure","description":"Release CI failure","target_url":"https://github.com/test/project/actions/runs/98/attempts/1","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
          newer)
            printf '[{"context":"Release gate","state":"pending","description":"Release CI pending","target_url":"https://github.com/test/project/actions/runs/100/attempts/1","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
          newer_attempt)
            printf '[{"context":"Release gate","state":"pending","description":"Release CI pending","target_url":"https://github.com/test/project/actions/runs/98/attempts/2","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
          current_success)
            printf '[{"context":"Release gate","state":"success","description":"Release CI success","target_url":"https://github.com/test/project/actions/runs/199/attempts/1","creator":{"login":"github-actions[bot]","type":"Bot"}}]\n'
            ;;
          foreign_claim)
            printf '[{"context":"Release gate","state":"pending","description":"Release CI pending","target_url":"https://github.com/test/project/actions/runs/98/attempts/1","creator":{"login":"octocat","type":"User"}}]\n'
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
    repos/test/project/actions/runs/98/attempts/1)
      if [ "$mode" = merge_parent_run_api_failure ]; then
        echo "parent workflow run unavailable" >&2
        exit 1
      fi
      if [ "$mode" = merge_malformed_parent_run ]; then
        printf '{}\n'
        exit 0
      fi
      parent_run_json 98 1
      ;;
    repos/test/project/actions/runs/98/attempts/2)
      parent_run_json 98 2
      ;;
    repos/test/project/actions/runs/100/attempts/1)
      parent_run_json 100 1
      ;;
    repos/test/project/actions/runs/98)
      if [ "$mode" = report_parent_recheck_failure ] ||
        [ "$mode" = merge_parent_recheck_failure ]; then
        echo "current parent workflow run unavailable" >&2
        exit 1
      fi
      case "$mode" in
        report_newer_attempt|merge_newer_parent_attempt)
          parent_run_json 98 2
          ;;
        *) parent_run_json 98 1 ;;
      esac
      ;;
    repos/test/project/actions/runs/98/attempts/1/jobs?per_page=100)
      case "$mode" in
        merge_parent_jobs_api_failure)
          echo "parent workflow jobs unavailable" >&2
          exit 1
          ;;
        merge_malformed_parent_jobs) printf '{}\n' ;;
        merge_incomplete_parent_jobs)
          printf '{"total_count":2,"jobs":[{"name":"Report release validation","run_attempt":1,"status":"completed","conclusion":"success"}]}\n'
          ;;
        merge_missing_report_job) printf '{"total_count":0,"jobs":[]}\n' ;;
        merge_failed_report_job)
          printf '{"total_count":1,"jobs":[{"name":"Report release validation","run_attempt":1,"status":"completed","conclusion":"failure"}]}\n'
          ;;
        merge_duplicate_report_job)
          printf '{"total_count":2,"jobs":[{"name":"Report release validation","run_attempt":1,"status":"completed","conclusion":"success"},{"name":"Report release validation","run_attempt":1,"status":"completed","conclusion":"success"}]}\n'
          ;;
        merge_wrong_report_job_attempt)
          printf '{"total_count":1,"jobs":[{"name":"Report release validation","run_attempt":2,"status":"completed","conclusion":"success"}]}\n'
          ;;
        *)
          printf '{"total_count":1,"jobs":[{"name":"Report release validation","run_attempt":1,"status":"completed","conclusion":"success"}]}\n'
          ;;
      esac
      ;;
    repos/test/project/actions/runs/99/attempts/1)
      if [ "$mode" = merge_run_api_failure ]; then
        echo "workflow run unavailable" >&2
        exit 1
      fi
      if [ "$mode" = merge_malformed_run ]; then
        printf '{}\n'
        exit 0
      fi
      run_path=.github/workflows/ci.yml
      run_head=$release_base
      run_branch=main
      run_status=completed
      run_conclusion='"success"'
      run_event=repository_dispatch
      run_attempt=1
      run_actor='github-actions[bot]'
      run_actor_type=Bot
      run_title="release-validation-98-1-$release_head"
      case "$mode" in
        merge_wrong_run) run_path=.github/workflows/release-plz.yml ;;
        merge_wrong_base) run_head=dddddddddddddddddddddddddddddddddddddddd ;;
        merge_wrong_branch) run_branch=release-plz-v9 ;;
        merge_wrong_event) run_event=workflow_dispatch ;;
        merge_wrong_actor) run_actor=octocat; run_actor_type=User ;;
        merge_wrong_title) run_title='CI main' ;;
        merge_wrong_child_attempt) run_attempt=2 ;;
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
      printf '{"id":99,"run_attempt":%s,"display_title":"%s","event":"%s","status":"%s","conclusion":%s,"head_branch":"%s","head_sha":"%s","path":"%s","html_url":"https://github.com/test/project/actions/runs/99","repository":{"full_name":"test/project"},"head_repository":{"full_name":"test/project"},"actor":{"login":"%s","type":"%s"}}\n' \
        "$run_attempt" "$run_title" "$run_event" "$run_status" "$run_conclusion" \
        "$run_branch" "$run_head" "$run_path" "$run_actor" "$run_actor_type"
      ;;
    repos/test/project/actions/runs/99/attempts/1/jobs?per_page=100)
      case "$mode" in
        merge_jobs_api_failure) echo "workflow jobs unavailable" >&2; exit 1 ;;
        merge_malformed_jobs) printf '{}\n' ;;
        merge_incomplete_jobs)
          printf '{"total_count":2,"jobs":[{"name":"Release gate","run_attempt":1,"status":"completed","conclusion":"success"}]}\n'
          ;;
        merge_missing_gate) printf '{"total_count":0,"jobs":[]}\n' ;;
        merge_failed_gate)
          printf '{"total_count":1,"jobs":[{"name":"Release gate","run_attempt":1,"status":"completed","conclusion":"failure"}]}\n'
          ;;
        merge_duplicate_gate)
          printf '{"total_count":2,"jobs":[{"name":"Release gate","run_attempt":1,"status":"completed","conclusion":"success"},{"name":"Release gate","run_attempt":1,"status":"completed","conclusion":"success"}]}\n'
          ;;
        merge_wrong_gate_attempt)
          printf '{"total_count":1,"jobs":[{"name":"Release gate","run_attempt":2,"status":"completed","conclusion":"success"}]}\n'
          ;;
        *)
          printf '{"total_count":1,"jobs":[{"name":"Release gate","run_attempt":1,"status":"completed","conclusion":"success"}]}\n'
          ;;
      esac
      ;;
    repos/test/project/actions/workflows/ci.yml/runs?*)
      expected_validation_query="repos/test/project/actions/workflows/ci.yml/runs?event=repository_dispatch&branch=main&head_sha=${release_base}&per_page=100"
      if [ "$1" != "$expected_validation_query" ]; then
        echo "unexpected release-validation filter: $1" >&2
        exit 1
      fi
      case "$mode" in
        validation_runs_api_failure)
          echo "validation runs unavailable" >&2
          exit 1
          ;;
        validation_malformed_runs) printf '{}\n' ;;
        validation_no_run|validation_wrong_title|validation_wrong_event|\
          validation_wrong_path|validation_wrong_run_branch|\
          validation_wrong_run_sha|validation_wrong_repo|\
          validation_wrong_head_repo|validation_wrong_run_actor|\
          validation_wrong_attempt)
          printf '{"workflow_runs":[]}\n'
          ;;
        validation_duplicate_run)
          run=$(validation_run_json)
          printf '{"workflow_runs":[%s,%s]}\n' "$run" "$run"
          ;;
        validation_mutated_run)
          saved_mode=$mode
          mode=valid
          run=$(validation_run_json)
          mode=$saved_mode
          printf '{"workflow_runs":[%s]}\n' "$run"
          ;;
        *) printf '{"workflow_runs":[%s]}\n' "$(validation_run_json)" ;;
      esac
      ;;
    repos/test/project/actions/runs/199/attempts/1)
      if [ "$mode" = validation_run_api_failure ]; then
        echo "validation run unavailable" >&2
        exit 1
      fi
      if [ "$mode" = validation_malformed_run ]; then
        printf '{}\n'
      else
        validation_run_json
      fi
      ;;
    repos/test/project/actions/runs/199/attempts/1/jobs?per_page=100)
      case "$mode" in
        validation_jobs_api_failure) echo "validation jobs unavailable" >&2; exit 1 ;;
        validation_malformed_jobs) printf '{}\n' ;;
        validation_incomplete_jobs)
          printf '{"total_count":2,"jobs":[{"name":"Release gate","run_attempt":1,"status":"completed","conclusion":"success"}]}\n'
          ;;
        validation_missing_gate) printf '{"total_count":0,"jobs":[]}\n' ;;
        validation_failed_gate)
          printf '{"total_count":1,"jobs":[{"name":"Release gate","run_attempt":1,"status":"completed","conclusion":"failure"}]}\n'
          ;;
        validation_duplicate_gate)
          printf '{"total_count":2,"jobs":[{"name":"Release gate","run_attempt":1,"status":"completed","conclusion":"success"},{"name":"Release gate","run_attempt":1,"status":"completed","conclusion":"success"}]}\n'
          ;;
        validation_wrong_gate_attempt)
          printf '{"total_count":1,"jobs":[{"name":"Release gate","run_attempt":2,"status":"completed","conclusion":"success"}]}\n'
          ;;
        *) printf '{"total_count":1,"jobs":[{"name":"Release gate","run_attempt":1,"status":"completed","conclusion":"success"}]}\n' ;;
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

if [ "${1:-} ${2:-}" = "run download" ]; then
  if [ "$mode" = merge_identity_download_failure ]; then
    echo "identity artifact unavailable" >&2
    exit 1
  fi
  run_id=${3:-}
  shift 3
  artifact_name=
  artifact_dir=
  artifact_repo=
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --name)
        artifact_name=${2:-}
        shift 2
        ;;
      --dir)
        artifact_dir=${2:-}
        shift 2
        ;;
      --repo)
        artifact_repo=${2:-}
        shift 2
        ;;
      *)
        echo "unexpected run download argument: $1" >&2
        exit 1
        ;;
    esac
  done
  expected_identity_head=$release_head
  case "$mode" in
    validation_source*) expected_identity_head=$sha ;;
  esac
  expected_name="release-identity-98-1-$expected_identity_head"
  if [ "$run_id" != 98 ] || [ "$artifact_repo" != test/project ] ||
    [ "$artifact_name" != "$expected_name" ] || [ -z "$artifact_dir" ]; then
    echo "unexpected identity artifact request" >&2
    exit 1
  fi
  mkdir -p "$artifact_dir"
  if [ "$mode" = merge_symlink_identity ]; then
    ln -s /dev/null "$artifact_dir/release-identity.json"
    exit 0
  fi
  identity_head=$expected_identity_head
  identity_base=$release_base
  identity_attempt=1
  case "$mode" in
    validation_source_bad_identity) identity_head=dddddddddddddddddddddddddddddddddddddddd ;;
    merge_bad_identity) identity_head=dddddddddddddddddddddddddddddddddddddddd ;;
    merge_bad_identity_base) identity_base=dddddddddddddddddddddddddddddddddddddddd ;;
    merge_bad_identity_attempt) identity_attempt=2 ;;
    merge_malformed_identity)
      printf '%s\n' '{not-json' > "$artifact_dir/release-identity.json"
      exit 0
      ;;
  esac
  printf '{"schema_version":1,"repository":"test/project","parent_run_id":98,"parent_run_attempt":%s,"base_sha":"%s","pr_number":42,"head_branch":"release-plz-v9","head_sha":"%s"}\n' \
    "$identity_attempt" "$identity_base" "$identity_head" \
    > "$artifact_dir/release-identity.json"
  if [ "$mode" = merge_extra_identity ]; then
    printf '%s\n' unexpected > "$artifact_dir/extra.txt"
  fi
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

echo "unexpected gh call: $*" >&2
exit 1
STUB
chmod +x "$work/bin/gh"

source_resolver="$work/source-resolver.sh"
sed -n \
  '/^      - name: Authenticate the requested release candidate$/,/^  quality:$/p' \
  "$ci_workflow" |
  sed '1,/^        run: |$/d; /^  quality:$/d; s/^          //' \
    > "$source_resolver"
chmod +x "$source_resolver"

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

check_source_request() {
  local name=$1 mode=$2 want_status=$3 want_text=$4 event_name=${5:-repository_dispatch}
  local event_file="$work/source-event.json" output_file="$work/source-output"
  local runner_temp="$work/source-runner" output status expected
  rm -rf "$runner_temp"
  mkdir "$runner_temp"
  : > "$output_file"
  jq -n \
    --arg claim "release-validation-98-1-$sha" \
    --arg base "$release_base" \
    --arg head "$sha" '
      {
        client_payload: {
          schema_version: 1,
          validation_claim: $claim,
          parent_run_id: 98,
          parent_run_attempt: 1,
          base_sha: $base,
          pr_number: 42,
          head_branch: "release-plz-v9",
          head_sha: $head
        }
      }
    ' > "$event_file"
  set +e
  output=$(PATH="$work/bin:$PATH" GH_MODE="$mode" GH_LOG="$log" \
    TEST_TAG="$tag" TEST_VERSION="$version" TEST_EXPECTED_NOTES="$expected_notes" \
    GITHUB_EVENT_NAME="$event_name" GITHUB_EVENT_PATH="$event_file" \
    GITHUB_REF=refs/heads/main GITHUB_SHA="$release_base" \
    GITHUB_REPOSITORY=test/project GITHUB_SERVER_URL=https://github.com \
    GITHUB_OUTPUT="$output_file" RUNNER_TEMP="$runner_temp" \
    "$source_resolver" 2>&1)
  status=$?
  set -e
  if ! assert_result "$name" "$status" "$want_status" "$output" "$want_text"; then
    return 0
  fi
  if [[ "$want_status" == 0 && "$event_name" == repository_dispatch ]]; then
    expected=$'release_validation=true\nsource_sha='"$sha"$'\nbase_sha='"$release_base"$'\nvalidation_claim=release-validation-98-1-'"$sha"
    if [[ $(<"$output_file") != "$expected" ]]; then
      printf 'FAIL %s: source resolver outputs were not exact\n' "$name" >&2
      failures=$((failures + 1))
    fi
  elif [[ "$want_status" == 0 ]]; then
    expected=$'release_validation=false\nsource_sha='"$release_base"$'\nbase_sha='"$release_base"$'\nvalidation_claim='
    if [[ $(<"$output_file") != "$expected" ]]; then
      printf 'FAIL %s: ordinary CI source outputs were not exact\n' "$name" >&2
      failures=$((failures + 1))
    fi
  fi
}

check_discovery() {
  local name=$1 mode=$2 want_status=$3 want_text=$4 want_file_text=${5:-} expected_head=${6:-}
  local attempts=${7:-1}
  local want_calls=${8:-} output status output_file="$work/github-output"
  local discovery_log="$work/discovery-calls"
  : > "$output_file"
  : > "$discovery_log"
  rm -f "$work/discovery-state"
  set +e
  output=$(PATH="$work/bin:$PATH" GH_MODE="$mode" GH_LOG="$log" \
    GH_DISCOVERY_STATE="$work/discovery-state" \
    GH_DISCOVERY_LOG="$discovery_log" \
    GITHUB_REPOSITORY=test/project EXPECTED_RELEASE_HEAD_SHA="$expected_head" \
    RELEASE_PR_DISCOVERY_ATTEMPTS="$attempts" \
    RELEASE_PR_DISCOVERY_RETRY_DELAY_SECONDS=0 \
    "$discover" "$output_file" 2>&1)
  status=$?
  set -e
  if ! assert_result "$name" "$status" "$want_status" "$output" "$want_text"; then
    return 0
  fi
  if [[ -n "$want_file_text" ]] && ! grep -Fq "$want_file_text" "$output_file"; then
    printf 'FAIL %s: output file did not contain %q\n' "$name" "$want_file_text" >&2
    failures=$((failures + 1))
  fi
  if [[ "$want_status" != 0 && -s "$output_file" ]]; then
    printf 'FAIL %s: failed discovery wrote trusted outputs\n' "$name" >&2
    failures=$((failures + 1))
  fi
  if [[ -n "$want_calls" && $(<"$discovery_log") != "$want_calls" ]]; then
    printf 'FAIL %s: discovery calls were not exact\n' "$name" >&2
    failures=$((failures + 1))
  fi
}

check_merge() {
  local name=$1 mode=$2 want_status=$3 want_text=$4 want_file_text=${5:-}
  local attempts=${6:-1}
  local source_arg=${7:-} event_sha=${8:-$sha}
  local output status output_file="$work/merge-output"
  local -a command=("$discover_merge" "$output_file")
  [[ -z "$source_arg" ]] || command+=("$source_arg")
  : > "$output_file"
  rm -f "$work/run-state"
  set +e
  output=$(PATH="$work/bin:$PATH" GH_MODE="$mode" GH_LOG="$log" \
    GH_GATE=merge GH_RUN_STATE="$work/run-state" GITHUB_SERVER_URL=https://github.com \
    RELEASE_GATE_MAX_ATTEMPTS="$attempts" RELEASE_GATE_RETRY_DELAY_SECONDS=0 \
    GITHUB_REPOSITORY=test/project GITHUB_SHA="$event_sha" \
    "${command[@]}" 2>&1)
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
  local claim_override=${8:-} target_override=${9:-}
  local current_attempt=${10:-1}
  local producer_attempt=${11:-$current_attempt}
  local output status claim_url target_url
  : > "$log"
  claim_url="https://github.com/test/project/actions/runs/98/attempts/$current_attempt"
  target_url=$claim_url
  if [[ "$state" == success ]]; then
    target_url=https://github.com/test/project/actions/runs/199/attempts/1
  fi
  [[ -z "$claim_override" ]] || claim_url=$claim_override
  [[ -z "$target_override" ]] || target_url=$target_override
  set +e
  output=$(PATH="$work/bin:$PATH" GH_MODE="$mode" GH_CLAIM="$claim" GH_LOG="$log" \
    GITHUB_REPOSITORY=test/project GITHUB_SERVER_URL=https://github.com \
    GITHUB_RUN_ID=98 GITHUB_RUN_ATTEMPT="$current_attempt" \
    GITHUB_SHA=cccccccccccccccccccccccccccccccccccccccc \
    RELEASE_PRODUCER_ATTEMPT="$producer_attempt" \
    TEST_VALIDATION_CLAIM="release-validation-98-$current_attempt-$sha" \
    "$report" 42 "$sha" "$state" "$claim_url" "$target_url" \
    "Release CI $state" 2>&1)
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

check_validation() {
  local name=$1 mode=$2 want_status=$3 want_text=$4 should_dispatch=$5
  local output status expected_output output_file="$work/validation-output"
  local branch=release-plz-v9
  local claim="release-validation-98-1-$sha"
  : > "$log"
  : > "$output_file"
  set +e
  output=$(PATH="$work/bin:$PATH" GH_MODE="$mode" GH_LOG="$log" \
    TEST_TAG="$tag" TEST_VERSION="$version" TEST_EXPECTED_NOTES="$expected_notes" \
    TEST_VALIDATION_CLAIM="$claim" GITHUB_REPOSITORY=test/project \
    GITHUB_SERVER_URL=https://github.com GITHUB_RUN_ID=98 GITHUB_RUN_ATTEMPT=1 \
    GITHUB_SHA=cccccccccccccccccccccccccccccccccccccccc \
    RELEASE_VALIDATION_DISCOVERY_ATTEMPTS=1 \
    RELEASE_VALIDATION_COMPLETION_ATTEMPTS=1 \
    RELEASE_VALIDATION_RETRY_DELAY_SECONDS=0 \
    "$dispatch_validation" 42 "$branch" "$sha" "$claim" "$output_file" 2>&1)
  status=$?
  set -e
  if ! assert_result "$name" "$status" "$want_status" "$output" "$want_text"; then
    return 0
  fi
  if [[ "$should_dispatch" == true ]]; then
    if ! grep -Fq -- '--method POST repos/test/project/dispatches' "$log" ||
       ! grep -Fq -- 'event_type=release_validation' "$log" ||
       ! grep -Fq -- "client_payload[validation_claim]=$claim" "$log" ||
       ! grep -Fq -- 'client_payload[base_sha]=cccccccccccccccccccccccccccccccccccccccc' "$log" ||
       ! grep -Fq -- "client_payload[head_sha]=$sha" "$log" ||
       grep -Fq -- 'workflow run' "$log"; then
      printf 'FAIL %s: default-branch repository dispatch was not recorded\n' "$name" >&2
      failures=$((failures + 1))
    fi
  elif [[ -s "$log" ]]; then
    printf 'FAIL %s: release CI was dispatched across a failed boundary\n' "$name" >&2
    failures=$((failures + 1))
  fi
  if [[ "$want_status" == 0 ]]; then
    expected_output=$'gate_result=success\nvalidated_ref='"$sha"$'\nrun_id=199\nrun_url=https://github.com/test/project/actions/runs/199/attempts/1'
    if [[ $(<"$output_file") != "$expected_output" ]]; then
      printf 'FAIL %s: validation outputs were not exact\n' "$name" >&2
      failures=$((failures + 1))
    fi
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

check_source_request "a repository dispatch authenticates the exact candidate" \
  validation_source 0 ""
check_source_request "ordinary CI remains bound to its event source" \
  validation_source 0 "" pull_request
check_source_request "a dispatch with an untrusted parent is refused" \
  merge_wrong_parent_path 1 "no trusted parent"
check_source_request "a dispatch for a moved candidate is refused" \
  validation_moved_pull 1 "changed across its trusted identity"
check_source_request "a superseded parent attempt cannot authorize a dispatch" \
  report_newer_attempt 1 "superseded this dispatch"
check_source_request "a mismatched identity artifact cannot authorize source" \
  validation_source_bad_identity 1 "does not bind this dispatch"

check_discovery "trusted release PR is discovered" valid 0 "Discovered trusted" "head_sha=$sha"
check_discovery "no release PR is a clean no-op" none 0 "No open" "pr_number="
check_discovery "multiple release PRs are refused" multiple 1 "found 2" "" "$sha" 2 list
check_discovery "release PRs on multiple pages are all counted" paged_multiple 1 "found 2"
check_discovery "malformed release PR data is refused" malformed_list 1 \
  "malformed pull-request data" "" "$sha" 2 list
check_discovery "a malformed release PR page is refused" malformed_page 1 "malformed pull-request data"
check_discovery "cross-repository release lookalike is ignored" cross_repo 0 "No open" "pr_number="
check_discovery "wrong-author release lookalike is ignored" wrong_author 0 "No open" "pr_number="
check_discovery "a trusted PR wins over a fork lookalike" trusted_and_cross_repo 0 "Discovered trusted" "head_sha=$sha"
check_discovery "a trusted PR wins over a wrong-author lookalike" trusted_and_wrong_author 0 "Discovered trusted" "head_sha=$sha"
check_discovery "unexpected release files are refused" bad_files 1 "unexpected file set"
check_discovery "a renamed workflow cannot masquerade as a release file" renamed_files 1 "unexpected file set"
check_discovery "a copied workflow cannot masquerade as a release file" copied_files 1 "unexpected file set"
check_discovery "malformed release file data is refused" malformed_files 1 "malformed release-PR file data"
check_discovery "a malformed release file page is refused" malformed_file_page 1 "malformed release-PR file data"
check_discovery "a release PR API failure is preserved" list_api_failure 1 \
  "Could not list open release pull requests" "" "$sha" 2 list
check_discovery "a release file API failure is preserved" files_api_failure 1 \
  "Could not list files" "" "$sha" 2 $'list\nfiles'
check_discovery "the generated release head is bound exactly" valid 0 \
  "Discovered trusted" "head_sha=$sha" "$sha"
check_discovery "a just-pushed release head is retried until visible" \
  propagating_head 0 "Discovered trusted" "head_sha=$sha" "$sha" 2 \
  $'list\nlist\nfiles'
check_discovery "a just-created release PR is retried until visible" \
  propagating_missing 0 "Discovered trusted" "head_sha=$sha" "$sha" 2 \
  $'list\nlist\nfiles'
check_discovery "a duplicate release PR appearing during retry is refused" \
  propagating_multiple 1 "found 2" "" "$sha" 2 $'list\nlist'
check_discovery "a persistently raced generated release head is refused" moved_head 1 \
  "differs from generated" "" "$sha" 2 $'list\nlist'
check_discovery "a missing generated release PR is refused after retry" none 1 \
  "has no trusted pull request" "" "$sha" 2 $'list\nlist'
check_discovery "an invalid release-PR retry policy is refused" valid 2 \
  "Invalid release-PR discovery retry policy" "" "$sha" 0

check_merge "a trusted release merge is discovered" valid 0 "exactly one trusted" "is_release_merge=true"
check_merge "an explicit release SHA overrides only merge authorization" valid 0 \
  "exactly one trusted" "is_release_merge=true" 1 "$sha" "$release_base"
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
check_merge "a bare child run URL is refused" merge_bare_run_url 1 "does not name one workflow run attempt"
check_merge "a rerun child attempt cannot replace dispatched evidence" merge_child_attempt_two 1 "not the dispatched child run's first attempt"
check_merge "a status from the wrong workflow is refused" merge_wrong_run 1 "does not belong"
check_merge "a child run for the wrong release head is refused" merge_wrong_base 1 "does not belong"
check_merge "a child run on the wrong branch is refused" merge_wrong_branch 1 "does not belong"
check_merge "a child run from the wrong event is refused" merge_wrong_event 1 "does not belong"
check_merge "a child run from the wrong actor is refused" merge_wrong_actor 1 "does not belong"
check_merge "a child run with the wrong claim is refused" merge_wrong_title 1 "does not belong"
check_merge "a child endpoint reporting another attempt is refused" merge_wrong_child_attempt 1 "does not belong"
check_merge "a failed release-validation run is refused" merge_failed_run 1 "did not succeed"
check_merge "an unfinished release-validation run is not trusted yet" merge_pending_run 1 "did not finish"
check_merge "the status-to-completed race is retried" merge_retrying_run 0 "exactly one trusted" "is_release_merge=true" 2
check_merge "malformed release-validation run data is refused" merge_malformed_run 1 "malformed state"
check_merge "a release-validation run API failure is preserved" merge_run_api_failure 1 "Could not read release-validation"
check_merge "a child claim from the wrong parent workflow is refused" merge_wrong_parent_path 1 "trusted Release-plz parent"
check_merge "a child claim from the wrong parent source is refused" merge_wrong_parent_head 1 "trusted Release-plz parent"
check_merge "a child claim from the wrong parent branch is refused" merge_wrong_parent_branch 1 "trusted Release-plz parent"
check_merge "a child claim from the wrong parent event is refused" merge_wrong_parent_event 1 "trusted Release-plz parent"
check_merge "a child claim from the wrong parent attempt is refused" merge_wrong_parent_attempt 1 "trusted Release-plz parent"
check_merge "a child claim from another parent repository is refused" merge_wrong_parent_repo 1 "trusted Release-plz parent"
check_merge "a child claim with another parent head repository is refused" merge_wrong_parent_head_repo 1 "trusted Release-plz parent"
check_merge "a failed claiming parent run is refused" merge_failed_parent_run 1 "did not succeed"
check_merge "an unfinished claiming parent run is refused" merge_pending_parent_run 1 "did not finish"
check_merge "the parent status-to-completed race is retried" merge_retrying_parent_run 0 "exactly one trusted" "is_release_merge=true" 2
check_merge "a claiming parent run API failure is preserved" merge_parent_run_api_failure 1 "Could not read claiming"
check_merge "malformed claiming parent data is refused" merge_malformed_parent_run 1 "trusted Release-plz parent"
check_merge "a missing parent reporting job is refused" merge_missing_report_job 1 "lacks one exact successful reporting"
check_merge "a failed parent reporting job is refused" merge_failed_report_job 1 "lacks one exact successful reporting"
check_merge "duplicate parent reporting jobs are refused" merge_duplicate_report_job 1 "lacks one exact successful reporting"
check_merge "a reporting job from another parent attempt is refused" merge_wrong_report_job_attempt 1 "lacks one exact successful reporting"
check_merge "incomplete parent job data is refused" merge_incomplete_parent_jobs 1 "malformed claiming Release-plz job data"
check_merge "malformed parent job data is refused" merge_malformed_parent_jobs 1 "malformed claiming Release-plz job data"
check_merge "a parent jobs API failure is preserved" merge_parent_jobs_api_failure 1 "Could not read jobs for claiming"
check_merge "a newer parent rerun supersedes old evidence" merge_newer_parent_attempt 1 "newer Release-plz attempt"
check_merge "a parent recheck failure is preserved" merge_parent_recheck_failure 1 "Could not confirm the current"
check_merge "a missing release identity artifact is refused" merge_identity_download_failure 1 "Could not download"
check_merge "a release identity with the wrong head is refused" merge_bad_identity 1 "does not bind this release"
check_merge "a release identity with the wrong base is refused" merge_bad_identity_base 1 "does not bind this release"
check_merge "a release identity with the wrong attempt is refused" merge_bad_identity_attempt 1 "does not bind this release"
check_merge "malformed release identity data is refused" merge_malformed_identity 1 "does not bind this release"
check_merge "extra release identity files are refused" merge_extra_identity 1 "unexpected file set"
check_merge "a linked release identity file is refused" merge_symlink_identity 1 "unexpected file set"
check_merge "a missing release gate job is refused" merge_missing_gate 1 "lacks one exact successful"
check_merge "a failed release gate job is refused" merge_failed_gate 1 "lacks one exact successful"
check_merge "duplicate release gate jobs are refused" merge_duplicate_gate 1 "lacks one exact successful"
check_merge "a gate job from another child attempt is refused" merge_wrong_gate_attempt 1 "lacks one exact successful"
check_merge "incomplete release-validation job data is refused" merge_incomplete_jobs 1 "malformed release-validation job data"
check_merge "malformed release-validation job data is refused" merge_malformed_jobs 1 "malformed release-validation job data"
check_merge "a release-validation jobs API failure is preserved" merge_jobs_api_failure 1 "Could not read jobs"

check_report "pending status is posted to the exact head" valid 0 "Release gate: pending" true
check_report "success status is posted to the exact head" valid 0 "Release gate: success" true success current
check_report "failure status is posted from the exact pending claim" valid 0 "Release gate: failure" true failure current
check_report "an exact pending retry is idempotent" valid 0 "already records pending" false pending current
check_report "an exact success retry is idempotent" valid 0 "already records success" false success current_success
check_report "an exact failure retry is idempotent" valid 0 "already records failure" false failure owned_failure
check_report "a completed failure cannot be upgraded within its attempt" valid 1 "no longer owns" false success owned_failure
check_report "a failed-job-only reporter rerun cannot reuse the old claim" valid 1 "no longer owns" false success owned_failure "" "" 2
check_report "a failed-job-only pending rerun cannot mint a new claim" report_newer_attempt 2 "producer attempt" false pending none "" "" 2 1
check_report "a fresh all-jobs attempt can post its pending claim" report_newer_attempt 0 "Release gate: pending" true pending none "" "" 2
check_report "a fresh all-jobs attempt can finalize its own claim" report_newer_attempt 0 "Release gate: success" true success current_attempt_two "" "" 2
check_report "a success cannot be downgraded to pending" valid 1 "cannot replace" false pending current_success
check_report "a success cannot be downgraded to failure" valid 1 "no longer owns" false failure current_success
check_report "a foreign bot claim is refused" valid 1 "untrusted creator" false pending foreign_claim
check_report "a moved PR head is refused" moved_head 1 "no longer matches" false
check_report "a closed release PR is refused" closed 1 "no longer matches" false
check_report "a pull API failure is preserved" api_failure 1 "Could not read" false
check_report "a status API failure is preserved" post_failure 1 "Could not post" false
check_report "a status query failure is preserved" status_query_failure 1 "Could not read existing" false
check_report "malformed status data is refused" valid 1 "malformed commit-status" false pending malformed
check_report "an older pending run cannot steal the claim" valid 1 "newer release validation" false pending newer
check_report "an older final run cannot overwrite the claim" valid 1 "newer release validation" false success newer
check_report "a newer attempt cannot be overwritten" valid 1 "newer release validation" false pending newer_attempt
check_report "a stale reporter is rejected at the final parent recheck" report_newer_attempt 1 "newer Release-plz attempt" false
check_report "a release PR on another main base is refused" report_wrong_base 1 "invalid parent source" false
check_report "a reporter from the wrong workflow is refused" report_wrong_parent_path 1 "does not belong" false
check_report "a parent recheck API failure is preserved" report_parent_recheck_failure 1 "newer Release-plz attempt" false
check_report "a bare claim URL is refused" valid 2 "Invalid release-status run URL" false pending none \
  https://github.com/test/project/actions/runs/98
check_report "a bare success target URL is refused" valid 2 "Invalid release-status run URL" false success none "" \
  https://github.com/test/project/actions/runs/199
check_report "a cross-repository claim URL is refused" valid 2 "do not belong" false pending none \
  https://github.com/other/project/actions/runs/98/attempts/1
check_report "a pending status cannot target a child" valid 2 "must target its claiming" false pending none "" \
  https://github.com/test/project/actions/runs/199/attempts/1
check_report "a success cannot target its parent claim" valid 2 "must target its validated child" false success none "" \
  https://github.com/test/project/actions/runs/98/attempts/1

check_validation "default-branch-controlled release validation is accepted" valid 0 "passed for" true
check_validation "a raced release PR head is refused before dispatch" validation_moved_pull 1 "no longer matches" false
check_validation "a raced release branch is refused before dispatch" validation_moved_branch 1 "no longer resolves" false
check_validation "unexpected release files are refused before dispatch" validation_bad_files 1 "unexpected file set" false
check_validation "a release branch lookup failure is preserved" validation_branch_api_failure 1 "Could not read release branch" false
check_validation "a release CI dispatch failure is preserved" validation_dispatch_failure 1 "Could not dispatch" false
check_validation "a release CI discovery failure is preserved" validation_runs_api_failure 1 "Could not discover" true
check_validation "malformed release CI run data is refused" validation_malformed_runs 1 "malformed release-validation run data" true
check_validation "a missing claimed child run times out" validation_no_run 1 "did not appear" true
check_validation "duplicate claimed child runs are refused" validation_duplicate_run 1 "matched 2" true
check_validation "a wrong child workflow is not claimed" validation_wrong_path 1 "did not appear" true
check_validation "a wrong child source is not claimed" validation_wrong_run_sha 1 "did not appear" true
check_validation "a wrong child actor is not claimed" validation_wrong_run_actor 1 "did not appear" true
check_validation "a rerun child attempt is not claimed" validation_wrong_attempt 1 "did not appear" true
check_validation "a mutated claimed child run is refused" validation_mutated_run 1 "crossed its trusted identity" true
check_validation "a child run API failure is preserved" validation_run_api_failure 1 "Could not read release-validation run" true
check_validation "an unfinished child run times out" validation_pending 1 "did not finish" true
check_validation "a failed child run is refused" validation_failed 1 "did not succeed" true
check_validation "a missing child release gate is refused" validation_missing_gate 1 "lacks one exact successful" true
check_validation "a failed child release gate is refused" validation_failed_gate 1 "lacks one exact successful" true
check_validation "duplicate child release gates are refused" validation_duplicate_gate 1 "lacks one exact successful" true
check_validation "a gate from another child attempt is refused" validation_wrong_gate_attempt 1 "lacks one exact successful" true
check_validation "incomplete child job data is refused" validation_incomplete_jobs 1 "malformed release-validation job data" true
check_validation "malformed child job data is refused" validation_malformed_jobs 1 "malformed release-validation job data" true
check_validation "a child jobs API failure is preserved" validation_jobs_api_failure 1 "Could not read jobs" true

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
