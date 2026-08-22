#!/usr/bin/env bash
# Dispatch default-branch-controlled CI for one trusted candidate and bind it.
set -euo pipefail

if [[ $# -ne 5 ]]; then
  echo "usage: $0 <pr-number> <head-branch> <head-sha> <claim> <github-output-file>" >&2
  exit 2
fi

pr_number=$1
head_branch=$2
head_sha=$3
claim=$4
output_file=$5
repository=${GITHUB_REPOSITORY:-}
server_url=${GITHUB_SERVER_URL:-}
parent_run_id=${GITHUB_RUN_ID:-}
parent_run_attempt=${GITHUB_RUN_ATTEMPT:-}
base_sha=${GITHUB_SHA:-}
root=$(cd "$(dirname "$0")/.." && pwd)

if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ||
      ! "$server_url" =~ ^https://[^/[:space:]]+$ ||
      ! "$pr_number" =~ ^[1-9][0-9]*$ ||
      ! "$head_branch" =~ ^release-plz-[A-Za-z0-9._/-]+$ ||
      "$head_branch" == *..* || "$head_branch" == *//* ||
      ! "$head_sha" =~ ^[0-9a-f]{40}$ ||
      ! "$parent_run_id" =~ ^[1-9][0-9]*$ ||
      ! "$parent_run_attempt" =~ ^[1-9][0-9]*$ ||
      ! "$base_sha" =~ ^[0-9a-f]{40}$ ||
      ! -f "$output_file" || -L "$output_file" ]]; then
  echo "Invalid release-validation dispatch arguments" >&2
  exit 2
fi
if ! git check-ref-format --branch "$head_branch" > /dev/null 2>&1; then
  echo "Invalid release-validation branch: $head_branch" >&2
  exit 2
fi

expected_claim="release-validation-${parent_run_id}-${parent_run_attempt}-${head_sha}"
if [[ "$claim" != "$expected_claim" ]]; then
  echo "Release-validation claim does not bind this run attempt and head" >&2
  exit 2
fi

discovery_attempts=${RELEASE_VALIDATION_DISCOVERY_ATTEMPTS:-12}
completion_attempts=${RELEASE_VALIDATION_COMPLETION_ATTEMPTS:-720}
retry_delay=${RELEASE_VALIDATION_RETRY_DELAY_SECONDS:-10}
if [[ ! "$discovery_attempts" =~ ^[1-9][0-9]*$ ||
      ! "$completion_attempts" =~ ^[1-9][0-9]*$ ||
      ! "$retry_delay" =~ ^[0-9]+$ ||
      "$discovery_attempts" -gt 60 ||
      "$completion_attempts" -gt 720 ||
      "$retry_delay" -gt 60 ]]; then
  echo "Invalid release-validation retry policy" >&2
  exit 2
fi

verify_pull_snapshot() {
  local pull
  if ! pull=$(gh api "repos/${repository}/pulls/${pr_number}"); then
    echo "Could not read release PR #$pr_number" >&2
    return 1
  fi
  if ! jq -e \
    --arg repository "$repository" \
    --arg head_branch "$head_branch" \
    --arg head_sha "$head_sha" \
    --argjson pr_number "$pr_number" '
      type == "object" and
      .number == $pr_number and
      .state == "open" and
      .base.ref == "main" and
      (.head.ref // "") == $head_branch and
      (.head.repo.full_name // "") == $repository and
      (.head.sha // "") == $head_sha and
      (.user.login // "") == "github-actions[bot]" and
      (.user.type // "") == "Bot"
    ' <<<"$pull" > /dev/null; then
    echo "Release PR #$pr_number no longer matches trusted head $head_sha" >&2
    return 1
  fi
}

verify_branch_ref() {
  local branch_ref
  if ! branch_ref=$(gh api "repos/${repository}/git/ref/heads/${head_branch}"); then
    echo "Could not read release branch $head_branch" >&2
    return 1
  fi
  if ! jq -e \
    --arg expected_ref "refs/heads/$head_branch" \
    --arg head_sha "$head_sha" '
      type == "object" and
      .ref == $expected_ref and
      .object.type == "commit" and
      .object.sha == $head_sha
    ' <<<"$branch_ref" > /dev/null; then
    echo "Release branch $head_branch no longer resolves to $head_sha" >&2
    return 1
  fi
}

# The workflow file and every executable script at this head must still be the
# trusted main versions: the generated PR is allowed to change only release-plz's
# three data files. Re-read both the PR and its live branch after that API walk to
# close movement between discovery and dispatch.
verify_pull_snapshot
verify_branch_ref
"$root/scripts/verify-release-pr-files.sh" "$repository" "$pr_number" > /dev/null
verify_pull_snapshot
verify_branch_ref

if ! gh api --method POST "repos/${repository}/dispatches" \
  -f event_type=release_validation \
  -F 'client_payload[schema_version]=1' \
  -f "client_payload[validation_claim]=$claim" \
  -F "client_payload[parent_run_id]=$parent_run_id" \
  -F "client_payload[parent_run_attempt]=$parent_run_attempt" \
  -f "client_payload[base_sha]=$base_sha" \
  -F "client_payload[pr_number]=$pr_number" \
  -f "client_payload[head_branch]=$head_branch" \
  -f "client_payload[head_sha]=$head_sha"; then
  echo "Could not dispatch trusted release validation for $head_branch" >&2
  exit 1
fi

runs_query="repos/${repository}/actions/workflows/ci.yml/runs?event=repository_dispatch&branch=main&head_sha=${base_sha}&per_page=100"
run=
for ((attempt = 1; attempt <= discovery_attempts; attempt++)); do
  if ! response=$(gh api "$runs_query"); then
    echo "Could not discover the dispatched release validation" >&2
    exit 1
  fi
  if ! matches=$(jq -ce \
    --arg claim "$claim" \
    --arg repository "$repository" \
    --arg base_sha "$base_sha" '
      if type != "object" or (.workflow_runs | type) != "array"
      then error("workflow runs are malformed")
      else [.workflow_runs[] |
        select(.display_title == $claim) |
        select(.run_attempt == 1) |
        select(.event == "repository_dispatch") |
        select(.path == ".github/workflows/ci.yml") |
        select(.head_branch == "main") |
        select(.head_sha == $base_sha) |
        select((.repository.full_name // "") == $repository) |
        select((.head_repository.full_name // "") == $repository) |
        select((.actor.login // "") == "github-actions[bot]") |
        select((.actor.type // "") == "Bot")]
      end
    ' <<<"$response"); then
    echo "GitHub returned malformed release-validation run data" >&2
    exit 1
  fi
  match_count=$(jq -r 'length' <<<"$matches")
  if [[ "$match_count" == 1 ]]; then
    run=$(jq -c '.[0]' <<<"$matches")
    break
  fi
  if [[ "$match_count" != 0 ]]; then
    echo "Release-validation claim matched $match_count workflow runs" >&2
    exit 1
  fi
  if (( attempt == discovery_attempts )); then
    echo "Dispatched release validation did not appear before timeout" >&2
    exit 1
  fi
  sleep "$retry_delay"
done

if ! run_id=$(jq -er \
  '.id | select(type == "number" and . > 0 and floor == .)' <<<"$run"); then
  echo "Release-validation run has an invalid identity" >&2
  exit 1
fi
run_base_url="${server_url%/}/${repository}/actions/runs/${run_id}"
run_url="$run_base_url/attempts/1"

validate_run_identity() {
  local snapshot=$1
  jq -e \
    --argjson run_id "$run_id" \
    --arg claim "$claim" \
    --arg repository "$repository" \
    --arg base_sha "$base_sha" \
    --arg run_base_url "$run_base_url" '
      type == "object" and
      .id == $run_id and
      .run_attempt == 1 and
      .display_title == $claim and
      .event == "repository_dispatch" and
      .path == ".github/workflows/ci.yml" and
      .head_branch == "main" and
      .head_sha == $base_sha and
      .html_url == $run_base_url and
      (.repository.full_name // "") == $repository and
      (.head_repository.full_name // "") == $repository and
      (.actor.login // "") == "github-actions[bot]" and
      (.actor.type // "") == "Bot"
    ' <<<"$snapshot" > /dev/null
}

for ((attempt = 1; attempt <= completion_attempts; attempt++)); do
  if ! run=$(gh api "repos/${repository}/actions/runs/${run_id}/attempts/1"); then
    echo "Could not read release-validation run $run_id" >&2
    exit 1
  fi
  if ! validate_run_identity "$run"; then
    echo "Release-validation run $run_id crossed its trusted identity boundary" >&2
    exit 1
  fi
  if ! run_status=$(jq -er \
    '.status | select(. == "queued" or . == "in_progress" or . == "completed")' \
    <<<"$run"); then
    echo "Release-validation run $run_id returned malformed state" >&2
    exit 1
  fi
  if [[ "$run_status" == completed ]]; then
    break
  fi
  if (( attempt == completion_attempts )); then
    echo "Release-validation run $run_id did not finish before timeout" >&2
    exit 1
  fi
  sleep "$retry_delay"
done

if [[ $(jq -r '.conclusion // ""' <<<"$run") != success ]]; then
  echo "Release-validation run $run_id did not succeed" >&2
  exit 1
fi

if ! jobs=$(gh api \
  "repos/${repository}/actions/runs/${run_id}/attempts/1/jobs?per_page=100"); then
  echo "Could not read jobs for release-validation run $run_id" >&2
  exit 1
fi
if ! gate_count=$(jq -er --argjson run_attempt 1 '
  if type != "object" or
    (.total_count | type) != "number" or
    (.jobs | type) != "array" or
    .total_count != (.jobs | length)
  then error("workflow jobs are malformed or incomplete")
  else [.jobs[] |
    select(.name == "Release gate") |
    select(.run_attempt == $run_attempt) |
    select(.status == "completed") |
    select(.conclusion == "success")] | length
  end
' <<<"$jobs"); then
  echo "GitHub returned malformed release-validation job data" >&2
  exit 1
fi
all_gate_count=$(jq -r '[.jobs[] | select(.name == "Release gate")] | length' <<<"$jobs")
if [[ "$gate_count" != 1 || "$all_gate_count" != 1 ]]; then
  echo "Release-validation run $run_id lacks one exact successful Release gate" >&2
  exit 1
fi

{
  echo "gate_result=success"
  echo "validated_ref=$head_sha"
  echo "run_id=$run_id"
  echo "run_url=$run_url"
} >> "$output_file"

echo "Release validation $run_id passed for $head_sha"
