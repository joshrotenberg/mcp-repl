#!/usr/bin/env bash
# Bind publication to the exact successful child CI run for this release head.
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <pr-number> <release-head-sha> <release-base-sha>" >&2
  exit 2
fi

pr_number=$1
head_sha=$2
base_sha=$3
repository=${GITHUB_REPOSITORY:-}
server_url=${GITHUB_SERVER_URL:-}
if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ||
      ! "$server_url" =~ ^https://[^/[:space:]]+$ ||
      ! "$pr_number" =~ ^[1-9][0-9]*$ ||
      ! "$head_sha" =~ ^[0-9a-f]{40}$ ||
      ! "$base_sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo "Invalid release-gate verification arguments" >&2
  exit 2
fi

if ! statuses=$(gh api \
  "repos/${repository}/commits/${head_sha}/statuses?per_page=100"); then
  echo "Could not read Release gate statuses for $head_sha" >&2
  exit 1
fi
if ! latest=$(jq -c '
  if type != "array" then error("statuses are not an array")
  else [.[] | select(.context == "Release gate")][0] // null
  end
' <<<"$statuses"); then
  echo "GitHub returned malformed commit-status data" >&2
  exit 1
fi
if [[ "$latest" == null ]] || ! jq -e '
  .state == "success" and
  .description == "Release validation passed" and
  (.creator.login // "") == "github-actions[bot]" and
  (.creator.type // "") == "Bot"
' <<<"$latest" > /dev/null; then
  echo "The latest classic Release gate is not a trusted success" >&2
  exit 1
fi

if ! target_url=$(jq -er \
  '.target_url | select(type == "string" and length > 0)' \
  <<<"$latest"); then
  echo "Release gate returned a malformed target URL" >&2
  exit 1
fi
run_prefix="${server_url%/}/${repository}/actions/runs/"
if [[ "$target_url" != "$run_prefix"* ]]; then
  echo "Release gate has an untrusted target URL: $target_url" >&2
  exit 1
fi
run_suffix=${target_url#"$run_prefix"}
if [[ ! "$run_suffix" =~ ^([1-9][0-9]*)/attempts/([1-9][0-9]*)$ ]]; then
  echo "Release gate target does not name one workflow run attempt: $target_url" >&2
  exit 1
fi
run_id=${BASH_REMATCH[1]}
run_attempt=${BASH_REMATCH[2]}
if [[ "$run_attempt" != 1 ]]; then
  echo "Release gate target is not the dispatched child run's first attempt" >&2
  exit 1
fi
run_base_url="${run_prefix}${run_id}"

max_attempts=${RELEASE_GATE_MAX_ATTEMPTS:-12}
retry_delay=${RELEASE_GATE_RETRY_DELAY_SECONDS:-5}
if [[ ! "$max_attempts" =~ ^[1-9][0-9]*$ ||
      ! "$retry_delay" =~ ^[0-9]+$ ||
      "$max_attempts" -gt 60 || "$retry_delay" -gt 60 ]]; then
  echo "Invalid release-gate retry policy" >&2
  exit 2
fi

# The status can become visible just before its child workflow transitions to
# completed. Wait through that legitimate API race while revalidating the run's
# immutable identity on every observation.
for ((attempt = 1; attempt <= max_attempts; attempt++)); do
  if ! run=$(gh api \
    "repos/${repository}/actions/runs/${run_id}/attempts/${run_attempt}"); then
    echo "Could not read release-validation run $run_id" >&2
    exit 1
  fi
  if ! run_status=$(jq -er \
    '.status | select(. == "queued" or . == "in_progress" or . == "completed")' \
    <<<"$run"); then
    echo "Release-validation run $run_id returned malformed state" >&2
    exit 1
  fi
  if ! jq -e \
    --argjson run_id "$run_id" \
    --argjson run_attempt "$run_attempt" \
    --arg repository "$repository" \
    --arg head_sha "$head_sha" \
    --arg base_sha "$base_sha" \
    --arg run_base_url "$run_base_url" '
      type == "object" and
      .id == $run_id and
      .run_attempt == $run_attempt and
      .event == "repository_dispatch" and
      .path == ".github/workflows/ci.yml" and
      .head_branch == "main" and
      .head_sha == $base_sha and
      .html_url == $run_base_url and
      (.repository.full_name // "") == $repository and
      (.head_repository.full_name // "") == $repository and
      (.actor.login // "") == "github-actions[bot]" and
      (.actor.type // "") == "Bot" and
      (.display_title | type == "string" and
        test("^release-validation-[1-9][0-9]*-[1-9][0-9]*-" + $head_sha + "$"))
    ' <<<"$run" > /dev/null; then
    echo "Release gate does not belong to the trusted child CI identity" >&2
    exit 1
  fi
  if [[ "$run_status" == completed ]]; then
    break
  fi
  if (( attempt == max_attempts )); then
    echo "Release-validation run $run_id did not finish before publication preflight" >&2
    exit 1
  fi
  sleep "$retry_delay"
done

if [[ $(jq -r '.conclusion // ""' <<<"$run") != success ]]; then
  echo "Release-validation run $run_id did not succeed" >&2
  exit 1
fi
if ! run_title=$(jq -er '.display_title' <<<"$run") ||
  [[ ! "$run_title" =~ ^release-validation-([1-9][0-9]*)-([1-9][0-9]*)-${head_sha}$ ]]; then
  echo "Release-validation run $run_id omitted its exact parent claim" >&2
  exit 1
fi
parent_run_id=${BASH_REMATCH[1]}
parent_run_attempt=${BASH_REMATCH[2]}
parent_url="${server_url%/}/${repository}/actions/runs/${parent_run_id}"
for ((attempt = 1; attempt <= max_attempts; attempt++)); do
  if ! parent_run=$(gh api \
    "repos/${repository}/actions/runs/${parent_run_id}/attempts/${parent_run_attempt}"); then
    echo "Could not read claiming Release-plz run attempt $parent_run_id/$parent_run_attempt" >&2
    exit 1
  fi
  if ! parent_status=$(jq -er \
    '.status | select(. == "queued" or . == "in_progress" or . == "completed")' \
    <<<"$parent_run") || ! jq -e \
    --argjson run_id "$parent_run_id" \
    --argjson run_attempt "$parent_run_attempt" \
    --arg repository "$repository" \
    --arg base_sha "$base_sha" \
    --arg parent_url "$parent_url" '
      type == "object" and
      .id == $run_id and
      .run_attempt == $run_attempt and
      .event == "push" and
      .head_branch == "main" and
      .head_sha == $base_sha and
      .path == ".github/workflows/release-plz.yml" and
      .html_url == $parent_url and
      (.repository.full_name // "") == $repository and
      (.head_repository.full_name // "") == $repository
    ' <<<"$parent_run" > /dev/null; then
    echo "Release-validation claim does not belong to the trusted Release-plz parent" >&2
    exit 1
  fi
  if [[ "$parent_status" == completed ]]; then
    break
  fi
  if (( attempt == max_attempts )); then
    echo "Claiming Release-plz run did not finish before publication preflight" >&2
    exit 1
  fi
  sleep "$retry_delay"
done
if [[ $(jq -r '.conclusion // ""' <<<"$parent_run") != success ]]; then
  echo "Claiming Release-plz run did not succeed" >&2
  exit 1
fi
if ! current_parent=$(gh api \
  "repos/${repository}/actions/runs/${parent_run_id}"); then
  echo "Could not confirm the current claiming Release-plz attempt" >&2
  exit 1
fi
if ! jq -e \
  --argjson run_id "$parent_run_id" \
  --argjson run_attempt "$parent_run_attempt" \
  --arg repository "$repository" \
  --arg base_sha "$base_sha" \
  --arg parent_url "$parent_url" '
    type == "object" and
    .id == $run_id and
    .run_attempt == $run_attempt and
    .head_branch == "main" and
    .head_sha == $base_sha and
    .path == ".github/workflows/release-plz.yml" and
    .html_url == $parent_url and
    (.repository.full_name // "") == $repository and
    (.head_repository.full_name // "") == $repository
  ' <<<"$current_parent" > /dev/null; then
  echo "A newer Release-plz attempt superseded the successful claim" >&2
  exit 1
fi

if ! parent_jobs=$(gh api \
  "repos/${repository}/actions/runs/${parent_run_id}/attempts/${parent_run_attempt}/jobs?per_page=100"); then
  echo "Could not read jobs for claiming Release-plz run attempt" >&2
  exit 1
fi
if ! report_count=$(jq -er --argjson run_attempt "$parent_run_attempt" '
  if type != "object" or
    (.total_count | type) != "number" or
    (.jobs | type) != "array" or
    .total_count != (.jobs | length)
  then error("workflow jobs are malformed or incomplete")
  else [.jobs[] |
    select(.name == "Report release validation") |
    select(.run_attempt == $run_attempt) |
    select(.status == "completed") |
    select(.conclusion == "success")] | length
  end
' <<<"$parent_jobs"); then
  echo "GitHub returned malformed claiming Release-plz job data" >&2
  exit 1
fi
all_report_count=$(jq -r \
  '[.jobs[] | select(.name == "Report release validation")] | length' \
  <<<"$parent_jobs")
if [[ "$report_count" != 1 || "$all_report_count" != 1 ]]; then
  echo "Claiming Release-plz run lacks one exact successful reporting job" >&2
  exit 1
fi

identity_dir=$(mktemp -d)
trap 'rm -rf "$identity_dir"' EXIT INT TERM
identity_name="release-identity-${parent_run_id}-${parent_run_attempt}-${head_sha}"
if ! gh run download "$parent_run_id" \
  --repo "$repository" \
  --name "$identity_name" \
  --dir "$identity_dir"; then
  echo "Could not download the claiming Release-plz identity artifact" >&2
  exit 1
fi
actual_entries=$(find "$identity_dir" -mindepth 1 -maxdepth 1 \
  -exec basename {} \; | LC_ALL=C sort)
if [[ "$actual_entries" != release-identity.json ||
      ! -f "$identity_dir/release-identity.json" ||
      -L "$identity_dir/release-identity.json" ]]; then
  echo "Claiming Release-plz identity artifact has an unexpected file set" >&2
  exit 1
fi
if ! jq -e \
  --arg repository "$repository" \
  --argjson parent_run_id "$parent_run_id" \
  --argjson parent_run_attempt "$parent_run_attempt" \
  --arg base_sha "$base_sha" \
  --argjson pr_number "$pr_number" \
  --arg head_sha "$head_sha" '
    type == "object" and
    (keys | sort) == ([
      "base_sha", "head_branch", "head_sha", "parent_run_attempt",
      "parent_run_id", "pr_number", "repository", "schema_version"
    ] | sort) and
    .schema_version == 1 and
    .repository == $repository and
    .parent_run_id == $parent_run_id and
    .parent_run_attempt == $parent_run_attempt and
    .base_sha == $base_sha and
    .pr_number == $pr_number and
    (.head_branch | type == "string" and
      test("^release-plz-[A-Za-z0-9._/-]+$")) and
    (.head_branch | contains("..") | not) and
    (.head_branch | contains("//") | not) and
    .head_sha == $head_sha
  ' "$identity_dir/release-identity.json" > /dev/null; then
  echo "Claiming Release-plz identity artifact does not bind this release" >&2
  exit 1
fi
if ! head_branch=$(jq -er '.head_branch' \
  "$identity_dir/release-identity.json") ||
  ! git check-ref-format --branch "$head_branch" > /dev/null 2>&1; then
  echo "Claiming Release-plz identity artifact has an invalid release branch" >&2
  exit 1
fi

if ! jobs=$(gh api \
  "repos/${repository}/actions/runs/${run_id}/attempts/${run_attempt}/jobs?per_page=100"); then
  echo "Could not read jobs for release-validation run $run_id" >&2
  exit 1
fi
if ! gate_count=$(jq -er --argjson run_attempt "$run_attempt" '
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

echo "Release gate is a trusted child CI success for $head_sha"
