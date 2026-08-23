#!/usr/bin/env bash
# Authenticate the one-shot v0.3.5 latest-alias and public-smoke recovery.
set -euo pipefail

if [[ $# -ne 2 || ( "$1" != preflight && "$1" != write ) ]]; then
  echo "usage: $0 <preflight|write> <github-output-file>" >&2
  exit 2
fi

phase=$1
output_file=$2
root=$(cd "$(dirname "$0")/.." && pwd)
release_targets="$root/scripts/release-targets.sh"
repository=${GITHUB_REPOSITORY:-}
source_sha=${GITHUB_SHA:-}
event_path=${GITHUB_EVENT_PATH:-}
current_run_id=${GITHUB_RUN_ID:-}
current_run_attempt=${GITHUB_RUN_ATTEMPT:-}

# This workflow is deliberately disposable and accepts only the stranded
# v0.3.5 publication. A future incident gets a separately reviewed controller
# and a new schema rather than widening this boundary.
expected_release_id=375116865
expected_release_merge_sha=7b51781718975772d96006f167887adb877618e7
expected_run_id=32617933653
expected_run_attempt=2
expected_release_tag=v0.3.5
expected_tag_object_sha=9a010344d30295cd74c558b1f20877fe719dda39
expected_record_asset_id=525939539
expected_record_size=7616
expected_record_digest=sha256:746b2df14a1a6d3cc8779210c3f5dd5e27691853ce231cb7842fd8b704427325
expected_source_epoch=1787459270
expected_image=ghcr.io/joshrotenberg/mcp-repl
expected_manifest_digest=sha256:3a84dbf2da546714bcd8bde7f975e1c73a2463851c6eb584c4917f293986d46c
expected_provenance_asset_id=525939528
expected_provenance_size=10996
expected_provenance_sha256=855025d566da00ff9ef19b8d8a6a907f905f9480ceda27fa1a3c4226f5db9211
expected_sbom_bundle_asset_id=525939537
expected_sbom_bundle_size=13971
expected_sbom_bundle_sha256=68be24b77e3ccce2ca0a4a8850b75b42fbe3a84a694922e0e85210b2921f592d
expected_sbom_asset_id=525939526
expected_sbom_size=3635
expected_sbom_sha256=e5cf3b6a397ec1150076700900d183860e143bc54dd9df8b2ea6e63149fcc849

if [[ "$repository" != joshrotenberg/mcp-repl ||
      ! "$source_sha" =~ ^[0-9a-f]{40}$ ||
      "${GITHUB_EVENT_NAME:-}" != repository_dispatch ||
      "${GITHUB_REF:-}" != refs/heads/main ||
      "${GITHUB_SERVER_URL:-}" != https://github.com ||
      "${GITHUB_ACTOR:-}" != joshrotenberg ||
      "${GITHUB_TRIGGERING_ACTOR:-}" != joshrotenberg ||
      "${GITHUB_WORKFLOW:-}" != "Release latest recovery" ||
      "${GITHUB_WORKFLOW_REF:-}" != \
        "joshrotenberg/mcp-repl/.github/workflows/release-latest-recovery.yml@refs/heads/main" ||
      "${GITHUB_WORKFLOW_SHA:-}" != "$source_sha" ||
      ! "$current_run_id" =~ ^[1-9][0-9]*$ ||
      ! "$current_run_attempt" =~ ^[1-9][0-9]*$ ||
      "$current_run_attempt" != 1 ||
      "${RUNNER_OS:-}" != Linux || "${RUNNER_ARCH:-}" != X64 ||
      ! -f "$event_path" || -L "$event_path" ||
      ! -f "$output_file" || -L "$output_file" ||
      "$event_path" -ef "$output_file" ]]; then
  echo "Invalid release-latest recovery environment" >&2
  exit 2
fi

for required_command in awk cat cmp docker git gh jq mktemp sha256sum tr wc; do
  command -v "$required_command" > /dev/null 2>&1 || {
    echo "release-latest recovery requires $required_command" >&2
    exit 2
  }
done

checked_out_sha=$(git -C "$root" rev-parse HEAD)
if [[ "$checked_out_sha" != "$source_sha" ]]; then
  echo "Recovery checkout $checked_out_sha does not match event source $source_sha" >&2
  exit 1
fi

if [[ "${DOCKER_CONFIG:-}" != "${RUNNER_TEMP:-}/verification-docker" ||
      ! -d "$DOCKER_CONFIG" || -L "$DOCKER_CONFIG" ||
      ! -f "$DOCKER_CONFIG/config.json" || -L "$DOCKER_CONFIG/config.json" ||
      ! -f "$DOCKER_CONFIG/cli-plugins/docker-buildx" ||
      -L "$DOCKER_CONFIG/cli-plugins/docker-buildx" ||
      ! -x "$DOCKER_CONFIG/cli-plugins/docker-buildx" ||
      $(jq -cS . "$DOCKER_CONFIG/config.json") != '{}' ||
      -n "${DOCKER_AUTH_CONFIG:-}" || -n "${REGISTRY_AUTH_FILE:-}" ]]; then
  echo "Release-latest authentication requires an isolated credential-free Buildx" >&2
  exit 2
fi

if ! request=$(jq -cer \
  --arg repository "$repository" \
  --arg release_merge_sha "$expected_release_merge_sha" \
  --argjson release_id "$expected_release_id" \
  --argjson run_id "$expected_run_id" \
  --argjson run_attempt "$expected_run_attempt" '
    select(type == "object") |
    select(.action == "release_latest_recovery") |
    select(.repository.full_name == $repository) |
    select(.repository.default_branch == "main") |
    select(.sender.login == "joshrotenberg" and .sender.type == "User") |
    .client_payload as $payload |
    select(($payload | type) == "object") |
    select(($payload | keys) == [
      "release_id",
      "release_merge_sha",
      "run_attempt",
      "run_id",
      "schema_version"
    ]) |
    select($payload.schema_version == 1) |
    select(
      ($payload.release_id | type) == "number" and
      $payload.release_id == ($payload.release_id | floor) and
      $payload.release_id == $release_id
    ) |
    select(
      ($payload.release_merge_sha | type) == "string" and
      $payload.release_merge_sha == $release_merge_sha
    ) |
    select(
      ($payload.run_id | type) == "number" and
      $payload.run_id == ($payload.run_id | floor) and
      $payload.run_id == $run_id
    ) |
    select(
      ($payload.run_attempt | type) == "number" and
      $payload.run_attempt == ($payload.run_attempt | floor) and
      $payload.run_attempt == $run_attempt
    ) |
    $payload
  ' "$event_path"); then
  echo "Release-latest recovery request is malformed or not the exact v0.3.5 incident" >&2
  exit 1
fi

release_id=$(jq -r '.release_id' <<<"$request")
release_merge_sha=$(jq -r '.release_merge_sha' <<<"$request")
run_id=$(jq -r '.run_id' <<<"$request")
run_attempt=$(jq -r '.run_attempt' <<<"$request")
if [[ "$source_sha" == "$release_merge_sha" ]] ||
   ! git -C "$root" cat-file -e "${release_merge_sha}^{commit}" 2> /dev/null ||
   ! git -C "$root" merge-base --is-ancestor "$release_merge_sha" "$source_sha"; then
  echo "Failed release merge is not a strict ancestor of the reviewed recovery source" >&2
  exit 1
fi

changed_paths=$(mktemp)
work=$(mktemp -d)
trap 'rm -f "$changed_paths"; rm -rf "$work"' EXIT
git -C "$root" diff --no-renames --name-only -z \
  "$release_merge_sha" "$source_sha" -- > "$changed_paths"
unexpected_change=false
while IFS= read -r -d '' path; do
  case "$path" in
    .github/actionlint.yaml | \
    .github/workflows/ci.yml | \
    .github/workflows/release-binaries.yml | \
    .github/workflows/release-draft-recovery.yml | \
    .github/workflows/release-latest-recovery.yml | \
    docs/releases.md | \
    scripts/publish-container-manifest.sh | \
    scripts/publish-release.sh | \
    scripts/smoke-release-latest-recovery.sh | \
    scripts/test-container-manifest.sh | \
    scripts/test-publish-release.sh | \
    scripts/test-release-draft-recovery.sh | \
    scripts/test-release-latest-recovery.sh | \
    scripts/test-release-workflow.sh | \
    scripts/verify-release-draft-recovery.sh | \
    scripts/verify-release-latest-recovery.sh | \
    scripts/verify-release.sh)
      ;;
    *)
      printf 'Latest recovery source changes an input outside the reviewed allowlist: %q\n' \
        "$path" >&2
      unexpected_change=true
      ;;
  esac
done < "$changed_paths"
if [[ "$unexpected_change" == true ]]; then
  echo "The v0.3.5 container identity cannot be recovered from changed product inputs" >&2
  exit 1
fi

if ! current_run=$(gh api \
  "repos/${repository}/actions/runs/${current_run_id}"); then
  echo "Could not read the current Release latest recovery run" >&2
  exit 1
fi
if ! jq -e \
  --arg repository "$repository" \
  --arg source_sha "$source_sha" \
  --argjson run_id "$current_run_id" \
  --argjson run_attempt "$current_run_attempt" '
    select(type == "object") |
    select(.id == $run_id and .run_attempt == $run_attempt) |
    select(.name == "Release latest recovery") |
    select(.event == "repository_dispatch") |
    select(.status == "in_progress" or
      (.status == "completed" and (.conclusion | type) == "string")) |
    select(.head_branch == "main" and .head_sha == $source_sha) |
    select(.path == ".github/workflows/release-latest-recovery.yml") |
    select(.repository.full_name == $repository) |
    select(.head_repository.full_name == $repository) |
    select(.actor.login == "joshrotenberg" and .actor.type == "User") |
    select(.triggering_actor.login == "joshrotenberg" and
      .triggering_actor.type == "User") |
    select(.head_commit.id == $source_sha) |
    select(.html_url ==
      ("https://github.com/" + $repository + "/actions/runs/" + ($run_id | tostring)))
  ' <<<"$current_run" > /dev/null; then
  echo "Current run is not the exact default-branch latest-recovery controller" >&2
  exit 1
fi

release_merge_output="$work/release-merge-output"
: > "$release_merge_output"
"$root/scripts/discover-release-merge.sh" \
  "$release_merge_output" "$release_merge_sha"
if [[ $(<"$release_merge_output") != is_release_merge=true ]]; then
  echo "Latest recovery does not identify exactly one trusted release merge" >&2
  exit 1
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
    select(.name == "Release publish" and .run_number == 22) |
    select(.event == "push") |
    select(.status == "completed" and .conclusion == "failure") |
    select(.head_branch == "main" and .head_sha == $release_merge_sha) |
    select(.path == ".github/workflows/release-publish.yml") |
    select(.workflow_id == 340084389) |
    select(.repository.full_name == $repository) |
    select(.head_repository.full_name == $repository) |
    select(.actor.login == "joshrotenberg" and .actor.type == "User") |
    select(
      .triggering_actor.login == "joshrotenberg" and
      .triggering_actor.type == "User"
    ) |
    select(.head_commit.id == $release_merge_sha) |
    select(
      .html_url ==
      ("https://github.com/" + $repository + "/actions/runs/" + ($run_id | tostring))
    )
  ' <<<"$run" > /dev/null; then
  echo "Run attempt does not identify the exact failed v0.3.5 Release publish event" >&2
  exit 1
fi

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
    "Publish immutable binary release / Publish the immutable container version" \
    "Publish immutable binary release / Publish the complete immutable GitHub release"
} > "$success_names_file"
if ! success_names=$(jq -Rsc 'split("\n")[:-1]' "$success_names_file") ||
   ! jq -e 'length == 28 and (unique | length) == 28' \
     <<<"$success_names" > /dev/null; then
  echo "Canonical release manifest did not produce the exact 28 successful jobs" >&2
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
      "Publish immutable binary release / Reconcile the mutable latest container alias";
    def skipped_name:
      "Publish immutable binary release / Anonymous public release smoke";
    def exact_step($job; $step; $conclusion):
      [.jobs[] | select(.name == $job) | .steps[] |
        select(
          .name == $step and .status == "completed" and
          .conclusion == $conclusion
        )] | length == 1;
    def failed_steps: [
      {name:"Set up job",status:"completed",conclusion:"success",number:1},
      {name:"Run actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",status:"completed",conclusion:"success",number:2},
      {name:"Install checksum-pinned Buildx",status:"completed",conclusion:"success",number:3},
      {name:"Run docker/setup-buildx-action@37fe631027851001ddb9b187196cc803df7f5f0e",status:"completed",conclusion:"success",number:4},
      {name:"Log in only for latest reconciliation",status:"completed",conclusion:"success",number:5},
      {name:"Reconcile latest to GitHub\u0027s immutable latest release",status:"completed",conclusion:"failure",number:6},
      {name:"Post Log in only for latest reconciliation",status:"completed",conclusion:"success",number:10},
      {name:"Post Run docker/setup-buildx-action@37fe631027851001ddb9b187196cc803df7f5f0e",status:"completed",conclusion:"success",number:11},
      {name:"Post Run actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",status:"completed",conclusion:"success",number:12},
      {name:"Complete job",status:"completed",conclusion:"success",number:13}
    ];
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
      [.jobs[] | select(.conclusion == "failure") | .name] == [failed_name]
    ) |
    select(
      [.jobs[] | select(.conclusion == "skipped") | .name] == [skipped_name]
    ) |
    select(all(.jobs[] | select(.conclusion == "success");
      (.steps | length) > 0 and
      all(.steps[]; .status == "completed" and
        (.conclusion == "success" or .conclusion == "skipped")))) |
    select(
      [.jobs[] | select(.name == failed_name) | .steps |
        map({name,status,conclusion,number})] == [failed_steps]
    ) |
    select(
      [.jobs[] | select(.name == skipped_name) | .steps] == [[]]
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
    )) |
    select(exact_step(
      "Publish immutable binary release / Publish the complete immutable GitHub release";
      "Create the tag, stage the exact set, and finalize once";
      "success"
    ))
  ' <<<"$jobs" > /dev/null; then
  echo "Failed run does not have the exact recoverable v0.3.5 latest-only topology" >&2
  exit 1
fi

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
release_tag="v$version"
if [[ "$release_tag" != "$expected_release_tag" ]]; then
  echo "Release merge is not the exact v0.3.5 source" >&2
  exit 1
fi

expected_notes_file="$work/expected-notes.md"
release_body_file="$work/release-body.md"
if ! "$root/scripts/extract-release-notes.sh" "$version" > "$expected_notes_file"; then
  echo "Could not derive canonical release notes for $release_tag" >&2
  exit 1
fi
trusted_exact_tag_ref() {
  local response normalized
  if ! response=$(gh api "repos/${repository}/git/ref/tags/${release_tag}"); then
    echo "Could not read the exact annotated v0.3.5 tag ref" >&2
    return 1
  fi
  if ! normalized=$(jq -cer \
    --arg tag "$release_tag" \
    --arg tag_object_sha "$expected_tag_object_sha" '
      select(type == "object") |
      select(.ref == ("refs/tags/" + $tag)) |
      select(.object.type == "tag" and .object.sha == $tag_object_sha) |
      {ref, type:.object.type, sha:.object.sha}
    ' <<<"$response"); then
    echo "GitHub tag ref does not identify the exact annotated v0.3.5 tag object" >&2
    return 1
  fi
  printf '%s\n' "$normalized"
}
tag_ref_before=$(trusted_exact_tag_ref)
if ! GH_REPO="$repository" \
  "$root/scripts/verify-release-tag.sh" \
    "$release_tag" "$release_merge_sha" > /dev/null; then
  echo "GitHub release tag $release_tag does not bind the recovered source" >&2
  exit 1
fi

if ! release=$(gh api "repos/${repository}/releases/${release_id}"); then
  echo "Could not read immutable GitHub release $release_tag" >&2
  exit 1
fi
if ! jq -e \
  --arg tag "$release_tag" \
  --argjson release_id "$release_id" '
    select(type == "object") |
    select(.id == $release_id and .tag_name == $tag and .name == $tag) |
    select(.target_commitish == "main") |
    select(.draft == false and .prerelease == false and .immutable == true) |
    select(.author.login == "github-actions[bot]" and .author.type == "Bot") |
    select(.published_at | type == "string" and length > 0)
  ' <<<"$release" > /dev/null ||
   ! jq -jer '(.body // "") | select(type == "string")' \
     <<<"$release" > "$release_body_file" ||
   ! cmp -s "$release_body_file" "$expected_notes_file"; then
  echo "GitHub release $release_tag is not the exact immutable bot-owned release" >&2
  exit 1
fi

trusted_latest_snapshot() {
  local latest normalized
  if ! latest=$(gh api "repos/${repository}/releases/latest"); then
    echo "Could not read GitHub's latest release" >&2
    return 1
  fi
  if ! normalized=$(jq -cer \
    --arg tag "$release_tag" \
    --argjson release_id "$release_id" '
      select(type == "object") |
      select(.id == $release_id and .tag_name == $tag and .name == $tag) |
      select(.target_commitish == "main") |
      select(.draft == false and .prerelease == false and .immutable == true) |
      select(.author.login == "github-actions[bot]" and .author.type == "Bot") |
      select(.published_at | type == "string" and length > 0) |
      {id, tag: .tag_name, published_at}
    ' <<<"$latest"); then
    echo "GitHub's latest release is not exact immutable $release_tag" >&2
    return 1
  fi
  printf '%s\n' "$normalized"
}

latest_before=$(trusted_latest_snapshot)

if ! asset_pages=$(gh api --paginate --slurp \
  "repos/${repository}/releases/${release_id}/assets?per_page=100"); then
  echo "Could not list assets on immutable GitHub release $release_tag" >&2
  exit 1
fi
expected_names=$("$release_targets" expected-release-assets "$release_tag" |
  jq -Rsc 'split("\n")[:-1] | sort')
if ! assets=$(jq -cer --argjson expected "$expected_names" '
    select(type == "array" and all(.[]; type == "array")) |
    [.[][]] as $assets |
    select(($assets | length) == 39) |
    select(all($assets[];
      type == "object" and
      (.id | type) == "number" and .id > 0 and .id == (.id | floor) and
      (.name | type) == "string" and
      (.size | type) == "number" and .size > 0 and .size == (.size | floor) and
      (.digest | type == "string" and test("^sha256:[0-9a-f]{64}$")) and
      .state == "uploaded" and
      .content_type == "application/octet-stream" and .label == null and
      .uploader.login == "github-actions[bot]" and .uploader.type == "Bot"
    )) |
    select(([$assets[].id] | unique | length) == 39) |
    select(([$assets[].name] | unique | length) == 39) |
    select(([$assets[].name] | sort) == $expected) |
    $assets
  ' <<<"$asset_pages"); then
  echo "GitHub release $release_tag does not have the exact trusted 39-asset inventory" >&2
  exit 1
fi

require_asset() {
  local name=$1 expected_id=$2 expected_size=$3 expected_digest=$4 label=$5
  local identity
  if ! identity=$(jq -cer \
    --arg name "$name" \
    --argjson id "$expected_id" \
    --argjson size "$expected_size" \
    --arg digest "$expected_digest" '
      [.[] | select(.name == $name)] |
      select(length == 1) | .[0] |
      select(.id == $id and .size == $size and .digest == $digest) |
      {id,name,size,digest}
    ' <<<"$assets"); then
    echo "$label does not have its exact immutable asset identity" >&2
    return 1
  fi
  REQUIRED_ASSET=$identity
}

record_name="mcp-repl-$release_tag-release.json"
provenance_name="mcp-repl-$release_tag-container.provenance.sigstore.json"
sbom_bundle_name="mcp-repl-$release_tag-container.sbom.sigstore.json"
sbom_name="mcp-repl-$release_tag-container.spdx.json"
require_asset "$record_name" "$expected_record_asset_id" \
  "$expected_record_size" "$expected_record_digest" "canonical release record"
record_identity=$REQUIRED_ASSET
require_asset "$provenance_name" "$expected_provenance_asset_id" \
  "$expected_provenance_size" "sha256:$expected_provenance_sha256" \
  "container provenance bundle"
provenance_identity=$REQUIRED_ASSET
require_asset "$sbom_bundle_name" "$expected_sbom_bundle_asset_id" \
  "$expected_sbom_bundle_size" "sha256:$expected_sbom_bundle_sha256" \
  "container SBOM bundle"
sbom_bundle_identity=$REQUIRED_ASSET
require_asset "$sbom_name" "$expected_sbom_asset_id" \
  "$expected_sbom_size" "sha256:$expected_sbom_sha256" \
  "container SPDX document"
sbom_identity=$REQUIRED_ASSET

sha256_file() {
  local path=$1 digest
  digest=$(sha256sum "$path" | awk '{print $1}')
  [[ "$digest" =~ ^[0-9a-f]{64}$ ]] || return 1
  printf 'sha256:%s\n' "$digest"
}

download_asset() {
  local identity=$1 label=$2
  local id name size digest path actual_size actual_digest
  id=$(jq -r '.id' <<<"$identity")
  name=$(jq -r '.name' <<<"$identity")
  size=$(jq -r '.size' <<<"$identity")
  digest=$(jq -r '.digest' <<<"$identity")
  path="$work/$name"
  if [[ ! "$name" =~ ^[A-Za-z0-9][A-Za-z0-9._-]*$ ]] ||
     ! gh api "repos/${repository}/releases/assets/${id}" \
       -H 'Accept: application/octet-stream' > "$path"; then
    echo "Could not download $label by immutable asset ID" >&2
    return 1
  fi
  if [[ ! -f "$path" || -L "$path" || ! -s "$path" ]]; then
    echo "$label download is not one regular nonempty file" >&2
    return 1
  fi
  actual_size=$(wc -c < "$path" | tr -d '[:space:]')
  actual_digest=$(sha256_file "$path")
  if [[ "$actual_size" != "$size" || "$actual_digest" != "$digest" ]]; then
    echo "$label differs from immutable release metadata" >&2
    return 1
  fi
  DOWNLOADED_ASSET=$path
}

download_asset "$record_identity" "canonical release record"
record_path=$DOWNLOADED_ASSET
download_asset "$provenance_identity" "container provenance bundle"
provenance_path=$DOWNLOADED_ASSET
download_asset "$sbom_bundle_identity" "container SBOM bundle"
sbom_bundle_path=$DOWNLOADED_ASSET
download_asset "$sbom_identity" "container SPDX document"
sbom_path=$DOWNLOADED_ASSET

canonical_record="$work/canonical-release-record.json"
if ! jq -cS . "$record_path" > "$canonical_record" 2> /dev/null ||
   ! cmp -s "$record_path" "$canonical_record"; then
  echo "Canonical release record is not compact sorted JSON" >&2
  exit 1
fi
expected_platform_rows=$(jq -cn '[
  {
    platform:"linux/amd64",
    runnable_digest:"sha256:8f31eb764fea23b4491b4aa08566949a6be5fde0b3aa7c8b529b0ec0559806a9"
  },
  {
    platform:"linux/arm64",
    runnable_digest:"sha256:14fb0771b1e2333c492fde114920eaa2149f823771d27fd95394651261d8e7d8"
  }
]')
if ! record=$(jq -cer \
  --arg release_tag "$release_tag" \
  --arg release_merge_sha "$release_merge_sha" \
  --arg image "$expected_image" \
  --arg manifest_digest "$expected_manifest_digest" \
  --argjson source_epoch "$expected_source_epoch" \
  --argjson expected_platforms "$expected_platform_rows" \
  --arg provenance_name "$provenance_name" \
  --arg provenance_sha256 "$expected_provenance_sha256" \
  --argjson provenance_size "$expected_provenance_size" \
  --arg sbom_bundle_name "$sbom_bundle_name" \
  --arg sbom_bundle_sha256 "$expected_sbom_bundle_sha256" \
  --argjson sbom_bundle_size "$expected_sbom_bundle_size" \
  --arg sbom_name "$sbom_name" \
  --arg sbom_sha256 "$expected_sbom_sha256" \
  --argjson sbom_size "$expected_sbom_size" '
    def identity($name; $size; $sha256):
      type == "object" and
      ((keys | sort) == (["name", "sha256", "size"] | sort)) and
      .name == $name and .size == $size and .sha256 == $sha256;
    select(type == "object") |
    select((keys | sort) == ([
      "container", "native", "package", "release_targets", "schema_version",
      "source_epoch", "source_sha", "tag", "version"
    ] | sort)) |
    select(.schema_version == 1 and .package == "mcp-repl") |
    select(.tag == $release_tag and .version == "0.3.5") |
    select(.source_sha == $release_merge_sha and .source_epoch == $source_epoch) |
    select(.release_targets == {
      name:"release-targets.json",
      sha256:"7517f938a08147aa4dab9cd17748e8eabe429058ce38960431d3966f18335956",
      size:4940
    }) |
    select((.native | type) == "array" and (.native | length) == 7) |
    select((.container | keys | sort) == ([
      "attestations", "image", "manifest_digest", "platforms", "sbom"
    ] | sort)) |
    select(.container.image == $image and
      .container.manifest_digest == $manifest_digest and
      .container.platforms == $expected_platforms) |
    select(.container.attestations.provenance |
      identity($provenance_name; $provenance_size; $provenance_sha256)) |
    select(.container.attestations.sbom |
      identity($sbom_bundle_name; $sbom_bundle_size; $sbom_bundle_sha256)) |
    select(.container.sbom | identity($sbom_name; $sbom_size; $sbom_sha256)) |
    {
      manifest_digest: .container.manifest_digest,
      platforms: .container.platforms
    }
  ' "$record_path"); then
  echo "Canonical release record does not contain the exact v0.3.5 container identity" >&2
  exit 1
fi

# The compact record is the normative byte identity for every durable release
# asset. Flatten all 35 native identities plus the three container identities,
# add the record itself, and compare complete name/size/digest tuples with the
# immutable GitHub inventory. Finding only the four recovery-relevant files is
# insufficient because an extra or replaced native asset would invalidate the
# canonical release that authorizes this registry write.
if ! record_asset_tuples=$(jq -cer \
  --arg record_name "$record_name" \
  --argjson record_size "$expected_record_size" \
  --arg record_digest "$expected_record_digest" '
    def identity:
      type == "object" and
      ((keys | sort) == (["name", "sha256", "size"] | sort)) and
      (.name | type == "string" and length > 0) and
      (.size | type == "number" and . > 0 and . == floor) and
      (.sha256 | type == "string" and test("^[0-9a-f]{64}$"));
    ([.native[] |
      .archive,
      .checksum,
      .sbom,
      .attestations.provenance,
      .attestations.sbom
    ] + [
      .container.sbom,
      .container.attestations.provenance,
      .container.attestations.sbom
    ]) as $recorded |
    select(($recorded | length) == 38 and all($recorded[]; identity)) |
    ($recorded | map({name,size,digest:("sha256:" + .sha256)})) + [{
      name:$record_name,
      size:$record_size,
      digest:$record_digest
    }] |
    select(length == 39) |
    select((map(.name) | unique | length) == 39) |
    sort_by(.name)
  ' "$record_path") ||
   ! remote_asset_tuples=$(jq -cer '
      map({name,size,digest}) | sort_by(.name)
    ' <<<"$assets") ||
   [[ "$remote_asset_tuples" != "$record_asset_tuples" ]]; then
  echo "Immutable GitHub assets differ from the canonical 39-file release record" >&2
  exit 1
fi

if ! jq -e '
    select(type == "object") |
    select(.spdxVersion == "SPDX-2.3") |
    select(.dataLicense == "CC0-1.0")
  ' "$sbom_path" > /dev/null; then
  echo "Container SPDX document is not an SPDX 2.3 document" >&2
  exit 1
fi

trusted_root="$work/trusted-root.jsonl"
if ! gh attestation trusted-root > "$trusted_root" ||
   [[ ! -f "$trusted_root" || -L "$trusted_root" || ! -s "$trusted_root" ]]; then
  echo "Could not load the Sigstore trusted root" >&2
  exit 1
fi
public_index="$work/v035-index.json"
if [[ $(docker buildx version) != "github.com/docker/buildx v0.36.1 "* ]] ||
   ! docker buildx imagetools inspect --raw \
     "$expected_image@$expected_manifest_digest" > "$public_index" ||
   [[ ! -f "$public_index" || -L "$public_index" || ! -s "$public_index" ]] ||
   [[ $(sha256_file "$public_index") != "$expected_manifest_digest" ]] ||
   ! public_mapping=$(jq -cer \
     -f "$root/scripts/container-runnable-mapping.jq" "$public_index") ||
   [[ "$public_mapping" != "$expected_platform_rows" ]]; then
  echo "Public immutable v0.3.5 index differs from its canonical release record" >&2
  exit 1
fi
common=(
  --repo "$repository"
  --custom-trusted-root "$trusted_root"
  --signer-workflow "$repository/.github/workflows/release-binaries.yml"
  --signer-digest "$release_merge_sha"
  --source-digest "$release_merge_sha"
  --source-ref refs/heads/main
  --deny-self-hosted-runners
)
if ! gh attestation verify "$public_index" \
  --bundle "$provenance_path" \
  --predicate-type https://slsa.dev/provenance/v1 \
  "${common[@]}" > /dev/null; then
  echo "Container provenance bundle failed trusted verification" >&2
  exit 1
fi
if ! gh attestation verify "$public_index" \
  --bundle "$sbom_bundle_path" \
  --predicate-type https://spdx.dev/Document/v2.3 \
  "${common[@]}" > /dev/null; then
  echo "Container SBOM bundle failed trusted verification" >&2
  exit 1
fi

# Re-read both latest selection and the annotated tag after the expensive
# record/bundle authentication. A second read-only write-boundary job repeats
# this entire controller before the write-scoped job starts, and the publisher
# independently repeats the live release checks around the only mutation.
if ! latest_after=$(trusted_latest_snapshot) ||
   ! tag_ref_after=$(trusted_exact_tag_ref); then
  echo "GitHub's exact immutable latest release changed during authentication" >&2
  exit 1
fi
if [[ "$latest_after" != "$latest_before" ||
      "$tag_ref_after" != "$tag_ref_before" ]] ||
   ! GH_REPO="$repository" \
     "$root/scripts/verify-release-tag.sh" \
       "$release_tag" "$release_merge_sha" > /dev/null; then
  echo "GitHub's exact immutable latest release changed during authentication" >&2
  exit 1
fi
if [[ $(jq -cS . "$DOCKER_CONFIG/config.json") != '{}' ]]; then
  echo "Credential-free immutable-index verification acquired registry credentials" >&2
  exit 1
fi

if ! container_config=$("$release_targets" container-build) ||
   ! build_inputs=$(jq -cer '
      select(type == "object") |
      select(.buildkit_image | type == "string" and
        test("^docker\\.io/moby/buildkit:v[0-9]+\\.[0-9]+\\.[0-9]+@sha256:[0-9a-f]{64}$")) |
      select(.buildx_version | type == "string" and
        test("^v[0-9]+\\.[0-9]+\\.[0-9]+$")) |
      select(.buildx_sha256["linux/amd64"] | type == "string" and
        test("^[0-9a-f]{64}$")) |
      {
        buildkit_image,
        buildx_asset:("buildx-" + .buildx_version + ".linux-amd64"),
        buildx_sha256:.buildx_sha256["linux/amd64"]
      }
    ' <<<"$container_config"); then
  echo "Could not authenticate current-main Buildx inputs" >&2
  exit 1
fi
buildkit_image=$(jq -r '.buildkit_image' <<<"$build_inputs")
buildx_asset=$(jq -r '.buildx_asset' <<<"$build_inputs")
buildx_sha256=$(jq -r '.buildx_sha256' <<<"$build_inputs")
record_asset_digest=$(jq -r '.digest' <<<"$record_identity")
container_platform_rows=$(jq -c '.platforms' <<<"$record")

output_temp="$work/github-output"
{
  echo "source_sha=$source_sha"
  echo "release_merge_sha=$release_merge_sha"
  echo "run_id=$run_id"
  echo "run_attempt=$run_attempt"
  echo "release_id=$release_id"
  echo "release_tag=$release_tag"
  echo "record_asset_id=$expected_record_asset_id"
  echo "record_digest=$record_asset_digest"
  echo "image=$expected_image"
  echo "source_epoch=$expected_source_epoch"
  echo "container_digest=$expected_manifest_digest"
  echo "container_platforms=$container_platform_rows"
  echo "buildkit_image=$buildkit_image"
  echo "buildx_asset=$buildx_asset"
  echo "buildx_sha256=$buildx_sha256"
} > "$output_temp"
cat "$output_temp" >> "$output_file"

echo "Authenticated exact v0.3.5 latest recovery from run $run_id attempt $run_attempt ($phase)"
