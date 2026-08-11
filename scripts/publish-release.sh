#!/usr/bin/env bash
# Verify a complete set of release archives and publish the draft release.
#
#   scripts/publish-release.sh <tag> [dist-directory]
#
# Refuses unless every target produced an archive and a checksum, the
# directory holds exactly those files, and each checksum verifies. Only then
# are the assets uploaded and the draft made public, so a failed target
# cannot expose a partial binary release.
#
# Lives here rather than inline in the workflow so the failure paths can be
# exercised without cutting a release: see scripts/test-release-guards.sh.
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  echo "usage: $0 <tag> [dist-directory]" >&2
  exit 2
fi

tag=$1
dist=${2:-dist}

targets=(
  x86_64-unknown-linux-gnu
  aarch64-unknown-linux-gnu
  x86_64-apple-darwin
  aarch64-apple-darwin
  x86_64-pc-windows-msvc
)

# Asking and answering are different failures. A command substitution that
# fails inside `[[ ]]` does not end the script even under `set -e`, so a `gh`
# error used to arrive as an empty string and get reported as a claim about
# the release's state.
if ! draft_state=$(gh release view "$tag" --json isDraft --jq .isDraft); then
  echo "Could not read release $tag; see the gh error above" >&2
  exit 1
fi
if [[ "$draft_state" != true ]]; then
  echo "Release $tag is already published; refusing to re-attach assets" >&2
  exit 1
fi

assets=()
for target in "${targets[@]}"; do
  extension=tar.gz
  [[ "$target" == x86_64-pc-windows-msvc ]] && extension=zip
  archive="${dist}/mcp-repl-${tag}-${target}.${extension}"
  # A bare test would exit through `set -e` and say nothing at all, leaving a
  # log that ends in a bare failure with no cause named anywhere.
  for required in "$archive" "$archive.sha256"; do
    if [[ ! -f "$required" ]]; then
      echo "Missing $required; the $target job produced no complete package" >&2
      exit 1
    fi
  done
  assets+=("$archive" "$archive.sha256")
done

expected_count=$((${#targets[@]} * 2))
file_count=$(find "$dist" -type f | wc -l | tr -d ' ')
if [[ "$file_count" != "$expected_count" ]]; then
  # Naming them turns "found 11" into something actionable, since the extra
  # or missing file is the whole question.
  echo "Expected exactly $expected_count release files, found $file_count:" >&2
  find "$dist" -type f | sort >&2
  exit 1
fi

(
  cd "$dist"
  # macOS ships `shasum` rather than `sha256sum`; CI has both, a maintainer
  # checking locally may not.
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum --check --strict ./*.sha256
  else
    shasum -a 256 --check ./*.sha256
  fi
)

# If an upload fails, the release stays private and a rerun safely replaces
# any assets that reached the draft before the failure.
gh release upload "$tag" "${assets[@]}" --clobber
gh release edit "$tag" --draft=false --latest
