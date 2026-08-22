#!/usr/bin/env bash
# Decide whether this exact main commit merged one trusted release-plz PR.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <github-output-file>" >&2
  exit 2
fi

output_file=$1
root=$(cd "$(dirname "$0")/.." && pwd)
repository=${GITHUB_REPOSITORY:-}
source_sha=${GITHUB_SHA:-}
if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ||
      ! "$source_sha" =~ ^[0-9a-fA-F]{40}$ ]]; then
  echo "Invalid release-merge environment" >&2
  exit 2
fi

if ! pull_pages=$(gh api --paginate --slurp \
  "repos/${repository}/commits/${source_sha}/pulls?per_page=100"); then
  echo "Could not read pull requests associated with $source_sha" >&2
  exit 1
fi
if ! candidates=$(jq -ce \
  --arg source_sha "$source_sha" '
    if type != "array" or any(.[]; type != "array")
    then error("associated pull pages are malformed")
    else [.[][] |
        select(.base.ref == "main") |
        select((.head.ref // "") | startswith("release-plz-")) |
        select(.merge_commit_sha == $source_sha)]
    end
  ' <<<"$pull_pages"); then
  echo "GitHub returned malformed associated pull-request data" >&2
  exit 1
fi

candidate_count=$(jq -r 'length' <<<"$candidates")
if [[ "$candidate_count" == 0 ]]; then
  echo "is_release_merge=false" >> "$output_file"
  echo "This commit did not merge a release-plz PR"
  exit 0
fi
if [[ "$candidate_count" != 1 ]]; then
  echo "Expected one merged release-plz PR, found $candidate_count" >&2
  exit 1
fi

pull=$(jq -c '.[0]' <<<"$candidates")
pr_number=$(jq -r '.number // ""' <<<"$pull")
head_sha=$(jq -r '.head.sha // ""' <<<"$pull")
base_sha=$(jq -r '.base.sha // ""' <<<"$pull")
if ! jq -e \
  --arg repository "$repository" '
    .state == "closed" and
    .merged_at != null and
    (.head.repo.full_name // "") == $repository and
    (.user.login // "") == "github-actions[bot]" and
    (.user.type // "") == "Bot"
  ' <<<"$pull" > /dev/null; then
  echo "Merged release PR does not match the trusted bot/repository boundary" >&2
  exit 1
fi

if [[ ! "$pr_number" =~ ^[1-9][0-9]*$ ||
      ! "$head_sha" =~ ^[0-9a-fA-F]{40}$ ||
      ! "$base_sha" =~ ^[0-9a-fA-F]{40}$ ]]; then
  echo "Merged release PR returned invalid identity commits" >&2
  exit 1
fi

# Bind publication to the exact tree that passed the release gate. A one-parent
# squash of the validated head must descend directly from that run's main SHA;
# checking the tree also rejects any merge-time conflict resolution or other
# content that was not present in the validated release candidate.
if ! source_commit=$(gh api "repos/${repository}/git/commits/${source_sha}"); then
  echo "Could not read release merge commit $source_sha" >&2
  exit 1
fi
if ! source_tree=$(jq -er \
  --arg source_sha "$source_sha" '
    select(.sha == $source_sha) |
    select(.parents | type == "array" and length == 1) |
    .tree.sha |
    select(type == "string" and test("^[0-9a-fA-F]{40}$"))
  ' <<<"$source_commit"); then
  echo "Release PR must produce one valid one-parent squash commit" >&2
  exit 1
fi
if ! source_parent=$(jq -er \
  '.parents[0].sha |
   select(type == "string" and test("^[0-9a-fA-F]{40}$"))' \
  <<<"$source_commit"); then
  echo "Release commit returned an invalid parent identity" >&2
  exit 1
fi
if [[ "$source_parent" != "$base_sha" ]]; then
  echo "Release commit does not descend from its validated main base" >&2
  exit 1
fi

if ! head_commit=$(gh api "repos/${repository}/git/commits/${head_sha}"); then
  echo "Could not read validated release head $head_sha" >&2
  exit 1
fi
if ! head_tree=$(jq -er \
  --arg head_sha "$head_sha" '
    select(.sha == $head_sha) |
    .tree.sha |
    select(type == "string" and test("^[0-9a-fA-F]{40}$"))
  ' <<<"$head_commit"); then
  echo "Validated release head returned invalid commit data" >&2
  exit 1
fi
if [[ "$source_tree" != "$head_tree" ]]; then
  echo "Release commit tree does not match the validated release head" >&2
  exit 1
fi

# Recheck both conditions that authorized the merge. Publication must remain
# safe even if repository rules drift or a same-named check run is introduced.
"$root/scripts/verify-release-pr-files.sh" "$repository" "$pr_number"
"$root/scripts/verify-release-gate.sh" "$head_sha" "$base_sha"

echo "is_release_merge=true" >> "$output_file"
echo "This commit merged exactly one trusted release-plz PR"
