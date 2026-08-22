#!/usr/bin/env bash
# Find the one release-plz PR that trusted main is allowed to validate.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <github-output-file>" >&2
  exit 2
fi

output_file=$1
root=$(cd "$(dirname "$0")/.." && pwd)
repository=${GITHUB_REPOSITORY:-}
expected_head_sha=${EXPECTED_RELEASE_HEAD_SHA:-}
if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "GITHUB_REPOSITORY must be an owner/repository name" >&2
  exit 2
fi
if [[ -n "$expected_head_sha" &&
      ! "$expected_head_sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo "EXPECTED_RELEASE_HEAD_SHA must be an exact lowercase commit" >&2
  exit 2
fi

if ! pull_pages=$(gh api --paginate --slurp \
  "repos/${repository}/pulls?state=open&base=main&per_page=100"); then
  echo "Could not list open release pull requests" >&2
  exit 1
fi

if ! candidates=$(jq -ce --arg repository "$repository" '
  if type != "array" or any(.[]; type != "array")
  then error("pull pages are malformed")
  else [.[][] |
      select(.base.ref == "main") |
      select((.head.ref // "") | startswith("release-plz-")) |
      select((.head.repo.full_name // "") == $repository) |
      select((.user.login // "") == "github-actions[bot]") |
      select((.user.type // "") == "Bot")]
  end
' <<<"$pull_pages"); then
  echo "GitHub returned malformed pull-request data" >&2
  exit 1
fi

candidate_count=$(jq -r 'length' <<<"$candidates")
if [[ "$candidate_count" == 0 ]]; then
  if [[ -n "$expected_head_sha" ]]; then
    echo "The generated release commit has no trusted pull request" >&2
    exit 1
  fi
  # A normal main push need not create a release PR. Explicit empty outputs
  # make every downstream job's skip condition deterministic.
  {
    echo "pr_number="
    echo "head_branch="
    echo "head_sha="
  } >> "$output_file"
  echo "No open release-plz PR"
  exit 0
fi
if [[ "$candidate_count" != 1 ]]; then
  echo "Expected at most one open release-plz PR, found $candidate_count" >&2
  exit 1
fi

pull=$(jq -c '.[0]' <<<"$candidates")
pr_number=$(jq -r '.number // ""' <<<"$pull")
head_branch=$(jq -r '.head.ref // ""' <<<"$pull")
head_sha=$(jq -r '.head.sha // ""' <<<"$pull")

if [[ ! "$pr_number" =~ ^[1-9][0-9]*$ ||
      ! "$head_sha" =~ ^[0-9a-fA-F]{40}$ ]]; then
  echo "Trusted release PR candidate has an invalid identity" >&2
  exit 1
fi
if [[ -n "$expected_head_sha" && "$head_sha" != "$expected_head_sha" ]]; then
  echo "Release PR head $head_sha differs from generated commit $expected_head_sha" >&2
  exit 1
fi

"$root/scripts/verify-release-pr-files.sh" "$repository" "$pr_number"

{
  echo "pr_number=$pr_number"
  echo "head_branch=$head_branch"
  echo "head_sha=$head_sha"
} >> "$output_file"
echo "Discovered trusted release PR #$pr_number at $head_sha"
