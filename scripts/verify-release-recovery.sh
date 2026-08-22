#!/usr/bin/env bash
# Authenticate an exceptional source-release recovery without letting its
# request choose executable workflow code or the source that will be released.
set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <pre-publish|post-publish> <github-output-file> [expected-release-merge-sha]" >&2
  exit 2
fi

phase=$1
case "$phase" in
  pre-publish)
    if [[ $# -ne 2 ]]; then
      echo "usage: $0 pre-publish <github-output-file>" >&2
      exit 2
    fi
    ;;
  post-publish)
    if [[ $# -ne 3 || ! "$3" =~ ^[0-9a-f]{40}$ ]]; then
      echo "usage: $0 post-publish <github-output-file> <expected-release-merge-sha>" >&2
      exit 2
    fi
    ;;
  *)
    echo "Invalid release-recovery phase: $phase" >&2
    exit 2
    ;;
esac
output_file=$2
expected_release_merge=${3:-}

root=$(cd "$(dirname "$0")/.." && pwd)
repository=${GITHUB_REPOSITORY:-}
source_sha=${GITHUB_SHA:-}
event_path=${GITHUB_EVENT_PATH:-}
if [[ "$repository" != "joshrotenberg/mcp-repl" ||
      ! "$source_sha" =~ ^[0-9a-f]{40}$ ||
      "${GITHUB_EVENT_NAME:-}" != repository_dispatch ||
      "${GITHUB_REF:-}" != refs/heads/main ||
      "${GITHUB_SERVER_URL:-}" != https://github.com ||
      "${GITHUB_ACTOR:-}" != joshrotenberg ||
      "${GITHUB_TRIGGERING_ACTOR:-}" != joshrotenberg ||
      ! -f "$event_path" || -L "$event_path" ||
      ! -f "$output_file" || -L "$output_file" ||
      "$event_path" -ef "$output_file" ]]; then
  echo "Invalid release-recovery environment" >&2
  exit 2
fi

checked_out_sha=$(git -C "$root" rev-parse HEAD)
if [[ "$checked_out_sha" != "$source_sha" ]]; then
  echo "Recovery checkout $checked_out_sha does not match event source $source_sha" >&2
  exit 1
fi

if ! request=$(jq -cer \
  --arg repository "$repository" '
    select(type == "object") |
    select(.action == "release_publish_recovery") |
    select(.repository.full_name == $repository) |
    select(.repository.default_branch == "main") |
    select(.sender.login == "joshrotenberg" and .sender.type == "User") |
    .client_payload as $payload |
    select(($payload | type) == "object") |
    select(($payload | keys) == [
      "release_merge_sha",
      "run_attempt",
      "run_id",
      "schema_version"
    ]) |
    select($payload.schema_version == 1) |
    select(
      ($payload.release_merge_sha | type) == "string" and
      ($payload.release_merge_sha | test("^[0-9a-f]{40}$"))
    ) |
    select(
      ($payload.run_id | type) == "number" and
      $payload.run_id == ($payload.run_id | floor) and
      $payload.run_id >= 1 and
      $payload.run_id <= 9007199254740991
    ) |
    select(
      ($payload.run_attempt | type) == "number" and
      $payload.run_attempt == ($payload.run_attempt | floor) and
      $payload.run_attempt >= 1 and
      $payload.run_attempt <= 1000
    ) |
    $payload
  ' "$event_path"); then
  echo "Release-recovery request is malformed or outside the trusted repository boundary" >&2
  exit 1
fi

release_merge_sha=$(jq -r '.release_merge_sha' <<<"$request")
run_id=$(jq -r '.run_id' <<<"$request")
run_attempt=$(jq -r '.run_attempt' <<<"$request")
if [[ -n "$expected_release_merge" &&
      "$expected_release_merge" != "$release_merge_sha" ]]; then
  echo "Recovery request changed its trusted release merge between stages" >&2
  exit 1
fi
if [[ "$release_merge_sha" == "$source_sha" ]]; then
  echo "Recovery must execute reviewed control-plane code newer than the failed release merge" >&2
  exit 1
fi
if ! git -C "$root" cat-file -e "${release_merge_sha}^{commit}" 2> /dev/null ||
   ! git -C "$root" merge-base --is-ancestor "$release_merge_sha" "$source_sha"; then
  echo "Failed release merge is not an ancestor of the recovery source" >&2
  exit 1
fi

# Recovery may update only the reviewed control plane and its tests/docs. This
# exact allowlist is intentionally narrower than directories: a change to any
# package, native archive, container, toolchain, or release-record input forces
# a fresh version instead of silently changing 0.3.1.
changed_paths=$(mktemp)
work=$(mktemp -d)
trap 'rm -f "$changed_paths"; rm -rf "$work"' EXIT INT TERM
git -C "$root" diff --no-renames --name-only -z \
  "$release_merge_sha" "$source_sha" -- > "$changed_paths"
unexpected_change=false
while IFS= read -r -d '' path; do
  case "$path" in
    .github/workflows/ci.yml | \
    .github/workflows/release-binaries.yml | \
    .github/workflows/release-publish.yml | \
    docs/releases.md | \
    scripts/discover-release-merge.sh | \
    scripts/reconcile-source-release.sh | \
    scripts/select-run-artifacts.sh | \
    scripts/test-release-recovery.sh | \
    scripts/test-select-run-artifacts.sh | \
    scripts/test-release-workflow.sh | \
    scripts/test-source-release.sh | \
    scripts/test-validate-oci-attestation.py | \
    scripts/validate-oci-attestation.py | \
    scripts/verify-release-recovery.sh)
      ;;
    *)
      printf 'Recovery source changes release input outside the reviewed allowlist: %q\n' \
        "$path" >&2
      unexpected_change=true
      ;;
  esac
done < "$changed_paths"
if [[ "$unexpected_change" == true ]]; then
  echo "Cut a fresh version instead of recovering changed release inputs" >&2
  exit 1
fi

release_merge_output="$work/release-merge-output"
: > "$release_merge_output"
"$root/scripts/discover-release-merge.sh" \
  "$release_merge_output" "$release_merge_sha"
if [[ $(<"$release_merge_output") != is_release_merge=true ]]; then
  echo "Recovery request does not identify exactly one trusted release merge" >&2
  exit 1
fi

if ! run=$(gh api \
  "repos/${repository}/actions/runs/${run_id}/attempts/${run_attempt}"); then
  echo "Could not read the failed Release publish run attempt" >&2
  exit 1
fi
if ! jq -e \
  --arg repository "$repository" \
  --arg release_merge_sha "$release_merge_sha" \
  --argjson run_id "$run_id" \
  --argjson run_attempt "$run_attempt" '
    select(type == "object") |
    select(.id == $run_id) |
    select(.run_attempt == $run_attempt) |
    select(.event == "push") |
    select(.status == "completed" and .conclusion == "failure") |
    select(.head_branch == "main") |
    select(.head_sha == $release_merge_sha) |
    select(.path == ".github/workflows/release-publish.yml") |
    select(.repository.full_name == $repository) |
    select(.head_repository.full_name == $repository) |
    select(.actor.login == "joshrotenberg" and .actor.type == "User") |
    select(
      .triggering_actor.login == "joshrotenberg" and
      .triggering_actor.type == "User"
    ) |
    select(
      .html_url ==
      ("https://github.com/" + $repository + "/actions/runs/" + ($run_id | tostring))
    )
  ' <<<"$run" > /dev/null; then
  echo "Run attempt does not identify the exact failed Release publish event" >&2
  exit 1
fi

if ! jobs=$(gh api \
  "repos/${repository}/actions/runs/${run_id}/attempts/${run_attempt}/jobs?per_page=100"); then
  echo "Could not read jobs for the failed Release publish run attempt" >&2
  exit 1
fi
if ! jq -e \
  --arg release_merge_sha "$release_merge_sha" \
  --argjson run_id "$run_id" \
  --argjson run_attempt "$run_attempt" '
    def exact_job($name; $conclusion; $step; $step_conclusion):
      [.jobs[] | select(.name == $name)] as $matches |
      ($matches | length) == 1 and
      ($matches[0] |
        .run_id == $run_id and
        .run_attempt == $run_attempt and
        .head_sha == $release_merge_sha and
        .workflow_name == "Release publish" and
        .status == "completed" and
        .conclusion == $conclusion and
        (.steps | type) == "array" and
        if $step == "" then
          (.steps | length) == 0
        else
          ([.steps[] |
            select(
              .name == $step and
              .status == "completed" and
              .conclusion == $step_conclusion
            )] | length) == 1
        end
      );
    select(type == "object") |
    select(.total_count == 5) |
    select((.jobs | type) == "array" and (.jobs | length) == 5) |
    select(exact_job(
      "Verify release merge";
      "success";
      "Match exactly one trusted release PR";
      "success"
    )) |
    select(exact_job(
      "Verify package without credentials";
      "success";
      "Verify exact package without a registry credential";
      "success"
    )) |
    select(exact_job(
      "Attempt locked crate publication";
      "success";
      "Upload exact locked source crate without build execution";
      "success"
    )) |
    select(exact_job(
      "Verify published crate identity";
      "failure";
      "Reconcile the exact crates.io package";
      "failure"
    )) |
    select(exact_job(
      "Publish immutable binary release";
      "skipped";
      "";
      ""
    ))
  ' <<<"$jobs" > /dev/null; then
  echo "Failed run does not have the exact recoverable publication topology" >&2
  exit 1
fi

{
  echo "is_release_merge=true"
  echo "recovery_release_sha=$release_merge_sha"
} >> "$output_file"
echo "Authenticated recovery of failed Release publish run $run_id attempt $run_attempt"
