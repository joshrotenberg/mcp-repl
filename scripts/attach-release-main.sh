#!/usr/bin/env bash
# Attach an exact event commit to the main branch shape required by release-plz.
set -euo pipefail

expected_sha=${1:-}
if [[ $# -ne 1 || ! "$expected_sha" =~ ^[0-9a-f]{40}$ ]]; then
  echo 'usage: attach-release-main.sh <40-character-event-sha>' >&2
  exit 2
fi

if ! actual_sha=$(git rev-parse HEAD) || [[ "$actual_sha" != "$expected_sha" ]]; then
  echo 'Checkout does not match the frozen main event commit' >&2
  exit 1
fi

# release-plz resolves repository metadata through the current branch's
# upstream. Attaching the branch changes only local Git topology; the checked
# out event commit remains immutable.
git checkout -B main "$expected_sha"
git branch --set-upstream-to=origin/main main

if ! branch=$(git symbolic-ref --short HEAD) ||
  ! actual_sha=$(git rev-parse HEAD) ||
  ! upstream=$(git rev-parse --abbrev-ref --symbolic-full-name '@{upstream}'); then
  echo 'Could not inspect the attached release-plz checkout' >&2
  exit 1
fi
if [[ "$branch" != main ||
      "$actual_sha" != "$expected_sha" ||
      "$upstream" != origin/main ]]; then
  echo 'Failed to attach the frozen event commit to origin/main' >&2
  exit 1
fi
