#!/usr/bin/env bash
# Post the required classic status only after revalidating the exact PR head.
set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "usage: $0 <pr-number> <head-sha> <state> <claim-url> <target-url> <description>" >&2
  exit 2
fi

pr_number=$1
expected_sha=$2
state=$3
claim_url=$4
target_url=$5
description=$6
repository=${GITHUB_REPOSITORY:-}
server_url=${GITHUB_SERVER_URL:-}
current_run_id=${GITHUB_RUN_ID:-}
current_run_attempt=${GITHUB_RUN_ATTEMPT:-}
current_source_sha=${GITHUB_SHA:-}
producer_attempt=${RELEASE_PRODUCER_ATTEMPT:-}

if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ||
      ! "$server_url" =~ ^https://[^/[:space:]]+$ ||
      ! "$pr_number" =~ ^[1-9][0-9]*$ ||
      ! "$expected_sha" =~ ^[0-9a-f]{40}$ ||
      ! "$current_run_id" =~ ^[1-9][0-9]*$ ||
      ! "$current_run_attempt" =~ ^[1-9][0-9]*$ ||
      ! "$producer_attempt" =~ ^[1-9][0-9]*$ ||
      "$producer_attempt" != "$current_run_attempt" ||
      ! "$current_source_sha" =~ ^[0-9a-f]{40}$ ||
      -z "$description" || ${#description} -gt 140 ]]; then
  echo "Invalid release-status arguments or producer attempt" >&2
  exit 2
fi
run_prefix="${server_url%/}/${repository}/actions/runs/"
if [[ "$claim_url" != "$run_prefix"* || "$target_url" != "$run_prefix"* ]]; then
  echo "Release-status URLs do not belong to this repository" >&2
  exit 2
fi
claim_suffix=${claim_url#"$run_prefix"}
target_suffix=${target_url#"$run_prefix"}
if [[ ! "$claim_suffix" =~ ^([1-9][0-9]*)/attempts/([1-9][0-9]*)$ ||
      ! "$target_suffix" =~ ^([1-9][0-9]*)/attempts/([1-9][0-9]*)$ ||
      "$claim_url" != "${run_prefix}${current_run_id}/attempts/${current_run_attempt}" ]]; then
  echo "Invalid release-status run URL" >&2
  exit 2
fi
case "$state" in
  pending | success | failure) ;;
  *)
    echo "Invalid release status: $state" >&2
    exit 2
    ;;
esac
if [[ "$state" == pending && "$target_url" != "$claim_url" ]]; then
  echo "A pending Release gate must target its claiming parent run" >&2
  exit 2
fi
if [[ "$state" == success && "$target_url" == "$claim_url" ]]; then
  echo "A successful Release gate must target its validated child CI run" >&2
  exit 2
fi

if ! pull=$(gh api "repos/${repository}/pulls/${pr_number}"); then
  echo "Could not read release PR #$pr_number" >&2
  exit 1
fi
if ! base_sha=$(jq -er '.base.sha' <<<"$pull") ||
  [[ ! "$base_sha" =~ ^[0-9a-f]{40}$ || "$base_sha" != "$current_source_sha" ]]; then
  echo "Release PR #$pr_number has an invalid parent source" >&2
  exit 1
fi
if ! head_branch=$(jq -er '.head.ref' <<<"$pull") ||
  [[ ! "$head_branch" =~ ^release-plz-[A-Za-z0-9._/-]+$ ||
     "$head_branch" == *..* || "$head_branch" == *//* ]] ||
  ! git check-ref-format --branch "$head_branch" > /dev/null 2>&1; then
  echo "Release PR #$pr_number has an invalid head branch" >&2
  exit 1
fi
if ! jq -e \
  --arg repository "$repository" \
  --arg expected_sha "$expected_sha" \
  --arg base_sha "$base_sha" \
  --arg head_branch "$head_branch" \
  --argjson pr_number "$pr_number" '
    type == "object" and
    .number == $pr_number and
    .state == "open" and
    .base.ref == "main" and
    .base.sha == $base_sha and
    (.head.ref // "") == $head_branch and
    (.head.repo.full_name // "") == $repository and
    (.head.sha // "") == $expected_sha and
    (.user.login // "") == "github-actions[bot]" and
    (.user.type // "") == "Bot"
  ' <<<"$pull" > /dev/null; then
  echo "Release PR #$pr_number no longer matches the trusted head $expected_sha" >&2
  exit 1
fi

validate_current_parent() {
  local endpoint=$1 snapshot
  if ! snapshot=$(gh api "$endpoint"); then
    echo "Could not read current Release-plz run attempt" >&2
    return 1
  fi
  jq -e \
    --argjson run_id "$current_run_id" \
    --argjson run_attempt "$current_run_attempt" \
    --arg repository "$repository" \
    --arg base_sha "$base_sha" \
    --arg parent_url "${run_prefix}${current_run_id}" '
      type == "object" and
      .id == $run_id and
      .run_attempt == $run_attempt and
      .event == "push" and
      (.status == "queued" or .status == "in_progress" or .status == "completed") and
      .head_branch == "main" and
      .head_sha == $base_sha and
      .path == ".github/workflows/release-plz.yml" and
      .html_url == $parent_url and
      (.repository.full_name // "") == $repository and
      (.head_repository.full_name // "") == $repository
    ' <<<"$snapshot" > /dev/null
}

if ! validate_current_parent \
  "repos/${repository}/actions/runs/${current_run_id}/attempts/${current_run_attempt}"; then
  echo "Status reporter does not belong to the trusted Release-plz parent" >&2
  exit 1
fi

# Resolve a status target back to the Release-plz run/attempt that owns it.
# Pending and failure statuses target that exact parent attempt. Successful
# statuses target an exact child CI attempt whose immutable title carries the
# same parent tuple.
status_owner() {
  local status_json=$1 status_url suffix run_id run_attempt snapshot path title
  if ! jq -e '
    type == "object" and
    (.creator.login // "") == "github-actions[bot]" and
    (.creator.type // "") == "Bot"
  ' <<<"$status_json" > /dev/null; then
    echo "Latest Release gate status has an untrusted creator" >&2
    return 1
  fi
  if ! status_url=$(jq -er '.target_url' <<<"$status_json") ||
    [[ "$status_url" != "$run_prefix"* ]]; then
    echo "Latest Release gate status has an unrecognized target URL" >&2
    return 1
  fi
  suffix=${status_url#"$run_prefix"}
  if [[ ! "$suffix" =~ ^([1-9][0-9]*)/attempts/([1-9][0-9]*)$ ]]; then
    echo "Latest Release gate status does not name one exact run attempt" >&2
    return 1
  fi
  run_id=${BASH_REMATCH[1]}
  run_attempt=${BASH_REMATCH[2]}
  if ! snapshot=$(gh api \
    "repos/${repository}/actions/runs/${run_id}/attempts/${run_attempt}"); then
    echo "Could not resolve the latest Release gate owner" >&2
    return 1
  fi
  if ! jq -e \
    --argjson run_id "$run_id" \
    --argjson run_attempt "$run_attempt" \
    --arg repository "$repository" \
    --arg run_url "${run_prefix}${run_id}" '
      type == "object" and
      .id == $run_id and
      .run_attempt == $run_attempt and
      .html_url == $run_url and
      (.repository.full_name // "") == $repository and
      (.head_repository.full_name // "") == $repository
    ' <<<"$snapshot" > /dev/null; then
    echo "Latest Release gate target has an untrusted run identity" >&2
    return 1
  fi
  path=$(jq -r '.path // ""' <<<"$snapshot")
  case "$path" in
    .github/workflows/release-plz.yml)
      if ! jq -e '
        .event == "push" and
        .head_branch == "main"
      ' <<<"$snapshot" > /dev/null; then
        echo "Latest Release gate parent target is untrusted" >&2
        return 1
      fi
      printf '%s %s\n' "$run_id" "$run_attempt"
      ;;
    .github/workflows/ci.yml)
      if ! jq -e \
        --arg base_sha "$base_sha" '
          .event == "repository_dispatch" and
          .head_sha == $base_sha and
          .head_branch == "main" and
          (.actor.login // "") == "github-actions[bot]" and
          (.actor.type // "") == "Bot"
        ' <<<"$snapshot" > /dev/null ||
        ! title=$(jq -er '.display_title' <<<"$snapshot") ||
        [[ ! "$title" =~ ^release-validation-([1-9][0-9]*)-([1-9][0-9]*)-${expected_sha}$ ]]; then
        echo "Latest Release gate child target is untrusted" >&2
        return 1
      fi
      printf '%s %s\n' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}"
      ;;
    *)
      echo "Latest Release gate target uses an untrusted workflow" >&2
      return 1
      ;;
  esac
}

# Statuses are append-only. Compare parent run/attempt tuples, never the child
# run ID exposed by a successful status. Serialization is the write-side CAS;
# these checks make delayed attempts fail closed.
if ! statuses=$(gh api \
  "repos/${repository}/commits/${expected_sha}/statuses?per_page=100"); then
  echo "Could not read existing Release gate statuses for $expected_sha" >&2
  exit 1
fi
if ! latest=$(jq -c \
  'if type != "array" then error("statuses are not an array")
   else [.[] | select(.context == "Release gate")][0] // null
   end' \
  <<<"$statuses"); then
  echo "GitHub returned malformed commit-status data" >&2
  exit 1
fi

# A retry of the same trusted transition is a no-op. This matters after the
# status POST succeeded but the caller was interrupted before observing it.
if [[ "$latest" != null ]]; then
  if ! owner=$(status_owner "$latest"); then
    exit 1
  fi
  owner_run_id=${owner%% *}
  owner_run_attempt=${owner##* }
  if jq -e \
  --arg state "$state" \
  --arg target_url "$target_url" \
  --arg description "$description" '
    .state == $state and
    .target_url == $target_url and
    .description == $description and
    (.creator.login // "") == "github-actions[bot]" and
    (.creator.type // "") == "Bot"
  ' <<<"$latest" > /dev/null; then
    echo "Release gate already records $state ($expected_sha)"
    exit 0
  fi

  if (( owner_run_id > current_run_id ||
        (owner_run_id == current_run_id &&
         owner_run_attempt > current_run_attempt) )); then
    echo "A newer release validation attempt already owns Release gate" >&2
    exit 1
  fi
  if [[ "$state" == pending ]]; then
    if (( owner_run_id == current_run_id &&
          owner_run_attempt == current_run_attempt )); then
      echo "This attempt cannot replace its existing Release gate state" >&2
      exit 1
    fi
  elif (( owner_run_id != current_run_id ||
          owner_run_attempt != current_run_attempt )) ||
    [[ $(jq -r '.state // ""' <<<"$latest") != pending ||
       $(jq -r '.target_url // ""' <<<"$latest") != "$claim_url" ]]; then
    echo "This run attempt no longer owns the Release gate claim" >&2
    exit 1
  fi
elif [[ "$state" != pending ]]; then
  echo "This run attempt has no Release gate claim" >&2
  exit 1
fi

# Refuse a transition if a newer rerun began after the status read.
if ! validate_current_parent \
  "repos/${repository}/actions/runs/${current_run_id}"; then
  echo "A newer Release-plz attempt superseded this status write" >&2
  exit 1
fi

if ! gh api \
  --method POST \
  "repos/${repository}/statuses/${expected_sha}" \
  -f state="$state" \
  -f target_url="$target_url" \
  -f description="$description" \
  -f context='Release gate' \
  > /dev/null; then
  echo "Could not post Release gate status to $expected_sha" >&2
  exit 1
fi

echo "Release gate: $state ($expected_sha)"
