#!/usr/bin/env bash
# Authenticate one exceptional, artifact-preserving GitHub draft recovery.
set -euo pipefail

if [[ $# -ne 2 || ( "$1" != preflight && "$1" != resume ) ]]; then
  echo "usage: $0 <preflight|resume> <github-output-file>" >&2
  exit 2
fi

phase=$1
output_file=$2
root=$(cd "$(dirname "$0")/.." && pwd)
release_targets="$root/scripts/release-targets.sh"
repository=${GITHUB_REPOSITORY:-}
source_sha=${GITHUB_SHA:-}
event_path=${GITHUB_EVENT_PATH:-}

if [[ "$repository" != joshrotenberg/mcp-repl ||
      ! "$source_sha" =~ ^[0-9a-f]{40}$ ||
      "${GITHUB_EVENT_NAME:-}" != repository_dispatch ||
      "${GITHUB_REF:-}" != refs/heads/main ||
      "${GITHUB_SERVER_URL:-}" != https://github.com ||
      "${GITHUB_ACTOR:-}" != joshrotenberg ||
      "${GITHUB_TRIGGERING_ACTOR:-}" != joshrotenberg ||
      ! -f "$event_path" || -L "$event_path" ||
      ! -f "$output_file" || -L "$output_file" ||
      "$event_path" -ef "$output_file" ]]; then
  echo "Invalid release-draft recovery environment" >&2
  exit 2
fi

checked_out_sha=$(git -C "$root" rev-parse HEAD)
if [[ "$checked_out_sha" != "$source_sha" ]]; then
  echo "Recovery checkout $checked_out_sha does not match event source $source_sha" >&2
  exit 1
fi

if ! request=$(jq -cer \
  --arg repository "$repository" '
    select(type == "object") |
    select(.action == "release_draft_recovery") |
    select(.repository.full_name == $repository) |
    select(.repository.default_branch == "main") |
    select(.sender.login == "joshrotenberg" and .sender.type == "User") |
    .client_payload as $payload |
    select(($payload | type) == "object") |
    select(($payload | keys) == [
      "release_merge_sha",
      "run_attempt",
      "run_id",
      "schema_version"
    ]) |
    select($payload.schema_version == 1) |
    select(
      ($payload.release_merge_sha | type) == "string" and
      ($payload.release_merge_sha | test("^[0-9a-f]{40}$"))
    ) |
    select(
      ($payload.run_id | type) == "number" and
      $payload.run_id == ($payload.run_id | floor) and
      $payload.run_id >= 1 and $payload.run_id <= 9007199254740991
    ) |
    select(
      ($payload.run_attempt | type) == "number" and
      $payload.run_attempt == ($payload.run_attempt | floor) and
      $payload.run_attempt >= 1 and $payload.run_attempt <= 1000
    ) |
    $payload
  ' "$event_path"); then
  echo "Release-draft recovery request is malformed or outside the trusted repository boundary" >&2
  exit 1
fi

release_merge_sha=$(jq -r '.release_merge_sha' <<<"$request")
run_id=$(jq -r '.run_id' <<<"$request")
run_attempt=$(jq -r '.run_attempt' <<<"$request")
if [[ "$release_merge_sha" == "$source_sha" ]]; then
  echo "Draft recovery must execute reviewed control-plane code newer than the release merge" >&2
  exit 1
fi
if ! git -C "$root" cat-file -e "${release_merge_sha}^{commit}" 2> /dev/null ||
   ! git -C "$root" merge-base --is-ancestor "$release_merge_sha" "$source_sha"; then
  echo "Failed release merge is not an ancestor of the recovery source" >&2
  exit 1
fi

# This controller may change only itself, the draft-aware publisher, and their
# tests/documentation. Package, build, target, attestation, and release-record
# inputs must remain byte-for-byte those that produced the retained artifact.
changed_paths=$(mktemp)
work=$(mktemp -d)
trap 'rm -f "$changed_paths"; rm -rf "$work"' EXIT INT TERM
git -C "$root" diff --no-renames --name-only -z \
  "$release_merge_sha" "$source_sha" -- > "$changed_paths"
unexpected_change=false
while IFS= read -r -d '' path; do
  case "$path" in
    .github/workflows/ci.yml | \
    .github/workflows/release-draft-recovery.yml | \
    docs/releases.md | \
    scripts/publish-release.sh | \
    scripts/test-publish-release.sh | \
    scripts/test-release-draft-recovery.sh | \
    scripts/test-release-workflow.sh | \
    scripts/verify-release-draft-recovery.sh | \
    scripts/verify-release.sh)
      ;;
    *)
      printf 'Draft recovery source changes an input outside the reviewed allowlist: %q\n' \
        "$path" >&2
      unexpected_change=true
      ;;
  esac
done < "$changed_paths"
if [[ "$unexpected_change" == true ]]; then
  echo "Cut a fresh version instead of recovering changed release inputs" >&2
  exit 1
fi

if [[ "$phase" == preflight ]]; then
  release_merge_output="$work/release-merge-output"
  : > "$release_merge_output"
  "$root/scripts/discover-release-merge.sh" \
    "$release_merge_output" "$release_merge_sha"
  if [[ $(<"$release_merge_output") != is_release_merge=true ]]; then
    echo "Draft recovery does not identify exactly one trusted release merge" >&2
    exit 1
  fi
fi

if ! run=$(gh api \
  "repos/${repository}/actions/runs/${run_id}/attempts/${run_attempt}"); then
  echo "Could not read the failed Release publish run attempt" >&2
  exit 1
fi
if ! jq -e \
  --arg repository "$repository" \
  --arg release_merge_sha "$release_merge_sha" \
  --argjson run_id "$run_id" \
  --argjson run_attempt "$run_attempt" '
    select(type == "object") |
    select(.id == $run_id and .run_attempt == $run_attempt) |
    select(.event == "push") |
    select(.status == "completed" and .conclusion == "failure") |
    select(.head_branch == "main" and .head_sha == $release_merge_sha) |
    select(.path == ".github/workflows/release-publish.yml") |
    select(.repository.full_name == $repository) |
    select(.head_repository.full_name == $repository) |
    select(.actor.login == "joshrotenberg" and .actor.type == "User") |
    select(
      .triggering_actor.login == "joshrotenberg" and
      .triggering_actor.type == "User"
    ) |
    select(
      .html_url ==
      ("https://github.com/" + $repository + "/actions/runs/" + ($run_id | tostring))
    )
  ' <<<"$run" > /dev/null; then
  echo "Run attempt does not identify the exact failed Release publish event" >&2
  exit 1
fi

# Derive every target- and platform-specific job name from the same immutable
# manifest that defined the authenticated producer topology. The allowlist
# above requires that manifest and its helper to be unchanged since the release
# merge, so current-main control code cannot widen this historical job set.
if ! native_rows=$("$release_targets" rows) ||
   ! container_platforms=$("$release_targets" container-platforms); then
  echo "Could not derive the canonical release job topology" >&2
  exit 1
fi
success_names_file="$work/success-job-names"
{
  printf '%s\n' \
    "Verify release merge" \
    "Verify package without credentials" \
    "Attempt locked crate publication" \
    "Verify published crate identity" \
    "Publish immutable binary release / Verify immutable release source" \
    "Publish immutable binary release / Build and package every target / Validate release target manifest"
  while IFS=$'\t' read -r target _archive _binary; do
    [[ -n "$target" ]] || continue
    printf 'Publish immutable binary release / Build and package every target / %s\n' \
      "$target"
    printf 'Publish immutable binary release / SBOM and attest %s\n' "$target"
  done <<<"$native_rows"
  printf '%s\n' \
    "Publish immutable binary release / Stage attested container platforms / Validate container build inputs"
  while IFS= read -r platform; do
    [[ -n "$platform" ]] || continue
    printf 'Publish immutable binary release / Stage attested container platforms / %s\n' \
      "$platform"
  done <<<"$container_platforms"
  printf '%s\n' \
    "Publish immutable binary release / Assemble and attest the container index" \
    "Publish immutable binary release / Assemble the canonical release set" \
    "Publish immutable binary release / Anonymous prepublication container smoke" \
    "Publish immutable binary release / Publish the immutable container version"
} > "$success_names_file"
if ! success_names=$(jq -Rsc 'split("\n")[:-1]' "$success_names_file") ||
   ! jq -e 'length == 27 and (unique | length) == 27' \
     <<<"$success_names" > /dev/null; then
  echo "Canonical release manifest did not produce the exact 27 successful jobs" >&2
  exit 1
fi

if ! jobs=$(gh api \
  "repos/${repository}/actions/runs/${run_id}/attempts/${run_attempt}/jobs?per_page=100"); then
  echo "Could not read jobs for the failed Release publish run attempt" >&2
  exit 1
fi
if ! jq -e \
  --arg release_merge_sha "$release_merge_sha" \
  --argjson run_id "$run_id" \
  --argjson run_attempt "$run_attempt" \
  --argjson success_names "$success_names" '
    def failed_name:
      "Publish immutable binary release / Publish the complete immutable GitHub release";
    def skipped_names: [
      "Publish immutable binary release / Reconcile the mutable latest container alias",
      "Publish immutable binary release / Anonymous public release smoke"
    ];
    def exact_step($job; $step; $conclusion):
      [.jobs[] | select(.name == $job) | .steps[] |
        select(
          .name == $step and .status == "completed" and
          .conclusion == $conclusion
        )] | length == 1;
    select(type == "object") |
    select(.total_count == 30) |
    select((.jobs | type) == "array" and (.jobs | length) == 30) |
    select(all(.jobs[];
      .run_id == $run_id and .run_attempt == $run_attempt and
      .head_sha == $release_merge_sha and
      .workflow_name == "Release publish" and .status == "completed" and
      (.steps | type) == "array"
    )) |
    select(
      ([.jobs[] | select(.conclusion == "success") | .name] | sort) ==
      ($success_names | sort)
    ) |
    select(
      ([.jobs[] | select(.conclusion == "failure") | .name] | sort) ==
      [failed_name]
    ) |
    select(
      ([.jobs[] | select(.conclusion == "skipped") | .name] | sort) ==
      (skipped_names | sort)
    ) |
    select(all(.jobs[] | select(.conclusion == "success");
      all(.steps[]; .status == "completed" and
        (.conclusion == "success" or .conclusion == "skipped")))) |
    select(all(.jobs[] | select(.conclusion == "skipped");
      (.steps | length) == 0)) |
    select(
      [.jobs[] | .steps[] | select(.conclusion == "failure") | .name] ==
      ["Create the tag, stage the exact set, and finalize once"]
    ) |
    select(exact_step(
      "Verify published crate identity";
      "Reconcile the exact crates.io package";
      "success"
    )) |
    select(exact_step(
      "Publish immutable binary release / Assemble the canonical release set";
      "Preserve the exact canonical release set";
      "success"
    )) |
    select(exact_step(
      "Publish immutable binary release / Publish the immutable container version";
      "Publish or verify the exact version image";
      "success"
    ))
  ' <<<"$jobs" > /dev/null; then
  echo "Failed run does not have the exact recoverable draft-publication topology" >&2
  exit 1
fi

if ! artifacts=$(gh api \
  "repos/${repository}/actions/runs/${run_id}/artifacts?per_page=100"); then
  echo "Could not read artifacts for the failed Release publish run" >&2
  exit 1
fi
artifact_name="release-supply-$run_attempt"
if ! artifact=$(jq -ce \
  --arg artifact_name "$artifact_name" \
  --arg release_merge_sha "$release_merge_sha" \
  --argjson run_id "$run_id" '
    select(type == "object") |
    select((.total_count | type) == "number" and .total_count <= 100) |
    select((.artifacts | type) == "array" and
      (.artifacts | length) == .total_count) |
    [.artifacts[] | select(.name | startswith("release-supply-"))] as $sets |
    select(($sets | length) == 1) |
    $sets[0] |
    select(.name == $artifact_name and .expired == false) |
    select(.id | type == "number" and . > 0 and floor == .) |
    select(.size_in_bytes | type == "number" and . > 0 and floor == .) |
    select(.digest | type == "string" and test("^sha256:[0-9a-f]{64}$")) |
    select(.workflow_run.id == $run_id) |
    select(.workflow_run.head_branch == "main") |
    select(.workflow_run.head_sha == $release_merge_sha)
  ' <<<"$artifacts"); then
  echo "Run does not retain one exact unexpired canonical release-set artifact" >&2
  exit 1
fi
artifact_id=$(jq -r '.id' <<<"$artifact")
artifact_digest=$(jq -r '.digest' <<<"$artifact")

cargo_manifest="$work/Cargo.toml"
if ! git -C "$root" show "${release_merge_sha}:Cargo.toml" > "$cargo_manifest"; then
  echo "Could not read Cargo metadata from the release merge" >&2
  exit 1
fi
version=$(awk '
  /^\[package\][[:space:]]*$/ { in_package = 1; next }
  /^\[/ { in_package = 0 }
  in_package && /^[[:space:]]*version[[:space:]]*=/ {
    if (match($0, /"[^"]+"/)) print substr($0, RSTART + 1, RLENGTH - 2)
  }
' "$cargo_manifest")
if [[ ! "$version" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ||
      $(grep -Ec '^[[:space:]]*version[[:space:]]*=' "$cargo_manifest") -lt 1 ]]; then
  echo "Release merge has invalid package version metadata" >&2
  exit 1
fi
release_tag="v$version"
expected_notes_file="$work/expected-notes.md"
release_body_file="$work/release-body.md"
if ! "$root/scripts/extract-release-notes.sh" "$version" > "$expected_notes_file"; then
  echo "Could not derive canonical release notes for $release_tag" >&2
  exit 1
fi

owner=${repository%%/*}
repository_name=${repository#*/}
# GitHub's release-by-tag REST endpoint hides private drafts. GraphQL can find
# one, but its singular field alone cannot prove that two same-tag drafts do
# not exist. Bind it to exactly one match across the complete authenticated
# REST release listing before trusting the numeric identity.
# These dollar-prefixed names belong to GraphQL, not the shell.
# shellcheck disable=SC2016
release_query='query($owner:String!,$name:String!,$tag:String!){repository(owner:$owner,name:$name){release(tagName:$tag){databaseId tagName}}}'
if ! graph_release=$(gh api graphql \
  -f "query=$release_query" \
  -F "owner=$owner" \
  -F "name=$repository_name" \
  -F "tag=$release_tag"); then
  echo "Could not discover the GraphQL identity for GitHub release $release_tag" >&2
  exit 1
fi
if ! graph_release_id=$(jq -er \
  --arg tag "$release_tag" '
    select(type == "object") |
    select((has("errors") | not) or .errors == null or .errors == []) |
    .data.repository.release |
    select(type == "object" and .tagName == $tag) |
    .databaseId |
    select(type == "number" and . > 0 and floor == .)
  ' <<<"$graph_release"); then
  echo "GitHub returned an invalid GraphQL recovery release identity" >&2
  exit 1
fi
if ! release_pages=$(gh api --paginate --slurp \
  "repos/${repository}/releases?per_page=100"); then
  echo "Could not enumerate GitHub releases while binding $release_tag" >&2
  exit 1
fi
if ! release_id=$(jq -er \
  --arg tag "$release_tag" \
  --argjson graph_id "$graph_release_id" '
    select(
      type == "array" and
      all(.[];
        type == "array" and
        all(.[];
          type == "object" and
          (.id | type) == "number" and .id > 0 and
          .id == (.id | floor) and
          (.tag_name | type) == "string"))
    ) |
    [ .[][] | select(.tag_name == $tag) ] |
    select(length == 1) | .[0].id |
    select(type == "number" and . > 0 and floor == . and . == $graph_id)
  ' <<<"$release_pages"); then
  echo "GraphQL and the complete REST release list do not bind one exact $release_tag identity" >&2
  exit 1
fi
if ! release=$(gh api "repos/${repository}/releases/${release_id}"); then
  echo "Could not read exact GitHub release $release_tag (id $release_id)" >&2
  exit 1
fi
if ! jq -e \
  --arg tag "$release_tag" \
  --argjson release_id "$release_id" '
    select(type == "object") |
    select(.id == $release_id and .tag_name == $tag and .name == $tag) |
    select(.target_commitish == "main" and .prerelease == false) |
    select(.author.login == "github-actions[bot]" and .author.type == "Bot") |
    select(
      (.draft == true and .immutable == false and .published_at == null) or
      (.draft == false and .immutable == true and
        (.published_at | type) == "string" and (.published_at | length) > 0)
    )
  ' <<<"$release" > /dev/null; then
  echo "GitHub release $release_tag is not the trusted resumable draft or immutable release" >&2
  exit 1
fi
if ! jq -jer '(.body // "") | select(type == "string")' \
  <<<"$release" > "$release_body_file" ||
   ! cmp -s "$release_body_file" "$expected_notes_file"; then
  echo "GitHub release $release_tag does not contain the exact canonical notes" >&2
  exit 1
fi

if ! asset_pages=$(gh api --paginate --slurp \
  "repos/${repository}/releases/${release_id}/assets?per_page=100"); then
  echo "Could not list assets on GitHub release $release_tag" >&2
  exit 1
fi
expected_names=$("$release_targets" \
  expected-release-assets "$release_tag" | jq -Rsc 'split("\n")[:-1] | sort')
if ! jq -e \
  --argjson expected "$expected_names" \
  --argjson published "$(jq -r '.draft == false' <<<"$release")" '
    select(type == "array" and all(.[]; type == "array")) |
    [ .[][] ] as $assets |
    select(($assets | length) <= ($expected | length)) |
    select(all($assets[];
      (.id | type) == "number" and .id > 0 and .id == (.id | floor) and
      (.name | type) == "string" and
      (.size | type) == "number" and .size > 0 and .size == (.size | floor) and
      .state == "uploaded"
    )) |
    ([$assets[].id] | sort) as $ids |
    select(($ids | unique | length) == ($ids | length)) |
    ([$assets[].name] | sort) as $actual |
    select(($actual | unique | length) == ($actual | length)) |
    select((($actual - $expected) | length) == 0) |
    select(($published | not) or $actual == $expected)
  ' <<<"$asset_pages" > /dev/null; then
  echo "GitHub release $release_tag has an unsafe or noncanonical asset inventory" >&2
  exit 1
fi

if ! GH_REPO="$repository" \
  "$root/scripts/verify-release-tag.sh" \
    "$release_tag" "$release_merge_sha" > /dev/null; then
  echo "GitHub release tag $release_tag does not bind the recovered source" >&2
  exit 1
fi

{
  echo "release_merge_sha=$release_merge_sha"
  echo "run_id=$run_id"
  echo "run_attempt=$run_attempt"
  echo "artifact_id=$artifact_id"
  echo "artifact_digest=$artifact_digest"
  echo "release_id=$release_id"
  echo "release_tag=$release_tag"
} >> "$output_file"
echo "Authenticated exact draft recovery for $release_tag from run $run_id attempt $run_attempt"
