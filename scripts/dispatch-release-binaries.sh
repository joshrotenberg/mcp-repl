#!/usr/bin/env bash
# Dispatch native publication only for an existing draft at an immutable tag.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <tag> <source-sha>" >&2
  exit 2
fi

tag=$1
source_sha=$2
repository=${GH_REPO:-}
root=$(cd "$(dirname "$0")/.." && pwd)
if [[ ! "$repository" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "GH_REPO must be an owner/repository name" >&2
  exit 2
fi
if [[ ! "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "Refusing invalid release tag: $tag" >&2
  exit 2
fi
if [[ ! "$source_sha" =~ ^[0-9a-fA-F]{40}$ ]]; then
  echo "Refusing invalid release source SHA: $source_sha" >&2
  exit 2
fi

"$root/scripts/verify-release-tag.sh" "$tag" "$source_sha"

if ! draft_state=$(gh release view "$tag" \
  --repo "$repository" \
  --json isDraft \
  --jq .isDraft); then
  echo "Could not read release $tag; see the gh error above" >&2
  exit 1
fi
case "$draft_state" in
  true)
    "$root/scripts/verify-release.sh" "$tag" draft > /dev/null
    ;;
  false)
    "$root/scripts/verify-release.sh" "$tag" published > /dev/null
    echo "Release $tag is already published; nothing to dispatch"
    exit 0
    ;;
  *)
    echo "Release $tag returned an unexpected draft state: $draft_state" >&2
    exit 1
    ;;
esac

if ! active_runs=$(gh api \
  "repos/${repository}/actions/workflows/release-binaries.yml/runs?event=workflow_dispatch&head_sha=${source_sha}&per_page=100" \
  --jq "[.workflow_runs[] | select(.head_sha == \"${source_sha}\" and .status != \"completed\")] | length"); then
  echo "Could not check existing native publication runs for $tag" >&2
  exit 1
fi
if [[ ! "$active_runs" =~ ^[0-9]+$ ]]; then
  echo "GitHub returned an invalid native publication run count: $active_runs" >&2
  exit 1
fi
if (( active_runs > 0 )); then
  echo "Native publication for $tag is already active; nothing to dispatch"
  exit 0
fi

if ! gh workflow run release-binaries.yml \
  --repo "$repository" \
  --ref "$tag" \
  -f "tag=$tag" \
  -f "source_sha=$source_sha"; then
  echo "Could not dispatch native publication for $tag" >&2
  exit 1
fi

echo "Dispatched native publication for $tag"
