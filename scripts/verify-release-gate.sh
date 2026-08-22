#!/usr/bin/env bash
# Bind publication to the trusted successful Release-plz run for this PR head.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <release-head-sha> <release-base-sha>" >&2
  exit 2
fi

head_sha=$1
base_sha=$2
repository=${GITHUB_REPOSITORY:-}
server_url=${GITHUB_SERVER_URL:-}
if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ||
      ! "$server_url" =~ ^https://[^/[:space:]]+$ ||
      ! "$head_sha" =~ ^[0-9a-fA-F]{40}$ ||
      ! "$base_sha" =~ ^[0-9a-fA-F]{40}$ ]]; then
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
run_id=${target_url#"$run_prefix"}
if [[ ! "$run_id" =~ ^[1-9][0-9]*$ ]]; then
  echo "Release gate target does not name one workflow run: $target_url" >&2
  exit 1
fi

max_attempts=${RELEASE_GATE_MAX_ATTEMPTS:-12}
retry_delay=${RELEASE_GATE_RETRY_DELAY_SECONDS:-5}
if [[ ! "$max_attempts" =~ ^[1-9][0-9]*$ ||
      ! "$retry_delay" =~ ^[0-9]+$ ||
      "$max_attempts" -gt 60 || "$retry_delay" -gt 60 ]]; then
  echo "Invalid release-gate retry policy" >&2
  exit 2
fi

# The classic status becomes visible a few seconds before its owning workflow
# transitions to completed. Wait through that legitimate merge race, but do
# not publish from a run that ultimately fails or never finishes.
for ((attempt = 1; attempt <= max_attempts; attempt++)); do
  if ! run=$(gh api "repos/${repository}/actions/runs/${run_id}"); then
    echo "Could not read Release-plz run $run_id" >&2
    exit 1
  fi
  if ! run_status=$(jq -er \
    '.status | select(type == "string" and length > 0)' \
    <<<"$run"); then
    echo "Release-plz run $run_id returned malformed state" >&2
    exit 1
  fi
  if [[ "$run_status" == completed ]]; then
    break
  fi
  if (( attempt == max_attempts )); then
    echo "Release-plz run $run_id did not finish before publication preflight" >&2
    exit 1
  fi
  sleep "$retry_delay"
done

if ! jq -e \
  --argjson run_id "$run_id" \
  --arg repository "$repository" \
  --arg base_sha "$base_sha" \
  --arg target_url "$target_url" '
    .id == $run_id and
    (.event == "push" or .event == "workflow_dispatch") and
    .status == "completed" and
    .conclusion == "success" and
    .head_branch == "main" and
    .head_sha == $base_sha and
    .path == ".github/workflows/release-plz.yml" and
    .html_url == $target_url and
    (.repository.full_name // "") == $repository and
    (.head_repository.full_name // "") == $repository
  ' <<<"$run" > /dev/null; then
  echo "Release gate does not belong to the trusted successful Release-plz run" >&2
  exit 1
fi

echo "Release gate is a trusted success for $head_sha"
