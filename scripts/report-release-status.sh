#!/usr/bin/env bash
# Post the required classic status only after revalidating the exact PR head.
set -euo pipefail

if [[ $# -ne 5 ]]; then
  echo "usage: $0 <pr-number> <head-sha> <state> <target-url> <description>" >&2
  exit 2
fi

pr_number=$1
expected_sha=$2
state=$3
target_url=$4
description=$5
repository=${GITHUB_REPOSITORY:-}

if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ||
      ! "$pr_number" =~ ^[1-9][0-9]*$ ||
      ! "$expected_sha" =~ ^[0-9a-fA-F]{40}$ ||
      ! "$target_url" =~ ^https://.*/actions/runs/([1-9][0-9]*)$ ||
      -z "$description" || ${#description} -gt 140 ]]; then
  echo "Invalid release-status arguments" >&2
  exit 2
fi
current_run_id=${BASH_REMATCH[1]}
case "$state" in
  pending | success | failure) ;;
  *)
    echo "Invalid release status: $state" >&2
    exit 2
    ;;
esac

if ! pull=$(gh api "repos/${repository}/pulls/${pr_number}"); then
  echo "Could not read release PR #$pr_number" >&2
  exit 1
fi

if ! jq -e \
  --arg repository "$repository" \
  --arg expected_sha "$expected_sha" \
  --argjson pr_number "$pr_number" '
    .number == $pr_number and
    .state == "open" and
    .base.ref == "main" and
    ((.head.ref // "") | startswith("release-plz-")) and
    (.head.repo.full_name // "") == $repository and
    (.head.sha // "") == $expected_sha and
    (.user.login // "") == "github-actions[bot]" and
    (.user.type // "") == "Bot"
  ' <<<"$pull" > /dev/null; then
  echo "Release PR #$pr_number no longer matches the trusted head $expected_sha" >&2
  exit 1
fi

# Statuses are append-only. Claim this context with a pending status, and
# require the same run to still own the newest claim before it can finalize.
# Run IDs increase monotonically, so a delayed older workflow cannot steal the
# context back from a newer validation of the same unchanged release head.
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

if [[ "$state" == pending ]]; then
  if [[ "$latest" != null ]]; then
    latest_url=$(jq -er '.target_url // ""' <<<"$latest")
    if [[ ! "$latest_url" =~ ^https://.*/actions/runs/([1-9][0-9]*)$ ]]; then
      echo "Latest Release gate status has an unrecognized target URL" >&2
      exit 1
    fi
    latest_run_id=${BASH_REMATCH[1]}
    if (( latest_run_id > current_run_id )); then
      echo "A newer release validation run already owns Release gate" >&2
      exit 1
    fi
  fi
else
  if [[ "$latest" == null ||
        $(jq -r '.target_url // ""' <<<"$latest") != "$target_url" ]]; then
    echo "This run no longer owns the Release gate claim" >&2
    exit 1
  fi
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
